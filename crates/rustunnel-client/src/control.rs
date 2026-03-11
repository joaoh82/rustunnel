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
use crate::proxy;

// ── timeouts & intervals ──────────────────────────────────────────────────────

const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const PING_INTERVAL: Duration = Duration::from_secs(30);
const PONG_DEADLINE: Duration = Duration::from_secs(10);

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
pub async fn connect(config: &ClientConfig, tunnels: &[TunnelDef]) -> Result<()> {
    let sp = display::spinner("Connecting to tunnel server…");

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
    .map_err(|e| Error::Connection(format!("control WS: {e}")))?;

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

    let session_id = match recv_frame_timeout(&mut ctrl_ws, AUTH_TIMEOUT).await? {
        ControlFrame::AuthOk {
            session_id,
            server_version,
        } => {
            info!(%session_id, %server_version, "authenticated");
            session_id
        }
        ControlFrame::AuthError { message } => {
            sp.finish_and_clear();
            return Err(Error::Auth(message));
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

    for tunnel in tunnels {
        let request_id = Uuid::new_v4().to_string();
        let protocol = proto_to_enum(&tunnel.proto)?;
        let local_addr = format!("{}:{}", tunnel.local_host, tunnel.local_port);

        send_frame(
            &mut ctrl_ws,
            &ControlFrame::RegisterTunnel {
                request_id: request_id.clone(),
                protocol,
                subdomain: tunnel.subdomain.clone(),
                local_addr,
            },
        )
        .await?;

        match recv_frame_timeout(&mut ctrl_ws, AUTH_TIMEOUT).await? {
            ControlFrame::TunnelRegistered { public_url, .. } => {
                info!(%public_url, "tunnel registered");
                registered.push((tunnel.clone(), public_url));
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

    // 4. Data WebSocket (yamux) ————————————————————————————————————————————
    let data_conn: Option<DataConn> =
        connect_data_ws(&config.server, session_id, config.insecure).await;

    // Spawn a background driver task that continuously polls the yamux
    // connection (Mode::Server — accepts inbound streams opened by the server).
    // It reads the 16-byte conn_id prefix the server writes into each stream,
    // then forwards (conn_id, stream) pairs to the main loop via a channel.
    //
    // Keeping a clone of stream_tx alive prevents the channel from closing
    // when/if the driver task exits early (e.g. yamux session reset), so the
    // main loop's select arm blocks instead of spinning.
    let (stream_tx, stream_rx) = mpsc::channel::<(Uuid, YamuxStream)>(16);
    let _stream_tx_keep = stream_tx.clone();

    if let Some(conn) = data_conn {
        tokio::spawn(drive_client_mux(conn, stream_tx));
    } else {
        warn!("data WebSocket unavailable — proxy connections will be skipped");
    }

    // 5. Print startup display ————————————————————————————————————————————
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

    // 6. Main event loop ——————————————————————————————————————————————————
    main_loop(&mut ctrl_ws, stream_rx, &registered).await
}

// ── main loop ─────────────────────────────────────────────────────────────────

async fn main_loop(
    ctrl_ws: &mut CtrlWs,
    mut stream_rx: mpsc::Receiver<(Uuid, YamuxStream)>,
    registered: &[(TunnelDef, String)],
) -> Result<()> {
    let mut ping_interval = tokio::time::interval(PING_INTERVAL);
    ping_interval.tick().await; // skip the immediate first tick

    let mut last_pong = tokio::time::Instant::now();
    let mut awaiting_pong = false;

    // Pending maps to handle the two orderings of NewConnection vs stream:
    //   pending_conns:   NewConnection arrived first  → wait for matching stream
    //   pending_streams: stream arrived first         → wait for matching NewConnection
    let mut pending_conns: std::collections::HashMap<Uuid, String> =
        std::collections::HashMap::new();
    let mut pending_streams: std::collections::HashMap<Uuid, YamuxStream> =
        std::collections::HashMap::new();

    loop {
        tokio::select! {
            biased;

            // ── Ctrl-C / SIGTERM ──────────────────────────────────────────
            _ = tokio::signal::ctrl_c() => {
                info!("received interrupt — shutting down");
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

            // ── Inbound yamux stream from background driver ───────────────
            pair = stream_rx.recv() => {
                if let Some((conn_id, stream)) = pair {
                    if let Some(local_addr) = pending_conns.remove(&conn_id) {
                        // NewConnection arrived earlier — proxy immediately.
                        tokio::spawn(proxy::proxy_connection(stream, local_addr, conn_id));
                    } else {
                        // NewConnection hasn't arrived yet — stash the stream.
                        debug!(%conn_id, "stream arrived before NewConnection — buffering");
                        pending_streams.insert(conn_id, stream);
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
                                debug!(%conn_id, %client_addr, "new connection from server");

                                match find_local_addr(registered, &protocol) {
                                    None => {
                                        warn!(%conn_id, ?protocol,
                                            "no local address configured for protocol");
                                    }
                                    Some(local_addr) => {
                                        if let Some(stream) = pending_streams.remove(&conn_id) {
                                            // Stream already arrived — proxy immediately.
                                            tokio::spawn(proxy::proxy_connection(
                                                stream, local_addr, conn_id,
                                            ));
                                        } else {
                                            // Stream hasn't arrived yet — stash the address.
                                            debug!(%conn_id, "NewConnection arrived before stream — buffering");
                                            pending_conns.insert(conn_id, local_addr);
                                        }
                                    }
                                }
                            }

                            ControlFrame::Ping { timestamp } => {
                                send_frame(ctrl_ws, &ControlFrame::Pong { timestamp }).await?;
                            }

                            ControlFrame::Pong { .. } => {
                                awaiting_pong = false;
                                last_pong = tokio::time::Instant::now();
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
/// For each accepted stream, reads the 16-byte conn_id prefix the server
/// wrote, then forwards the (conn_id, stream) pair to the main loop.
async fn drive_client_mux(mut conn: DataConn, stream_tx: mpsc::Sender<(Uuid, YamuxStream)>) {
    loop {
        match poll_fn(|cx| conn.poll_next_inbound(cx)).await {
            Some(Ok(mut stream)) => {
                let mut id_bytes = [0u8; 16];
                match stream.read_exact(&mut id_bytes).await {
                    Ok(()) => {
                        let conn_id = Uuid::from_bytes(id_bytes);
                        if stream_tx.send((conn_id, stream)).await.is_err() {
                            break; // main loop exited
                        }
                    }
                    Err(e) => warn!("read conn_id from yamux stream: {e}"),
                }
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

fn proto_to_enum(proto: &str) -> Result<TunnelProtocol> {
    match proto.to_lowercase().as_str() {
        "http" => Ok(TunnelProtocol::Http),
        "https" => Ok(TunnelProtocol::Https),
        "tcp" => Ok(TunnelProtocol::Tcp),
        other => Err(Error::Config(format!("unknown protocol: {other}"))),
    }
}

/// Find the local address string (`"host:port"`) for a registered tunnel
/// matching `protocol`.  Returns a raw string so that `TcpStream::connect`
/// can perform DNS resolution (e.g. for `localhost`).
fn find_local_addr(
    registered: &[(TunnelDef, String)],
    protocol: &TunnelProtocol,
) -> Option<String> {
    for (def, _) in registered {
        let matches = match protocol {
            TunnelProtocol::Http | TunnelProtocol::Https => {
                def.proto == "http" || def.proto == "https"
            }
            TunnelProtocol::Tcp => def.proto == "tcp",
        };
        if matches {
            return Some(format!("{}:{}", def.local_host, def.local_port));
        }
    }
    None
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
