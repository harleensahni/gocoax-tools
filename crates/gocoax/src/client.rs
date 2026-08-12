//! Async HTTP client for the GoCoax MoCA adapter's `ms` (management-station)
//! JSON endpoints.
//!
//! Handles Basic auth (every request) and the device's csrf-cookie dance:
//! a GET of `/index.html` returns a `Set-Cookie: csrf_token=<hex>` header;
//! that token is reusable indefinitely and is cached for the lifetime of
//! the `Client`, attached to every POST as both an `X-CSRF-TOKEN` header
//! and a `Cookie: csrf_token=<t>` header. If a POST comes back `403`, the
//! cached token is discarded, refetched once, and the POST retried once.

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::RwLock;

use crate::config::ResolvedCreds;
use crate::decode::{decode_net_nodes, get, DeviceStatus, MocaNode, MAX_NODES};
use crate::ms::{self, parse_ms_response, MsCmd};
use crate::phy::{decode_fmr, PhyRates};
use crate::{Error, Result};

/// Request body for registers read with no arguments.
const EMPTY_BODY: &str = r#"{"data":[]}"#;
/// Request body for registers read with a single placeholder argument (the
/// device ignores its value for these GET-style commands).
const ZERO_ARG_BODY: &str = r#"{"data":[0]}"#;

pub struct ClientOpts {
    pub request_timeout: Duration,
    pub connect_timeout: Duration,
    /// When true, log each HTTP request and its outcome to stderr.
    pub verbose: bool,
}

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    creds: ResolvedCreds,
    verbose: bool,
    // Cached csrf token, shared across calls for this Client's lifetime.
    // A token is reusable indefinitely; we only refetch it on a 403.
    csrf: RwLock<Option<String>>,
}

impl Client {
    pub fn new(host: &str, creds: ResolvedCreds, opts: ClientOpts) -> Result<Client> {
        // Note: reqwest only auto-manages cookies when built with its
        // "cookies" feature, which this workspace does not enable (see
        // Cargo.toml: reqwest features = ["json"]). So there is no cookie
        // jar to disable here -- we already fully control the csrf cookie
        // via the cache below and an explicit `Cookie` header per request.
        // CRITICAL device-interop requirement: the InterNiche WebServer 2.0 on
        // these adapters does CASE-SENSITIVE HTTP header matching and only
        // recognizes `Authorization` (capital A). hyper/reqwest normalize
        // HTTP/1.1 header names to lowercase, so without this the device
        // rejects every request with 401 (even though the Basic credentials are
        // correct). `http1_title_case_headers()` makes hyper emit
        // `Authorization`/`Host`/etc., which the device accepts. This cannot be
        // caught by the wiremock tests (wiremock is RFC-compliant, i.e.
        // case-insensitive) — it only shows up against real hardware.
        let http = reqwest::Client::builder()
            .connect_timeout(opts.connect_timeout)
            .timeout(opts.request_timeout)
            .http1_title_case_headers()
            .build()
            .map_err(|e: reqwest::Error| Error::Http(e.to_string()))?;
        Ok(Client {
            http,
            base_url: format!("http://{host}"),
            creds,
            verbose: opts.verbose,
            csrf: RwLock::new(None),
        })
    }

    /// Log a line to stderr when verbose mode is on.
    fn log(&self, args: std::fmt::Arguments) {
        if self.verbose {
            eprintln!("[gocoax] {} {args}", self.base_url);
        }
    }

    /// Ensure a csrf token is cached, returning it. Fetches one via GET
    /// `/index.html` if the cache is empty.
    async fn ensure_csrf(&self) -> Result<String> {
        if let Some(t) = self.csrf.read().await.clone() {
            return Ok(t);
        }
        self.fetch_csrf().await
    }

    async fn clear_csrf(&self) {
        *self.csrf.write().await = None;
    }

    async fn fetch_csrf(&self) -> Result<String> {
        let url = format!("{}/index.html", self.base_url);
        self.log(format_args!("GET /index.html (fetch csrf)"));
        let resp = self
            .http
            .get(&url)
            .basic_auth(&self.creds.username, Some(&self.creds.password))
            .send()
            .await
            .map_err(map_reqwest_err)?;
        let status = resp.status();
        self.log(format_args!("GET /index.html -> {}", status.as_u16()));
        if !status.is_success() {
            return Err(status_to_error(status.as_u16()));
        }
        let token = resp
            .headers()
            .get(reqwest::header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(extract_csrf_token)
            .ok_or(Error::Csrf)?;
        *self.csrf.write().await = Some(token.clone());
        Ok(token)
    }

    async fn post_once(&self, cmd: MsCmd, body: &str, token: &str) -> Result<reqwest::Response> {
        let url = format!("{}{}", self.base_url, cmd.path());
        self.log(format_args!("POST {} {body}", cmd.path()));
        let resp = self
            .http
            .post(&url)
            .basic_auth(&self.creds.username, Some(&self.creds.password))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("X-CSRF-TOKEN", token)
            .header("Cookie", format!("csrf_token={token}"))
            .body(body.to_string())
            .send()
            .await
            .map_err(map_reqwest_err)?;
        self.log(format_args!("POST {} -> {}", cmd.path(), resp.status().as_u16()));
        Ok(resp)
    }

    /// Read one `ms` register: POST `cmd.path()` with `body`, attaching the
    /// cached csrf token, and parse the response's `data` word array.
    ///
    /// On a `403` (stale/rejected csrf token), the cached token is cleared,
    /// refetched once, and the POST retried once.
    pub async fn read(&self, cmd: MsCmd, body: &str) -> Result<Vec<u32>> {
        let token = self.ensure_csrf().await?;
        let mut resp = self.post_once(cmd, body, &token).await?;
        if resp.status().as_u16() == 403 {
            self.clear_csrf().await;
            let token = self.ensure_csrf().await?;
            resp = self.post_once(cmd, body, &token).await?;
        }
        let status = resp.status();
        if !status.is_success() {
            return Err(status_to_error(status.as_u16()));
        }
        let text = resp.text().await.map_err(map_reqwest_err)?;
        parse_ms_response(&text)
    }

    /// Read the registers backing [`DeviceStatus`] and decode them.
    pub async fn device_status(&self) -> Result<DeviceStatus> {
        let local = self.read(ms::LOCAL_INFO, EMPTY_BODY).await?;
        let mac = self.read(ms::MAC_INFO, ZERO_ARG_BODY).await?;
        let frame = self.read(ms::FRAME_INFO, ZERO_ARG_BODY).await?;
        let ip = self.read(ms::IP_ADDR, ZERO_ARG_BODY).await?;
        let lof = self.read(ms::LOF, ZERO_ARG_BODY).await?;
        // ETH_INFO (per-port link/speed, register 0x307) was added in newer
        // firmware; older adapters don't implement it and return 400. Read it
        // best-effort: on any failure fall back to an empty payload so a
        // firmware-drifted device still reports up with all its other data,
        // and the per-port eth metrics are simply omitted (metrics::render
        // already skips absent eth_ports). Propagating the error here would
        // instead mark the whole device down -- and make the remediator try to
        // reboot a healthy adapter over a feature its firmware simply lacks.
        let eth = match self.read(ms::ETH_INFO, ZERO_ARG_BODY).await {
            Ok(words) => words,
            Err(e) => {
                self.log(format_args!("ETH_INFO (0x307) read failed: {e}; omitting per-port eth metrics"));
                Vec::new()
            }
        };
        DeviceStatus::decode(&local, &mac, &frame, &ip, &lof, &eth)
    }

    /// Read LOCAL_INFO, then per-present-node NET_INFO and FMR_INFO, and
    /// assemble the full PHY-rate matrix (mirrors `phyRates.html`'s
    /// `formLoad`/`refreshPage` sequence).
    pub async fn phy_rates(&self) -> Result<PhyRates> {
        let local = self.read(ms::LOCAL_INFO, EMPTY_BODY).await?;
        let node_bitmask = get(&local, 12, "0x15")?;
        let nc_node_id = get(&local, 1, "0x15")?;
        let moca_net_raw = get(&local, 11, "0x15")?;
        // finalVer sent to FMR_INFO: the device's JS computes this per node
        // pair as min(ncMocaVer, nodeMocaVer) < 0x20 ? 1 : 2. All observed
        // hardware is uniformly MoCA 2.5, so we simplify to a single
        // network-wide value derived from LOCAL_INFO's own moca-version
        // field (word 11, the same field DeviceStatus::decode uses).
        let final_ver: u32 = if moca_net_raw >= 0x20 { 2 } else { 1 };

        let mut node_vers = vec![0u8; MAX_NODES as usize];
        let mut rates = PhyRates::default();
        for node in 0..MAX_NODES {
            if node_bitmask & (1 << node) == 0 {
                continue;
            }
            let body = format!(r#"{{"data":[{node}]}}"#);
            let net = self.read(ms::NET_INFO, &body).await?;
            let ver = get(&net, 4, "0x16")? as u8;
            node_vers[node as usize] = ver;
            rates.node_versions.push((node, ver));
        }
        let nc_moca_ver = node_vers.get(nc_node_id as usize).copied().unwrap_or(0);

        for node in 0..MAX_NODES {
            if node_bitmask & (1 << node) == 0 {
                continue;
            }
            let body = format!(r#"{{"data":[{},{}]}}"#, 1u32 << node, final_ver);
            let fmr = self.read(ms::FMR_INFO, &body).await?;
            let links = decode_fmr(node, node_bitmask, &node_vers, nc_moca_ver, &fmr)?;
            // NOTE: the device also has a separate `rateGcd` block
            // (phyRates.html lines ~236-249) that computes the diagonal
            // "GCD" cell independently of the per-peer NPER path. It
            // coincides with the self-link (from==to) NPER value for MoCA
            // 2.5 nodes (all our hardware), but would diverge for an
            // exact-MoCA-2.0 node with gapVLper==0. We take the simpler
            // self-link NPER here; revisit if exact-2.0 nodes appear.
            for link in &links {
                if link.from_node == link.to_node {
                    rates.gcd_mbps.push((link.from_node, link.nper_mbps));
                }
            }
            rates.links.extend(links);
        }
        Ok(rates)
    }

    /// Read LOCAL_INFO for the node bitmask, then per-present-node NET_INFO,
    /// and decode the MoCA self-report node list (mirrors `moca_nodes.html`'s
    /// enumeration). All async reads happen up front, into a map, so the
    /// sync `decode_net_nodes` closure can simply look them up.
    pub async fn moca_nodes(&self) -> Result<Vec<MocaNode>> {
        let local = self.read(ms::LOCAL_INFO, EMPTY_BODY).await?;
        let node_bitmask = get(&local, 12, "0x15")?;

        let mut nets: HashMap<u32, Vec<u32>> = HashMap::new();
        for node in 0..MAX_NODES {
            if node_bitmask & (1 << node) == 0 {
                continue;
            }
            let body = format!(r#"{{"data":[{node}]}}"#);
            let net = self.read(ms::NET_INFO, &body).await?;
            nets.insert(node, net);
        }

        decode_net_nodes(&local, |id| nets.get(&id).cloned().unwrap_or_default())
    }

    /// Trigger a device reboot (fire-and-forget).
    ///
    /// The adapter power-cycles the instant it receives `0xb00` and drops the
    /// connection without sending an HTTP response — its own web UI fires this
    /// POST with empty callbacks and just reloads after 10s. So a **timeout or
    /// dropped connection after the request was sent means the reboot took
    /// effect** and is reported as success. Only a genuine failure to reach the
    /// device (connect error) or an auth rejection (401) is an error.
    pub async fn reboot(&self) -> Result<()> {
        let token = self.ensure_csrf().await?;
        match self.reboot_post_once(&token).await? {
            None => Ok(()),
            // Stale csrf token — refetch once and retry.
            Some(403) => {
                self.clear_csrf().await;
                let token = self.ensure_csrf().await?;
                match self.reboot_post_once(&token).await? {
                    None => Ok(()),
                    Some(code) => Err(status_to_error(code)),
                }
            }
            Some(code) => Err(status_to_error(code)),
        }
    }

    /// Send the reboot POST once. `Ok(None)` = the device accepted it (a 2xx, or
    /// a timeout / dropped connection mid-response, which is how a real reboot
    /// manifests). `Ok(Some(code))` = a non-2xx reply (e.g. 401/403). `Err` =
    /// we never reached the device (connect failure), so it did not reboot.
    async fn reboot_post_once(&self, token: &str) -> Result<Option<u16>> {
        let url = format!("{}{}", self.base_url, ms::REBOOT.path());
        self.log(format_args!("POST {} (reboot)", ms::REBOOT.path()));
        let res = self
            .http
            .post(&url)
            .basic_auth(&self.creds.username, Some(&self.creds.password))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("X-CSRF-TOKEN", token)
            .header("Cookie", format!("csrf_token={token}"))
            .body(EMPTY_BODY.to_string())
            .send()
            .await;
        match res {
            Ok(resp) => {
                let code = resp.status().as_u16();
                self.log(format_args!("POST {} -> {code}", ms::REBOOT.path()));
                if (200..300).contains(&code) {
                    Ok(None)
                } else {
                    Ok(Some(code))
                }
            }
            // Never established a connection → the reboot was not sent.
            Err(e) if e.is_connect() => {
                self.log(format_args!("reboot connect failed: {e}"));
                Err(Error::Http(e.to_string()))
            }
            // Connected and sent, then timed out / connection dropped → the
            // device rebooted before it could reply. Treat as success.
            Err(e) => {
                self.log(format_args!(
                    "reboot: no response after send ({e}) — device rebooted, treating as success"
                ));
                Ok(None)
            }
        }
    }
}

fn status_to_error(code: u16) -> Error {
    match code {
        401 => Error::Auth,
        403 => Error::Csrf,
        other => Error::HttpStatus(other),
    }
}

fn map_reqwest_err(e: reqwest::Error) -> Error {
    if e.is_timeout() {
        Error::Timeout
    } else {
        Error::Http(e.to_string())
    }
}

/// Extract the `csrf_token` value from a `Set-Cookie` header, e.g.
/// `"csrf_token=ABC123; SameSite=Strict"` -> `Some("ABC123")`.
fn extract_csrf_token(set_cookie: &str) -> Option<String> {
    set_cookie
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("csrf_token="))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_token_from_set_cookie() {
        assert_eq!(
            extract_csrf_token("csrf_token=ABC123; SameSite=Strict"),
            Some("ABC123".to_string())
        );
        assert_eq!(extract_csrf_token("other=1; csrf_token=XYZ"), Some("XYZ".to_string()));
        assert_eq!(extract_csrf_token("other=1"), None);
    }
}
