use gocoax::ms::parse_ms_response;
use gocoax::{DeviceStatus, PhyRates};
use gocoax_exporter::metrics::{render, DeviceOutcome};

#[test]
fn renders_down_device_without_data() {
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
    // must not emit info/data lines for a down device
    assert!(!out.contains("gocoax_info{device=\"ff\""));
}

fn load(name: &str) -> Vec<u32> {
    let body = std::fs::read_to_string(format!("tests/fixtures/{name}")).unwrap();
    parse_ms_response(&body).unwrap()
}

#[test]
fn renders_up_device_with_status_data() {
    let local = load("localInfo_0x15.json");
    let mac = load("macInfo_0x103.json");
    let frame = load("frameInfo_0x14.json");
    let ip = load("ipAddr_0x20b.json");
    let lof = load("lof_0x1003.json");
    let eth = load("ethInfo_0x307.json");

    let status = DeviceStatus::decode(&local, &mac, &frame, &ip, &lof, &eth).unwrap();
    let phy = PhyRates { node_versions: vec![(0, 0x25), (1, 0x25)], ..Default::default() };

    let out = render(&[DeviceOutcome {
        name: "ff",
        host: "192.0.2.250",
        up: true,
        error_reason: None,
        duration_secs: 0.42,
        status: Some(&status),
        phy: Some(&phy),
        error_counts: &[],
        last_success_ts: Some(1_700_000_000.0),
    }]);

    assert!(out.contains("gocoax_up{device=\"ff\"} 1"));
    assert!(out.contains(r#"gocoax_info{device="ff",host="192.0.2.250",mac="94:cc:04:00:00:01""#));
    assert!(out.contains("gocoax_moca_nodes{device=\"ff\"} 2"));
    assert!(out.contains("gocoax_ethernet_rx_frames_total{device=\"ff\",port=\"1\",status=\"dropped\"} 46"));
    assert!(out.contains("gocoax_ethernet_link_up{device=\"ff\",port=\"1\"} 1"));
    assert!(out.contains("gocoax_ethernet_speed_mbps{device=\"ff\",port=\"1\"} 1000"));
    assert!(out.contains("gocoax_node_moca_version{device=\"ff\",node=\"0\"} 25"));
    assert!(out.contains("gocoax_node_moca_version{device=\"ff\",node=\"1\"} 25"));
}
