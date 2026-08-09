use gocoax::client::{Client, ClientOpts};
use gocoax::config::ResolvedCreds;
use gocoax::ms::IP_ADDR;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn creds() -> ResolvedCreds { ResolvedCreds { username: "admin".into(), password: "g".into() } }
fn opts() -> ClientOpts { ClientOpts { request_timeout: Duration::from_secs(2), connect_timeout: Duration::from_secs(1) } }

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
