//! HTTP tunnel end-to-end integration test.
//!
//! # What this tests
//!
//! Full proxy chain:
//!   Public HTTPS client
//!     → rustunnel HTTPS edge (TLS termination, subdomain routing)
//!       → yamux stream (injected directly via `core.resolve_pending_conn`)
//!         → local "Hello, World!" HTTP server
//!
//! The yamux injection bypasses the missing `/_data/<session_id>` endpoint by
//! directly calling `TunnelCore::resolve_pending_conn`.  The HTTP proxy code
//! itself (edge/http.rs) runs unchanged.
//!
//! # Assertions
//! - Response body is exactly "Hello, World!"
//! - Response status is 200
//! - `captured_requests` table has one entry after the request

#[path = "../common/mod.rs"]
mod common;

use std::net::SocketAddr;

use axum::{routing::get, Router};
use common::*;
use tokio::sync::oneshot;

// ── local "Hello, World!" HTTP server ─────────────────────────────────────────

async fn start_hello_world_server() -> (SocketAddr, oneshot::Sender<()>) {
    let app = Router::new().route("/", get(|| async { "Hello, World!" }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind hello-world server");
    let addr = listener.local_addr().unwrap();

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .ok();
    });

    // Small grace period for the listener to be ready.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    (addr, shutdown_tx)
}

// ── main test ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn http_tunnel_proxies_hello_world() {
    init_tracing();

    // 1. Start the local HTTP server.
    let (local_addr, _hello_shutdown) = start_hello_world_server().await;

    // 2. Start the rustunnel server.
    let server = TestServer::start().await;

    // 3. Connect test client and register an HTTP tunnel.
    let mut client = TestClient::connect(&server).await.expect("client auth");
    let session_id = client.session_id.unwrap();

    let (_tunnel_id, subdomain, _public_url) = client
        .register_http_tunnel(Some("hello"))
        .await
        .expect("tunnel registration");

    // 4. Connect the data WebSocket bridge.  The server opens one yamux stream
    //    per incoming HTTP request (Mode::Client); this bridge accepts those
    //    streams (Mode::Server) and forwards data to the local hello-world server.
    connect_data_bridge(&server, session_id, local_addr);

    // 5. Small delay to let the data WebSocket connect and yamux handshake complete.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // 6. Make an HTTPS request to the tunnel.
    //    The Host header carries the subdomain that the edge uses for routing.
    let https_url = format!("https://127.0.0.1:{}/", server.https_port);
    let host = format!("{subdomain}.{}", server.domain);

    let resp = insecure_http_client()
        .get(&https_url)
        .header("Host", &host)
        .send()
        .await
        .expect("HTTPS request to tunnel");

    assert_eq!(resp.status(), 200, "unexpected status: {}", resp.status());

    let body = resp.text().await.expect("response body");
    assert_eq!(body, "Hello, World!", "unexpected body: {body:?}");

    // 7. Verify that the capture store recorded the request.
    //    Allow a short propagation delay (capture is async).
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM captured_requests")
        .fetch_one(&server.db.local)
        .await
        .expect("DB query");

    assert_eq!(count, 1, "expected 1 captured request, got {count}");
}

// ── routing: unknown subdomain returns 502 ────────────────────────────────────

#[tokio::test]
async fn unknown_subdomain_returns_502() {
    init_tracing();
    let server = TestServer::start().await;

    let resp = insecure_http_client()
        .get(format!("https://127.0.0.1:{}/", server.https_port))
        .header("Host", "notregistered.localhost")
        .send()
        .await
        .expect("HTTPS request");

    assert_eq!(resp.status(), 502);
}

// ── tunnel registration records the correct public URL ─────────────────────────

#[tokio::test]
async fn http_tunnel_public_url_contains_subdomain() {
    init_tracing();
    let server = TestServer::start().await;

    let mut client = TestClient::connect(&server).await.expect("auth");
    let (_, _, public_url) = client
        .register_http_tunnel(Some("myapp"))
        .await
        .expect("register");

    assert!(
        public_url.contains("myapp"),
        "public_url should contain 'myapp'; got: {public_url}"
    );
    assert!(
        public_url.contains("localhost"),
        "public_url should contain domain; got: {public_url}"
    );
}

// ── duplicate subdomain registration fails ────────────────────────────────────

#[tokio::test]
async fn duplicate_subdomain_returns_tunnel_error() {
    init_tracing();
    let server = TestServer::start().await;

    let mut client = TestClient::connect(&server).await.expect("auth");

    // First registration succeeds.
    client
        .register_http_tunnel(Some("clash"))
        .await
        .expect("first registration");

    // Second with the same subdomain must fail.
    let err = client
        .register_http_tunnel(Some("clash"))
        .await
        .expect_err("duplicate should fail");

    assert!(
        err.contains("TunnelError"),
        "expected TunnelError; got: {err}"
    );
}

// ── multiple requests through the same tunnel ─────────────────────────────────

#[tokio::test]
async fn multiple_requests_through_tunnel() {
    init_tracing();

    let (local_addr, _hello_shutdown) = start_hello_world_server().await;
    let server = TestServer::start().await;

    let mut client = TestClient::connect(&server).await.expect("auth");
    let session_id = client.session_id.unwrap();
    let (_, subdomain, _) = client
        .register_http_tunnel(Some("multi"))
        .await
        .expect("register");

    connect_data_bridge(&server, session_id, local_addr);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let http_client = insecure_http_client();
    let url = format!("https://127.0.0.1:{}/", server.https_port);
    let host = format!("{subdomain}.localhost");

    for i in 0..3 {
        let resp = http_client
            .get(&url)
            .header("Host", &host)
            .send()
            .await
            .unwrap_or_else(|e| panic!("request {i}: {e}"));
        assert_eq!(resp.status(), 200, "request {i} failed");
        assert_eq!(resp.text().await.unwrap(), "Hello, World!");
    }
}
