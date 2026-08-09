//! LAN discovery: find MoCA adapters on a subnet by fingerprinting the
//! InterNiche embedded web server they expose. The adapter's HTTP server
//! answers `GET /` with a `Server: InterNiche ...` header even without
//! authentication (a 401 still carries the header), so an unauthenticated
//! probe is enough to identify candidate hosts.

use std::net::Ipv4Addr;
use std::time::Duration;

use tokio::task::JoinSet;

use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub ip: Ipv4Addr,
    pub server: Option<String>,
    pub mac: Option<String>,
}

/// True if an HTTP response `Server` header identifies a MoCA adapter web UI.
pub fn is_moca_server(server_header: &str) -> bool {
    server_header.to_ascii_lowercase().contains("interniche")
}

/// Expand a dotted IPv4 CIDR (e.g. "192.0.2.0/24") into host addresses
/// (excludes network + broadcast). Errors on malformed input or prefix < 16
/// (too many hosts to scan).
pub fn cidr_hosts(cidr: &str) -> Result<Vec<Ipv4Addr>> {
    let (addr, prefix) = cidr
        .split_once('/')
        .ok_or_else(|| Error::Config(format!("bad cidr {cidr:?}")))?;
    let base: Ipv4Addr = addr
        .parse()
        .map_err(|_| Error::Config(format!("bad cidr address {addr:?}")))?;
    let prefix: u32 = prefix
        .parse()
        .map_err(|_| Error::Config(format!("bad cidr prefix {prefix:?}")))?;
    if !(16..=32).contains(&prefix) {
        return Err(Error::Config(format!("cidr prefix {prefix} out of range 16..=32")));
    }
    let base_u = u32::from(base);
    // prefix is validated to 16..=32 above, so 32 - prefix is always 0..=16
    // and this shift never overflows (a prefix == 0 case, which would need
    // the special-cased mask 0, cannot reach here).
    let mask = u32::MAX << (32 - prefix);
    let network = base_u & mask;
    let broadcast = network | !mask;
    let mut out = Vec::new();
    if prefix >= 31 {
        // /31 and /32: no network/broadcast exclusion
        for h in network..=broadcast {
            out.push(Ipv4Addr::from(h));
        }
    } else {
        for h in (network + 1)..broadcast {
            out.push(Ipv4Addr::from(h));
        }
    }
    Ok(out)
}

/// Probe each host concurrently: GET http://<ip>/ with a short timeout,
/// read the `Server` header (present on the 401 too -- no auth needed).
/// Returns hosts whose Server header passes `is_moca_server`. Connection
/// errors and timeouts are treated as "not an adapter" and silently
/// dropped -- they are the expected outcome for the vast majority of
/// addresses in a scanned subnet.
pub async fn http_fingerprint(
    hosts: &[Ipv4Addr],
    connect_ms: u64,
    total_ms: u64,
    concurrency: usize,
) -> Vec<Found> {
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(connect_ms))
        .timeout(Duration::from_millis(total_ms))
        .build()
    {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let concurrency = concurrency.max(1);
    let mut found = Vec::new();
    let mut hosts_iter = hosts.iter().copied();
    let mut in_flight: JoinSet<Option<Found>> = JoinSet::new();

    // Keep up to `concurrency` probes in flight: seed the pool, then for
    // every completion pull one more host in, until the iterator and the
    // pool are both drained.
    for ip in hosts_iter.by_ref().take(concurrency) {
        in_flight.spawn(probe_host(client.clone(), ip));
    }
    while let Some(result) = in_flight.join_next().await {
        if let Ok(Some(f)) = result {
            found.push(f);
        }
        if let Some(ip) = hosts_iter.next() {
            in_flight.spawn(probe_host(client.clone(), ip));
        }
    }

    found
}

async fn probe_host(client: reqwest::Client, ip: Ipv4Addr) -> Option<Found> {
    let url = format!("http://{ip}/");
    let resp = client.get(&url).send().await.ok()?;
    let server = resp
        .headers()
        .get(reqwest::header::SERVER)
        .and_then(|v| v.to_str().ok())?;
    if is_moca_server(server) {
        Some(Found { ip, server: Some(server.to_string()), mac: None })
    } else {
        None
    }
}

/// Known MoCA-adapter MAC OUIs (first 3 bytes, lowercase, colon-less).
/// GoCoax / MaxLinear-based units observed: "94cc04". Extend as needed.
pub const MOCA_OUIS: &[&str] = &["94cc04"];

/// True if a MAC's OUI is in `ouis` (MAC may be colon- or dash-separated).
/// Each octet is zero-padded to two hex digits before comparison, so BSD
/// `arp`'s zero-compressed form (e.g. "94:cc:4:...") still normalizes to
/// the full OUI ("94cc04").
pub fn mac_oui_matches(mac: &str, ouis: &[&str]) -> bool {
    let octets: Vec<&str> = mac.split(['-', ':']).collect();
    if octets.len() < 3 {
        return false;
    }
    let oui: String = octets[..3]
        .iter()
        .map(|o| format!("{:0>2}", o.to_ascii_lowercase()))
        .collect();
    ouis.iter().any(|candidate| candidate.eq_ignore_ascii_case(&oui))
}

/// Parse `arp -a` style output into (ip, mac) pairs. Expects lines of the
/// macOS/BSD form `? (192.0.2.250) at 94:cc:4:00:00:01 on en0 ...`.
/// Lines lacking a parenthesized IP or an `at <mac>` token are skipped.
pub fn parse_arp_table(text: &str) -> Vec<(Ipv4Addr, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(open) = line.find('(') else { continue };
        let Some(close) = line[open..].find(')') else { continue };
        let ip_str = &line[open + 1..open + close];
        let Ok(ip) = ip_str.parse::<Ipv4Addr>() else { continue };

        let mut mac = None;
        let mut tokens = line.split_whitespace();
        while let Some(tok) = tokens.next() {
            if tok == "at" {
                mac = tokens.next();
                break;
            }
        }
        let Some(mac) = mac else { continue };
        out.push((ip, mac.to_string()));
    }
    out
}

/// Read the system ARP table (`arp -a`) and return entries whose OUI
/// matches one of `ouis`.
pub fn mac_filter(ouis: &[&str]) -> Result<Vec<Found>> {
    let output = std::process::Command::new("arp")
        .arg("-a")
        .output()
        .map_err(|e| Error::Config(format!("failed to run `arp -a`: {e}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let entries = parse_arp_table(&stdout);
    Ok(entries
        .into_iter()
        .filter(|(_, mac)| mac_oui_matches(mac, ouis))
        .map(|(ip, mac)| Found { ip, server: None, mac: Some(mac) })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_interniche_server() {
        assert!(is_moca_server("InterNiche Technologies WebServer 2.0"));
        assert!(is_moca_server("interniche technologies webserver 2.0"));
        assert!(!is_moca_server("nginx/1.25"));
        assert!(!is_moca_server(""));
    }

    #[test]
    fn cidr_24_expands_to_254_hosts() {
        let hosts = cidr_hosts("192.0.2.0/24").unwrap();
        assert_eq!(hosts.len(), 254);
        assert_eq!(hosts[0], "192.0.2.1".parse::<Ipv4Addr>().unwrap());
        assert_eq!(hosts[253], "192.0.2.254".parse::<Ipv4Addr>().unwrap());
    }

    #[test]
    fn cidr_rejects_too_wide_and_malformed() {
        assert!(cidr_hosts("10.0.0.0/8").is_err()); // prefix < 16
        assert!(cidr_hosts("not-a-cidr").is_err());
        assert!(cidr_hosts("192.168.1.0/33").is_err());
    }

    #[test]
    fn parses_bsd_arp_output() {
        // macOS `arp -a` format
        let text = "\
? (192.0.2.250) at 94:cc:4:00:00:01 on en0 ifscope [ethernet]
? (192.0.2.1) at 0:11:22:33:44:55 on en0 ifscope [ethernet]";
        let entries = parse_arp_table(text);
        assert!(entries.iter().any(|(ip, mac)|
            ip.to_string() == "192.0.2.250" && mac.contains("94:cc")));
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn oui_matching_is_normalized() {
        assert!(mac_oui_matches("94:cc:04:00:00:01", MOCA_OUIS));
        assert!(mac_oui_matches("94-CC-04-00-00-01", MOCA_OUIS));
        // BSD arp zero-compresses: 94:cc:4:00:00:01 -> still OUI 94cc04
        assert!(mac_oui_matches("94:cc:4:00:00:01", MOCA_OUIS));
        assert!(!mac_oui_matches("00:11:22:33:44:55", MOCA_OUIS));
    }
}
