//! Regression test for a device-interop requirement that the wiremock tests
//! cannot catch: the InterNiche WebServer 2.0 on these adapters does
//! CASE-SENSITIVE HTTP header matching and only accepts `Authorization`
//! (capital A). hyper/reqwest lowercase HTTP/1.1 header names by default, so
//! without `http1_title_case_headers()` the device rejects every request with
//! 401. wiremock is RFC-compliant (case-insensitive) and happily accepts the
//! lowercase form, so this can only be verified by inspecting the raw bytes the
//! client actually puts on the wire — which is what this test does.

use std::time::Duration;

use gocoax::client::{Client, ClientOpts};
use gocoax::config::ResolvedCreds;
use gocoax::ms::IP_ADDR;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn client_sends_title_case_authorization_header() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Accept the first connection (the GET /index.html csrf fetch), capture its
    // raw request bytes, and respond 200 so the client doesn't error before we
    // have what we need.
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = sock.read(&mut buf).await.unwrap();
        let resp = "HTTP/1.1 200 OK\r\nSet-Cookie: csrf_token=ABC; SameSite=Strict\r\n\
                    Content-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = sock.write_all(resp.as_bytes()).await;
        String::from_utf8_lossy(&buf[..n]).into_owned()
    });

    let host = format!("{}:{}", addr.ip(), addr.port());
    let client = Client::new(
        &host,
        ResolvedCreds { username: "admin".into(), password: "pw".into() },
        ClientOpts {
            request_timeout: Duration::from_secs(2),
            connect_timeout: Duration::from_secs(1),
            verbose: false,
        },
    )
    .unwrap();

    // We only care about the FIRST request's bytes; the follow-up POST will fail
    // (the listener closes after one accept) and that's fine.
    let _ = client.read(IP_ADDR, r#"{"data":[0]}"#).await;

    let req = server.await.unwrap();

    assert!(
        req.contains("Authorization: Basic "),
        "device requires capital-case `Authorization`; raw request was:\n{req}"
    );
    assert!(
        !req.contains("authorization: "),
        "must NOT send lowercase `authorization` (device rejects it with 401)"
    );
    // Title-casing applies to all header names hyper emits.
    assert!(req.contains("Host: "), "expected title-cased `Host` header");
}
