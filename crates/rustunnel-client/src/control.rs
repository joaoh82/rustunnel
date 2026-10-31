//! Control-plane connection and main client loop.
//!
//! # Architecture
//!
//! Two WebSocket connections are used:
//!
//! 1. **Control WS** (`wss://<server>/_control`) — carries JSON `ControlFrame`
//!    messages as binary WebSocket frames.  This matches the server's session
//!    handler verbatim.
//!
//! 2. **Data WS** (`wss://<server>/_data/<session_id>`) — carries raw yamux
//!    frames via `WsCompat`.  The client operates as `Mode::Client` and opens
//!    one outbound yamux stream per incoming `NewConnection` event.
//!
//!    NOTE: The server must expose a `/_data/<session_id>` WebSocket endpoint
//!    that links the yamux session to the matching control session and calls
//!    `MuxSession::next_inbound()` when a `DataStreamOpen` frame arrives on
//!    the control plane.  This endpoint is not yet implemented; until it is,
//!    `connect_data_ws` will fail gracefully and data proxying will be skipped.
//!
//! # Flow per proxied connection
//!
//! 1. Server sends `NewConnection { conn_id, client_addr, protocol }`.
//! 2. Client opens an outbound yamux stream on the data WS.
//! 3. Client sends `DataStreamOpen { conn_id }` on the control WS.
//! 4. Client connects to the local service and copies bytes bidirectionally.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::poll_fn;
use futures_util::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use futures_util::sink::Sink;
use futures_util::stream::Stream;
use futures_util::{SinkExt, StreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};
use uuid::Uuid;
use yamux::{Connection, Mode, Stream as YamuxStream};

use rustunnel_protocol::{decode_frame, encode_frame, ControlFrame, TunnelProtocol};

use crate::config::{ClientConfig, TunnelDef};
use crate::display::{self, TunnelDisplay};
use crate::error::{Error, Result};
use crate::inspect::tap::ConnTap;
use crate::inspect::{Inspector, SessionStatus, TunnelInfo};
use crate::output;
use crate::proxy;

// ── timeouts & intervals ──────────────────────────────────────────────────────

const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const PING_INTERVAL: Duration = Duration::from_secs(30);
const PONG_DEADLINE: Duration = Duration::from_secs(10);
const DATA_WS_RECONNECT_DELAY: Duration = Duration::from_millis(200);
const DATA_WS_RECONNECT_INTERVAL: Duration = Duration::from_millis(500);
const DATA_WS_RECONNECT_MAX_ATTEMPTS: u32 = 5;

// ── insecure TLS (local dev only) ─────────────────────────────────────────────

/// A `ServerCertVerifier` that accepts any certificate.
/// **Never use this in production.**
#[derive(Debug)]
struct NoCertVerifier;

impl ServerCertVerifier for NoCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Build a `tokio_tungstenite::Connector` that skips certificate verification.
fn insecure_connector() -> Connector {
    let tls_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoCertVerifier))
        .with_no_client_auth();
    Connector::Rustls(Arc::new(tls_config))
}

// ── WsCompat — WebSocket ↔ futures::io bridge (mirrors server/control/mux.rs) ─

/// Adapts a `WebSocketStream` into `futures::io::{AsyncRead, AsyncWrite}` so
/// that yamux can operate over WebSocket binary frames.
struct WsCompat<S> {
    inner: WebSocketStream<S>,
    read_buf: Vec<u8>,
    read_pos: usize,
}

impl<S> WsCompat<S> {
    fn new(ws: WebSocketStream<S>) -> Self {
        Self {
            inner: ws,
            read_buf: Vec::new(),
            read_pos: 0,
        }
    }
}

impl<S> AsyncRead for WsCompat<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        loop {
            // Drain leftover bytes from a previous large message.
            if this.read_pos < this.read_buf.len() {
                let n = (this.read_buf.len() - this.read_pos).min(buf.len());
                buf[..n].copy_from_slice(&this.read_buf[this.read_pos..this.read_pos + n]);
                this.read_pos += n;
                return Poll::Ready(Ok(n));
            }

            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(Ok(0)), // EOF
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, e)))
                }
                Poll::Ready(Some(Ok(msg))) => match msg {
                    Message::Binary(data) => {
                        let n = data.len().min(buf.len());
                        buf[..n].copy_from_slice(&data[..n]);
                        if n < data.len() {
                            this.read_buf = data[n..].to_vec();
                            this.read_pos = 0;
                        }
                        return Poll::Ready(Ok(n));
                    }
                    Message::Close(_) => return Poll::Ready(Ok(0)),
                    _ => continue, // skip ping/pong/text WS frames
                },
            }
        }
    }
}

impl<S> AsyncWrite for WsCompat<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let msg = Message::Binary(buf.to_vec());
        match Pin::new(&mut this.inner).poll_ready(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => {
                return Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, e)))
            }
            Poll::Ready(Ok(())) => {}
        }
        if let Err(e) = Pin::new(&mut this.inner).start_send(msg) {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, e)));
        }
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner)
            .poll_flush(cx)
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner)
            .poll_close(cx)
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e))
    }
}

// ── yamux data connection ─────────────────────────────────────────────────────

type CtrlWs = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type DataConn = Connection<WsCompat<MaybeTlsStream<tokio::net::TcpStream>>>;

// ── public entry point ────────────────────────────────────────────────────────

/// Establish the control WS, authenticate, register all tunnels, then run the
/// main event loop until the connection closes or Ctrl-C is pressed.
///
/// Returns `Ok(())` on a clean exit and `Err(_)` on any unrecoverable error.
pub async fn connect(
    config: &ClientConfig,
    tunnels: &[TunnelDef],
    inspector: &Arc<Inspector>,
) -> Result<()> {
    let sp = display::spinner("Connecting to tunnel server…");
    inspector.set_status(SessionStatus::Connecting);
    inspector.set_server(config.server.clone());

    // 1. Control WebSocket —————————————————————————————————————————————————
    let ctrl_url = format!("wss://{}/_control", config.server);
    let (mut ctrl_ws, _) = if config.insecure {
        tokio_tungstenite::connect_async_tls_with_config(
            &ctrl_url,
            None,
            false,
            Some(insecure_connector()),
        )
        .await
    } else {
        tokio_tungstenite::connect_async(&ctrl_url).await
    }
    .map_err(|e| {
        Error::Connection(format!(
            "cannot reach {} ({e}) — check network/DNS, or pass --server <host:port> \
             or --region <eu|us|ap> to pick a different edge",
            config.server
        ))
    })?;

    sp.set_message("Authenticating…");

    // 2. Auth ——————————————————————————————————————————————————————————————
    let token = config.auth_token.clone().unwrap_or_default();
    send_frame(
        &mut ctrl_ws,
        &ControlFrame::Auth {
            token,
            client_version: env!("CARGO_PKG_VERSION").to_string(),
        },
    )
    .await?;

    let (session_id, server_supports_lb) = match recv_frame_timeout(&mut ctrl_ws, AUTH_TIMEOUT)
        .await
        .map_err(|e| with_server_context(e, &config.server))?
    {
        ControlFrame::AuthOk {
            session_id,
            server_version,
        } => {
            info!(%session_id, %server_version, "authenticated");
            // TUNNEL-7 Phase 6: gate emission of the new wire fields and
            // probe-loop spawning on server version. An older edge that
            // doesn't recognise `TunnelHealthy` / `TunnelUnhealthy` would
            // log a `decode_frame` warning per frame; we just refrain
            // from sending them.
            let supports_lb = crate::version::server_supports_load_balancing(&server_version);
            if !supports_lb {
                debug!(
                    %server_version,
                    "server does not advertise TUNNEL-7 load-balancing support; \
                     group fields and health probes will be suppressed for this session"
                );
            }
            (session_id, supports_lb)
        }
        ControlFrame::AuthError { message } => {
            sp.finish_and_clear();
            let token_desc = if config.auth_token.as_deref().unwrap_or("").is_empty() {
                "no auth token was provided".to_string()
            } else {
                format!(
                    "the token came from the {}",
                    config
                        .auth_token_source
                        .as_deref()
                        .unwrap_or("client configuration")
                )
            };
            return Err(Error::Auth(format!(
                "server {} rejected authentication: {message} ({token_desc}) — \
                     get a token at https://rustunnel.com (Dashboard -> API Keys), \
                     or for a self-hosted server create one with `rustunnel token create`",
                config.server
            )));
        }
        other => {
            sp.finish_and_clear();
            return Err(Error::Connection(format!(
                "unexpected frame during auth: {other:?}"
            )));
        }
    };

    sp.set_message("Registering tunnels…");

    // 3. Register tunnels —————————————————————————————————————————————————
    let mut registered: Vec<(TunnelDef, String)> = Vec::new();
    let mut notices: Vec<String> = Vec::new();

    for tunnel in tunnels {
        // P2P subscribers don't register a tunnel — they send P2pConnect instead.
        if tunnel.p2p_target.is_some() {
            registered.push((tunnel.clone(), String::new()));
            continue;
        }

        let request_id = Uuid::new_v4().to_string();
        let protocol = proto_to_enum(&tunnel.proto)?;
        let local_addr = format!("{}:{}", tunnel.local_host, tunnel.local_port);

        // ── Version gate (TUNNEL-7 Phase 6) ───────────────────────────────
        // Only emit the load-balancing fields when the edge advertises
        // support. An older edge would just ignore them at the wire level
        // (Option fields with #[serde(default)]) but we suppress them
        // anyway to (a) keep logs clean and (b) make the user-visible
        // behaviour symmetric: configured group + old edge → same as
        // ungrouped + new edge. We warn loudly so the misconfig doesn't
        // silently degrade.
        let (effective_group, effective_group_key_hash, effective_health_check) =
            if server_supports_lb {
                let group_key_hash = tunnel.group_key.as_ref().map(|raw| {
                    use sha2::{Digest, Sha256};
                    hex::encode(Sha256::digest(raw.as_bytes()))
                });
                let health_check_wire = match &tunnel.health_check {
                    Some(cfg) => Some(health_check_to_wire(cfg)?),
                    None => None,
                };
                (tunnel.group.clone(), group_key_hash, health_check_wire)
            } else {
                if tunnel.group.is_some() || tunnel.group_key.is_some() {
                    warn!(
                        subdomain = ?tunnel.subdomain,
                        group = ?tunnel.group,
                        "server does not support load balancing — registering as a solo tunnel; \
                         bump your edge to 0.7+ to enable group dispatch"
                    );
                }
                if tunnel.health_check.is_some() {
                    warn!(
                        subdomain = ?tunnel.subdomain,
                        "server does not support health probes — disabling client-side probe loop \
                         for this tunnel; bump your edge to 0.7+ to enable failover routing"
                    );
                }
                (None, None, None)
            };

        send_frame(
            &mut ctrl_ws,
            &ControlFrame::RegisterTunnel {
                request_id: request_id.clone(),
                protocol,
                subdomain: tunnel.subdomain.clone(),
                local_addr,
                p2p_secret_hash: tunnel.p2p_secret_hash.clone(),
                p2p_name: tunnel.p2p_name.clone(),
                group: effective_group,
                group_key_hash: effective_group_key_hash,
                health_check: effective_health_check,
                client_surface: client_surface_from_env(),
            },
        )
        .await?;

        match recv_frame_timeout(&mut ctrl_ws, AUTH_TIMEOUT).await? {
            ControlFrame::TunnelRegistered {
                public_url,
                tunnel_id,
                notice,
                ..
            } => {
                info!(%public_url, %tunnel_id, "tunnel registered");
                if let Some(notice) = notice.filter(|n| !n.is_empty()) {
                    notices.push(notice);
                }
                // Store tunnel_id on the TunnelDef for later use (P2P NAT info).
                let mut tdef = tunnel.clone();
                tdef.registered_tunnel_id = Some(tunnel_id);
                registered.push((tdef, public_url));
            }
            ControlFrame::TunnelError { message, .. } => {
                sp.finish_and_clear();
                return Err(Error::Tunnel(message));
            }
            other => {
                sp.finish_and_clear();
                return Err(Error::Connection(format!(
                    "unexpected frame during registration: {other:?}"
                )));
            }
        }
    }

    sp.finish_and_clear();

    // 3b. P2P publisher: probe NAT and report to server ──────────────────
    for (tunnel, _url) in &registered {
        if let (Some(_name), Some(tid)) = (&tunnel.p2p_name, tunnel.registered_tunnel_id) {
            info!("P2P publisher: probing NAT type via STUN...");
            let stun_result = crate::stun::probe_nat(&[]).await;
            info!(
                nat = stun_result.nat_type.as_str(),
                mapped = ?stun_result.mapped_addrs,
                "P2P publisher: NAT classification complete"
            );

            let mapped: Vec<String> = stun_result
                .mapped_addrs
                .iter()
                .map(|a| a.to_string())
                .collect();
            let locals: Vec<String> = stun_result
                .local_addrs
                .iter()
                .map(|a| a.to_string())
                .collect();

            send_frame(
                &mut ctrl_ws,
                &ControlFrame::P2pNatInfo {
                    tunnel_id: tid,
                    nat_type: stun_result.nat_type.as_str().to_string(),
                    mapped_addrs: mapped,
                    local_addrs: locals,
                },
            )
            .await?;
        }
    }

    // 3c. P2P subscriber: start local TCP listener ────────────────────────
    // The subscriber listens on its local port. Each incoming connection
    // triggers a P2pConnect to the server, establishing a relay on demand.
    let p2p_sub_info: Option<(String, String, Vec<u8>)> =
        tunnels.iter().find_map(
            |t| match (&t.p2p_target, &t.p2p_secret_hash, &t.p2p_secret) {
                (Some(target), Some(hash), Some(secret)) => {
                    Some((target.clone(), hash.clone(), secret.as_bytes().to_vec()))
                }
                (Some(target), Some(hash), None) => {
                    Some((target.clone(), hash.clone(), Vec::new()))
                }
                _ => None,
            },
        );

    let p2p_accept_rx = if let Some(sub) = tunnels.iter().find(|t| t.p2p_target.is_some()) {
        let addr = format!("{}:{}", sub.local_host, sub.local_port);
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| Error::Connection(format!("P2P subscriber: cannot bind {addr}: {e}")))?;
        info!(%addr, "P2P subscriber listening for incoming connections");
        let (tx, rx) = mpsc::channel::<tokio::net::TcpStream>(16);
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        debug!(%peer, "P2P subscriber: accepted local connection");
                        if tx.send(stream).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("P2P subscriber: accept error: {e}");
                    }
                }
            }
        });
        Some(rx)
    } else {
        None
    };

    // 4. Data WebSocket (yamux) ————————————————————————————————————————————
    let data_conn: Option<DataConn> =
        connect_data_ws(&config.server, session_id, config.insecure).await;

    // Spawn a background driver task that continuously polls the yamux
    // connection (Mode::Server — accepts inbound streams opened by the server).
    // It reads the 16-byte conn_id prefix the server writes into each stream,
    // then forwards (conn_id, stream) pairs to the main loop via a channel.
    //
    // When the driver exits (yamux error or data WS drop), stream_tx is
    // dropped and stream_rx.recv() returns None, signalling main_loop to
    // return an error and trigger reconnect.
    let (stream_tx, stream_rx) = mpsc::channel::<(Uuid, YamuxStream)>(16);

    if let Some(conn) = data_conn {
        tokio::spawn(drive_client_mux(conn, stream_tx));
    } else {
        warn!("data WebSocket unavailable — proxy connections will be skipped");
    }

    // 5. Publish state + print startup display ————————————————————————————
    //    The UIs (terminal + web inspector) read the tunnel list from the
    //    inspector, so populate it before deciding what to print.
    inspector.set_tunnels(
        registered
            .iter()
            .map(|(t, url)| TunnelInfo {
                name: tunnel_label(t),
                proto: t.proto.clone(),
                local: format!("{}:{}", t.local_host, t.local_port),
                public_url: if url.is_empty() {
                    match &t.p2p_target {
                        Some(target) => format!("p2p://{target}"),
                        None => String::new(),
                    }
                } else {
                    url.clone()
                },
                healthy: None,
            })
            .collect(),
    );
    inspector.set_status(SessionStatus::Online);

    //    Human mode: startup box (unless the terminal UI owns the screen).
    //    JSON mode: one NDJSON `tunnel_ready` event per tunnel (preceded by
    //    `reconnected` when this is a re-connect).
    if output::json_mode() {
        if output::take_reconnect_pending() {
            output::emit(&output::Event::Reconnected);
        }
        for (t, url) in &registered {
            // P2P subscribers don't register a tunnel and have no URL of
            // their own — report the target they are wired to instead.
            let url = if url.is_empty() {
                match &t.p2p_target {
                    Some(target) => format!("p2p://{target}"),
                    None => continue,
                }
            } else {
                url.clone()
            };
            output::emit(&output::Event::tunnel_ready(
                &t.proto,
                &url,
                t.local_port,
                &t.local_host,
                t.registered_tunnel_id.map(|id| id.to_string()),
                t.subdomain.clone().or_else(|| t.p2p_name.clone()),
            ));
        }
    } else if !crate::tui::active() {
        let display_tunnels: Vec<TunnelDisplay> = registered
            .iter()
            .map(|(t, url)| TunnelDisplay {
                name: t.subdomain.clone().unwrap_or_else(|| "tunnel".into()),
                proto: t.proto.clone(),
                local: format!("{}:{}", t.local_host, t.local_port),
                public_url: url.clone(),
            })
            .collect();
        display::print_startup_box(&display_tunnels);
        if let Some(url) = inspector.session().inspect_url {
            display::print_inspect_url(&url);
        }
    }

    // Bandwidth (and any future) notices ride on TunnelRegistered. Released
    // clients ignore the field. Print the server text as-is.
    for notice in &notices {
        if output::json_mode() {
            output::emit(&output::Event::Notice {
                message: notice.clone(),
            });
        } else {
            inspector.push_log(notice.clone());
            if !crate::tui::active() {
                eprintln!("{notice}");
                eprintln!();
            }
        }
    }

    // 6. Spawn health-probe tasks for tunnels with a health_check ─────────
    //    Each probe task writes `TunnelHealthy` / `TunnelUnhealthy` frames
    //    into `outbound_frame_tx`; main_loop drains that channel and writes
    //    to the control WS. Probe tasks die when the channel closes.
    //
    //    Skipped entirely when the server doesn't advertise load-balancing
    //    support (TUNNEL-7 Phase 6) — the probe's only output channel is
    //    those frames, which an older edge would log as `decode_frame`
    //    errors. We already warned at registration time above; the spawn
    //    branch just no-ops.
    let (outbound_frame_tx, outbound_frame_rx) =
        mpsc::channel::<rustunnel_protocol::ControlFrame>(32);
    if server_supports_lb {
        for (tunnel, _url) in &registered {
            let Some(cfg) = &tunnel.health_check else {
                continue;
            };
            let Some(tunnel_id) = tunnel.registered_tunnel_id else {
                continue;
            };
            let spec = match crate::health::ProbeSpec::from_config(cfg) {
                Ok(s) => s,
                Err(e) => {
                    warn!(%tunnel_id, "invalid health_check config: {e}; skipping probe");
                    continue;
                }
            };
            let local_addr = format!("{}:{}", tunnel.local_host, tunnel.local_port);
            let tx = outbound_frame_tx.clone();
            info!(
                %tunnel_id, ?spec.kind, interval_ms = spec.interval.as_millis() as u64,
                "starting client-side health probe"
            );
            tokio::spawn(crate::health::run_probe_loop(
                tunnel_id, local_addr, spec, tx,
            ));
        }
    }
    drop(outbound_frame_tx); // probe tasks own clones; main holder dropped so the channel closes when they exit

    // 7. Main event loop ——————————————————————————————————————————————————
    main_loop(
        &mut ctrl_ws,
        stream_rx,
        &registered,
        &config.server,
        session_id,
        config.insecure,
        p2p_accept_rx,
        p2p_sub_info,
        outbound_frame_rx,
        inspector,
    )
    .await
}

/// `RUSTUNNEL_SURFACE=mcp` is set by the MCP server. Anything else, including
/// an unset variable, stays off the wire so released-CLI payloads are unchanged.
fn client_surface_from_env() -> Option<String> {
    client_surface_from_value(std::env::var("RUSTUNNEL_SURFACE").ok().as_deref())
}

fn client_surface_from_value(raw: Option<&str>) -> Option<String> {
    raw.filter(|s| s.trim().eq_ignore_ascii_case("mcp"))
        .map(|_| "mcp".to_string())
}

/// Display name for a tunnel: its subdomain, P2P name, or a generic fallback.
fn tunnel_label(def: &TunnelDef) -> String {
    def.subdomain
        .clone()
        .or_else(|| def.p2p_name.clone())
        .or_else(|| def.p2p_target.clone())
        .unwrap_or_else(|| "tunnel".into())
}

/// Everything needed to proxy one inbound connection.
struct ConnMeta {
    local_addr: String,
    protocol: TunnelProtocol,
    client_addr: String,
    tunnel_label: String,
}

/// Start the proxy task for one connection, tapping it when the tunnel carries
/// HTTP and inspection is enabled.
fn spawn_proxy(meta: ConnMeta, stream: YamuxStream, conn_id: Uuid, inspector: &Arc<Inspector>) {
    if meta.protocol == TunnelProtocol::Udp {
        tokio::spawn(proxy::proxy_udp_connection(
            stream,
            meta.local_addr,
            conn_id,
            Arc::clone(inspector),
        ));
        return;
    }

    // Only HTTP tunnels carry parseable traffic; TCP stays a raw byte pipe.
    let is_http = matches!(meta.protocol, TunnelProtocol::Http | TunnelProtocol::Https);
    let tap = (is_http && inspector.capture_enabled()).then(|| {
        ConnTap::new(
            Arc::clone(inspector),
            meta.tunnel_label,
            conn_id,
            meta.client_addr,
        )
    });

    tokio::spawn(proxy::proxy_connection(
        stream,
        meta.local_addr,
        conn_id,
        Arc::clone(inspector),
        tap,
    ));
}

// ── main loop ─────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn main_loop(
    ctrl_ws: &mut CtrlWs,
    mut stream_rx: mpsc::Receiver<(Uuid, YamuxStream)>,
    registered: &[(TunnelDef, String)],
    server: &str,
    session_id: Uuid,
    insecure: bool,
    mut p2p_accept_rx: Option<mpsc::Receiver<tokio::net::TcpStream>>,
    p2p_sub_info: Option<(String, String, Vec<u8>)>, // (target_name, secret_hash, raw_secret)
    mut outbound_frame_rx: mpsc::Receiver<ControlFrame>,
    inspector: &Arc<Inspector>,
) -> Result<()> {
    let mut ping_interval = tokio::time::interval(PING_INTERVAL);
    ping_interval.tick().await; // skip the immediate first tick

    let mut last_pong = tokio::time::Instant::now();
    let mut awaiting_pong = false;

    // Pending maps to handle the two orderings of NewConnection vs stream:
    //   pending_conns:   NewConnection arrived first  → wait for matching stream
    //   pending_streams: stream arrived first         → wait for matching NewConnection
    let mut pending_conns: std::collections::HashMap<Uuid, ConnMeta> =
        std::collections::HashMap::new();
    let mut pending_streams: std::collections::HashMap<Uuid, YamuxStream> =
        std::collections::HashMap::new();

    // P2P subscriber state: maps conn_id → already-accepted local TCP stream.
    // When the yamux stream arrives for this conn_id, we bridge them directly
    // instead of opening a new outbound connection.
    let mut p2p_request_to_local: std::collections::HashMap<String, tokio::net::TcpStream> =
        std::collections::HashMap::new();
    let mut p2p_local_streams: std::collections::HashMap<Uuid, tokio::net::TcpStream> =
        std::collections::HashMap::new();

    // Dummy future that never resolves — used when there's no P2P listener.
    let p2p_disabled_rx = &mut None::<mpsc::Receiver<tokio::net::TcpStream>>;

    loop {
        // Pick the right P2P accept channel (real or disabled).
        let p2p_rx = p2p_accept_rx
            .as_mut()
            .unwrap_or(p2p_disabled_rx.get_or_insert_with(|| mpsc::channel(1).1));

        tokio::select! {
            biased;

            // ── P2P subscriber: accepted local TCP connection ─────────────
            Some(local_stream) = p2p_rx.recv() => {
                if let Some((ref target, ref secret_hash, _)) = p2p_sub_info {
                    let request_id = Uuid::new_v4().to_string();
                    debug!(%request_id, target, "P2P subscriber: sending connect for local connection");
                    send_frame(ctrl_ws, &ControlFrame::P2pConnect {
                        request_id: request_id.clone(),
                        target_tunnel_name: target.clone(),
                        secret_hash: secret_hash.clone(),
                    }).await?;
                    p2p_request_to_local.insert(request_id, local_stream);
                }
            }

            // ── Ctrl-C / SIGTERM ──────────────────────────────────────────
            //    In TUI mode the terminal is in raw mode, so no SIGINT is
            //    generated — the UI's quit key drives the shutdown arm below.
            _ = tokio::signal::ctrl_c() => {
                info!("received interrupt — shutting down");
                let _ = ctrl_ws.close(None).await;
                return Ok(());
            }

            // ── Shutdown requested by the terminal UI ─────────────────────
            _ = inspector.shutdown_signal() => {
                info!("shutdown requested — closing session");
                let _ = ctrl_ws.close(None).await;
                return Ok(());
            }

            // ── Periodic ping ─────────────────────────────────────────────
            _ = ping_interval.tick() => {
                if awaiting_pong && last_pong.elapsed() > PONG_DEADLINE {
                    return Err(Error::Connection("heartbeat timeout".into()));
                }
                let ts = now_ms();
                send_frame(ctrl_ws, &ControlFrame::Ping { timestamp: ts }).await?;
                awaiting_pong = true;
            }

            // ── Outbound frame from a probe task ──────────────────────────
            //    Health-probe tasks (TUNNEL-7 Phase 4) push TunnelHealthy /
            //    TunnelUnhealthy frames here. The select arm serialises all
            //    writes to the WS — probe tasks never touch ctrl_ws directly.
            Some(frame) = outbound_frame_rx.recv() => {
                // Mirror health transitions into the UIs on the way past.
                match &frame {
                    ControlFrame::TunnelHealthy { tunnel_id } => {
                        note_health(registered, inspector, *tunnel_id, true);
                    }
                    ControlFrame::TunnelUnhealthy { tunnel_id, .. } => {
                        note_health(registered, inspector, *tunnel_id, false);
                    }
                    _ => {}
                }
                send_frame(ctrl_ws, &frame).await?;
            }

            // ── Inbound yamux stream from background driver ───────────────
            pair = stream_rx.recv() => {
                match pair {
                    Some((conn_id, stream)) => {
                        // Check if this is a P2P subscriber stream with an
                        // already-accepted local TCP connection.
                        if let Some(local_tcp) = p2p_local_streams.remove(&conn_id) {
                            debug!(%conn_id, "P2P subscriber: bridging local TCP ↔ yamux");
                            tokio::spawn(proxy::proxy_p2p_relay(
                                stream, local_tcp, conn_id, Arc::clone(inspector),
                            ));
                        } else if let Some(meta) = pending_conns.remove(&conn_id) {
                            // NewConnection arrived earlier — proxy immediately.
                            spawn_proxy(meta, stream, conn_id, inspector);
                        } else {
                            // NewConnection hasn't arrived yet — stash the stream.
                            debug!(%conn_id, "stream arrived before NewConnection — buffering");
                            pending_streams.insert(conn_id, stream);
                        }
                    }
                    None => {
                        // The yamux driver exited (data WebSocket dropped or
                        // yamux error). Reconnect the data WS independently
                        // without tearing down the control WS or re-registering
                        // tunnels.
                        warn!("data WebSocket dropped — reconnecting");
                        pending_conns.clear();
                        pending_streams.clear();
                        tokio::time::sleep(DATA_WS_RECONNECT_DELAY).await;
                        let mut reconnected = false;
                        for attempt in 1..=DATA_WS_RECONNECT_MAX_ATTEMPTS {
                            if let Some(conn) = connect_data_ws(server, session_id, insecure).await {
                                let (new_tx, new_rx) = mpsc::channel::<(Uuid, YamuxStream)>(16);
                                tokio::spawn(drive_client_mux(conn, new_tx));
                                stream_rx = new_rx;
                                reconnected = true;
                                info!(attempt, "data WebSocket reconnected");
                                break;
                            }
                            debug!(attempt, "data WS reconnect attempt failed — retrying");
                            tokio::time::sleep(DATA_WS_RECONNECT_INTERVAL).await;
                        }
                        if !reconnected {
                            return Err(Error::Connection(
                                "failed to reconnect data WebSocket".into(),
                            ));
                        }
                    }
                }
            }

            // ── Inbound control frame ─────────────────────────────────────
            msg = ctrl_ws.next() => {
                match msg {
                    None => {
                        info!("server closed control WebSocket");
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        return Err(Error::Connection(e.to_string()));
                    }
                    Some(Ok(msg)) => {
                        let frame = match parse_binary(msg) {
                            Ok(f) => f,
                            Err(_) => continue, // ignore non-binary frames
                        };

                        match frame {
                            ControlFrame::NewConnection { conn_id, client_addr, protocol } => {
                                debug!(%conn_id, %client_addr, ?protocol, "new connection from server");

                                // For P2P subscriber connections, the local TCP
                                // stream is already accepted and stored in
                                // p2p_local_streams. The bridge will happen when
                                // the yamux stream arrives. Skip normal proxy.
                                if p2p_local_streams.contains_key(&conn_id) {
                                    debug!(%conn_id, "P2P subscriber: NewConnection for pending relay — skip proxy");
                                    continue;
                                }

                                match find_local_target(registered, &protocol) {
                                    None => {
                                        warn!(%conn_id, ?protocol,
                                            "no local address configured for protocol");
                                    }
                                    Some((local_addr, tunnel_label)) => {
                                        let meta = ConnMeta {
                                            local_addr,
                                            protocol,
                                            client_addr: client_addr.clone(),
                                            tunnel_label,
                                        };
                                        if let Some(stream) = pending_streams.remove(&conn_id) {
                                            spawn_proxy(meta, stream, conn_id, inspector);
                                        } else {
                                            debug!(%conn_id, "NewConnection arrived before stream — buffering");
                                            pending_conns.insert(conn_id, meta);
                                        }
                                    }
                                }
                            }

                            ControlFrame::P2pConnected { request_id, conn_id } => {
                                debug!(%conn_id, %request_id, "P2P relay established");
                                // Move the pre-accepted local TCP stream from
                                // request_id map to conn_id map. When the yamux
                                // stream arrives, we'll bridge them.
                                if let Some(local) = p2p_request_to_local.remove(&request_id) {
                                    // Check if yamux stream already arrived.
                                    if let Some(yamux) = pending_streams.remove(&conn_id) {
                                        debug!(%conn_id, "P2P: yamux already arrived — bridging");
                                        tokio::spawn(proxy::proxy_p2p_relay(
                                            yamux, local, conn_id, Arc::clone(inspector),
                                        ));
                                    } else {
                                        p2p_local_streams.insert(conn_id, local);
                                    }
                                }
                            }

                            ControlFrame::P2pPunchInstructions {
                                conn_id,
                                peer_addrs,
                                strategy,
                                punch_timeout_ms,
                            } => {
                                info!(%conn_id, %strategy, ?peer_addrs, "P2P direct: received punch instructions");
                                // Attempt hole punching in the background.
                                // If successful, the direct connection replaces
                                // the relay. If it fails, the relay continues
                                // to work transparently.
                                let addrs: Vec<std::net::SocketAddr> = peer_addrs
                                    .iter()
                                    .filter_map(|a| a.parse().ok())
                                    .collect();
                                let secret_bytes = p2p_sub_info.as_ref()
                                    .map(|(_, _, s)| s.clone())
                                    .unwrap_or_default();
                                if !addrs.is_empty() {
                                    tokio::spawn(async move {
                                        let result = crate::p2p_direct::attempt_direct_connection(
                                            &addrs,
                                            &strategy,
                                            "subscriber",
                                            &secret_bytes,
                                            punch_timeout_ms,
                                        )
                                        .await;
                                        match result {
                                            Some(_conn) => {
                                                info!(%conn_id, "P2P direct: connection established (upgrade from relay)");
                                                // TODO: replace relay with direct QUIC connection
                                            }
                                            None => {
                                                debug!(%conn_id, "P2P direct: punch failed — continuing with relay");
                                            }
                                        }
                                    });
                                }
                            }

                            ControlFrame::P2pError { request_id, message } => {
                                warn!(%request_id, %message, "P2P connect failed");
                                // Drop the local TCP stream — connection refused to peer.
                                p2p_request_to_local.remove(&request_id);
                            }

                            ControlFrame::Ping { timestamp } => {
                                send_frame(ctrl_ws, &ControlFrame::Pong { timestamp }).await?;
                            }

                            ControlFrame::Pong { timestamp } => {
                                awaiting_pong = false;
                                last_pong = tokio::time::Instant::now();
                                // Round-trip to the edge, shown as live latency.
                                inspector.set_latency(now_ms().saturating_sub(timestamp));
                            }

                            other => {
                                debug!(?other, "unexpected control frame — ignored");
                            }
                        }
                    }
                }
            }
        }
    }
}

// ── data connection ───────────────────────────────────────────────────────────

async fn connect_data_ws(server: &str, session_id: Uuid, insecure: bool) -> Option<DataConn> {
    let url = format!("wss://{}/_data/{}", server, session_id);
    let result = if insecure {
        tokio_tungstenite::connect_async_tls_with_config(
            &url,
            None,
            false,
            Some(insecure_connector()),
        )
        .await
    } else {
        tokio_tungstenite::connect_async(&url).await
    };
    match result {
        Ok((ws, _)) => {
            let compat = WsCompat::new(ws);
            // Mode::Server: the server (Mode::Client) opens streams and writes
            // first; we accept inbound streams via poll_next_inbound.
            let conn = Connection::new(compat, yamux::Config::default(), Mode::Server);
            info!(%session_id, "data WebSocket connected, yamux Mode::Server");
            Some(conn)
        }
        Err(e) => {
            debug!("data WebSocket not available ({e})");
            None
        }
    }
}

/// Background driver for the client-side yamux connection.
///
/// Continuously polls `poll_next_inbound` (which also drives all outbound IO).
/// For each accepted stream, a dedicated task is spawned to read the 16-byte
/// conn_id prefix the server wrote and forward the (conn_id, stream) pair to
/// the main loop. Spawning a task per stream means `poll_next_inbound` is
/// called again immediately, so concurrent streams are accepted in parallel
/// instead of serially.
async fn drive_client_mux(mut conn: DataConn, stream_tx: mpsc::Sender<(Uuid, YamuxStream)>) {
    loop {
        match poll_fn(|cx| conn.poll_next_inbound(cx)).await {
            Some(Ok(mut stream)) => {
                // Spawn so the driver returns to poll_next_inbound immediately,
                // allowing the next inbound stream to be accepted in parallel.
                let stream_tx = stream_tx.clone();
                tokio::spawn(async move {
                    let mut id_bytes = [0u8; 16];
                    match stream.read_exact(&mut id_bytes).await {
                        Ok(()) => {
                            let conn_id = Uuid::from_bytes(id_bytes);
                            let _ = stream_tx.send((conn_id, stream)).await;
                        }
                        Err(e) => warn!("read conn_id from yamux stream: {e}"),
                    }
                });
            }
            Some(Err(e)) => {
                debug!("yamux data conn error: {e}");
                break;
            }
            None => {
                debug!("yamux data conn closed");
                break;
            }
        }
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Append the server address to connection-class errors so timeout/closed
/// messages say *which* server did not answer.
fn with_server_context(e: Error, server: &str) -> Error {
    match e {
        Error::Connection(msg) => Error::Connection(format!("{msg} (server {server})")),
        other => other,
    }
}

fn proto_to_enum(proto: &str) -> Result<TunnelProtocol> {
    match proto.to_lowercase().as_str() {
        "http" => Ok(TunnelProtocol::Http),
        "https" => Ok(TunnelProtocol::Https),
        "tcp" => Ok(TunnelProtocol::Tcp),
        "udp" => Ok(TunnelProtocol::Udp),
        "p2p" => Ok(TunnelProtocol::P2p),
        other => Err(Error::Config(format!("unknown protocol: {other}"))),
    }
}

/// Convert the user-facing `HealthCheckConfig` into the wire-level
/// `HealthCheckSpec` carried by `RegisterTunnel`. Validates that
/// `kind = "http"` includes a path. Used by the registration loop above.
fn health_check_to_wire(
    cfg: &crate::config::HealthCheckConfig,
) -> Result<rustunnel_protocol::HealthCheckSpec> {
    use rustunnel_protocol::{HealthCheckKind, HealthCheckSpec};
    let kind = match cfg.kind.as_str() {
        "tcp" => HealthCheckKind::Tcp,
        "http" => HealthCheckKind::Http,
        other => {
            return Err(Error::Config(format!(
                "unknown health_check kind '{other}' (expected 'tcp' or 'http')"
            )));
        }
    };
    if matches!(kind, HealthCheckKind::Http) && cfg.path.is_none() {
        return Err(Error::Config(
            "health_check kind='http' requires a path".into(),
        ));
    }
    Ok(HealthCheckSpec {
        kind,
        interval_secs: cfg.interval_secs.max(1),
        timeout_secs: cfg.timeout_secs.max(1),
        max_failed: cfg.max_failed.max(1),
        http_path: cfg.path.clone(),
        http_expect_2xx: cfg.expect_2xx,
        // Per-tenant webhook URL (separate from the operator-side
        // `[load_balancing] alert_webhook_url` on the edge). Server
        // fires this when *this tunnel's* group goes 0/N healthy.
        alert_webhook_url: cfg.alert_webhook.clone(),
    })
}

/// Find the local address (`"host:port"`) and display label of the registered
/// tunnel matching `protocol`.  The address stays a raw string so that
/// `TcpStream::connect` can perform DNS resolution (e.g. for `localhost`).
fn find_local_target(
    registered: &[(TunnelDef, String)],
    protocol: &TunnelProtocol,
) -> Option<(String, String)> {
    for (def, _) in registered {
        let matches = match protocol {
            TunnelProtocol::Http | TunnelProtocol::Https => {
                def.proto == "http" || def.proto == "https"
            }
            TunnelProtocol::Tcp => def.proto == "tcp",
            TunnelProtocol::Udp => def.proto == "udp",
            TunnelProtocol::P2p => def.proto == "p2p",
        };
        if matches {
            return Some((
                format!("{}:{}", def.local_host, def.local_port),
                tunnel_label(def),
            ));
        }
    }
    None
}

/// Reflect a health-probe transition into the UIs, keyed by the tunnel's local
/// address (what the UI tunnel list is keyed on).
fn note_health(
    registered: &[(TunnelDef, String)],
    inspector: &Arc<Inspector>,
    tunnel_id: Uuid,
    healthy: bool,
) {
    for (def, _) in registered {
        if def.registered_tunnel_id == Some(tunnel_id) {
            inspector.set_tunnel_health(&format!("{}:{}", def.local_host, def.local_port), healthy);
            return;
        }
    }
}

async fn send_frame(ws: &mut CtrlWs, frame: &ControlFrame) -> Result<()> {
    let bytes = encode_frame(frame);
    ws.send(Message::Binary(bytes))
        .await
        .map_err(|e| Error::Connection(e.to_string()))
}

async fn recv_frame_timeout(ws: &mut CtrlWs, timeout: Duration) -> Result<ControlFrame> {
    let msg = tokio::time::timeout(timeout, ws.next())
        .await
        .map_err(|_| Error::Connection("timeout waiting for server response".into()))?
        .ok_or_else(|| Error::Connection("connection closed".into()))?
        .map_err(|e| Error::Connection(e.to_string()))?;
    parse_binary(msg)
}

fn parse_binary(msg: Message) -> Result<ControlFrame> {
    match msg {
        Message::Binary(data) => decode_frame(&data).map_err(Error::Protocol),
        other => Err(Error::Connection(format!(
            "expected binary frame, got {other:?}"
        ))),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::client_surface_from_value;

    #[test]
    fn client_surface_is_sent_only_for_mcp() {
        assert_eq!(client_surface_from_value(None), None);
        assert_eq!(client_surface_from_value(Some("cli")), None);
        assert_eq!(client_surface_from_value(Some("")), None);
        assert_eq!(
            client_surface_from_value(Some("mcp")).as_deref(),
            Some("mcp")
        );
        assert_eq!(
            client_surface_from_value(Some(" MCP ")).as_deref(),
            Some("mcp")
        );
    }
}
