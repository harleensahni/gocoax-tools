use gocoax::decode::{decode_net_nodes, DeviceStatus, EthCounters};
use gocoax::ms::parse_ms_response;
use gocoax::phy::{decode_fmr, PhyLink};

fn load(name: &str) -> Vec<u32> {
    let body = std::fs::read_to_string(format!("tests/fixtures/{name}")).unwrap();
    parse_ms_response(&body).unwrap()
}

#[test]
fn device_status_decodes_from_real_fixtures() {
    let local = load("localInfo_0x15.json");
    let mac = load("macInfo_0x103.json");
    let frame = load("frameInfo_0x14.json");
    let ip = load("ipAddr_0x20b.json");
    let lof = load("lof_0x1003.json");
    let eth = load("ethInfo_0x307.json");

    let s = DeviceStatus::decode(&local, &mac, &frame, &ip, &lof, &eth).unwrap();

    assert_eq!(s.soc_version, "1.18.15");
    assert_eq!(s.moca_version, "2.5");
    assert_eq!(s.node_bitmask, 0x03);
    assert_eq!(s.node_count, 2);
    assert_eq!(s.my_node_id, 1);
    assert!(s.link_up);
    assert_eq!(s.mac, "94:cc:04:00:00:01");
    assert_eq!(s.ip.to_string(), "192.0.2.250");
    assert_eq!(s.beacon_channel_mhz, 1150);
    assert_eq!(s.eth.tx_good, 317682);
    assert_eq!(s.eth.tx_bad, 0);
    assert_eq!(s.eth.rx_dropped, 46);
    assert_eq!(s.eth_ports.len(), 1);
    assert_eq!(s.eth_ports[0].port, 1);
    assert!(s.eth_ports[0].link_up);
    assert_eq!(s.eth_ports[0].speed_mbps, 1000);
    assert!(s.eth_ports[0].duplex_full);
}

#[test]
fn eth_counters_bounds_checked() {
    // too-short array must error, not panic
    assert!(EthCounters::decode(&[0, 1, 2]).is_err());
}

#[test]
fn phy_rates_decode_to_ui_values() {
    let fmr = load("fmrInfo_0x1D_node0.json");
    // 2-node network: nodes 0 and 1 both MoCA 2.5 (0x25); NC is node 0 (2.5).
    // node_vers indexed by node id: [node0=0x25, node1=0x25]; bitmask 0b11=0x03.
    let links = decode_fmr(0, 0x03, &[0x25, 0x25], 0x25, &fmr).unwrap();
    // self rate 701 (matches UI screenshot exactly); 0->1 = 3656 (also matches UI screenshot exactly).
    let self_link = links.iter().find(|l| l.from_node == 0 && l.to_node == 0).unwrap();
    assert_eq!(self_link.nper_mbps, 701);
    let to1: &PhyLink = links.iter().find(|l| l.from_node == 0 && l.to_node == 1).unwrap();
    assert_eq!(to1.nper_mbps, 3656);
    assert_eq!(to1.vlper_mbps, 0); // VLPER ofdmb is 0 for this link in the fixture
}

#[test]
fn net_nodes_enumerates_from_fixtures() {
    let local = load("localInfo_0x15.json"); // node_bitmask 0x03 -> nodes {0,1}
    let net0 = load("netInfo_0x16.json"); // node 0's netInfo (moca 2.5)
    // We only captured node 0's netInfo; return it for both present nodes so
    // the enumeration/version logic is exercised deterministically.
    let nodes = decode_net_nodes(&local, |_id| net0.clone()).unwrap();
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0].node_id, 0);
    assert_eq!(nodes[0].moca_version, "2.5");
    assert!(nodes[0].mac.starts_with("94:cc:04"));
}
