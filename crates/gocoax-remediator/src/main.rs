//! gocoax-remediator: polls Prometheus for problematic MoCA adapters (per
//! configured rules) and reboots them, subject to a cooldown + circuit
//! breaker, exposing its own reboot history as `/metrics`.
//!
//! This binary is the only impure layer: it owns the clock (`SystemTime`),
//! the shared mutable state, the poll loop, and the axum server. Everything
//! it calls into (`config`, `prom`, `state`, `metrics`) is pure/testable on
//! its own.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{routing::get, Router};
use clap::Parser;

use gocoax::config::{Config, Device};
use gocoax::{Client, ClientOpts};

use gocoax_remediator::config::{load, RemediatorConfig};
use gocoax_remediator::metrics::{render, MetricsSnapshot};
use gocoax_remediator::prom::query_devices;
use gocoax_remediator::state::{decide, record_reboot, Decision, DeviceState, Limits};

#[derive(Parser)]
struct Cli {
    #[arg(long)]
    config: String,
}

/// Mutable counters/state, guarded by a single mutex: one poll-loop writer,
/// occasional `/metrics` readers. Contention is not a concern at this scale.
#[derive(Default)]
struct Inner {
    devices: HashMap<String, DeviceState>,
    reboots_total: HashMap<(String, String), u64>,
    would_reboot_total: HashMap<(String, String), u64>,
    last_reboot_ts: HashMap<String, f64>,
    circuit_open: HashMap<String, bool>,
}

struct AppState {
    cfg: Config,
    rcfg: RemediatorConfig,
    /// Client used only for Prometheus queries (not device reboots -- those
    /// need per-device credentials, so a `gocoax::Client` is built on demand
    /// in `reboot_device`).
    http: reqwest::Client,
    inner: Mutex<Inner>,
}

impl AppState {
    fn snapshot(&self) -> MetricsSnapshot {
        let inner = self.inner.lock().unwrap();
        MetricsSnapshot {
            reboots_total: inner.reboots_total.iter().map(|((d, r), c)| (d.clone(), r.clone(), *c)).collect(),
            would_reboot_total: inner
                .would_reboot_total
                .iter()
                .map(|((d, r), c)| (d.clone(), r.clone(), *c))
                .collect(),
            last_reboot_ts: inner.last_reboot_ts.iter().map(|(d, t)| (d.clone(), *t)).collect(),
            circuit_open: inner.circuit_open.iter().map(|(d, o)| (d.clone(), *o)).collect(),
        }
    }
}

/// A coarse "day bucket" used purely to detect day rollovers for the
/// circuit breaker's daily counter -- an integer count of days since the
/// Unix epoch (UTC), rendered as a string. This sidesteps pulling in a date
/// library (none is a declared dependency) while still being consistent
/// between `decide`/`record_reboot` calls within the same poll and stable
/// across polls within the same UTC day.
fn day_bucket(now: SystemTime) -> String {
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    (secs / 86_400).to_string()
}

/// Build a fresh `gocoax::Client` for `dev` and call `reboot()`. Built on
/// demand (rather than cached) since reboots are rare and this keeps
/// `AppState` free of per-device client bookkeeping.
async fn reboot_device(cfg: &Config, dev: &Device) -> gocoax::Result<()> {
    let creds = cfg.creds_for(dev)?;
    let opts = ClientOpts {
        request_timeout: Duration::from_secs(cfg.request_timeout_secs),
        connect_timeout: Duration::from_secs(cfg.connect_timeout_secs),
    };
    let client = Client::new(&dev.host, creds, opts)?;
    client.reboot().await
}

/// Run one poll: evaluate every rule, decide, and act. A device that
/// matches multiple rules in the same poll only reboots once -- tracked via
/// `handled_this_poll` (needed because `dry_run` deliberately never calls
/// `record_reboot`, so the cooldown alone wouldn't catch a same-poll repeat
/// in that mode).
async fn poll_once(state: &AppState) {
    let now = SystemTime::now();
    let now_unix = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64();
    let today = day_bucket(now);
    let limits = Limits { cooldown_secs: state.rcfg.cooldown_secs, max_reboots_per_day: state.rcfg.max_reboots_per_day };

    let mut handled_this_poll: HashSet<String> = HashSet::new();

    for rule in &state.rcfg.rule {
        let devices = match query_devices(&state.http, &state.rcfg.prometheus_url, &rule.expr).await {
            Ok(devices) => devices,
            Err(e) => {
                eprintln!("gocoax-remediator: warning: rule '{}' query failed: {e}", rule.name);
                continue;
            }
        };

        for device_name in devices {
            if handled_this_poll.contains(&device_name) {
                continue;
            }

            let Some(dev) = state.cfg.device.iter().find(|d| d.name == device_name) else {
                eprintln!(
                    "gocoax-remediator: warning: device '{device_name}' (from rule '{}') not found in config, skipping",
                    rule.name
                );
                continue;
            };

            let decision = {
                let inner = state.inner.lock().unwrap();
                let st = inner.devices.get(&device_name).cloned().unwrap_or_default();
                decide(&st, &limits, now_unix, &today)
            };

            match decision {
                Decision::Reboot => {
                    handled_this_poll.insert(device_name.clone());
                    if state.rcfg.dry_run {
                        let mut inner = state.inner.lock().unwrap();
                        *inner.would_reboot_total.entry((device_name.clone(), rule.name.clone())).or_insert(0) += 1;
                        println!("gocoax-remediator: would reboot device={device_name} reason={}", rule.name);
                    } else {
                        match reboot_device(&state.cfg, dev).await {
                            Ok(()) => {
                                let mut inner = state.inner.lock().unwrap();
                                *inner.reboots_total.entry((device_name.clone(), rule.name.clone())).or_insert(0) += 1;
                                inner.last_reboot_ts.insert(device_name.clone(), now_unix);
                                let dst = inner.devices.entry(device_name.clone()).or_default();
                                record_reboot(dst, now_unix, &today);
                                inner.circuit_open.insert(device_name.clone(), false);
                                println!("gocoax-remediator: rebooted device={device_name} reason={}", rule.name);
                            }
                            Err(e) => {
                                // Deliberately not calling record_reboot: a
                                // failed reboot attempt shouldn't consume
                                // cooldown/circuit-breaker budget, so the
                                // next poll retries it.
                                eprintln!(
                                    "gocoax-remediator: warning: reboot failed device={device_name} reason={}: {e}",
                                    rule.name
                                );
                            }
                        }
                    }
                }
                Decision::Cooldown => {
                    // Skip silently -- expected, not noteworthy.
                }
                Decision::CircuitOpen => {
                    let mut inner = state.inner.lock().unwrap();
                    inner.circuit_open.insert(device_name.clone(), true);
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let (cfg, rcfg) = load(&cli.config)?;

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(cfg.request_timeout_secs))
        .connect_timeout(Duration::from_secs(cfg.connect_timeout_secs))
        .build()?;

    let listen = rcfg.listen.clone();
    let poll_interval = Duration::from_secs(rcfg.poll_interval_secs);

    let state = Arc::new(AppState { cfg, rcfg, http, inner: Mutex::new(Inner::default()) });

    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(poll_interval);
            loop {
                ticker.tick().await;
                poll_once(&state).await;
            }
        });
    }

    let app = Router::new().route(
        "/metrics",
        get({
            let state = state.clone();
            move || {
                let state = state.clone();
                async move { render(&state.snapshot()) }
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind(&listen).await?;
    println!("gocoax-remediator listening on {listen}");
    axum::serve(listener, app).await?;
    Ok(())
}
