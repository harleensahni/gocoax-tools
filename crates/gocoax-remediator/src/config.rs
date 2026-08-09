//! Config for the remediator daemon.
//!
//! The daemon reads the *same* `config.toml` the exporter uses (for the
//! `[[device]]` list and credentials) plus one extra `[remediator]` table.
//! `gocoax::config::Config` ignores unknown tables, so parsing the same text
//! twice -- once as a `gocoax::config::Config`, once as a `RemediatorConfig`
//! -- is the simplest way to reuse the existing device/credential parsing
//! without touching the `gocoax` crate.

use std::error::Error;

use serde::Deserialize;

fn d_poll() -> u64 {
    60
}
fn d_cooldown() -> u64 {
    1800
}
fn d_maxday() -> u32 {
    4
}
fn d_listen() -> String {
    "0.0.0.0:9421".into()
}
/// `dry_run` must fail CLOSED: if the key is omitted from config, the
/// daemon must NOT perform live reboots. A user has to opt in explicitly
/// with `dry_run = false`.
fn d_dry_run() -> bool {
    true
}

/// One trigger rule: a PromQL expression whose result vector's `device`
/// labels name adapters considered problematic. `name` becomes the `reason`
/// label on the reboot metrics.
#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub name: String,
    pub expr: String,
}

#[derive(Debug, Deserialize)]
pub struct RemediatorConfig {
    pub prometheus_url: String,
    #[serde(default = "d_poll")]
    pub poll_interval_secs: u64,
    #[serde(default = "d_cooldown")]
    pub cooldown_secs: u64,
    #[serde(default = "d_maxday")]
    pub max_reboots_per_day: u32,
    #[serde(default = "d_listen")]
    pub listen: String,
    #[serde(default = "d_dry_run")]
    pub dry_run: bool,
    /// Log each poll cycle and per-device decision (matched rule, cooldown,
    /// breaker, action) to stderr. Off by default.
    #[serde(default)]
    pub verbose: bool,
    /// Trigger rules; may be empty (the daemon just polls nothing and
    /// exposes /metrics).
    #[serde(default)]
    pub rule: Vec<Rule>,
}

/// Wrapper purely so `toml` can deserialize the `[remediator]` table on its
/// own, independent of `gocoax::config::Config`'s parsing of the rest of the
/// file.
#[derive(Debug, Deserialize)]
struct TopLevel {
    remediator: RemediatorConfig,
}

/// Parse `path` twice: once as a `gocoax::config::Config` (devices +
/// credentials), once as a `[remediator]` table. Both parses read the same
/// file text; each struct ignores fields/tables it doesn't know about.
pub fn load(path: &str) -> Result<(gocoax::config::Config, RemediatorConfig), Box<dyn Error>> {
    let text = std::fs::read_to_string(path)?;
    let cfg = gocoax::config::Config::from_toml(&text)?;
    let top: TopLevel = toml::from_str(&text)?;
    Ok((cfg, top.remediator))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOML: &str = r#"
username = "admin"
password = "g"

[[device]]
name = "ff"
host = "10.0.0.1"

[remediator]
prometheus_url = "http://prometheus:9090"

[[remediator.rule]]
name = "unreachable"
expr = "max_over_time(gocoax_up[10m]) == 0"
"#;

    #[test]
    fn parses_devices_and_remediator_table_with_defaults() {
        let dir = std::env::temp_dir().join(format!("gocoax-remediator-test-{}", std::process::id()));
        std::fs::write(&dir, TOML).unwrap();

        let (cfg, rcfg) = load(dir.to_str().unwrap()).unwrap();
        std::fs::remove_file(&dir).ok();

        assert_eq!(cfg.device.len(), 1);
        assert_eq!(cfg.device[0].name, "ff");

        assert_eq!(rcfg.prometheus_url, "http://prometheus:9090");
        assert_eq!(rcfg.poll_interval_secs, 60);
        assert_eq!(rcfg.cooldown_secs, 1800);
        assert_eq!(rcfg.max_reboots_per_day, 4);
        assert_eq!(rcfg.listen, "0.0.0.0:9421");
        // dry_run must default to true (fail closed): omitting the key
        // must never silently enable live reboots.
        assert!(rcfg.dry_run);
        assert_eq!(rcfg.rule.len(), 1);
        assert_eq!(rcfg.rule[0].name, "unreachable");
    }

    #[test]
    fn overrides_and_empty_rule_list_both_work() {
        let toml = r#"
username = "admin"
password = "g"

[remediator]
prometheus_url = "http://prometheus:9090"
poll_interval_secs = 30
cooldown_secs = 60
max_reboots_per_day = 2
listen = "127.0.0.1:9421"
dry_run = true
"#;
        let dir = std::env::temp_dir().join(format!("gocoax-remediator-test2-{}", std::process::id()));
        std::fs::write(&dir, toml).unwrap();
        let (_cfg, rcfg) = load(dir.to_str().unwrap()).unwrap();
        std::fs::remove_file(&dir).ok();

        assert_eq!(rcfg.poll_interval_secs, 30);
        assert_eq!(rcfg.cooldown_secs, 60);
        assert_eq!(rcfg.max_reboots_per_day, 2);
        assert_eq!(rcfg.listen, "127.0.0.1:9421");
        assert!(rcfg.dry_run);
        assert!(rcfg.rule.is_empty());
    }

    #[test]
    fn dry_run_must_be_explicitly_disabled_to_enable_live_reboots() {
        let toml = r#"
username = "admin"
password = "g"

[remediator]
prometheus_url = "http://prometheus:9090"
dry_run = false
"#;
        let dir = std::env::temp_dir().join(format!("gocoax-remediator-test3-{}", std::process::id()));
        std::fs::write(&dir, toml).unwrap();
        let (_cfg, rcfg) = load(dir.to_str().unwrap()).unwrap();
        std::fs::remove_file(&dir).ok();

        assert!(!rcfg.dry_run);
    }
}
