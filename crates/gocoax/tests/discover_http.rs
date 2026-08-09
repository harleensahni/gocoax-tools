use gocoax::discover::Found;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn fingerprints_interniche_host() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401)
            .insert_header("Server", "InterNiche Technologies WebServer 2.0"))
        .mount(&server).await;
    // server.uri() is 127.0.0.1:<port>; extract the port host is 127.0.0.1
    let ip = "127.0.0.1".parse().unwrap();
    // Point the scan at the mock by probing its exact address via a 1-host list.
    // http_fingerprint builds URLs as http://<ip>/, so run the mock on the
    // default and assert detection through a direct helper call instead:
    let found = probe_one(&server.uri()).await;
    assert!(found, "expected InterNiche server to be fingerprinted");
    let _ = (ip, Found { ip, server: None, mac: None }); // keep imports used
}

// Small helper mirroring http_fingerprint's per-host logic against a full URL,
// so the test doesn't depend on the mock listening on port 80.
async fn probe_one(base_url: &str) -> bool {
    let client = reqwest::Client::builder().build().unwrap();
    match client.get(base_url).send().await {
        Ok(resp) => resp.headers().get("server")
            .and_then(|v| v.to_str().ok())
            .map(gocoax::discover::is_moca_server)
            .unwrap_or(false),
        Err(_) => false,
    }
}
