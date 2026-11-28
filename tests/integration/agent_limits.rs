//! TUNNEL-61: agent-plan limits, account access, and the browser warning.
//!
//! The plan row is the source of the numeric limits. The warning is the
//! exception: it shows only while the user is `kind = agent`, unclaimed, and
//! still on the plan named `agent`.

#[path = "../common/mod.rs"]
mod common;

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::{routing::get, routing::post, Router};
use common::*;
use rustunnel_protocol::{ControlFrame, TunnelProtocol};
use rustunnel_server::db::{hash_token, AgentPlanLimits};
use rustunnel_server::policy::{self, code};
use serde_json::Value;
use sqlx::PgPool;
use tokio::sync::oneshot;
use uuid::Uuid;

struct Seed {
    user_id: Uuid,
    raw: String,
    pool: PgPool,
}

impl Seed {
    async fn create(pool: &PgPool, kind: &str, plan: &str) -> Self {
        let user_id = Uuid::new_v4();
        let email = format!("{kind}-{user_id}@example.com");
        sqlx::query(
            "INSERT INTO users (id, email, password_hash, kind, status, auth_method, source) \
             VALUES ($1, $2, '', $3, 'active', $4, 'cli')",
        )
        .bind(user_id)
        .bind(&email)
        .bind(kind)
        .bind(if kind == "agent" { "agent" } else { "password" })
        .execute(pool)
        .await
        .expect("insert user");
        sqlx::query(
            "INSERT INTO subscriptions (user_id, plan_id, status) \
             SELECT $1, id, 'active' FROM plans WHERE name = $2",
        )
        .bind(user_id)
        .bind(plan)
        .execute(pool)
        .await
        .expect("insert subscription");

        let raw = format!("rt_test_{user_id}");
        let token_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO tokens \
                (id, token_hash, label, created_at, user_id, expires_at, status, unlimited) \
             VALUES ($1, $2, 'agent-test', NOW(), $3, \
                     CASE WHEN $4 THEN NOW() + interval '7 days' ELSE NULL END, \
                     'active', false)",
        )
        .bind(&token_id)
        .bind(hash_token(&raw))
        .bind(user_id)
        .bind(kind == "agent")
        .execute(pool)
        .await
        .expect("insert token");

        Self {
            user_id,
            raw,
            pool: pool.clone(),
        }
    }

    async fn cleanup(self) {
        let _ = sqlx::query("DELETE FROM tunnel_log WHERE user_id = $1")
            .bind(self.user_id)
            .execute(&self.pool)
            .await;
        let _ = sqlx::query("DELETE FROM tokens WHERE user_id = $1")
            .bind(self.user_id)
            .execute(&self.pool)
            .await;
        let _ = sqlx::query("DELETE FROM subscriptions WHERE user_id = $1")
            .bind(self.user_id)
            .execute(&self.pool)
            .await;
        let _ = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(self.user_id)
            .execute(&self.pool)
            .await;
    }
}

fn auth_err(result: Result<TestClient, String>) -> String {
    match result {
        Err(err) => err,
        Ok(_) => panic!("expected auth to fail"),
    }
}

fn assert_code(err: &str, error_code: &str) {
    assert!(
        err.contains(&format!("[{error_code}]")),
        "expected {error_code} in {err}"
    );
}

async fn hello() -> (SocketAddr, oneshot::Sender<()>) {
    let app = Router::new()
        .route("/", get(|| async { "Hello, World!" }))
        .route("/", post(|| async { "posted" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .ok();
    });
    (addr, tx)
}

async fn fetch(
    port: u16,
    host: &str,
    method: reqwest::Method,
    accept: &str,
    headers: &[(&str, &str)],
) -> reqwest::Response {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(8))
        .build()
        .unwrap();
    let mut req = client
        .request(method, format!("https://127.0.0.1:{port}/"))
        .header("Host", host)
        .header("Accept", accept);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    req.send().await.expect("edge request")
}

async fn close_blocked(server: &TestServer, client: &mut TestClient) {
    let gate = policy::AccountGate::new(
        server.db.pg.clone(),
        policy::ACCOUNT_CACHE_TTL,
        policy::BANDWIDTH_CACHE_TTL,
    );
    let disconnect = policy::disconnect_blocked_sessions(&server.db.pg, &server.core, &gate);
    tokio::pin!(disconnect);
    let mut finished = false;
    loop {
        tokio::select! {
            result = &mut disconnect, if !finished => {
                result.expect("session recheck");
                finished = true;
            }
            frame = client.recv_timeout(Duration::from_secs(2)) => {
                match frame {
                    Ok(ControlFrame::Ping { timestamp }) => {
                        let _ = client.send(&ControlFrame::Pong { timestamp }).await;
                    }
                    Err(_) => break,
                    Ok(_) => {}
                }
            }
        }
    }
    if !finished {
        disconnect.await.expect("session recheck");
    }
}

async fn wait_gone(server: &TestServer, session_id: Uuid) {
    for _ in 0..50 {
        if !server.core.sessions.contains_key(&session_id) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("session {session_id} was not closed");
}

#[tokio::test]
async fn agent_token_opens_one_random_http_tunnel_and_rejects_the_rest() {
    init_tracing();
    let server = TestServer::start().await;
    let seed = Seed::create(&server.db.pg, "agent", "agent").await;

    let mut client = TestClient::connect_with_token(&server, &seed.raw)
        .await
        .expect("agent auth");
    let (id, subdomain, public_url) = client
        .register_http_tunnel(None)
        .await
        .expect("first http tunnel");
    assert_ne!(id, Uuid::nil());
    assert_eq!(subdomain.len(), 8);
    assert!(subdomain.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(public_url.starts_with(&format!("http://{subdomain}.")));

    let second = client
        .register_http_tunnel(None)
        .await
        .expect_err("second tunnel");
    assert_code(&second, code::TUNNEL_LIMIT);
    assert!(second.contains("agent plan allows 1 concurrent tunnel"));

    let tcp = client.register_tcp_tunnel().await.expect_err("tcp");
    assert_code(&tcp, code::PROTOCOL_NOT_ALLOWED);
    assert!(tcp.contains("agent plan does not allow tcp tunnels"));

    let udp = client.register_udp_tunnel().await.expect_err("udp");
    assert_code(&udp, code::PROTOCOL_NOT_ALLOWED);
    assert!(udp.contains("agent plan does not allow udp tunnels"));

    let p2p = client
        .register_p2p_tunnel("agent-peer", "secret-hash")
        .await
        .expect_err("p2p");
    assert_code(&p2p, code::PROTOCOL_NOT_ALLOWED);
    assert!(p2p.contains("agent plan does not allow p2p tunnels"));

    seed.cleanup().await;
}

#[tokio::test]
async fn custom_subdomain_and_protocol_use_platform_codes() {
    init_tracing();
    let server = TestServer::start().await;
    let seed = Seed::create(&server.db.pg, "agent", "agent").await;
    let mut client = TestClient::connect_with_token(&server, &seed.raw)
        .await
        .unwrap();

    let custom = client
        .register_http_tunnel(Some("my-agent-app"))
        .await
        .expect_err("custom subdomain");
    assert_code(&custom, code::CUSTOM_SUBDOMAIN_NOT_ALLOWED);
    assert!(custom.contains("agent plan does not allow a custom subdomain"));

    // Protocol is checked before the subdomain and the tunnel cap.
    let both = client
        .send(&ControlFrame::RegisterTunnel {
            request_id: Uuid::new_v4().to_string(),
            protocol: TunnelProtocol::Tcp,
            subdomain: Some("also-custom".into()),
            local_addr: "127.0.0.1:0".into(),
            p2p_secret_hash: None,
            p2p_name: None,
            group: None,
            group_key_hash: None,
            health_check: None,
            client_surface: None,
        })
        .await;
    assert!(both.is_ok());
    let frame = client.recv_timeout(Duration::from_secs(5)).await.unwrap();
    let ControlFrame::TunnelError {
        message, code: got, ..
    } = frame
    else {
        panic!("expected TunnelError, got {frame:?}");
    };
    assert_eq!(got.as_deref(), Some(code::PROTOCOL_NOT_ALLOWED));
    assert!(message.contains("does not allow tcp"));

    let mut https = TestClient::connect_with_token(&server, &seed.raw)
        .await
        .unwrap();
    https
        .send(&ControlFrame::RegisterTunnel {
            request_id: Uuid::new_v4().to_string(),
            protocol: TunnelProtocol::Https,
            subdomain: None,
            local_addr: "127.0.0.1:0".into(),
            p2p_secret_hash: None,
            p2p_name: None,
            group: None,
            group_key_hash: None,
            health_check: None,
            client_surface: None,
        })
        .await
        .unwrap();
    let frame = https.recv_timeout(Duration::from_secs(5)).await.unwrap();
    assert!(
        matches!(frame, ControlFrame::TunnelRegistered { .. }),
        "https is on the agent plan: {frame:?}"
    );

    seed.cleanup().await;
}

#[tokio::test]
async fn bandwidth_cap_refuses_new_tunnels_and_proxied_traffic() {
    init_tracing();
    let server = TestServer::start().await;
    let cap = AgentPlanLimits::BANDWIDTH_LIMIT_BYTES;

    let closed = Seed::create(&server.db.pg, "agent", "agent").await;
    sqlx::query(
        "INSERT INTO tunnel_log \
            (id, tunnel_id, protocol, label, session_id, registered_at, unregistered_at, \
             region_id, user_id, bytes_proxied, bytes_in, bytes_out) \
         VALUES ($1, $1, 'http', 'old', $2, NOW(), NOW(), 'test', $3, $4, $4, 0)",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(Uuid::new_v4().to_string())
    .bind(closed.user_id)
    .bind(cap)
    .execute(&server.db.pg)
    .await
    .unwrap();
    let mut client = TestClient::connect_with_token(&server, &closed.raw)
        .await
        .unwrap();
    let denied = client
        .register_http_tunnel(None)
        .await
        .expect_err("over cap");
    assert_code(&denied, code::BANDWIDTH_CAP_REACHED);
    assert!(denied.contains(&cap.to_string()));
    let tcp = client
        .register_tcp_tunnel()
        .await
        .expect_err("tcp still first");
    assert_code(&tcp, code::PROTOCOL_NOT_ALLOWED);
    closed.cleanup().await;

    let open = Seed::create(&server.db.pg, "agent", "agent").await;
    let mut client = TestClient::connect_with_token(&server, &open.raw)
        .await
        .unwrap();
    let (_, subdomain, _) = client.register_http_tunnel(None).await.unwrap();
    sqlx::query(
        "UPDATE tunnel_log SET bytes_proxied = $2, bytes_in = $2 \
         WHERE user_id = $1 AND unregistered_at IS NULL",
    )
    .bind(open.user_id)
    .bind(cap)
    .execute(&server.db.pg)
    .await
    .unwrap();
    let host = format!("{subdomain}.{}", server.domain);
    let json = fetch(
        server.https_port,
        &host,
        reqwest::Method::GET,
        "application/json",
        &[],
    )
    .await;
    assert_eq!(json.status(), 402);
    let body: Value = json.json().await.unwrap();
    assert_eq!(body["code"], code::BANDWIDTH_CAP_REACHED);

    let page = fetch(
        server.https_port,
        &host,
        reqwest::Method::GET,
        "text/html",
        &[("Sec-Fetch-Mode", "navigate")],
    )
    .await;
    assert_eq!(page.status(), 402);
    let html = page.text().await.unwrap();
    assert!(html.contains("Bandwidth cap reached"));
    assert!(!html.contains("http://") && !html.contains("https://"));
    open.cleanup().await;
}

#[tokio::test]
async fn long_lived_tunnel_is_cut_off_when_live_bytes_cross_the_cap() {
    init_tracing();
    let server = TestServer::start().await;
    let cap = AgentPlanLimits::BANDWIDTH_LIMIT_BYTES;
    let (local, _shutdown) = hello().await;
    let seed = Seed::create(&server.db.pg, "agent", "agent").await;
    let mut client = TestClient::connect_with_token(&server, &seed.raw)
        .await
        .unwrap();
    let session_id = client.session_id.unwrap();
    let (tunnel_id, subdomain, _) = client.register_http_tunnel(None).await.unwrap();
    connect_data_bridge(&server, session_id, local)
        .await
        .expect("bridge");
    let host = format!("{subdomain}.{}", server.domain);

    let first = fetch(server.https_port, &host, reqwest::Method::GET, "*/*", &[]).await;
    assert_eq!(first.status(), 200);
    assert_eq!(first.text().await.unwrap(), "Hello, World!");

    let stored: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(bytes_proxied), 0)::bigint FROM tunnel_log WHERE user_id = $1",
    )
    .bind(seed.user_id)
    .fetch_one(&server.db.pg)
    .await
    .unwrap();
    assert!(
        stored < cap,
        "the open row is still under the cap before the live counter moves: {stored}"
    );

    server
        .core
        .http_routes
        .get(&subdomain)
        .expect("live tunnel")
        .members
        .get(&tunnel_id)
        .expect("member")
        .info
        .bytes_in
        .store(cap as u64, Ordering::Relaxed);

    let blocked = fetch(
        server.https_port,
        &host,
        reqwest::Method::GET,
        "application/json",
        &[],
    )
    .await;
    assert_eq!(blocked.status(), 402);
    let body: Value = blocked.json().await.unwrap();
    assert_eq!(body["code"], code::BANDWIDTH_CAP_REACHED);

    let page = fetch(
        server.https_port,
        &host,
        reqwest::Method::GET,
        "text/html",
        &[("Sec-Fetch-Mode", "navigate")],
    )
    .await;
    assert_eq!(page.status(), 402);

    let still: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(bytes_proxied), 0)::bigint FROM tunnel_log WHERE user_id = $1",
    )
    .bind(seed.user_id)
    .fetch_one(&server.db.pg)
    .await
    .unwrap();
    assert!(
        still < cap,
        "402 must come from the in-memory counter, not a flushed row: {still}"
    );
    seed.cleanup().await;
}

#[tokio::test]
async fn open_stream_past_the_cap_is_closed_without_a_database_write() {
    init_tracing();
    let server = TestServer::start().await;
    let cap = AgentPlanLimits::BANDWIDTH_LIMIT_BYTES;
    let (local, _hold) = hold_open_body(cap as u64).await;

    let seed = Seed::create(&server.db.pg, "agent", "agent").await;
    let mut client = TestClient::connect_with_token(&server, &seed.raw)
        .await
        .unwrap();
    let session_id = client.session_id.unwrap();
    let (tunnel_id, subdomain, _) = client.register_http_tunnel(None).await.unwrap();
    connect_data_bridge(&server, session_id, local)
        .await
        .expect("bridge");
    let host = format!("{subdomain}.{}", server.domain);
    let port = server.https_port;
    let reader = tokio::spawn(async move {
        discard_https_body(port, &host, "/stream").await;
    });

    let mut saw = 0u64;
    for _ in 0..400 {
        saw = server.core.get_tunnel_bytes_proxied(&tunnel_id);
        if saw >= cap as u64 {
            break;
        }
        if let Ok(ControlFrame::Ping { timestamp }) =
            client.recv_timeout(Duration::from_millis(50)).await
        {
            let _ = client.send(&ControlFrame::Pong { timestamp }).await;
        }
    }
    assert!(
        saw >= cap as u64,
        "open stream did not reach the cap in memory: {saw}"
    );
    let stored: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(bytes_proxied), 0)::bigint FROM tunnel_log WHERE user_id = $1",
    )
    .bind(seed.user_id)
    .fetch_one(&server.db.pg)
    .await
    .unwrap();
    assert!(
        stored < cap,
        "usage must come from the live stream, not a database write: {stored}"
    );

    close_blocked(&server, &mut client).await;
    wait_gone(&server, session_id).await;
    let _ = reader.await;
    seed.cleanup().await;
}

/// Local HTTP server that writes `bytes` and then leaves the response open.
async fn hold_open_body(bytes: u64) -> (SocketAddr, oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    let (tx, mut rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => break,
                accepted = listener.accept() => {
                    let Ok((sock, _)) = accepted else { break };
                    tokio::spawn(async move {
                        write_open_body(sock, bytes).await;
                    });
                }
            }
        }
    });
    (addr, tx)
}

async fn write_open_body(mut sock: tokio::net::TcpStream, bytes: u64) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf = [0u8; 8192];
    let mut got = Vec::new();
    loop {
        let n = match sock.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        got.extend_from_slice(&buf[..n]);
        if got.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let header = "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                  Transfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n";
    if sock.write_all(header.as_bytes()).await.is_err() {
        return;
    }
    let chunk = vec![b'a'; 256 * 1024];
    let mut sent = 0u64;
    while sent < bytes {
        let n = ((bytes - sent) as usize).min(chunk.len());
        let size = format!("{n:x}\r\n");
        if sock.write_all(size.as_bytes()).await.is_err()
            || sock.write_all(&chunk[..n]).await.is_err()
            || sock.write_all(b"\r\n").await.is_err()
        {
            return;
        }
        sent += n as u64;
    }
    // No terminating chunk. The response stays open past the cap.
    let _ = sock.read(&mut buf).await;
}

async fn discard_https_body(port: u16, host: &str, path: &str) {
    use rustls::pki_types::ServerName;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::TlsConnector;

    let tcp = match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
        Ok(tcp) => tcp,
        Err(_) => return,
    };
    let connector = TlsConnector::from(insecure_client_tls());
    let Ok(name) = ServerName::try_from("localhost") else {
        return;
    };
    let Ok(mut tls) = connector.connect(name, tcp).await else {
        return;
    };
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/octet-stream\r\nConnection: close\r\n\r\n"
    );
    if tls.write_all(req.as_bytes()).await.is_err() {
        return;
    }
    let mut buf = [0u8; 64 * 1024];
    loop {
        match tls.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

#[tokio::test]
async fn banned_suspended_deactivated_and_expired_tokens_are_refused() {
    init_tracing();
    let server = TestServer::start().await;
    let admin = TestClient::connect(&server).await.unwrap();
    let admin_session = admin.session_id.unwrap();

    let seed = Seed::create(&server.db.pg, "agent", "agent").await;
    let mut client = TestClient::connect_with_token(&server, &seed.raw)
        .await
        .unwrap();
    let session_id = client.session_id.unwrap();

    sqlx::query("UPDATE users SET status = 'banned' WHERE id = $1")
        .bind(seed.user_id)
        .execute(&server.db.pg)
        .await
        .unwrap();
    close_blocked(&server, &mut client).await;
    wait_gone(&server, session_id).await;
    assert!(server.core.sessions.contains_key(&admin_session));
    let err = auth_err(TestClient::connect_with_token(&server, &seed.raw).await);
    assert_code(&err, code::ACCOUNT_SUSPENDED);
    assert!(err.contains("account banned"));

    sqlx::query("UPDATE users SET status = 'suspended' WHERE id = $1")
        .bind(seed.user_id)
        .execute(&server.db.pg)
        .await
        .unwrap();
    let err = auth_err(TestClient::connect_with_token(&server, &seed.raw).await);
    assert_code(&err, code::ACCOUNT_SUSPENDED);
    assert!(err.contains("account suspended"));

    sqlx::query("UPDATE users SET status = 'active', deactivated_at = NOW() WHERE id = $1")
        .bind(seed.user_id)
        .execute(&server.db.pg)
        .await
        .unwrap();
    let err = auth_err(TestClient::connect_with_token(&server, &seed.raw).await);
    assert_code(&err, code::ACCOUNT_DEACTIVATED);
    assert!(err.contains("account deactivated"));

    sqlx::query("UPDATE users SET deactivated_at = NULL WHERE id = $1")
        .bind(seed.user_id)
        .execute(&server.db.pg)
        .await
        .unwrap();
    sqlx::query("UPDATE tokens SET expires_at = NOW() - interval '1 minute' WHERE user_id = $1")
        .bind(seed.user_id)
        .execute(&server.db.pg)
        .await
        .unwrap();
    let err = auth_err(TestClient::connect_with_token(&server, &seed.raw).await);
    assert_code(&err, code::TOKEN_EXPIRED);
    assert!(err.contains("api token expired"));

    // An unlimited token skips expires_at and still honors a ban.
    sqlx::query("UPDATE tokens SET unlimited = true WHERE user_id = $1")
        .bind(seed.user_id)
        .execute(&server.db.pg)
        .await
        .unwrap();
    let mut unlimited = TestClient::connect_with_token(&server, &seed.raw)
        .await
        .unwrap();
    let unlimited_session = unlimited.session_id.unwrap();
    sqlx::query("UPDATE users SET status = 'banned' WHERE id = $1")
        .bind(seed.user_id)
        .execute(&server.db.pg)
        .await
        .unwrap();
    close_blocked(&server, &mut unlimited).await;
    wait_gone(&server, unlimited_session).await;

    sqlx::query("UPDATE users SET status = 'active' WHERE id = $1")
        .bind(seed.user_id)
        .execute(&server.db.pg)
        .await
        .unwrap();
    sqlx::query("UPDATE tokens SET unlimited = false, status = 'revoked', expires_at = NULL WHERE user_id = $1")
        .bind(seed.user_id)
        .execute(&server.db.pg)
        .await
        .unwrap();
    let err = auth_err(TestClient::connect_with_token(&server, &seed.raw).await);
    assert_code(&err, code::TOKEN_INACTIVE);

    drop(admin);
    seed.cleanup().await;
}

#[tokio::test]
async fn sweep_deactivates_only_expired_unclaimed_agents() {
    init_tracing();
    let server = TestServer::start().await;
    let expired = Seed::create(&server.db.pg, "agent", "agent").await;
    let fresh = Seed::create(&server.db.pg, "agent", "agent").await;
    let claimed = Seed::create(&server.db.pg, "agent", "agent").await;
    let human = Seed::create(&server.db.pg, "human", "free").await;

    sqlx::query("UPDATE tokens SET expires_at = NOW() - interval '1 minute' WHERE user_id = $1")
        .bind(expired.user_id)
        .execute(&server.db.pg)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET claimed_at = NOW() WHERE id = $1")
        .bind(claimed.user_id)
        .execute(&server.db.pg)
        .await
        .unwrap();
    sqlx::query("UPDATE tokens SET expires_at = NOW() - interval '1 minute' WHERE user_id = $1")
        .bind(claimed.user_id)
        .execute(&server.db.pg)
        .await
        .unwrap();
    sqlx::query("UPDATE tokens SET expires_at = NOW() - interval '1 minute' WHERE user_id = $1")
        .bind(human.user_id)
        .execute(&server.db.pg)
        .await
        .unwrap();

    policy::deactivate_expired_unclaimed_agents(&server.db.pg)
        .await
        .unwrap();
    let stamp: Option<String> =
        sqlx::query_scalar("SELECT deactivated_at::text FROM users WHERE id = $1")
            .bind(expired.user_id)
            .fetch_one(&server.db.pg)
            .await
            .unwrap();
    assert!(stamp.is_some());
    for id in [fresh.user_id, claimed.user_id, human.user_id] {
        let still: Option<String> =
            sqlx::query_scalar("SELECT deactivated_at::text FROM users WHERE id = $1")
                .bind(id)
                .fetch_one(&server.db.pg)
                .await
                .unwrap();
        assert!(still.is_none(), "{id} was deactivated");
    }
    let status: String = sqlx::query_scalar("SELECT status FROM users WHERE id = $1")
        .bind(expired.user_id)
        .fetch_one(&server.db.pg)
        .await
        .unwrap();
    assert_eq!(status, "active");
    policy::deactivate_expired_unclaimed_agents(&server.db.pg)
        .await
        .unwrap();
    let again: Option<String> =
        sqlx::query_scalar("SELECT deactivated_at::text FROM users WHERE id = $1")
            .bind(expired.user_id)
            .fetch_one(&server.db.pg)
            .await
            .unwrap();
    assert_eq!(again, stamp);

    expired.cleanup().await;
    fresh.cleanup().await;
    claimed.cleanup().await;
    human.cleanup().await;
}

#[tokio::test]
async fn browser_warning_is_only_for_unclaimed_agent_plan_navigations() {
    init_tracing();
    let server = TestServer::start().await;
    let (local, _shutdown) = hello().await;
    let seed = Seed::create(&server.db.pg, "agent", "agent").await;
    let mut client = TestClient::connect_with_token(&server, &seed.raw)
        .await
        .unwrap();
    let session_id = client.session_id.unwrap();
    let (_, subdomain, _) = client.register_http_tunnel(None).await.unwrap();
    connect_data_bridge(&server, session_id, local)
        .await
        .expect("bridge");
    let host = format!("{subdomain}.{}", server.domain);

    let page = fetch(
        server.https_port,
        &host,
        reqwest::Method::GET,
        "text/html",
        &[("Sec-Fetch-Mode", "navigate")],
    )
    .await;
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(html.contains("temporary tunnel created by an automated agent"));
    assert!(html.contains(&host));
    assert!(html.contains("Do not enter passwords"));
    assert!(html.contains(">Continue<"));
    assert!(!html.contains("src="));
    assert!(!html.contains("http://") && !html.contains("https://"));
    assert!(!html.contains("Hello, World!"));

    let absent_mode = fetch(
        server.https_port,
        &host,
        reqwest::Method::GET,
        "text/html,application/xhtml+xml",
        &[],
    )
    .await;
    assert!(absent_mode
        .text()
        .await
        .unwrap()
        .contains("Temporary unverified"));

    let curl = fetch(server.https_port, &host, reqwest::Method::GET, "*/*", &[]).await;
    assert_eq!(curl.text().await.unwrap(), "Hello, World!");
    let json = fetch(
        server.https_port,
        &host,
        reqwest::Method::GET,
        "application/json",
        &[],
    )
    .await;
    assert_eq!(json.text().await.unwrap(), "Hello, World!");
    let posted = fetch(
        server.https_port,
        &host,
        reqwest::Method::POST,
        "text/html",
        &[],
    )
    .await;
    assert_eq!(posted.text().await.unwrap(), "posted");
    let cors = fetch(
        server.https_port,
        &host,
        reqwest::Method::GET,
        "text/html",
        &[("Sec-Fetch-Mode", "cors")],
    )
    .await;
    assert_eq!(cors.text().await.unwrap(), "Hello, World!");
    let skipped = fetch(
        server.https_port,
        &host,
        reqwest::Method::GET,
        "text/html",
        &[("rustunnel-skip-warning", "1")],
    )
    .await;
    assert_eq!(skipped.text().await.unwrap(), "Hello, World!");

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(8))
        .build()
        .unwrap();
    let ack = client
        .get(format!(
            "https://127.0.0.1:{}/?rustunnel_warn_continue=1",
            server.https_port
        ))
        .header("Host", &host)
        .header("Accept", "text/html")
        .send()
        .await
        .unwrap();
    assert_eq!(ack.status(), 303);
    let cookie = ack
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(cookie.starts_with("rustunnel_warn_ok=1"));
    assert!(cookie.contains("Path=/"));
    assert!(cookie.contains("Max-Age=86400"));
    assert!(cookie.contains("Secure"));
    assert!(!cookie.to_ascii_lowercase().contains("domain="));
    assert_eq!(
        ack.headers().get("location").unwrap().to_str().unwrap(),
        "/"
    );

    let through = client
        .get(format!("https://127.0.0.1:{}/", server.https_port))
        .header("Host", &host)
        .header("Accept", "text/html")
        .header("Cookie", "rustunnel_warn_ok=1")
        .send()
        .await
        .unwrap();
    assert_eq!(through.text().await.unwrap(), "Hello, World!");

    sqlx::query("UPDATE users SET claimed_at = NOW() WHERE id = $1")
        .bind(seed.user_id)
        .execute(&server.db.pg)
        .await
        .unwrap();
    let claimed = fetch(
        server.https_port,
        &host,
        reqwest::Method::GET,
        "text/html",
        &[("Sec-Fetch-Mode", "navigate")],
    )
    .await;
    assert_eq!(claimed.text().await.unwrap(), "Hello, World!");
    seed.cleanup().await;

    let upgraded = Seed::create(&server.db.pg, "agent", "agent").await;
    let mut client = TestClient::connect_with_token(&server, &upgraded.raw)
        .await
        .unwrap();
    let session_id = client.session_id.unwrap();
    let (_, subdomain, _) = client.register_http_tunnel(None).await.unwrap();
    connect_data_bridge(&server, session_id, local)
        .await
        .expect("bridge");
    let host = format!("{subdomain}.{}", server.domain);
    let before = fetch(
        server.https_port,
        &host,
        reqwest::Method::GET,
        "text/html",
        &[("Sec-Fetch-Mode", "navigate")],
    )
    .await;
    assert!(before
        .text()
        .await
        .unwrap()
        .contains("Temporary unverified"));
    sqlx::query(
        "UPDATE subscriptions SET plan_id = (SELECT id FROM plans WHERE name = 'payg') \
         WHERE user_id = $1",
    )
    .bind(upgraded.user_id)
    .execute(&server.db.pg)
    .await
    .unwrap();
    let after = fetch(
        server.https_port,
        &host,
        reqwest::Method::GET,
        "text/html",
        &[("Sec-Fetch-Mode", "navigate")],
    )
    .await;
    assert_eq!(after.text().await.unwrap(), "Hello, World!");
    upgraded.cleanup().await;
}

#[tokio::test]
async fn hobby_never_shows_the_warning_and_keeps_its_own_copy() {
    init_tracing();
    let server = TestServer::start().await;
    let (local, _shutdown) = hello().await;
    let hobby = Seed::create(&server.db.pg, "human", "free").await;
    let mut client = TestClient::connect_with_token(&server, &hobby.raw)
        .await
        .unwrap();
    let session_id = client.session_id.unwrap();
    let (_, subdomain, _) = client.register_http_tunnel(None).await.unwrap();
    // Before any proxied request, so a NewConnection frame is not what the
    // next control read returns.
    let custom = client
        .register_http_tunnel(Some("hobby-name"))
        .await
        .expect_err("hobby custom");
    assert!(
        custom.contains("Hobby plan"),
        "hobby custom subdomain error was: {custom}"
    );
    assert!(!custom.contains(code::CUSTOM_SUBDOMAIN_NOT_ALLOWED));
    let (_id, _port) = client.register_tcp_tunnel().await.expect("hobby tcp");

    connect_data_bridge(&server, session_id, local)
        .await
        .expect("bridge");
    let host = format!("{subdomain}.{}", server.domain);
    let page = fetch(
        server.https_port,
        &host,
        reqwest::Method::GET,
        "text/html",
        &[("Sec-Fetch-Mode", "navigate")],
    )
    .await;
    assert_eq!(page.text().await.unwrap(), "Hello, World!");

    sqlx::query(
        "UPDATE tunnel_log SET bytes_proxied = 2000000000, bytes_in = 2000000000 \
         WHERE user_id = $1",
    )
    .bind(hobby.user_id)
    .execute(&server.db.pg)
    .await
    .unwrap();
    let still = fetch(
        server.https_port,
        &host,
        reqwest::Method::GET,
        "text/html",
        &[("Sec-Fetch-Mode", "navigate")],
    )
    .await;
    assert_eq!(still.status(), 200);
    assert_eq!(still.text().await.unwrap(), "Hello, World!");
    hobby.cleanup().await;

    let payg = Seed::create(&server.db.pg, "human", "payg").await;
    let mut client = TestClient::connect_with_token(&server, &payg.raw)
        .await
        .unwrap();
    let (_, sub, _) = client
        .register_http_tunnel(Some("payg-name"))
        .await
        .expect("payg custom subdomain");
    assert_eq!(sub, "payg-name");
    let _ = client.register_tcp_tunnel().await.expect("payg tcp");
    payg.cleanup().await;
}

#[tokio::test]
async fn admin_users_list_filters_kind_and_keeps_deactivated_rows() {
    init_tracing();
    let server = TestServer::start().await;
    let agent = Seed::create(&server.db.pg, "agent", "agent").await;
    let human = Seed::create(&server.db.pg, "human", "free").await;
    sqlx::query("UPDATE users SET deactivated_at = NOW() WHERE id = $1")
        .bind(agent.user_id)
        .execute(&server.db.pg)
        .await
        .unwrap();

    let client = reqwest::Client::new();
    let url = format!(
        "http://127.0.0.1:{}/api/admin/users?kind=agent&search={}",
        server.dashboard_port, agent.user_id
    );
    let listed: Value = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", server.admin_token))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let users = listed["users"].as_array().unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["kind"], "agent");
    assert_eq!(users[0]["source"], "cli");
    assert!(users[0]["claimed_at"].is_null());
    assert!(users[0]["deactivated_at"].is_string());

    let detail: Value = client
        .get(format!(
            "http://127.0.0.1:{}/api/admin/users/{}",
            server.dashboard_port, agent.user_id
        ))
        .header("Authorization", format!("Bearer {}", server.admin_token))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["kind"], "agent");
    assert_eq!(detail["source"], "cli");
    assert!(detail["deactivated_at"].is_string());

    let humans: Value = client
        .get(format!(
            "http://127.0.0.1:{}/api/admin/users?kind=human&search={}",
            server.dashboard_port, agent.user_id
        ))
        .header("Authorization", format!("Bearer {}", server.admin_token))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(humans["users"].as_array().unwrap().len(), 0);

    let unknown: Value = client
        .get(format!(
            "http://127.0.0.1:{}/api/admin/users?kind=nope&search={}",
            server.dashboard_port, human.user_id
        ))
        .header("Authorization", format!("Bearer {}", server.admin_token))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(unknown["total"], 0);

    agent.cleanup().await;
    human.cleanup().await;
}

#[tokio::test]
async fn warning_ack_is_not_an_open_redirect() {
    init_tracing();
    let server = TestServer::start().await;
    let (local, _shutdown) = hello().await;
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(8))
        .build()
        .unwrap();

    let human = Seed::create(&server.db.pg, "human", "free").await;
    let mut human_client = TestClient::connect_with_token(&server, &human.raw)
        .await
        .unwrap();
    let human_session = human_client.session_id.unwrap();
    let (_, human_sub, _) = human_client.register_http_tunnel(None).await.unwrap();
    connect_data_bridge(&server, human_session, local)
        .await
        .expect("human bridge");
    let human_host = format!("{human_sub}.{}", server.domain);
    let proxied = http
        .get(format!(
            "https://127.0.0.1:{}/?rustunnel_warn_continue=1",
            server.https_port
        ))
        .header("Host", &human_host)
        .header("Accept", "text/html")
        .header("Sec-Fetch-Mode", "navigate")
        .send()
        .await
        .unwrap();
    assert_eq!(proxied.status(), 200, "a human tunnel proxies the flag");
    assert!(proxied.headers().get("location").is_none());
    assert_eq!(proxied.text().await.unwrap(), "Hello, World!");

    let agent = Seed::create(&server.db.pg, "agent", "agent").await;
    let mut agent_client = TestClient::connect_with_token(&server, &agent.raw)
        .await
        .unwrap();
    let agent_session = agent_client.session_id.unwrap();
    let (_, agent_sub, _) = agent_client.register_http_tunnel(None).await.unwrap();
    connect_data_bridge(&server, agent_session, local)
        .await
        .expect("agent bridge");
    let agent_host = format!("{agent_sub}.{}", server.domain);
    let posted = http
        .post(format!(
            "https://127.0.0.1:{}/?rustunnel_warn_continue=1",
            server.https_port
        ))
        .header("Host", &agent_host)
        .header("Accept", "text/html")
        .send()
        .await
        .unwrap();
    assert_eq!(posted.status(), 200, "a POST carrying the flag is proxied");
    assert!(posted.headers().get("location").is_none());
    assert_eq!(posted.text().await.unwrap(), "posted");

    for path in [
        "/\\evil.com?rustunnel_warn_continue=1",
        "/%5Cevil.com?rustunnel_warn_continue=1",
        "//evil.com?rustunnel_warn_continue=1",
    ] {
        let (status, location) = raw_https_get(server.https_port, &agent_host, path).await;
        let off_host = location.as_deref().is_some_and(location_leaves_host);
        assert!(
            !off_host,
            "{path} status {status} location {location:?} left the host"
        );
        if status == 303 {
            assert_eq!(location.as_deref(), Some("/"), "{path}");
        }
    }

    human.cleanup().await;
    agent.cleanup().await;
}

fn location_leaves_host(location: &str) -> bool {
    let lower = location.to_ascii_lowercase();
    lower.starts_with("//")
        || lower.starts_with("/\\")
        || lower.contains("evil.com")
        || lower.contains("%5c")
        || lower.contains("\\")
}

async fn raw_https_get(port: u16, host: &str, path: &str) -> (u16, Option<String>) {
    use rustls::pki_types::ServerName;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::TlsConnector;

    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let connector = TlsConnector::from(insecure_client_tls());
    let name = ServerName::try_from("localhost").expect("server name");
    let mut tls = connector.connect(name, tcp).await.expect("tls");
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAccept: text/html\r\nConnection: close\r\n\r\n"
    );
    tls.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    let _ = tls.read_to_end(&mut buf).await;
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let mut location = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("location") {
            location = Some(value.trim().to_string());
        }
    }
    (status, location)
}
