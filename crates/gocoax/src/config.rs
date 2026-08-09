use crate::{Error, Result};
use serde::Deserialize;

fn d_listen() -> String { "0.0.0.0:9420".into() }
fn d_req() -> u64 { 8 }
fn d_con() -> u64 { 3 }
fn d_dead() -> u64 { 9 }

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default = "d_listen")]
    pub listen: String,
    #[serde(default = "d_req")]
    pub request_timeout_secs: u64,
    #[serde(default = "d_con")]
    pub connect_timeout_secs: u64,
    #[serde(default = "d_dead")]
    pub scrape_deadline_secs: u64,
    pub username: Option<String>,
    pub password: Option<String>,
    pub password_env: Option<String>,
    pub password_file: Option<String>,
    #[serde(default)]
    pub device: Vec<Device>,
}

#[derive(Debug, Deserialize)]
pub struct Device {
    pub name: String,
    pub host: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub password_env: Option<String>,
    pub password_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCreds {
    pub username: String,
    pub password: String,
}

impl Config {
    pub fn from_toml(s: &str) -> Result<Config> {
        toml::from_str(s).map_err(|e| Error::Config(e.to_string()))
    }

    pub fn creds_for(&self, dev: &Device) -> Result<ResolvedCreds> {
        let username = dev.username.clone()
            .or_else(|| self.username.clone())
            .unwrap_or_else(|| "admin".into());
        let password = resolve_password(
            dev.password.as_deref().or(self.password.as_deref()),
            dev.password_env.as_deref().or(self.password_env.as_deref()),
            dev.password_file.as_deref().or(self.password_file.as_deref()),
        )?;
        Ok(ResolvedCreds { username, password })
    }
}

fn resolve_password(inline: Option<&str>, env: Option<&str>, file: Option<&str>) -> Result<String> {
    if let Some(p) = inline { return Ok(p.to_string()); }
    if let Some(var) = env {
        return std::env::var(var).map_err(|_| Error::Config(format!("env {var} not set")));
    }
    if let Some(path) = file {
        return std::fs::read_to_string(path)
            .map(|s| s.trim().to_string())
            .map_err(|e| Error::Config(format!("password_file {path}: {e}")));
    }
    Err(Error::Config("no password configured".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_defaults_and_devices() {
        let c = Config::from_toml(
            "username=\"admin\"\npassword=\"g\"\n[[device]]\nname=\"a\"\nhost=\"10.0.0.1\"\n",
        ).unwrap();
        assert_eq!(c.listen, "0.0.0.0:9420");
        assert_eq!(c.scrape_deadline_secs, 9);
        assert_eq!(c.device.len(), 1);
        let cr = c.creds_for(&c.device[0]).unwrap();
        assert_eq!(cr.username, "admin");
        assert_eq!(cr.password, "g");
    }

    #[test]
    fn device_overrides_global() {
        let c = Config::from_toml(
            "username=\"admin\"\npassword=\"g\"\n[[device]]\nname=\"a\"\nhost=\"h\"\nusername=\"root\"\npassword=\"x\"\n",
        ).unwrap();
        let cr = c.creds_for(&c.device[0]).unwrap();
        assert_eq!(cr.username, "root");
        assert_eq!(cr.password, "x");
    }
}
