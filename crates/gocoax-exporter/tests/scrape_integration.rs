use std::sync::Arc;
use std::time::{Duration, Instant};

use gocoax_exporter::scrape::{scrape, AppState};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("tests/fixtures/{name}")).unwrap()
}

async fn mount_ms(server: &MockServer, ms_path: &str, fixture_name: &str) {
    Mock::given(method("POST"))
        .and(path(ms_path))
        .respond_with(ResponseTemplate::new(200).set_body_string(fixture(fixture_name)))
        .mount(server)
        .await;
}

/// Spin up a wiremock server that plays a real device: a csrf-issuing GET
/// plus every `/ms/...` read `Client::device_status`/`Client::phy_rates`
/// issue, each answered from a captured fixture.
async fn mock_device() -> MockServer {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/index.html"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("Set-Cookie", "csrf_token=ABC123; SameSite=Strict"),
        )
        .mount(&server)
        .await;

    mount_ms(&server, "/ms/0/0x15", "localInfo_0x15.json").await; // LOCAL_INFO
    mount_ms(&server, "/ms/1/0x103/GET", "macInfo_0x103.json").await; // MAC_INFO
    mount_ms(&server, "/ms/0/0x14", "frameInfo_0x14.json").await; // FRAME_INFO
    mount_ms(&server, "/ms/1/0x20b/GET", "ipAddr_0x20b.json").await; // IP_ADDR
    mount_ms(&server, "/ms/0/0x1003/GET", "lof_0x1003.json").await; // LOF
    mount_ms(&server, "/ms/1/0x307/GET", "ethInfo_0x307.json").await; // ETH_INFO
    mount_ms(&server, "/ms/0/0x16", "netInfo_0x16.json").await; // NET_INFO (per node)
    mount_ms(&server, "/ms/0/0x1D", "fmrInfo_0x1D_node0.json").await; // FMR_INFO (per node)

    server
}

#[tokio::test]
async fn scrape_reports_up_device_with_real_metrics() {
    let server = mock_device().await;
    let host = server.uri().replace("http://", "");

    let toml = format!(
        r#"
username = "admin"
password = "g"

[[device]]
name = "ff"
host = "{host}"
"#
    );

    let state = Arc::new(AppState::from_config_text(&toml).unwrap());
    let out = scrape(state).await;

    assert!(out.contains("gocoax_up{device=\"ff\"} 1"), "missing up=1 line:\n{out}");
    assert!(
        out.contains(r#"gocoax_info{device="ff""#) && out.contains(r#"mac="94:cc:04:00:00:01""#),
        "missing gocoax_info line with mac:\n{out}"
    );
    // No error was recorded for a fully successful scrape.
    assert!(!out.contains("gocoax_scrape_errors_total{device=\"ff\""));
    // A full success stamps last-success.
    assert!(out.contains("gocoax_last_success_timestamp_seconds{device=\"ff\"}"));
}

#[tokio::test]
async fn scrape_reports_down_device_as_unreachable_and_still_returns_200_text() {
    // Port 1 (tcpmux) is essentially never listening; connecting refuses
    // immediately rather than hanging, so this stays fast without relying
    // on the scrape deadline.
    let toml = r#"
scrape_deadline_secs = 5
username = "admin"
password = "g"

[[device]]
name = "ff"
host = "127.0.0.1:1"
"#;

    let state = Arc::new(AppState::from_config_text(toml).unwrap());
    let out = scrape(state).await;

    assert!(out.contains("gocoax_up{device=\"ff\"} 0"), "missing up=0 line:\n{out}");
    assert!(
        out.contains("gocoax_scrape_errors_total{device=\"ff\",reason=\"unreachable\"}"),
        "missing unreachable error line:\n{out}"
    );
    // The device never reported any data.
    assert!(!out.contains("gocoax_info{device=\"ff\""));
}

#[tokio::test]
async fn scrape_respects_global_deadline_for_a_slow_device() {
    // Delay the csrf GET (the very first call `device_status()` makes) well
    // past the 1s scrape deadline configured below. `#[tokio::test]` runs a
    // real clock by default (no `tokio::time::pause()`), so this delay and
    // the deadline's `tokio::time::timeout` race for real -- proving the
    // deadline actually cuts the scrape short rather than the test just
    // happening to be fast.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/index.html"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Set-Cookie", "csrf_token=ABC123; SameSite=Strict")
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&server)
        .await;

    let host = server.uri().replace("http://", "");
    let toml = format!(
        r#"
scrape_deadline_secs = 1
username = "admin"
password = "g"

[[device]]
name = "ff"
host = "{host}"
"#
    );

    let state = Arc::new(AppState::from_config_text(&toml).unwrap());

    let start = Instant::now();
    let out = scrape(state).await;
    let elapsed = start.elapsed();

    // Well under the 3s response delay -- proves the deadline fired instead
    // of the scrape waiting out the slow device.
    assert!(elapsed < Duration::from_secs(2), "scrape did not respect the deadline: took {elapsed:?}");
    assert!(out.contains("gocoax_up{device=\"ff\"} 0"), "missing up=0 line:\n{out}");
    assert!(
        out.contains("gocoax_scrape_errors_total{device=\"ff\",reason=\"timeout\"}"),
        "missing timeout error line:\n{out}"
    );
}
