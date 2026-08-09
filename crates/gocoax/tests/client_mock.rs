use gocoax::client::{Client, ClientOpts};
use gocoax::config::ResolvedCreds;
use gocoax::ms::IP_ADDR;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn creds() -> ResolvedCreds { ResolvedCreds { username: "admin".into(), password: "g".into() } }
fn opts() -> ClientOpts { ClientOpts { request_timeout: Duration::from_secs(2), connect_timeout: Duration::from_secs(1), verbose: false } }

#[tokio::test]
async fn read_fetches_csrf_then_posts() {
    let server = MockServer::start().await;
    // GET any page issues a csrf cookie
    Mock::given(method("GET")).and(path("/index.html"))
        .respond_with(ResponseTemplate::new(200)
            .insert_header("Set-Cookie", "csrf_token=ABC123; SameSite=Strict"))
        .mount(&server).await;
    // POST returns the ip register
    Mock::given(method("POST")).and(path("/ms/1/0x20b/GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":["0xc00002fa"]}"#))
        .mount(&server).await;

    let host = server.uri().replace("http://", "");
    let client = Client::new(&host, creds(), opts()).unwrap();
    let words = client.read(IP_ADDR, r#"{"data":[0]}"#).await.unwrap();
    assert_eq!(words, vec![0xc00002fa]);
}

#[tokio::test]
async fn auth_failure_maps_to_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200)
        .insert_header("Set-Cookie", "csrf_token=X; SameSite=Strict")).mount(&server).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401)).mount(&server).await;
    let host = server.uri().replace("http://", "");
    let client = Client::new(&host, creds(), opts()).unwrap();
    let err = client.read(IP_ADDR, r#"{"data":[0]}"#).await.unwrap_err();
    assert!(matches!(err, gocoax::Error::Auth));
}

#[tokio::test]
async fn refetches_csrf_on_403_then_succeeds() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200)
        .insert_header("Set-Cookie", "csrf_token=T; SameSite=Strict")).mount(&server).await;
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    Mock::given(method("POST")).respond_with(move |_: &wiremock::Request| {
        if h.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(403)
        } else {
            ResponseTemplate::new(200).set_body_string(r#"{"data":["0x1"]}"#)
        }
    }).mount(&server).await;
    let host = server.uri().replace("http://", "");
    let client = Client::new(&host, creds(), opts()).unwrap();
    let words = client.read(IP_ADDR, r#"{"data":[0]}"#).await.unwrap();
    assert_eq!(words, vec![1]);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

// A reboot that gets a clean 2xx is a success.
#[tokio::test]
async fn reboot_ok_on_2xx() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200)
        .insert_header("Set-Cookie", "csrf_token=X; SameSite=Strict")).mount(&server).await;
    Mock::given(method("POST")).and(path("/ms/1/0xb00"))
        .respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let host = server.uri().replace("http://", "");
    let client = Client::new(&host, creds(), opts()).unwrap();
    assert!(client.reboot().await.is_ok());
}

// The real device power-cycles and never replies, so the POST times out AFTER
// being sent. That must be treated as SUCCESS (the reboot fired), not an error.
#[tokio::test]
async fn reboot_ok_when_device_drops_after_send() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200)
        .insert_header("Set-Cookie", "csrf_token=X; SameSite=Strict")).mount(&server).await;
    // Delay (5s) exceeds the 2s request timeout -> the POST is sent but no
    // response arrives in time, exactly like a real reboot.
    Mock::given(method("POST")).and(path("/ms/1/0xb00"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&server).await;
    let host = server.uri().replace("http://", "");
    let client = Client::new(&host, creds(), opts()).unwrap();
    assert!(client.reboot().await.is_ok(), "post-send timeout should be treated as reboot success");
}

// A genuine auth rejection on the reboot is still an error (the reboot did not happen).
#[tokio::test]
async fn reboot_err_on_auth_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200)
        .insert_header("Set-Cookie", "csrf_token=X; SameSite=Strict")).mount(&server).await;
    Mock::given(method("POST")).and(path("/ms/1/0xb00"))
        .respond_with(ResponseTemplate::new(401)).mount(&server).await;
    let host = server.uri().replace("http://", "");
    let client = Client::new(&host, creds(), opts()).unwrap();
    assert!(matches!(client.reboot().await, Err(gocoax::Error::Auth)));
}
