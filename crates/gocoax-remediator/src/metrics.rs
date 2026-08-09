//! Pure Prometheus text renderer for the remediator's own metrics.
//!
//! `render` performs no I/O and no clock reads -- it only formats whatever
//! snapshot of already-accumulated state the caller (the poll loop /
//! `/metrics` handler in `main.rs`) hands it.

use std::fmt::Write as _;

/// A point-in-time copy of the remediator's mutable state, flattened to
/// plain rows so `render` doesn't need to know about `main.rs`'s locking or
/// map types.
#[derive(Debug, Clone, Default)]
pub struct MetricsSnapshot {
    /// (device, reason) -> count of reboots actually performed.
    pub reboots_total: Vec<(String, String, u64)>,
    /// (device, reason) -> count of reboots that `dry_run` suppressed.
    pub would_reboot_total: Vec<(String, String, u64)>,
    /// device -> unix timestamp (seconds) of its last reboot.
    pub last_reboot_ts: Vec<(String, f64)>,
    /// device -> whether the circuit breaker is currently open.
    pub circuit_open: Vec<(String, bool)>,
}

/// Minimal Prometheus label-value escaping (backslash, quote, newline).
/// Device/reason names come from trusted, operator-supplied config, so this
/// is deliberately not a full validator -- just enough to keep the
/// exposition text well-formed.
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// Render Prometheus exposition-format text for the given snapshot. Always
/// emits `gocoax_remediator_up 1` (the daemon is, by definition, up if it's
/// serving this request); the other blocks are omitted entirely when empty,
/// same as the exporter's `render`.
pub fn render(snap: &MetricsSnapshot) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "# HELP gocoax_remediator_up Whether the remediator daemon is up (always 1 while serving).");
    let _ = writeln!(out, "# TYPE gocoax_remediator_up gauge");
    let _ = writeln!(out, "gocoax_remediator_up 1");

    if !snap.reboots_total.is_empty() {
        let _ = writeln!(out, "# HELP gocoax_remediator_reboots_total Count of reboots performed, by device and triggering rule.");
        let _ = writeln!(out, "# TYPE gocoax_remediator_reboots_total counter");
        for (device, reason, count) in &snap.reboots_total {
            let _ = writeln!(
                out,
                "gocoax_remediator_reboots_total{{device=\"{}\",reason=\"{}\"}} {}",
                esc(device),
                esc(reason),
                count
            );
        }
    }

    if !snap.would_reboot_total.is_empty() {
        let _ = writeln!(
            out,
            "# HELP gocoax_remediator_would_reboot_total Count of reboots that dry_run suppressed, by device and triggering rule."
        );
        let _ = writeln!(out, "# TYPE gocoax_remediator_would_reboot_total counter");
        for (device, reason, count) in &snap.would_reboot_total {
            let _ = writeln!(
                out,
                "gocoax_remediator_would_reboot_total{{device=\"{}\",reason=\"{}\"}} {}",
                esc(device),
                esc(reason),
                count
            );
        }
    }

    if !snap.last_reboot_ts.is_empty() {
        let _ = writeln!(out, "# HELP gocoax_remediator_last_reboot_timestamp_seconds Unix timestamp of the device's last reboot performed by this daemon.");
        let _ = writeln!(out, "# TYPE gocoax_remediator_last_reboot_timestamp_seconds gauge");
        for (device, ts) in &snap.last_reboot_ts {
            let _ = writeln!(
                out,
                "gocoax_remediator_last_reboot_timestamp_seconds{{device=\"{}\"}} {}",
                esc(device),
                ts
            );
        }
    }

    if !snap.circuit_open.is_empty() {
        let _ = writeln!(out, "# HELP gocoax_remediator_circuit_open Whether the circuit breaker has tripped for a device (1=open, no longer auto-rebooting).");
        let _ = writeln!(out, "# TYPE gocoax_remediator_circuit_open gauge");
        for (device, open) in &snap.circuit_open {
            let _ = writeln!(
                out,
                "gocoax_remediator_circuit_open{{device=\"{}\"}} {}",
                esc(device),
                if *open { 1 } else { 0 }
            );
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn up_line_always_present_even_for_empty_snapshot() {
        let out = render(&MetricsSnapshot::default());
        assert!(out.contains("gocoax_remediator_up 1"));
        assert!(!out.contains("gocoax_remediator_reboots_total{"));
        assert!(!out.contains("gocoax_remediator_would_reboot_total{"));
        assert!(!out.contains("gocoax_remediator_last_reboot_timestamp_seconds{"));
        assert!(!out.contains("gocoax_remediator_circuit_open{"));
    }

    #[test]
    fn one_reboot_renders_counter_and_timestamp_lines() {
        let snap = MetricsSnapshot {
            reboots_total: vec![("ff".to_string(), "unreachable".to_string(), 1)],
            would_reboot_total: vec![],
            last_reboot_ts: vec![("ff".to_string(), 1_700_000_000.0)],
            circuit_open: vec![("ff".to_string(), false)],
        };
        let out = render(&snap);
        assert!(out.contains("gocoax_remediator_up 1"));
        assert!(out.contains("gocoax_remediator_reboots_total{device=\"ff\",reason=\"unreachable\"} 1"));
        assert!(out.contains("gocoax_remediator_last_reboot_timestamp_seconds{device=\"ff\"} 1700000000"));
        assert!(out.contains("gocoax_remediator_circuit_open{device=\"ff\"} 0"));
        assert!(!out.contains("gocoax_remediator_would_reboot_total{"));
    }

    #[test]
    fn dry_run_would_reboot_and_open_circuit_render() {
        let snap = MetricsSnapshot {
            reboots_total: vec![],
            would_reboot_total: vec![("gg".to_string(), "link_down".to_string(), 3)],
            last_reboot_ts: vec![],
            circuit_open: vec![("gg".to_string(), true)],
        };
        let out = render(&snap);
        assert!(out.contains("gocoax_remediator_would_reboot_total{device=\"gg\",reason=\"link_down\"} 3"));
        assert!(out.contains("gocoax_remediator_circuit_open{device=\"gg\"} 1"));
        assert!(!out.contains("gocoax_remediator_reboots_total{"));
    }
}
