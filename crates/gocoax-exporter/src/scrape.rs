//! Scrape engine: fans a `/metrics` request out into one concurrent read per
//! configured device, bounded by a global deadline, with per-device failure
//! isolation. Owns the clock (timestamps, durations) and the persistent
//! error/last-success counters; [`crate::metrics::render`] stays a pure
//! formatter of whatever this module hands it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gocoax::config::Config;
use gocoax::{Client, ClientOpts, DeviceStatus, Error, PhyRates};
use tokio::task::JoinSet;

use crate::metrics::{render, DeviceOutcome};

/// Map a `gocoax::Error` to the stable, low-cardinality reason label used in
/// `gocoax_scrape_errors_total{reason=...}` (spec §6).
pub fn reason_for(err: &Error) -> &'static str {
    match err {
        Error::Timeout => "timeout",
        Error::Auth => "auth",
        Error::Csrf => "csrf",
        Error::HttpStatus(_) => "http_status",
        Error::Decode { .. } => "decode",
        Error::Http(_) => "unreachable",
        Error::Config(_) => "config",
    }
}

/// Shared exporter state: the parsed config, one built `Client` per
/// configured device, and the persistent counters that outlive any single
/// scrape (Prometheus counters must keep accumulating across scrapes, and
/// `render` has no clock of its own to stamp a last-success time).
pub struct AppState {
    cfg: Config,
    /// (device name, device host, client), in config order.
    clients: Vec<(String, String, Client)>,
    /// scrape_errors_total accumulator, keyed by (device name, reason).
    errors: Mutex<HashMap<(String, String), u64>>,
    /// Unix timestamp (seconds) of each device's last fully successful scrape.
    last_ok: Mutex<HashMap<String, f64>>,
}

impl AppState {
    /// Parse `text` as a device config and eagerly build an HTTP client per
    /// device (this only builds the `reqwest::Client`s; it makes no network
    /// calls).
    pub fn from_config_text(text: &str) -> Result<AppState, Error> {
        let cfg = Config::from_toml(text)?;
        let request_timeout = Duration::from_secs(cfg.request_timeout_secs);
        let connect_timeout = Duration::from_secs(cfg.connect_timeout_secs);

        let mut clients = Vec::with_capacity(cfg.device.len());
        for dev in &cfg.device {
            let creds = cfg.creds_for(dev)?;
            let opts = ClientOpts { request_timeout, connect_timeout };
            let client = Client::new(&dev.host, creds, opts)?;
            clients.push((dev.name.clone(), dev.host.clone(), client));
        }

        Ok(AppState {
            cfg,
            clients,
            errors: Mutex::new(HashMap::new()),
            last_ok: Mutex::new(HashMap::new()),
        })
    }

    /// The `host:port` this exporter's `/metrics` server should bind to.
    pub fn listen(&self) -> &str {
        &self.cfg.listen
    }
}

/// Owned per-device scrape result (spawned tasks can't hand back a borrowing
/// `DeviceOutcome` -- that's assembled afterwards once everything's owned).
struct DeviceResult {
    name: String,
    host: String,
    up: bool,
    error_reason: Option<&'static str>,
    duration_secs: f64,
    status: Option<DeviceStatus>,
    phy: Option<PhyRates>,
}

/// Scrape one device: `device_status()`, then (only if that succeeded --
/// phy data is meaningless for an unreachable device) `phy_rates()`.
/// Never panics on device data; any failure is captured as a classified
/// reason rather than propagated.
async fn run_device(name: String, host: String, client: &Client) -> DeviceResult {
    let start = Instant::now();

    let (status, mut error_reason) = match client.device_status().await {
        Ok(s) => (Some(s), None),
        Err(e) => (None, Some(reason_for(&e))),
    };
    let up = status.is_some();

    let mut phy = None;
    if up {
        match client.phy_rates().await {
            Ok(p) => phy = Some(p),
            Err(e) => error_reason = Some(reason_for(&e)),
        }
    }

    DeviceResult {
        name,
        host,
        up,
        error_reason,
        duration_secs: start.elapsed().as_secs_f64(),
        status,
        phy,
    }
}

/// Scrape every configured device concurrently, bounded by
/// `scrape_deadline_secs`, and return the rendered Prometheus text. Always
/// returns *some* text -- even if every device is unreachable -- since a
/// down device still renders `gocoax_up{...} 0` plus error/duration lines.
///
/// Fan-out: one task per device. Each task writes its own result the moment
/// it finishes (independent of the others), so a slow device can't hold up
/// a fast one's data; devices that haven't finished when the deadline fires
/// are reported as `up=0` with `reason="timeout"`.
pub async fn scrape(state: Arc<AppState>) -> String {
    let n = state.clients.len();
    let deadline = Duration::from_secs(state.cfg.scrape_deadline_secs);
    let scrape_start = Instant::now();

    let mut set: JoinSet<(usize, DeviceResult)> = JoinSet::new();
    for i in 0..n {
        let state = state.clone();
        set.spawn(async move {
            let (name, host, client) = &state.clients[i];
            let result = run_device(name.clone(), host.clone(), client).await;
            (i, result)
        });
    }

    let mut slots: Vec<Option<DeviceResult>> = (0..n).map(|_| None).collect();
    {
        let drain = async {
            while let Some(joined) = set.join_next().await {
                if let Ok((i, result)) = joined {
                    slots[i] = Some(result);
                }
                // A `JoinError` (task panicked) leaves that slot `None`,
                // same as a device that never finished in time -- it's
                // reported as a timeout below rather than crashing the
                // whole scrape.
            }
        };
        let _ = tokio::time::timeout(deadline, drain).await;
    }
    // Any devices still running past the deadline are abandoned; stop them
    // rather than let them run indefinitely in the background.
    set.abort_all();

    let elapsed_on_timeout = scrape_start.elapsed().as_secs_f64();
    let now_ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);

    let results: Vec<DeviceResult> = slots
        .into_iter()
        .enumerate()
        .map(|(i, slot)| {
            slot.unwrap_or_else(|| {
                let (name, host, _client) = &state.clients[i];
                // A missing slot means either the device genuinely didn't
                // finish before `scrape_deadline_secs`, or its task panicked
                // (see the `JoinError` note above) -- both are reported
                // identically as `reason="timeout"`. This conflation is
                // deliberate: `gocoax::Error` has no "panic" variant to
                // classify against, and a panicking task is itself a bug we
                // want surfaced (not silently swallowed), so folding it into
                // the existing timeout bucket rather than inventing a new
                // reason label is the simplest choice that still keeps a
                // single misbehaving device from crashing the whole scrape.
                DeviceResult {
                    name: name.clone(),
                    host: host.clone(),
                    up: false,
                    error_reason: Some("timeout"),
                    duration_secs: elapsed_on_timeout,
                    status: None,
                    phy: None,
                }
            })
        })
        .collect();

    // Update the persistent counters: bump scrape_errors_total for any
    // classified failure (including synthesized timeouts above), and stamp
    // last-success for any device that came back clean.
    {
        let mut errors = state.errors.lock().unwrap();
        let mut last_ok = state.last_ok.lock().unwrap();
        for r in &results {
            match r.error_reason {
                Some(reason) => {
                    *errors.entry((r.name.clone(), reason.to_string())).or_insert(0) += 1;
                }
                None if r.up => {
                    last_ok.insert(r.name.clone(), now_ts);
                }
                None => {}
            }
        }
    }

    // Snapshot the accumulated counters/timestamps to build the (borrowing)
    // DeviceOutcome list that render() consumes.
    let errors_snapshot = state.errors.lock().unwrap();
    let mut error_counts_by_device: HashMap<&str, Vec<(&str, u64)>> = HashMap::new();
    for ((device, reason), count) in errors_snapshot.iter() {
        error_counts_by_device.entry(device.as_str()).or_default().push((reason.as_str(), *count));
    }
    let last_ok_snapshot = state.last_ok.lock().unwrap();

    let outcomes: Vec<DeviceOutcome> = results
        .iter()
        .map(|r| DeviceOutcome {
            name: &r.name,
            host: &r.host,
            up: r.up,
            error_reason: r.error_reason,
            duration_secs: r.duration_secs,
            status: r.status.as_ref(),
            phy: r.phy.as_ref(),
            error_counts: error_counts_by_device.get(r.name.as_str()).map_or(&[], Vec::as_slice),
            last_success_ts: last_ok_snapshot.get(&r.name).copied(),
        })
        .collect();

    render(&outcomes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_errors_to_reasons() {
        assert_eq!(reason_for(&Error::Timeout), "timeout");
        assert_eq!(reason_for(&Error::Auth), "auth");
        assert_eq!(reason_for(&Error::Http("x".into())), "unreachable");
        assert_eq!(reason_for(&Error::Csrf), "csrf");
        assert_eq!(reason_for(&Error::HttpStatus(500)), "http_status");
        assert_eq!(
            reason_for(&Error::Decode { cmd: "0x14".into(), reason: "short".into() }),
            "decode"
        );
        assert_eq!(reason_for(&Error::Config("bad".into())), "config");
    }

    #[test]
    fn app_state_parses_config_and_exposes_listen() {
        let toml = r#"
listen = "127.0.0.1:9420"
username = "admin"
password = "g"

[[device]]
name = "ff"
host = "10.0.0.1"
"#;
        let state = AppState::from_config_text(toml).unwrap();
        assert_eq!(state.listen(), "127.0.0.1:9420");
        assert_eq!(state.clients.len(), 1);
    }

    #[test]
    fn app_state_propagates_config_errors() {
        // No password configured anywhere -> Config::creds_for fails.
        let toml = r#"
[[device]]
name = "ff"
host = "10.0.0.1"
"#;
        let result = AppState::from_config_text(toml);
        assert!(matches!(result, Err(Error::Config(_))));
    }
}
