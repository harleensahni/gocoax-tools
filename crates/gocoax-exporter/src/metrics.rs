//! Pure Prometheus text renderer for gocoax device scrape outcomes.
//!
//! `render` performs no I/O and no clock reads. It only formats whatever
//! already-decoded data the scrape layer (a later task) hands it, including
//! per-device timings — see spec §7 for the metric catalog this mirrors.

use std::fmt::Write as _;

use gocoax::DeviceStatus;
use gocoax::PhyRates;

/// The result of scraping one configured device, ready to be rendered as
/// Prometheus metrics. Borrows everything so `render` can stay allocation-
/// light and callers don't have to clone decoded structs just to report them.
pub struct DeviceOutcome<'a> {
    pub name: &'a str,
    pub host: &'a str,
    pub up: bool,
    /// Classified failure reason for *this* scrape: unreachable|timeout|auth|csrf|http_status|decode.
    pub error_reason: Option<&'a str>,
    pub duration_secs: f64,
    pub status: Option<&'a DeviceStatus>,
    pub phy: Option<&'a PhyRates>,
    /// Accumulated scrape-error counts by classified reason, persisted
    /// across scrapes by the caller (the scrape layer owns the counters and
    /// the clock; `render` just formats whatever totals it's handed).
    pub error_counts: &'a [(&'a str, u64)],
    /// Unix timestamp (seconds) of this device's last fully successful
    /// scrape, if any. `None` if it has never had one.
    pub last_success_ts: Option<f64>,
}

/// Render Prometheus exposition-format text for a batch of scrape outcomes.
///
/// Pure function of its input: no HTTP calls, no `SystemTime::now()`. The
/// scrape layer supplies every per-outcome value (including
/// `duration_secs`); `render` only formats what it is given, and never
/// panics on the data it's handed (it's already-decoded, so this is just
/// string formatting).
pub fn render(outcomes: &[DeviceOutcome]) -> String {
    let mut out = String::new();

    push_up(&mut out, outcomes);
    push_scrape_errors(&mut out, outcomes);
    push_scrape_duration(&mut out, outcomes);
    push_last_success(&mut out, outcomes);
    push_info(&mut out, outcomes);
    push_link_up(&mut out, outcomes);
    push_nodes(&mut out, outcomes);
    push_phy_rate_mbps(&mut out, outcomes);
    push_phy_rate_gcd_mbps(&mut out, outcomes);
    push_eth_frames(&mut out, outcomes, "tx", |eth| (eth.tx_good, eth.tx_bad, eth.tx_dropped));
    push_eth_frames(&mut out, outcomes, "rx", |eth| (eth.rx_good, eth.rx_bad, eth.rx_dropped));
    push_ethernet_link_up(&mut out, outcomes);
    push_ethernet_speed_mbps(&mut out, outcomes);
    push_node_moca_version(&mut out, outcomes);

    out
}

/// Minimal Prometheus label-value escaping (backslash, quote, newline).
/// Device/host/etc. come from trusted, operator-supplied config, so this is
/// deliberately not a full validator -- just enough to keep the exposition
/// text well-formed.
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn bool01(b: bool) -> u8 {
    if b {
        1
    } else {
        0
    }
}

fn push_up(out: &mut String, outcomes: &[DeviceOutcome]) {
    let _ = writeln!(out, "# HELP gocoax_up Current device read health (1=up, 0=down).");
    let _ = writeln!(out, "# TYPE gocoax_up gauge");
    for o in outcomes {
        let _ = writeln!(out, "gocoax_up{{device=\"{}\"}} {}", esc(o.name), bool01(o.up));
    }
}

fn push_scrape_errors(out: &mut String, outcomes: &[DeviceOutcome]) {
    let rows: Vec<(&DeviceOutcome, &str, u64)> = outcomes
        .iter()
        .flat_map(|o| o.error_counts.iter().map(move |&(reason, count)| (o, reason, count)))
        .collect();
    if rows.is_empty() {
        return;
    }
    let _ = writeln!(
        out,
        "# HELP gocoax_scrape_errors_total Count of scrape errors by classified reason."
    );
    let _ = writeln!(out, "# TYPE gocoax_scrape_errors_total counter");
    for (o, reason, count) in rows {
        let _ = writeln!(
            out,
            "gocoax_scrape_errors_total{{device=\"{}\",reason=\"{}\"}} {}",
            esc(o.name),
            esc(reason),
            count
        );
    }
}

fn push_last_success(out: &mut String, outcomes: &[DeviceOutcome]) {
    let rows: Vec<_> = outcomes.iter().filter_map(|o| o.last_success_ts.map(|ts| (o, ts))).collect();
    if rows.is_empty() {
        return;
    }
    let _ = writeln!(
        out,
        "# HELP gocoax_last_success_timestamp_seconds Unix timestamp of the device's last fully successful scrape."
    );
    let _ = writeln!(out, "# TYPE gocoax_last_success_timestamp_seconds gauge");
    for (o, ts) in rows {
        let _ = writeln!(
            out,
            "gocoax_last_success_timestamp_seconds{{device=\"{}\"}} {}",
            esc(o.name),
            ts
        );
    }
}

fn push_scrape_duration(out: &mut String, outcomes: &[DeviceOutcome]) {
    let _ = writeln!(out, "# HELP gocoax_scrape_duration_seconds Per-device scrape duration in seconds.");
    let _ = writeln!(out, "# TYPE gocoax_scrape_duration_seconds gauge");
    for o in outcomes {
        let _ = writeln!(
            out,
            "gocoax_scrape_duration_seconds{{device=\"{}\"}} {}",
            esc(o.name),
            o.duration_secs
        );
    }
}

/// Outcomes with decoded status, i.e. the ones allowed to emit info/data
/// lines. A down device always has `status: None`, so gating every
/// device-data metric on this is what keeps DOWN devices info/data-free.
fn with_status<'a, 'b>(outcomes: &'b [DeviceOutcome<'a>]) -> Vec<(&'b DeviceOutcome<'a>, &'a DeviceStatus)> {
    outcomes.iter().filter_map(|o| o.status.map(|s| (o, s))).collect()
}

/// Outcomes with both decoded status AND phy data. `status` is the
/// down-device gate (see `with_status`); phy metrics must respect it too,
/// even though `phy` is a separate optional field that could in principle
/// be `Some` while `status` is `None` (a partial-failure scrape where the
/// status call failed but the phy-rate call succeeded). Per the down-device
/// contract, a device without status never emits *any* data lines,
/// including phy ones.
fn with_status_and_phy<'a, 'b>(outcomes: &'b [DeviceOutcome<'a>]) -> Vec<(&'b DeviceOutcome<'a>, &'a PhyRates)> {
    outcomes
        .iter()
        .filter_map(|o| if o.status.is_some() { o.phy.map(|p| (o, p)) } else { None })
        .collect()
}

fn push_info(out: &mut String, outcomes: &[DeviceOutcome]) {
    let rows = with_status(outcomes);
    if rows.is_empty() {
        return;
    }
    let _ = writeln!(out, "# HELP gocoax_info Device identity/firmware info (label carrier, always 1).");
    let _ = writeln!(out, "# TYPE gocoax_info gauge");
    for (o, s) in rows {
        let _ = writeln!(
            out,
            "gocoax_info{{device=\"{}\",host=\"{}\",mac=\"{}\",ip=\"{}\",soc_version=\"{}\",moca_version=\"{}\"}} 1",
            esc(o.name),
            esc(o.host),
            esc(&s.mac),
            esc(&s.ip.to_string()),
            esc(&s.soc_version),
            esc(&s.moca_version)
        );
    }
}

fn push_link_up(out: &mut String, outcomes: &[DeviceOutcome]) {
    let rows = with_status(outcomes);
    if rows.is_empty() {
        return;
    }
    let _ = writeln!(out, "# HELP gocoax_moca_link_up Whether the device's MoCA link is up.");
    let _ = writeln!(out, "# TYPE gocoax_moca_link_up gauge");
    for (o, s) in rows {
        let _ = writeln!(out, "gocoax_moca_link_up{{device=\"{}\"}} {}", esc(o.name), bool01(s.link_up));
    }
}

fn push_nodes(out: &mut String, outcomes: &[DeviceOutcome]) {
    let rows = with_status(outcomes);
    if rows.is_empty() {
        return;
    }
    let _ = writeln!(out, "# HELP gocoax_moca_nodes Number of nodes on the MoCA network.");
    let _ = writeln!(out, "# TYPE gocoax_moca_nodes gauge");
    for (o, s) in rows {
        let _ = writeln!(out, "gocoax_moca_nodes{{device=\"{}\"}} {}", esc(o.name), s.node_count);
    }
}

fn push_phy_rate_mbps(out: &mut String, outcomes: &[DeviceOutcome]) {
    let rows = with_status_and_phy(outcomes);
    if rows.iter().all(|(_, p)| p.links.is_empty()) {
        return;
    }
    let _ = writeln!(out, "# HELP gocoax_phy_rate_mbps PHY rate between a node pair (self pair = per-node rate).");
    let _ = writeln!(out, "# TYPE gocoax_phy_rate_mbps gauge");
    for (o, p) in rows {
        for link in &p.links {
            let _ = writeln!(
                out,
                "gocoax_phy_rate_mbps{{device=\"{}\",from_node=\"{}\",to_node=\"{}\",type=\"nper\"}} {}",
                esc(o.name),
                link.from_node,
                link.to_node,
                link.nper_mbps
            );
            let _ = writeln!(
                out,
                "gocoax_phy_rate_mbps{{device=\"{}\",from_node=\"{}\",to_node=\"{}\",type=\"vlper\"}} {}",
                esc(o.name),
                link.from_node,
                link.to_node,
                link.vlper_mbps
            );
        }
    }
}

fn push_phy_rate_gcd_mbps(out: &mut String, outcomes: &[DeviceOutcome]) {
    let rows = with_status_and_phy(outcomes);
    if rows.iter().all(|(_, p)| p.gcd_mbps.is_empty()) {
        return;
    }
    let _ = writeln!(out, "# HELP gocoax_phy_rate_gcd_mbps Per-node GCD PHY rate.");
    let _ = writeln!(out, "# TYPE gocoax_phy_rate_gcd_mbps gauge");
    for (o, p) in rows {
        for &(node, mbps) in &p.gcd_mbps {
            let _ = writeln!(
                out,
                "gocoax_phy_rate_gcd_mbps{{device=\"{}\",node=\"{}\"}} {}",
                esc(o.name),
                node,
                mbps
            );
        }
    }
}

fn push_eth_frames(
    out: &mut String,
    outcomes: &[DeviceOutcome],
    direction: &str,
    counts: impl Fn(&gocoax::EthCounters) -> (u64, u64, u64),
) {
    let rows = with_status(outcomes);
    if rows.is_empty() {
        return;
    }
    let metric = format!("gocoax_ethernet_{direction}_frames_total");
    let _ = writeln!(out, "# HELP {metric} Ethernet {direction} frame counters by status.");
    let _ = writeln!(out, "# TYPE {metric} counter");
    for (o, s) in &rows {
        let (good, bad, dropped) = counts(&s.eth);
        for (status, value) in [("good", good), ("bad", bad), ("dropped", dropped)] {
            let _ = writeln!(
                out,
                "{metric}{{device=\"{}\",port=\"1\",status=\"{status}\"}} {value}",
                esc(o.name)
            );
        }
    }
}

fn push_ethernet_link_up(out: &mut String, outcomes: &[DeviceOutcome]) {
    let rows = with_status(outcomes);
    if rows.iter().all(|(_, s)| s.eth_ports.is_empty()) {
        return;
    }
    let _ = writeln!(out, "# HELP gocoax_ethernet_link_up Whether an ethernet port's link is up.");
    let _ = writeln!(out, "# TYPE gocoax_ethernet_link_up gauge");
    for (o, s) in &rows {
        for port in &s.eth_ports {
            let _ = writeln!(
                out,
                "gocoax_ethernet_link_up{{device=\"{}\",port=\"{}\"}} {}",
                esc(o.name),
                port.port,
                bool01(port.link_up)
            );
        }
    }
}

fn push_ethernet_speed_mbps(out: &mut String, outcomes: &[DeviceOutcome]) {
    let rows = with_status(outcomes);
    if rows.iter().all(|(_, s)| s.eth_ports.is_empty()) {
        return;
    }
    let _ = writeln!(out, "# HELP gocoax_ethernet_speed_mbps Negotiated ethernet port speed in Mbps.");
    let _ = writeln!(out, "# TYPE gocoax_ethernet_speed_mbps gauge");
    for (o, s) in &rows {
        for port in &s.eth_ports {
            let _ = writeln!(
                out,
                "gocoax_ethernet_speed_mbps{{device=\"{}\",port=\"{}\"}} {}",
                esc(o.name),
                port.port,
                port.speed_mbps
            );
        }
    }
}

fn push_node_moca_version(out: &mut String, outcomes: &[DeviceOutcome]) {
    let rows = with_status_and_phy(outcomes);
    if rows.iter().all(|(_, p)| p.node_versions.is_empty()) {
        return;
    }
    let _ = writeln!(out, "# HELP gocoax_node_moca_version Per-node MoCA protocol version (e.g. 25 = 2.5).");
    let _ = writeln!(out, "# TYPE gocoax_node_moca_version gauge");
    for (o, p) in rows {
        for &(node, raw) in &p.node_versions {
            let version = (raw >> 4) as u32 * 10 + (raw & 0xf) as u32;
            let _ = writeln!(
                out,
                "gocoax_node_moca_version{{device=\"{}\",node=\"{}\"}} {}",
                esc(o.name),
                node,
                version
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn down_device_emits_only_health_lines() {
        let out = render(&[DeviceOutcome {
            name: "ff",
            host: "10.0.0.1",
            up: false,
            error_reason: Some("timeout"),
            duration_secs: 8.0,
            status: None,
            phy: None,
            error_counts: &[("timeout", 1)],
            last_success_ts: None,
        }]);
        assert!(out.contains("gocoax_up{device=\"ff\"} 0"));
        assert!(out.contains("gocoax_scrape_errors_total{device=\"ff\",reason=\"timeout\"}"));
        assert!(!out.contains("gocoax_info{device=\"ff\""));
    }

    #[test]
    fn healthy_device_without_error_reason_emits_no_errors_block() {
        let out = render(&[DeviceOutcome {
            name: "ff",
            host: "10.0.0.1",
            up: true,
            error_reason: None,
            duration_secs: 0.2,
            status: None,
            phy: None,
            error_counts: &[],
            last_success_ts: None,
        }]);
        assert!(out.contains("gocoax_up{device=\"ff\"} 1"));
        assert!(!out.contains("gocoax_scrape_errors_total"));
    }

    #[test]
    fn scrape_errors_render_real_accumulated_totals() {
        // The counter must reflect the caller's accumulated total, not a
        // per-call constant -- and a device can have accumulated more than
        // one distinct reason over its history.
        let out = render(&[DeviceOutcome {
            name: "ff",
            host: "10.0.0.1",
            up: false,
            error_reason: Some("auth"),
            duration_secs: 0.1,
            status: None,
            phy: None,
            error_counts: &[("timeout", 3), ("auth", 5)],
            last_success_ts: None,
        }]);
        assert!(out.contains("gocoax_scrape_errors_total{device=\"ff\",reason=\"timeout\"} 3"));
        assert!(out.contains("gocoax_scrape_errors_total{device=\"ff\",reason=\"auth\"} 5"));
    }

    #[test]
    fn last_success_timestamp_emitted_only_when_present() {
        let out = render(&[
            DeviceOutcome {
                name: "ff",
                host: "10.0.0.1",
                up: true,
                error_reason: None,
                duration_secs: 0.1,
                status: None,
                phy: None,
                error_counts: &[],
                last_success_ts: Some(1_700_000_000.0),
            },
            DeviceOutcome {
                name: "gg",
                host: "10.0.0.2",
                up: false,
                error_reason: Some("timeout"),
                duration_secs: 8.0,
                status: None,
                phy: None,
                error_counts: &[("timeout", 1)],
                last_success_ts: None,
            },
        ]);
        assert!(out.contains("gocoax_last_success_timestamp_seconds{device=\"ff\"} 1700000000"));
        assert!(!out.contains("gocoax_last_success_timestamp_seconds{device=\"gg\"}"));
    }

    #[test]
    fn down_device_with_partial_phy_data_still_emits_no_phy_lines() {
        // Regression: `device_status()` and `phy_rates()` are separate calls
        // (Task 7), so a partial-failure scrape can produce status: None
        // with phy: Some(_). The down-device contract says NO data lines
        // (info/link_up/nodes/eth/phy) when status is absent, regardless of
        // what phy holds.
        use gocoax::{PhyLink, PhyRates};

        let phy = PhyRates {
            gcd_mbps: vec![(0, 701)],
            links: vec![PhyLink { from_node: 0, to_node: 1, nper_mbps: 3656, vlper_mbps: 0 }],
            node_versions: vec![(0, 0x25), (1, 0x25)],
        };

        let out = render(&[DeviceOutcome {
            name: "x",
            host: "10.0.0.2",
            up: false,
            error_reason: Some("decode"),
            duration_secs: 1.0,
            status: None,
            phy: Some(&phy),
            error_counts: &[("decode", 1)],
            last_success_ts: None,
        }]);

        assert!(out.contains("gocoax_up{device=\"x\"} 0"));
        assert!(out.contains("gocoax_scrape_errors_total{device=\"x\",reason=\"decode\"}"));
        assert!(!out.contains("gocoax_phy_rate_mbps"));
        assert!(!out.contains("gocoax_phy_rate_gcd_mbps"));
        assert!(!out.contains("gocoax_node_moca_version"));
    }
}
