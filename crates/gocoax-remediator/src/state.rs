//! The cooldown + circuit-breaker state machine.
//!
//! Pure and clock-free: callers pass in `now_unix` and `today` (an opaque
//! "day bucket" string -- see `main.rs` for how it's computed) rather than
//! this module reading the clock itself, which is what keeps `decide` and
//! `record_reboot` deterministic and unit-testable.

/// Safety limits, sourced from `[remediator]` config.
pub struct Limits {
    pub cooldown_secs: u64,
    pub max_reboots_per_day: u32,
}

/// Per-device cooldown/circuit-breaker bookkeeping.
#[derive(Debug, Clone, Default)]
pub struct DeviceState {
    /// Unix timestamp (seconds) of the last reboot this daemon performed.
    pub last_reboot_unix: Option<f64>,
    /// The "day bucket" `reboots_today` was accumulated for. Compared
    /// against the caller-supplied `today` to detect a day rollover.
    pub day: String,
    /// Reboots performed so far during `day`.
    pub reboots_today: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Reboot,
    Cooldown,
    CircuitOpen,
}

/// Decide whether `device` may be rebooted right now. Does not mutate `st`
/// -- callers call `record_reboot` separately once a reboot has actually
/// happened (or, for `dry_run`, deliberately skip that call so the "would
/// reboot" decision keeps re-firing every poll).
///
/// Precedence: the circuit breaker is checked first, since it's the more
/// persistent condition -- a device that has hit its daily cap should read
/// as `CircuitOpen` even if it also happens to be inside its cooldown
/// window from that same last reboot.
pub fn decide(st: &DeviceState, limits: &Limits, now_unix: f64, today: &str) -> Decision {
    let reboots_today = if st.day == today { st.reboots_today } else { 0 };
    if reboots_today >= limits.max_reboots_per_day {
        return Decision::CircuitOpen;
    }
    if let Some(last) = st.last_reboot_unix {
        if now_unix - last < limits.cooldown_secs as f64 {
            return Decision::Cooldown;
        }
    }
    Decision::Reboot
}

/// Record a reboot that just happened: stamps `last_reboot_unix`, rolls the
/// daily counter over on a day change, and bumps it.
pub fn record_reboot(st: &mut DeviceState, now_unix: f64, today: &str) {
    st.last_reboot_unix = Some(now_unix);
    if st.day != today {
        st.day = today.to_string();
        st.reboots_today = 0;
    }
    st.reboots_today += 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits { cooldown_secs: 1800, max_reboots_per_day: 4 }
    }

    #[test]
    fn fresh_device_may_reboot() {
        let st = DeviceState::default();
        assert_eq!(decide(&st, &limits(), 1_000_000.0, "1"), Decision::Reboot);
    }

    #[test]
    fn within_cooldown_is_blocked() {
        let mut st = DeviceState::default();
        record_reboot(&mut st, 1_000_000.0, "1");
        // Only 60s later, well inside the 1800s cooldown.
        assert_eq!(decide(&st, &limits(), 1_000_060.0, "1"), Decision::Cooldown);
    }

    #[test]
    fn cooldown_elapsed_allows_reboot_again() {
        let mut st = DeviceState::default();
        record_reboot(&mut st, 1_000_000.0, "1");
        // 1800s later, exactly at the cooldown boundary -> no longer < cooldown.
        assert_eq!(decide(&st, &limits(), 1_001_800.0, "1"), Decision::Reboot);
    }

    #[test]
    fn max_reboots_per_day_trips_circuit_breaker() {
        let mut st = DeviceState::default();
        // Space reboots far enough apart that cooldown never blocks them,
        // so we're isolating the circuit-breaker behavior specifically.
        for i in 0..4 {
            record_reboot(&mut st, 1_000_000.0 + i as f64 * 10_000.0, "1");
        }
        assert_eq!(st.reboots_today, 4);
        assert_eq!(decide(&st, &limits(), 1_100_000.0, "1"), Decision::CircuitOpen);
    }

    #[test]
    fn circuit_breaker_takes_precedence_over_cooldown() {
        let mut st = DeviceState::default();
        for i in 0..4 {
            record_reboot(&mut st, 1_000_000.0 + i as f64 * 10.0, "1");
        }
        // Immediately after the 4th reboot: would also be inside cooldown,
        // but CircuitOpen must win.
        assert_eq!(decide(&st, &limits(), 1_000_031.0, "1"), Decision::CircuitOpen);
    }

    #[test]
    fn new_day_resets_daily_count() {
        let mut st = DeviceState::default();
        for i in 0..4 {
            record_reboot(&mut st, 1_000_000.0 + i as f64 * 10_000.0, "day-a");
        }
        assert_eq!(decide(&st, &limits(), 1_000_030.0, "day-a"), Decision::CircuitOpen);

        // A new day, far enough past the last reboot to also clear cooldown.
        assert_eq!(decide(&st, &limits(), 2_000_000.0, "day-b"), Decision::Reboot);
    }
}
