//! gocoax-remediator: polls Prometheus for problematic MoCA adapters (per
//! configured rules) and reboots them, subject to a cooldown + circuit
//! breaker, exposing its own reboot history as `/metrics`.
//!
//! This binary is deliberately thin: it owns the clock (`SystemTime`) and
//! wires the real `Rebooter` into `poller::poll_once`, plus the axum
//! `/metrics` server. All decision logic lives in the library (`config`,
//! `prom`, `state`, `metrics`, `poller`), which is unit/integration tested
//! on its own.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{routing::get, Router};
use clap::Parser;

use gocoax_remediator::config::load;
use gocoax_remediator::metrics::render;
use gocoax_remediator::poller::{poll_once, AppState, RealRebooter};

#[derive(Parser)]
struct Cli {
    #[arg(long)]
    config: String,
}

/// A coarse "day bucket" used purely to detect day rollovers for the
/// circuit breaker's daily counter -- an integer count of days since the
/// Unix epoch (UTC), rendered as a string. This sidesteps pulling in a date
/// library (none is a declared dependency) while staying consistent
/// between `decide`/`record_reboot` calls within the same poll and stable
/// across polls within the same UTC day.
fn day_bucket(now: SystemTime) -> String {
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    (secs / 86_400).to_string()
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

    let state = Arc::new(AppState::new(cfg, rcfg, http));

    {
        let state = state.clone();
        tokio::spawn(async move {
            let rebooter = RealRebooter;
            let mut ticker = tokio::time::interval(poll_interval);
            loop {
                ticker.tick().await;
                let now = SystemTime::now();
                let now_unix = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64();
                let today = day_bucket(now);
                poll_once(&state, now_unix, &today, &rebooter).await;
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
