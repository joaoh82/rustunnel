//! Shared test helpers: TestServer + TestClient + yamux injection utilities.
#![allow(dead_code)]
//!
//! # Architecture note — data plane
//!
//! The server now has a `/_data/<session_id>` WebSocket endpoint on the
//! control port.  For HTTP / TCP proxy tests we still *inject* yamux streams
//! directly into `TunnelCore::resolve_pending_conn()` to keep tests fast and
//! self-contained (no real WebSocket round-trip needed).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use futures_util::future::poll_fn;
use futures_util::io::AsyncReadExt;
use futures_util::{SinkExt, StreamExt};
use rcgen::{CertificateParams, KeyPair};
use rustls::ClientConfig;
#[allow(unused_imports)]
use rustls::ServerConfig as RustlsConfig;
use rustunnel_server::db::Db;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use uuid::Uuid;
use yamux::{Config as YamuxConfig, Connection, Mode};

use rustunnel_server::control::mux::WsCompat;

use rustunnel_protocol::{decode_frame, encode_frame, ControlFrame, TunnelProtocol};
use rustunnel_server::config::{
    AuthSection, DatabaseSection, LimitsSection, LoggingSection, ServerConfig, ServerSection,
    TlsSection,
};
use rustunnel_server::core::TunnelCore;
use rustunnel_server::tls::acme::build_tls_config;

// ── TLS helpers ───────────────────────────────────────────────────────────────

/// A `rustls::ClientConfig` that accepts any server certificate.
/// For use in tests only — never in production.
pub fn insecure_client_tls() -> Arc<ClientConfig> {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::DigitallySignedStruct;

    #[derive(Debug)]
    struct AcceptAnyCert;

    impl ServerCertVerifier for AcceptAnyCert {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            vec![
                rustls::SignatureScheme::RSA_PKCS1_SHA256,
                rustls::SignatureScheme::RSA_PKCS1_SHA384,
                rustls::SignatureScheme::RSA_PKCS1_SHA512,
                rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
                rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
                rustls::SignatureScheme::ECDSA_NISTP521_SHA512,
                rustls::SignatureScheme::RSA_PSS_SHA256,
                rustls::SignatureScheme::RSA_PSS_SHA384,
                rustls::SignatureScheme::RSA_PSS_SHA512,
                rustls::SignatureScheme::ED25519,
            ]
        }
    }

    Arc::new(
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
            .with_no_client_auth(),
    )
}

// ── port selection ────────────────────────────────────────────────────────────

/// Bind port 0 to let the OS assign a free port, then release it.
/// There is a small TOCTOU window; acceptable for tests.
pub fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind port 0");
    l.local_addr().unwrap().port()
}

// ── cert generation ───────────────────────────────────────────────────────────

/// Generate a self-signed cert + key, write PEM files to `dir`, return paths.
pub fn generate_test_cert(dir: &TempDir) -> (String, String) {
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");

    let params = CertificateParams::new(vec!["localhost".to_string(), "127.0.0.1".to_string()])
        .expect("cert params");
    let key_pair = KeyPair::generate().expect("key pair");
    let cert = params.self_signed(&key_pair).expect("self-signed cert");

    std::fs::write(&cert_path, cert.pem()).expect("write cert");
    std::fs::write(&key_path, key_pair.serialize_pem()).expect("write key");

    (
        cert_path.to_str().unwrap().to_string(),
        key_path.to_str().unwrap().to_string(),
    )
}

// ── TestServer ────────────────────────────────────────────────────────────────

/// A running server instance with all components live on random ports.
pub struct TestServer {
    pub control_port: u16,
    pub http_port: u16,
    pub https_port: u16,
    pub dashboard_port: u16,
    pub domain: String,
    pub admin_token: String,
    pub core: Arc<TunnelCore>,
    pub db: Db,
    pub config: Arc<ServerConfig>,

    // Kept alive for the test's duration.
    _temp_dir: TempDir,
    task_handles: Vec<AbortHandle>,
}

impl TestServer {
    /// Start a server with default settings (require_auth = true, random ports).
    pub async fn start() -> Self {
        Self::start_with(true, "integration-test-token").await
    }

    /// Start a server with configurable auth settings.
    pub async fn start_with(require_auth: bool, admin_token: &str) -> Self {
        let control_port = free_port();
        let http_port = free_port();
        let https_port = free_port();
        let dashboard_port = free_port();
        let tcp_low = free_port();
        // Reserve a small range; each test gets its own server so no overlap.
        let tcp_high = tcp_low.saturating_add(9);
        Self::start_on_ports(
            control_port,
            http_port,
            https_port,
            dashboard_port,
            [tcp_low, tcp_high],
            require_auth,
            admin_token,
        )
        .await
    }

    /// Start a server on specific pre-allocated ports.
    /// Used by reconnect tests to restart on the same port set.
    pub async fn start_on_ports(
        control_port: u16,
        http_port: u16,
        https_port: u16,
        dashboard_port: u16,
        tcp_port_range: [u16; 2],
        require_auth: bool,
        admin_token: &str,
    ) -> Self {
        let [tcp_low, tcp_high] = tcp_port_range;

        let temp_dir = TempDir::new().expect("temp dir");
        let (cert_path, key_path) = generate_test_cert(&temp_dir);

        let pg_url = std::env::var("TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://rustunnel:test@localhost:5432/rustunnel_test".to_string()
        });

        let config = Arc::new(ServerConfig {
            server: ServerSection {
                domain: "localhost".to_string(),
                http_port,
                https_port,
                control_port,
                dashboard_port,
            },
            tls: TlsSection {
                cert_path: cert_path.clone(),
                key_path: key_path.clone(),
                acme_enabled: false,
                acme_email: String::new(),
                acme_staging: false,
                acme_account_dir: temp_dir.path().to_str().unwrap().to_string(),
                cloudflare_api_token: String::new(),
                cloudflare_zone_id: String::new(),
            },
            auth: AuthSection {
                admin_token: admin_token.to_string(),
                require_auth,
            },
            database: DatabaseSection {
                url: pg_url,
                captured_path: ":memory:".to_string(),
            },
            logging: LoggingSection {
                level: "warn".to_string(),
                format: "pretty".to_string(),
                audit_log_path: None,
            },
            limits: LimitsSection {
                max_tunnels_per_session: 10,
                max_connections_per_tunnel: 100,
                rate_limit_rps: 10_000,
                ip_rate_limit_rps: 100_000,
                request_body_max_bytes: 1024 * 1024,
                tcp_port_range: [tcp_low, tcp_high],
            },
        });

        // Database.
        let db = rustunnel_server::db::init_db(&config.database)
            .await
            .expect("init_db");

        // TLS.
        let tls_cfg = build_tls_config(&cert_path, &key_path).expect("build_tls_config");
        let tls_handle = Arc::new(ArcSwap::new(Arc::new(tls_cfg)));
        let tls_snapshot = tls_handle.load_full();

        // Shared tunnel core.
        let core = Arc::new(TunnelCore::new(
            config.limits.tcp_port_range,
            config.limits.max_tunnels_per_session,
            config.limits.max_connections_per_tunnel,
            config.limits.ip_rate_limit_rps,
        ));

        let (capture_tx, capture_rx) = mpsc::channel(256);
        let mut task_handles = Vec::new();

        // a) Control plane.
        let control_addr: SocketAddr = format!("127.0.0.1:{control_port}").parse().unwrap();
        let h = tokio::spawn({
            let core = Arc::clone(&core);
            let cfg = Arc::clone(&config);
            let tls_handle = Arc::clone(&tls_handle);
            let db = db.clone();
            async move {
                let _ = rustunnel_server::control::server::run_control_plane(
                    control_addr,
                    core,
                    cfg,
                    tls_handle,
                    rustunnel_server::audit::noop_audit(),
                    db,
                )
                .await;
            }
        });
        task_handles.push(h.abort_handle());

        // b) HTTP + HTTPS edge.
        let http_addr: SocketAddr = format!("127.0.0.1:{http_port}").parse().unwrap();
        let https_addr: SocketAddr = format!("127.0.0.1:{https_port}").parse().unwrap();
        let h = tokio::spawn({
            let core = Arc::clone(&core);
            let domain = config.server.domain.clone();
            let limits = rustunnel_server::edge::HttpEdgeConfig {
                rate_limit_rps: config.limits.rate_limit_rps,
                request_body_max_bytes: config.limits.request_body_max_bytes,
            };
            async move {
                let _ = rustunnel_server::edge::run_http_edge(
                    http_addr,
                    https_addr,
                    tls_snapshot,
                    core,
                    domain,
                    Some(capture_tx),
                    limits,
                )
                .await;
            }
        });
        task_handles.push(h.abort_handle());

        // c) TCP edge.
        let h = tokio::spawn({
            let core = Arc::clone(&core);
            async move {
                rustunnel_server::edge::run_tcp_edge(core).await;
            }
        });
        task_handles.push(h.abort_handle());

        // d) Dashboard.
        let dashboard_addr: SocketAddr = format!("127.0.0.1:{dashboard_port}").parse().unwrap();
        let h = tokio::spawn({
            let core = Arc::clone(&core);
            let admin_token = config.auth.admin_token.clone();
            async move {
                let _ = rustunnel_server::dashboard::run_dashboard(
                    dashboard_addr,
                    core,
                    db,
                    capture_rx,
                    admin_token,
                    rustunnel_server::audit::noop_audit(),
                )
                .await;
            }
        });
        task_handles.push(h.abort_handle());

        // Wait for all sockets to bind before returning.
        tokio::time::sleep(Duration::from_millis(100)).await;

        Self {
            control_port,
            http_port,
            https_port,
            dashboard_port,
            domain: "localhost".to_string(),
            admin_token: admin_token.to_string(),
            core,
            db,
            config,
            _temp_dir: temp_dir,
            task_handles,
        }
    }

    /// Stop the server by aborting all tasks.
    pub fn stop(&self) {
        for h in &self.task_handles {
            h.abort();
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop();
    }
}

// ── TestClient ────────────────────────────────────────────────────────────────

type CtrlWs = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// A test control-plane client that speaks the binary WebSocket protocol
/// directly (no CLI, no reconnect logic).
pub struct TestClient {
    ws: CtrlWs,
    pub session_id: Option<Uuid>,
}

impl TestClient {
    /// Connect and authenticate with `token`.
    pub async fn connect_with_token(server: &TestServer, token: &str) -> Result<Self, String> {
        let url = format!("wss://127.0.0.1:{}/_control", server.control_port);

        let connector = Connector::Rustls(insecure_client_tls());
        let (ws, _) =
            tokio_tungstenite::connect_async_tls_with_config(&url, None, false, Some(connector))
                .await
                .map_err(|e| format!("WS connect: {e}"))?;

        let mut client = Self {
            ws,
            session_id: None,
        };

        // Send Auth frame.
        client
            .send(&ControlFrame::Auth {
                token: token.to_string(),
                client_version: "test".to_string(),
            })
            .await?;

        // Receive response.
        match client.recv_timeout(Duration::from_secs(5)).await? {
            ControlFrame::AuthOk { session_id, .. } => {
                client.session_id = Some(session_id);
                Ok(client)
            }
            ControlFrame::AuthError { message } => Err(format!("AuthError: {message}")),
            other => Err(format!("unexpected frame: {other:?}")),
        }
    }

    /// Connect with the server's admin token.
    pub async fn connect(server: &TestServer) -> Result<Self, String> {
        Self::connect_with_token(server, &server.admin_token).await
    }

    /// Connect and expect the auth to fail, returning the error message.
    pub async fn connect_expect_auth_error(
        server: &TestServer,
        token: &str,
    ) -> Result<String, String> {
        let url = format!("wss://127.0.0.1:{}/_control", server.control_port);
        let connector = Connector::Rustls(insecure_client_tls());
        let (ws, _) =
            tokio_tungstenite::connect_async_tls_with_config(&url, None, false, Some(connector))
                .await
                .map_err(|e| format!("WS connect: {e}"))?;

        let mut client = Self {
            ws,
            session_id: None,
        };

        client
            .send(&ControlFrame::Auth {
                token: token.to_string(),
                client_version: "test".to_string(),
            })
            .await?;

        match client.recv_timeout(Duration::from_secs(5)).await? {
            ControlFrame::AuthError { message } => Ok(message),
            ControlFrame::AuthOk { .. } => Err("expected AuthError but got AuthOk".into()),
            other => Err(format!("unexpected frame: {other:?}")),
        }
    }

    // ── tunnel registration ───────────────────────────────────────────────────

    /// Register an HTTP tunnel.  Returns `(tunnel_id, subdomain, public_url)`.
    pub async fn register_http_tunnel(
        &mut self,
        subdomain: Option<&str>,
    ) -> Result<(Uuid, String, String), String> {
        let req_id = Uuid::new_v4().to_string();
        self.send(&ControlFrame::RegisterTunnel {
            request_id: req_id.clone(),
            protocol: TunnelProtocol::Http,
            subdomain: subdomain.map(str::to_string),
            local_addr: "127.0.0.1:0".to_string(), // advisory only
        })
        .await?;

        match self.recv_timeout(Duration::from_secs(5)).await? {
            ControlFrame::TunnelRegistered {
                tunnel_id,
                public_url,
                ..
            } => {
                // Derive the subdomain from the public_url.
                let sub = public_url
                    .trim_start_matches("http://")
                    .split('.')
                    .next()
                    .unwrap_or("")
                    .to_string();
                Ok((tunnel_id, sub, public_url))
            }
            ControlFrame::TunnelError { message, .. } => Err(format!("TunnelError: {message}")),
            other => Err(format!("unexpected frame: {other:?}")),
        }
    }

    /// Register a TCP tunnel.  Returns `(tunnel_id, assigned_port)`.
    pub async fn register_tcp_tunnel(&mut self) -> Result<(Uuid, u16), String> {
        let req_id = Uuid::new_v4().to_string();
        self.send(&ControlFrame::RegisterTunnel {
            request_id: req_id,
            protocol: TunnelProtocol::Tcp,
            subdomain: None,
            local_addr: "127.0.0.1:0".to_string(),
        })
        .await?;

        match self.recv_timeout(Duration::from_secs(5)).await? {
            ControlFrame::TunnelRegistered {
                tunnel_id,
                assigned_port,
                ..
            } => {
                let port = assigned_port.ok_or("missing assigned_port")?;
                Ok((tunnel_id, port))
            }
            ControlFrame::TunnelError { message, .. } => Err(format!("TunnelError: {message}")),
            other => Err(format!("unexpected frame: {other:?}")),
        }
    }

    // ── frame I/O ─────────────────────────────────────────────────────────────

    pub async fn send(&mut self, frame: &ControlFrame) -> Result<(), String> {
        let bytes = encode_frame(frame);
        self.ws
            .send(Message::Binary(bytes))
            .await
            .map_err(|e| format!("send: {e}"))
    }

    pub async fn recv_timeout(&mut self, timeout: Duration) -> Result<ControlFrame, String> {
        let msg = tokio::time::timeout(timeout, self.ws.next())
            .await
            .map_err(|_| "recv timeout".to_string())?
            .ok_or("WS closed")?
            .map_err(|e| format!("recv: {e}"))?;

        match msg {
            Message::Binary(data) => decode_frame(&data).map_err(|e| format!("decode: {e}")),
            other => Err(format!("expected binary frame, got {other:?}")),
        }
    }

    /// Wait for the next `NewConnection` frame (up to 10 s).
    pub async fn wait_new_connection(&mut self) -> Result<Uuid, String> {
        let deadline = Duration::from_secs(10);
        match self.recv_timeout(deadline).await? {
            ControlFrame::NewConnection { conn_id, .. } => Ok(conn_id),
            ControlFrame::Ping { timestamp } => {
                // Reply with Pong and retry.
                self.send(&ControlFrame::Pong { timestamp }).await?;
                match self.recv_timeout(deadline).await? {
                    ControlFrame::NewConnection { conn_id, .. } => Ok(conn_id),
                    other => Err(format!("expected NewConnection, got {other:?}")),
                }
            }
            other => Err(format!("expected NewConnection, got {other:?}")),
        }
    }
}

// ── data WebSocket bridge ─────────────────────────────────────────────────────

/// Connect a `/_data/<session_id>` WebSocket to the server and run a yamux
/// bridge task that handles all proxy connections automatically.
///
/// The server runs yamux `Mode::Client` and opens one outbound stream per
/// incoming HTTP/TCP connection.  It writes a 16-byte conn_id prefix to each
/// stream so the client can route it.  This function connects a `Mode::Server`
/// yamux connection, accepts those inbound streams, reads the conn_id, and
/// bidirectionally copies data between the stream and `local_addr`.
///
/// Returns immediately after spawning the background task.
pub fn connect_data_bridge(server: &TestServer, session_id: Uuid, local_addr: SocketAddr) {
    let url = format!(
        "wss://127.0.0.1:{}/_data/{}",
        server.control_port, session_id
    );
    let connector = Connector::Rustls(insecure_client_tls());

    tokio::spawn(async move {
        let (ws, _) = match tokio_tungstenite::connect_async_tls_with_config(
            &url,
            None,
            false,
            Some(connector),
        )
        .await
        {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("[test data bridge] WS connect failed: {e}");
                return;
            }
        };

        // yamux Mode::Server: the server (Mode::Client) opens outbound streams.
        let ws_compat = WsCompat::new(ws);
        let mut conn = Connection::new(ws_compat, YamuxConfig::default(), Mode::Server);

        loop {
            match poll_fn(|cx| conn.poll_next_inbound(cx)).await {
                Some(Ok(mut stream)) => {
                    // Read the 16-byte conn_id prefix the server wrote.
                    let mut id_bytes = [0u8; 16];
                    if stream.read_exact(&mut id_bytes).await.is_err() {
                        continue;
                    }
                    let _conn_id = Uuid::from_bytes(id_bytes);

                    // Bridge the yamux stream to the local service.
                    tokio::spawn(async move {
                        match tokio::net::TcpStream::connect(local_addr).await {
                            Ok(mut local) => {
                                let mut remote = stream.compat();
                                let _ =
                                    tokio::io::copy_bidirectional(&mut local, &mut remote).await;
                            }
                            Err(e) => {
                                eprintln!("[test data bridge] connect {local_addr}: {e}");
                            }
                        }
                    });
                }
                Some(Err(e)) => {
                    eprintln!("[test data bridge] yamux error: {e}");
                    break;
                }
                None => break,
            }
        }
    });
}

// ── yamux stream injection (legacy, kept for reference) ──────────────────────

/// Yamux 0.13 uses a lazy SYN: the SYN frame is only sent when the opener
/// writes the first byte.  Therefore we cannot open a stream on Side A and
/// immediately accept it on Side B without anyone writing.
///
/// Solution: the SERVER-mode connection opens the stream (which the HTTP/TCP
/// edge receives via `resolve_pending_conn`).  When the edge writes its first
/// bytes (HTTP request or TCP data), the DATA+SYN frame travels through the
/// in-memory duplex to the CLIENT-mode connection, which then accepts it as
/// an inbound stream.  That inbound stream is sent to the proxy task via a
/// oneshot channel.
///
/// Returns `(edge_stream, proxy_rx)`:
/// * `edge_stream` — inject into `core.resolve_pending_conn()`.
/// * `proxy_rx` — resolves to the proxy-side stream *after* the edge writes
///   its first byte; the proxy task awaits this before bridging
///   to the local service.
pub async fn make_yamux_pair() -> (yamux::Stream, tokio::sync::oneshot::Receiver<yamux::Stream>) {
    // Large buffer so a full HTTP response fits without backpressure.
    let (io_a, io_b) = tokio::io::duplex(2 * 1024 * 1024);

    let (edge_tx, edge_rx) = tokio::sync::oneshot::channel::<yamux::Stream>();
    let (proxy_tx, proxy_rx) = tokio::sync::oneshot::channel::<yamux::Stream>();

    // Server driver: opens one outbound stream (the edge will get this stream
    // and write DATA+SYN through it).  Keepalive loop processes ACKs and data.
    tokio::spawn(async move {
        let mut conn = Connection::new(io_b.compat(), YamuxConfig::default(), Mode::Server);
        let stream = poll_fn(|cx| conn.poll_new_outbound(cx))
            .await
            .expect("yamux server open outbound");
        let _ = edge_tx.send(stream);
        // Keep driving so stream commands (DATA+SYN from edge writes) are
        // flushed to io_b and ACKs from the client side are processed.
        loop {
            match tokio::time::timeout(
                Duration::from_secs(30),
                poll_fn(|cx| conn.poll_next_inbound(cx)),
            )
            .await
            {
                Ok(None) | Ok(Some(Err(_))) => break,
                Ok(Some(Ok(_))) | Err(_) => {}
            }
        }
    });

    // Client driver: waits for the DATA+SYN frame that arrives when the edge
    // writes its first byte.  Accepts the inbound stream and hands it to the
    // proxy task.
    tokio::spawn(async move {
        let mut conn = Connection::new(io_a.compat(), YamuxConfig::default(), Mode::Client);
        // This await returns only after the server side sends DATA+SYN.
        // connection closed before a stream arrived → ignore
        if let Some(Ok(stream)) = poll_fn(|cx| conn.poll_next_inbound(cx)).await {
            let _ = proxy_tx.send(stream);
        }
        // Keep driving for subsequent data frames.
        loop {
            match tokio::time::timeout(
                Duration::from_secs(30),
                poll_fn(|cx| conn.poll_next_inbound(cx)),
            )
            .await
            {
                Ok(None) | Ok(Some(Err(_))) => break,
                Ok(Some(Ok(_))) | Err(_) => {}
            }
        }
    });

    let edge_stream = edge_rx
        .await
        .expect("yamux server driver died before opening stream");
    (edge_stream, proxy_rx)
}

/// Inject a proxy path for a single `NewConnection`:
/// 1. Creates a yamux pair (edge_stream + lazy proxy_rx).
/// 2. Delivers `edge_stream` to the waiting edge task via `resolve_pending_conn`.
/// 3. Spawns a proxy task that waits for the proxy-side stream (appears once
///    the edge writes its first byte), then bridges it to `local_addr`.
pub async fn inject_proxy(core: &Arc<TunnelCore>, conn_id: Uuid, local_addr: SocketAddr) {
    let (edge_stream, proxy_rx) = make_yamux_pair().await;

    // Deliver the stream to the waiting edge task.
    core.resolve_pending_conn(&conn_id, edge_stream);

    // Proxy: wait for the client-side yamux stream (arrives after edge's first
    // write), then bridge bidirectionally to the local service.
    tokio::spawn(async move {
        use tokio_util::compat::FuturesAsyncReadCompatExt;

        let proxy_stream = match proxy_rx.await {
            Ok(s) => s,
            Err(_) => {
                eprintln!("[test proxy] yamux proxy stream never appeared");
                return;
            }
        };
        let mut local = match tokio::net::TcpStream::connect(local_addr).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[test proxy] connect {local_addr}: {e}");
                return;
            }
        };
        let mut remote = proxy_stream.compat();
        let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
    });
}

// ── reqwest helper ────────────────────────────────────────────────────────────

/// Build a `reqwest::Client` that accepts any TLS certificate.
pub fn insecure_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest client")
}

// ── tracing (one-time init) ───────────────────────────────────────────────────

/// Install a quiet tracing subscriber (warn level) if not already installed.
/// Also installs the rustls crypto provider (aws-lc-rs) once per process.
pub fn init_tracing() {
    // rustls requires a process-level CryptoProvider when both aws-lc-rs and
    // ring are present in the dependency graph.  Install it idempotently.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let _ = tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")))
        .with(fmt::layer().with_test_writer())
        .try_init();
}
