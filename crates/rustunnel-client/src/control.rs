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
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::poll_fn;
use futures_util::io::{AsyncRead, AsyncWrite};
use futures_util::sink::Sink;
use futures_util::stream::Stream;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};
use uuid::Uuid;
use yamux::{Connection, Mode};

use rustunnel_protocol::{decode_frame, encode_frame, ControlFrame, TunnelProtocol};

use crate::config::{ClientConfig, TunnelDef};
use crate::display::{self, TunnelDisplay};
use crate::error::{Error, Result};
use crate::proxy;

// ── timeouts & intervals ──────────────────────────────────────────────────────

const AUTH_TIMEOUT:  Duration = Duration::from_secs(10);
const PING_INTERVAL: Duration = Duration::from_secs(30);
const PONG_DEADLINE: Duration = Duration::from_secs(10);

// ── WsCompat — WebSocket ↔ futures::io bridge (mirrors server/control/mux.rs) ─

/// Adapts a `WebSocketStream` into `futures::io::{AsyncRead, AsyncWrite}` so
/// that yamux can operate over WebSocket binary frames.
struct WsCompat<S> {
    inner:    WebSocketStream<S>,
    read_buf: Vec<u8>,
    read_pos: usize,
}

impl<S> WsCompat<S> {
    fn new(ws: WebSocketStream<S>) -> Self {
        Self { inner: ws, read_buf: Vec::new(), read_pos: 0 }
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
        let msg = Message::Binary(buf.to_vec().into());
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
    let (mut ctrl_ws, _) = tokio_tungstenite::connect_async(&ctrl_url)
        .await
        .map_err(|e| Error::Connection(format!("control WS: {e}")))?;

    sp.set_message("Authenticating…");

    // 2. Auth ——————————————————————————————————————————————————————————————
    let token = config.auth_token.clone().unwrap_or_default();
    send_frame(&mut ctrl_ws, &ControlFrame::Auth {
        token,
        client_version: env!("CARGO_PKG_VERSION").to_string(),
    }).await?;

    let session_id = match recv_frame_timeout(&mut ctrl_ws, AUTH_TIMEOUT).await? {
        ControlFrame::AuthOk { session_id, server_version } => {
            info!(%session_id, %server_version, "authenticated");
            session_id
        }
        ControlFrame::AuthError { message } => {
            sp.finish_and_clear();
            return Err(Error::Auth(message));
        }
        other => {
            sp.finish_and_clear();
            return Err(Error::Connection(format!("unexpected frame during auth: {other:?}")));
        }
    };

    sp.set_message("Registering tunnels…");

    // 3. Register tunnels —————————————————————————————————————————————————
    let mut registered: Vec<(TunnelDef, String)> = Vec::new();

    for tunnel in tunnels {
        let request_id = Uuid::new_v4().to_string();
        let protocol = proto_to_enum(&tunnel.proto)?;
        let local_addr = format!("{}:{}", tunnel.local_host, tunnel.local_port);

        send_frame(&mut ctrl_ws, &ControlFrame::RegisterTunnel {
            request_id: request_id.clone(),
            protocol,
            subdomain: tunnel.subdomain.clone(),
            local_addr,
        }).await?;

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
    // NOTE: The server must implement a `/_data/<session_id>` WebSocket
    // endpoint for this to succeed.  If it is unavailable, data proxying is
    // skipped but the control loop still runs.
    let mut data_conn: Option<DataConn> = connect_data_ws(&config.server, session_id).await;

    if data_conn.is_none() {
        warn!("data WebSocket unavailable — proxy connections will be skipped \
               until the server implements /_data/<session_id>");
    }

    // 5. Print startup display ————————————————————————————————————————————
    let display_tunnels: Vec<TunnelDisplay> = registered
        .iter()
        .map(|(t, url)| TunnelDisplay {
            name:       t.subdomain.clone().unwrap_or_else(|| "tunnel".into()),
            proto:      t.proto.clone(),
            local:      format!("{}:{}", t.local_host, t.local_port),
            public_url: url.clone(),
        })
        .collect();
    display::print_startup_box(&display_tunnels);

    // 6. Main event loop ——————————————————————————————————————————————————
    main_loop(&mut ctrl_ws, &mut data_conn, &registered).await
}

// ── main loop ─────────────────────────────────────────────────────────────────

async fn main_loop(
    ctrl_ws:    &mut CtrlWs,
    data_conn:  &mut Option<DataConn>,
    registered: &[(TunnelDef, String)],
) -> Result<()> {
    let mut ping_interval = tokio::time::interval(PING_INTERVAL);
    ping_interval.tick().await; // skip the immediate first tick

    let mut last_pong = tokio::time::Instant::now();
    let mut awaiting_pong = false;

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

                                let local_addr = find_local_addr(registered, &protocol);

                                if let Some(ref mut conn) = data_conn {
                                    // Open an outbound yamux stream for this connection.
                                    match poll_fn(|cx| conn.poll_new_outbound(cx)).await {
                                        Ok(stream) => {
                                            // Notify the server AFTER opening the stream so
                                            // `mux.next_inbound()` on the server side has a
                                            // stream waiting.
                                            if let Err(e) = send_frame(
                                                ctrl_ws,
                                                &ControlFrame::DataStreamOpen { conn_id },
                                            ).await {
                                                warn!(%conn_id, "send DataStreamOpen: {e}");
                                                continue;
                                            }

                                            if let Some(addr) = local_addr {
                                                tokio::spawn(proxy::proxy_connection(
                                                    stream, addr, conn_id,
                                                ));
                                            } else {
                                                warn!(%conn_id, ?protocol,
                                                    "no local address configured for this protocol");
                                            }
                                        }
                                        Err(e) => {
                                            warn!(%conn_id, "yamux open_stream failed: {e}");
                                        }
                                    }
                                } else {
                                    debug!(%conn_id, "data conn unavailable, skipping proxy");
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

async fn connect_data_ws(server: &str, session_id: Uuid) -> Option<DataConn> {
    let url = format!("wss://{}/_data/{}", server, session_id);
    match tokio_tungstenite::connect_async(&url).await {
        Ok((ws, _)) => {
            let compat = WsCompat::new(ws);
            let conn = Connection::new(compat, yamux::Config::default(), Mode::Client);
            info!(%session_id, "data WebSocket connected, yamux Mode::Client");
            Some(conn)
        }
        Err(e) => {
            debug!("data WebSocket not available ({e})");
            None
        }
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn proto_to_enum(proto: &str) -> Result<TunnelProtocol> {
    match proto.to_lowercase().as_str() {
        "http"  => Ok(TunnelProtocol::Http),
        "https" => Ok(TunnelProtocol::Https),
        "tcp"   => Ok(TunnelProtocol::Tcp),
        other   => Err(Error::Config(format!("unknown protocol: {other}"))),
    }
}

/// Find the local socket address for a registered tunnel matching `protocol`.
fn find_local_addr(
    registered: &[(TunnelDef, String)],
    protocol: &TunnelProtocol,
) -> Option<SocketAddr> {
    for (def, _) in registered {
        let matches = match protocol {
            TunnelProtocol::Http | TunnelProtocol::Https => {
                def.proto == "http" || def.proto == "https"
            }
            TunnelProtocol::Tcp => def.proto == "tcp",
        };
        if matches {
            let addr = format!("{}:{}", def.local_host, def.local_port);
            return addr.parse().ok();
        }
    }
    None
}

async fn send_frame(ws: &mut CtrlWs, frame: &ControlFrame) -> Result<()> {
    let bytes = encode_frame(frame);
    ws.send(Message::Binary(bytes.into()))
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
        Message::Binary(data) => {
            decode_frame(&data).map_err(Error::Protocol)
        }
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
