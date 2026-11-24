use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use tracing::instrument::WithSubscriber;
use wakezilla::client_server;
use wakezilla::shutdown_auth::{
    generate_key, sign_request, NONCE_HEADER, SIGNATURE_HEADER, TIMESTAMP_HEADER,
};

#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn rejected_shutdown_logs_transport_peer_without_credentials() {
    let capture = LogCapture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let key = generate_key();
    let signed = sign_request(&generate_key(), "POST", "/machines/turn-off").unwrap();
    let peer: SocketAddr = "192.0.2.10:43210".parse().unwrap();
    let app = client_server::build_router(Some(key.clone()));
    let response = app
        .oneshot(
            Request::post("/machines/turn-off")
                .extension(ConnectInfo(peer))
                .header("x-forwarded-for", "203.0.113.50")
                .header(TIMESTAMP_HEADER, signed.timestamp.as_str())
                .header(NONCE_HEADER, signed.nonce.as_str())
                .header(SIGNATURE_HEADER, signed.signature.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .with_subscriber(subscriber)
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("Rejected shutdown request"));
    assert!(logs.contains("192.0.2.10:43210"));
    assert!(logs.contains("/machines/turn-off"));
    assert!(logs.contains("status=401"));
    assert!(!logs.contains("Accepted shutdown request"));
    assert!(!logs.contains("203.0.113.50"));
    for credential in [&key, &signed.nonce, &signed.signature] {
        assert!(!logs.contains(credential));
    }
}
