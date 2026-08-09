//! The impure poll loop: evaluate every configured rule against Prometheus,
//! decide what to do about each returned device via the pure `state`
//! module, and act (a dry-run count, or a real reboot via the injected
//! [`Rebooter`]). Mirrors `gocoax-exporter`'s `scrape.rs`: this is where the
//! clock, locking, and side effects live, so it's the one module in this
//! crate that isn't pure -- but the reboot action itself is injected
//! (`Rebooter`) and the clock is injected (`now_unix`/`today` parameters),
//! which is what makes [`poll_once`] deterministically testable below.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use gocoax::config::{Config, Device};
use gocoax::{Client, ClientOpts};

use crate::config::RemediatorConfig;
use crate::metrics::MetricsSnapshot;
use crate::prom::query_devices;
use crate::state::{decide, record_reboot, Decision, DeviceState, Limits};

/// The reboot action, injected so [`poll_once`] can be tested without a
/// real device. `main` wires up [`RealRebooter`]; tests wire up a fake that
/// records calls and can be told to fail on command.
///
/// A plain async-fn-in-trait (stable since Rust 1.75) rather than a boxed
/// `Fn` closure: `poll_once` is generic over `R: Rebooter`, so no `dyn`/
/// pinned-boxed-future machinery is needed. Spelled out as `-> impl Future
/// + Send` (rather than `async fn`) so the returned future is explicitly
/// `Send` -- required since `poll_once` runs inside a `tokio::spawn`ed task.
pub trait Rebooter {
    fn reboot(&self, cfg: &Config, dev: &Device) -> impl std::future::Future<Output = gocoax::Result<()>> + Send;
}

/// The real implementation: builds a fresh `gocoax::Client` per attempt
/// (reboots are rare, so this keeps `AppState` free of per-device client
/// bookkeeping) and calls `Client::reboot()`.
pub struct RealRebooter;

impl Rebooter for RealRebooter {
    async fn reboot(&self, cfg: &Config, dev: &Device) -> gocoax::Result<()> {
        let creds = cfg.creds_for(dev)?;
        let opts = ClientOpts {
            request_timeout: Duration::from_secs(cfg.request_timeout_secs),
            connect_timeout: Duration::from_secs(cfg.connect_timeout_secs),
            verbose: false,
        };
        let client = Client::new(&dev.host, creds, opts)?;
        client.reboot().await
    }
}

/// Mutable counters/state, guarded by a single mutex: one poll-loop writer,
/// occasional `/metrics` readers. Contention is not a concern at this scale.
#[derive(Default)]
struct Inner {
    devices: HashMap<String, DeviceState>,
    reboots_total: HashMap<(String, String), u64>,
    would_reboot_total: HashMap<(String, String), u64>,
    reboot_failures_total: HashMap<(String, String), u64>,
    last_reboot_ts: HashMap<String, f64>,
    circuit_open: HashMap<String, bool>,
}

pub struct AppState {
    pub cfg: Config,
    pub rcfg: RemediatorConfig,
    /// Used only for Prometheus queries; device reboots go through the
    /// injected `Rebooter`, which builds its own per-device client.
    pub http: reqwest::Client,
    inner: Mutex<Inner>,
}

impl AppState {
    pub fn new(cfg: Config, rcfg: RemediatorConfig, http: reqwest::Client) -> AppState {
        AppState { cfg, rcfg, http, inner: Mutex::new(Inner::default()) }
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let inner = self.inner.lock().unwrap();
        MetricsSnapshot {
            reboots_total: inner.reboots_total.iter().map(|((d, r), c)| (d.clone(), r.clone(), *c)).collect(),
            would_reboot_total: inner
                .would_reboot_total
                .iter()
                .map(|((d, r), c)| (d.clone(), r.clone(), *c))
                .collect(),
            reboot_failures_total: inner
                .reboot_failures_total
                .iter()
                .map(|((d, r), c)| (d.clone(), r.clone(), *c))
                .collect(),
            last_reboot_ts: inner.last_reboot_ts.iter().map(|(d, t)| (d.clone(), *t)).collect(),
            circuit_open: inner.circuit_open.iter().map(|(d, o)| (d.clone(), *o)).collect(),
        }
    }
}

/// Run one poll: evaluate every rule, decide, and act.
///
/// `now_unix`/`today` are injected rather than read from the clock in here,
/// so callers -- tests especially -- can control time deterministically;
/// see `state::decide`/`record_reboot` for how they're used. `today` is an
/// opaque "day bucket" string; `main.rs` derives it from `SystemTime`.
///
/// Safety invariants enforced here (see docs/remediator.md):
/// - A device matching multiple rules in one poll only acts once
///   (`handled_this_poll`) -- needed even though `record_reboot` also
///   naturally blocks a same-poll repeat via cooldown, because dry-run and
///   live-reboot both now call it up front (see next point) but we still
///   want a single log line / single counter bump per poll, not one per
///   matching rule.
/// - `record_reboot` is called for every *attempt* -- dry-run, successful
///   live reboot, or failed live reboot alike -- so the cooldown and daily
///   circuit breaker apply identically no matter which of those three
///   happens. Only a successful live reboot advances the *exposed*
///   `last_reboot_timestamp_seconds` metric; only a failed one bumps
///   `reboot_failures_total`.
/// - `circuit_open{device}` is refreshed to match the latest `decide()`
///   result every time a device is evaluated, so it clears itself the poll
///   after the breaker condition stops holding (e.g. a day rollover).
pub async fn poll_once<R: Rebooter>(state: &AppState, now_unix: f64, today: &str, rebooter: &R) {
    let limits = Limits { cooldown_secs: state.rcfg.cooldown_secs, max_reboots_per_day: state.rcfg.max_reboots_per_day };
    let verbose = state.rcfg.verbose;
    let mut handled_this_poll: HashSet<String> = HashSet::new();

    if verbose {
        eprintln!(
            "gocoax-remediator: poll: evaluating {} rule(s){}",
            state.rcfg.rule.len(),
            if state.rcfg.dry_run { " [dry-run]" } else { "" }
        );
    }

    for rule in &state.rcfg.rule {
        let devices = match query_devices(&state.http, &state.rcfg.prometheus_url, &rule.expr).await {
            Ok(devices) => devices,
            Err(e) => {
                eprintln!("gocoax-remediator: warning: rule '{}' query failed: {e}", rule.name);
                continue;
            }
        };
        if verbose {
            eprintln!(
                "gocoax-remediator:   rule '{}' matched {} device(s): {devices:?}",
                rule.name,
                devices.len()
            );
        }

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

            let (decision, st) = {
                let inner = state.inner.lock().unwrap();
                let st = inner.devices.get(&device_name).cloned().unwrap_or_default();
                (decide(&st, &limits, now_unix, today), st)
            };

            // Always refresh circuit_open to match the latest decision, so
            // it clears itself on the first poll the condition no longer
            // holds (rather than staying stuck at 1 from a stale reboot).
            {
                let mut inner = state.inner.lock().unwrap();
                inner.circuit_open.insert(device_name.clone(), matches!(decision, Decision::CircuitOpen));
            }

            match decision {
                Decision::Reboot => {
                    handled_this_poll.insert(device_name.clone());

                    // Record the attempt up front, before knowing dry-run
                    // vs. success vs. failure -- so cooldown/breaker apply
                    // identically to all three (no retry-storm on a
                    // persistently-failing device, and dry-run's preview
                    // reflects the same cadence live mode would have).
                    {
                        let mut inner = state.inner.lock().unwrap();
                        let dst = inner.devices.entry(device_name.clone()).or_default();
                        record_reboot(dst, now_unix, today);
                    }

                    if state.rcfg.dry_run {
                        let mut inner = state.inner.lock().unwrap();
                        *inner.would_reboot_total.entry((device_name.clone(), rule.name.clone())).or_insert(0) += 1;
                        println!("gocoax-remediator: would reboot device={device_name} reason={}", rule.name);
                    } else {
                        match rebooter.reboot(&state.cfg, dev).await {
                            Ok(()) => {
                                let mut inner = state.inner.lock().unwrap();
                                *inner.reboots_total.entry((device_name.clone(), rule.name.clone())).or_insert(0) += 1;
                                inner.last_reboot_ts.insert(device_name.clone(), now_unix);
                                println!("gocoax-remediator: rebooted device={device_name} reason={}", rule.name);
                            }
                            Err(e) => {
                                let mut inner = state.inner.lock().unwrap();
                                *inner
                                    .reboot_failures_total
                                    .entry((device_name.clone(), rule.name.clone()))
                                    .or_insert(0) += 1;
                                eprintln!(
                                    "gocoax-remediator: warning: reboot failed device={device_name} reason={}: {e}",
                                    rule.name
                                );
                            }
                        }
                    }
                }
                Decision::Cooldown => {
                    // Expected/quiet unless verbose. Report how long is left.
                    if verbose {
                        let left = st
                            .last_reboot_unix
                            .map(|last| (last + limits.cooldown_secs as f64 - now_unix).max(0.0))
                            .unwrap_or(0.0);
                        eprintln!(
                            "gocoax-remediator:     device={device_name} reason={} -> in cooldown ({left:.0}s left), skipping",
                            rule.name
                        );
                    }
                }
                Decision::CircuitOpen => {
                    // circuit_open metric already set above.
                    if verbose {
                        eprintln!(
                            "gocoax-remediator:     device={device_name} reason={} -> circuit breaker open (>= {} reboots today), skipping",
                            rule.name, limits.max_reboots_per_day
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Rule;
    use std::sync::Mutex as StdMutex;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A fake `Rebooter` that records every device it was called for (in
    /// call order) and fails for any device name in `fail`.
    #[derive(Default)]
    struct FakeRebooter {
        calls: StdMutex<Vec<String>>,
        fail: StdMutex<HashSet<String>>,
    }

    impl FakeRebooter {
        fn fail_device(&self, name: &str) {
            self.fail.lock().unwrap().insert(name.to_string());
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Rebooter for FakeRebooter {
        async fn reboot(&self, _cfg: &Config, dev: &Device) -> gocoax::Result<()> {
            self.calls.lock().unwrap().push(dev.name.clone());
            if self.fail.lock().unwrap().contains(&dev.name) {
                Err(gocoax::Error::Timeout)
            } else {
                Ok(())
            }
        }
    }

    /// A wiremock Prometheus that answers every `/api/v1/query` with a
    /// success vector where each series carries `device=<one of devices>`.
    async fn prom_returning(devices: &[&str]) -> MockServer {
        let result: Vec<serde_json::Value> = devices
            .iter()
            .map(|d| serde_json::json!({"metric": {"device": d}, "value": [1_690_000_000, "0"]}))
            .collect();
        let body = serde_json::json!({
            "status": "success",
            "data": {"resultType": "vector", "result": result}
        });
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/query"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    fn cfg_with_device(name: &str) -> Config {
        let toml = format!(
            "username = \"admin\"\npassword = \"g\"\n\n[[device]]\nname = \"{name}\"\nhost = \"10.0.0.1:1\"\n"
        );
        Config::from_toml(&toml).unwrap()
    }

    fn rcfg(prometheus_url: String, rules: Vec<Rule>, cooldown_secs: u64, max_reboots_per_day: u32, dry_run: bool) -> RemediatorConfig {
        RemediatorConfig {
            prometheus_url,
            poll_interval_secs: 60,
            cooldown_secs,
            max_reboots_per_day,
            listen: "127.0.0.1:0".into(),
            dry_run,
            verbose: false,
            rule: rules,
        }
    }

    fn rule(name: &str) -> Rule {
        Rule { name: name.to_string(), expr: "up == 0".to_string() }
    }

    fn count_of(rows: &[(String, String, u64)], device: &str, reason: &str) -> u64 {
        rows.iter().find(|(d, r, _)| d == device && r == reason).map(|(_, _, c)| *c).unwrap_or(0)
    }

    fn is_circuit_open(rows: &[(String, bool)], device: &str) -> bool {
        rows.iter().find(|(d, _)| d == device).map(|(_, o)| *o).unwrap_or(false)
    }

    #[tokio::test]
    async fn dry_run_never_calls_rebooter_and_simulates_cooldown() {
        let server = prom_returning(&["ff"]).await;
        let state = AppState::new(
            cfg_with_device("ff"),
            rcfg(server.uri(), vec![rule("unreachable")], 1800, 4, true),
            reqwest::Client::new(),
        );
        let fake = FakeRebooter::default();

        poll_once(&state, 1_000_000.0, "1", &fake).await;
        assert!(fake.calls().is_empty(), "dry_run must never invoke the real reboot action");
        assert_eq!(count_of(&state.snapshot().would_reboot_total, "ff", "unreachable"), 1);

        // A second poll well inside the cooldown window must NOT re-fire
        // (dry-run's preview has to reflect the same cadence live mode
        // would produce).
        poll_once(&state, 1_000_060.0, "1", &fake).await;
        assert_eq!(count_of(&state.snapshot().would_reboot_total, "ff", "unreachable"), 1);
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn two_rules_matching_same_device_only_acts_once() {
        let server = prom_returning(&["ff"]).await;
        let state = AppState::new(
            cfg_with_device("ff"),
            rcfg(server.uri(), vec![rule("unreachable"), rule("link_down")], 1800, 4, false),
            reqwest::Client::new(),
        );
        let fake = FakeRebooter::default();

        poll_once(&state, 1_000_000.0, "1", &fake).await;

        assert_eq!(fake.calls().len(), 1, "should only reboot once even though two rules matched");
        let snap = state.snapshot();
        let total: u64 = snap.reboots_total.iter().map(|(_, _, c)| c).sum();
        assert_eq!(total, 1);
    }

    #[tokio::test]
    async fn cooldown_across_polls_allows_exactly_one_reboot() {
        let server = prom_returning(&["ff"]).await;
        let state = AppState::new(
            cfg_with_device("ff"),
            rcfg(server.uri(), vec![rule("unreachable")], 1800, 4, false),
            reqwest::Client::new(),
        );
        let fake = FakeRebooter::default();

        poll_once(&state, 1_000_000.0, "1", &fake).await;
        // Second poll, 60s later: well inside the 1800s cooldown.
        poll_once(&state, 1_000_060.0, "1", &fake).await;

        assert_eq!(fake.calls().len(), 1, "cooldown must block the second poll's reboot");
        assert_eq!(count_of(&state.snapshot().reboots_total, "ff", "unreachable"), 1);
    }

    #[tokio::test]
    async fn circuit_breaker_trips_after_max_reboots_per_day_and_reports_open() {
        let server = prom_returning(&["ff"]).await;
        // cooldown_secs = 0 isolates the breaker: cooldown never blocks a
        // subsequent poll on its own.
        let state = AppState::new(
            cfg_with_device("ff"),
            rcfg(server.uri(), vec![rule("unreachable")], 0, 2, false),
            reqwest::Client::new(),
        );
        let fake = FakeRebooter::default();

        poll_once(&state, 1_000_000.0, "1", &fake).await;
        poll_once(&state, 1_000_100.0, "1", &fake).await;
        assert_eq!(fake.calls().len(), 2);
        assert!(!is_circuit_open(&state.snapshot().circuit_open, "ff"));

        // Third poll, same day: breaker should trip, no further reboot.
        poll_once(&state, 1_000_200.0, "1", &fake).await;
        assert_eq!(fake.calls().len(), 2, "breaker must stop further reboots once tripped");
        assert!(is_circuit_open(&state.snapshot().circuit_open, "ff"));
    }

    #[tokio::test]
    async fn failed_reboot_counts_as_failure_not_success_and_still_respects_cooldown() {
        let server = prom_returning(&["ff"]).await;
        let state = AppState::new(
            cfg_with_device("ff"),
            rcfg(server.uri(), vec![rule("unreachable")], 1800, 4, false),
            reqwest::Client::new(),
        );
        let fake = FakeRebooter::default();
        fake.fail_device("ff");

        poll_once(&state, 1_000_000.0, "1", &fake).await;
        let snap = state.snapshot();
        assert_eq!(count_of(&snap.reboot_failures_total, "ff", "unreachable"), 1);
        assert_eq!(count_of(&snap.reboots_total, "ff", "unreachable"), 0);
        assert!(snap.last_reboot_ts.is_empty(), "a failed attempt must not set the exposed last-reboot timestamp");

        // Second poll, still inside cooldown: must NOT retry (no
        // retry-storm on a persistently-failing device).
        poll_once(&state, 1_000_060.0, "1", &fake).await;
        assert_eq!(fake.calls().len(), 1);
        assert_eq!(count_of(&state.snapshot().reboot_failures_total, "ff", "unreachable"), 1);
    }
}
