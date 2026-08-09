use crate::{Error, Result};
use std::net::Ipv4Addr;

/// MoCA networks top out at 16 nodes (node ids `0..MAX_NODES`), so this
/// bounds every node-bitmask walk in the crate (`decode_net_nodes` here,
/// `decode_fmr` in `phy.rs`, and `Client::phy_rates`/`moca_nodes`).
pub const MAX_NODES: u32 = 16;

pub fn word_to_ipv4(w: u32) -> Ipv4Addr {
    Ipv4Addr::new((w >> 24) as u8, (w >> 16) as u8, (w >> 8) as u8, w as u8)
}

pub fn words_to_mac(hi: u32, lo: u32) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        (hi >> 24) & 0xff, (hi >> 16) & 0xff, (hi >> 8) & 0xff, hi & 0xff,
        (lo >> 24) & 0xff, (lo >> 16) & 0xff
    )
}

pub fn words_to_ascii(words: &[u32]) -> String {
    let mut out = String::new();
    for &w in words {
        for shift in [24u32, 16, 8, 0] {
            let b = ((w >> shift) & 0xff) as u8;
            if b == 0 { return out; }
            if b.is_ascii_graphic() || b == b'.' { out.push(b as char); }
        }
    }
    out
}

pub fn u64_from_pair(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | (lo as u64)
}

pub fn get(words: &[u32], idx: usize, cmd: &str) -> Result<u32> {
    words.get(idx).copied().ok_or_else(|| Error::Decode {
        cmd: cmd.into(),
        reason: format!("index {idx} out of range (len {})", words.len()),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EthCounters {
    pub tx_good: u64,
    pub tx_bad: u64,
    pub tx_dropped: u64,
    pub rx_good: u64,
    pub rx_bad: u64,
    pub rx_dropped: u64,
}

impl EthCounters {
    pub fn decode(frame: &[u32]) -> Result<EthCounters> {
        let pair = |hi: usize, lo: usize| -> Result<u64> {
            Ok(u64_from_pair(get(frame, hi, "0x14")?, get(frame, lo, "0x14")?))
        };
        Ok(EthCounters {
            tx_good: pair(12, 13)?,
            tx_bad: pair(30, 31)?,
            tx_dropped: pair(48, 49)?,
            rx_good: pair(66, 67)?,
            rx_bad: pair(84, 85)?,
            rx_dropped: pair(102, 103)?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct DeviceStatus {
    pub soc_version: String,
    pub moca_version: String,
    pub node_bitmask: u32,
    pub node_count: u32,
    pub my_node_id: u32,
    pub link_up: bool,
    pub mac: String,
    pub ip: Ipv4Addr,
    pub beacon_channel_mhz: u32,
    pub eth: EthCounters,
    pub eth_ports: Vec<EthPort>,
}

impl DeviceStatus {
    pub fn decode(
        local: &[u32],
        mac: &[u32],
        frame: &[u32],
        ip: &[u32],
        lof: &[u32],
        eth: &[u32],
    ) -> Result<DeviceStatus> {
        let moca_raw = get(local, 11, "0x15")?;
        let node_bitmask = get(local, 12, "0x15")?;
        let soc_words = [get(local, 21, "0x15")?, get(local, 22, "0x15")?];
        Ok(DeviceStatus {
            soc_version: words_to_ascii(&soc_words),
            moca_version: format!("{}.{}", (moca_raw >> 4) & 0xf, moca_raw & 0xf),
            node_bitmask,
            node_count: node_bitmask.count_ones(),
            my_node_id: get(local, 0, "0x15")?,
            link_up: get(local, 5, "0x15")? == 1,
            mac: words_to_mac(get(mac, 0, "0x103")?, get(mac, 1, "0x103")?),
            ip: word_to_ipv4(get(ip, 0, "0x20b")?),
            beacon_channel_mhz: get(lof, 0, "0x1003")?,
            eth: EthCounters::decode(frame)?,
            eth_ports: decode_eth_ports(eth)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EthPort {
    pub port: u32,
    pub link_up: bool,
    pub speed_mbps: u32,
    pub duplex_full: bool,
}

const SPEED_MBPS: [u32; 6] = [10, 100, 1000, 0, 2500, 0];

/// Decode ethInfo (0x307): per-port triples [link, speed_idx, duplex],
/// starting at port index 1 (matches the device UI, which skips port 0 on
/// MXL371x). speed_idx maps via SPEED_MBPS = [10,100,1000,0,2500,0] (0 =
/// Auto-Neg/NA/unknown); an out-of-range index is clamped to 0 (unknown).
pub fn decode_eth_ports(eth: &[u32]) -> Result<Vec<EthPort>> {
    let ports = eth.len() / 3;
    let mut out = Vec::new();
    // Start at port 1 (the device UI skips port 0 on MXL371x).
    for i in 1..ports {
        let link = get(eth, i * 3, "0x307")?;
        let speed_idx = get(eth, i * 3 + 1, "0x307")? as usize;
        let duplex = get(eth, i * 3 + 2, "0x307")?;
        let speed_mbps = *SPEED_MBPS.get(speed_idx).unwrap_or(&0);
        out.push(EthPort {
            port: i as u32,
            link_up: link != 0,
            speed_mbps,
            duplex_full: duplex != 0,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MocaNode {
    pub node_id: u32,
    pub mac: String,
    pub moca_version: String,
}

/// From localInfo (for `node_bitmask`) plus a per-node netInfo lookup, list
/// the MoCA nodes present on the network. `net_of(node)` returns that
/// node's netInfo words (0x16).
pub fn decode_net_nodes(local: &[u32], net_of: impl Fn(u32) -> Vec<u32>) -> Result<Vec<MocaNode>> {
    let node_bitmask = get(local, 12, "0x15")?;
    let mut out = Vec::new();
    for i in 0..MAX_NODES {
        if node_bitmask & (1 << i) == 0 {
            continue;
        }
        let net = net_of(i);
        let ver_raw = get(&net, 4, "0x16")? & 0xff;
        out.push(MocaNode {
            node_id: i,
            mac: words_to_mac(get(&net, 0, "0x16")?, get(&net, 1, "0x16")?),
            moca_version: format!("{}.{}", (ver_raw >> 4) & 0xf, ver_raw & 0xf),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ip_decodes() {
        assert_eq!(word_to_ipv4(0xc00002fa), "192.0.2.250".parse::<Ipv4Addr>().unwrap());
    }

    #[test]
    fn mac_decodes() {
        assert_eq!(words_to_mac(0x94cc0400, 0x00010000), "94:cc:04:00:00:01");
    }

    #[test]
    fn ascii_decodes_soc_version() {
        // localInfo[21..=22] = "1.18" + ".15\0"
        assert_eq!(words_to_ascii(&[0x312e3138, 0x2e313500]), "1.18.15");
    }

    #[test]
    fn u64_pair_and_bounds() {
        assert_eq!(u64_from_pair(0, 0x0004d8f2), 317682);
        assert!(get(&[1, 2], 5, "0x14").is_err());
        assert_eq!(get(&[1, 2], 1, "0x14").unwrap(), 2);
    }

    #[test]
    fn eth_ports_decode_port1() {
        let eth = [0u32, 0, 0, 1, 2, 1];
        let ports = decode_eth_ports(&eth).unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0], EthPort { port: 1, link_up: true, speed_mbps: 1000, duplex_full: true });
    }
}
