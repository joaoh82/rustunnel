//! Per-client WebSocket session handler.
//!
//! Lifecycle
//! ---------
//! 1. Auth handshake (5 s timeout).
//! 2. Main select loop: frames from the WebSocket OR control messages from
//!    the router (NewConnection, Shutdown).
//! 3. Heartbeat: Ping every 30 s; drop session if Pong not received in 10 s.
//! 4. Cleanup: `core.remove_session` on any exit path.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::poll_fn;
use futures_util::io::AsyncWriteExt;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{interval, timeout, Instant};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use uuid::Uuid;

use rustunnel_protocol::{decode_frame, encode_frame, ControlFrame, TunnelProtocol};

use crate::audit::{AuditEvent, AuditTx};
use crate::config::ServerConfig;
use crate::control::mux::MuxSession;
use crate::core::{ControlMessage, TunnelCore};
use crate::error::{Error, Result};

// ── constants ─────────────────────────────────────────────────────────────────

const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const PING_INTERVAL: Duration = Duration::from_secs(30);
const PONG_DEADLINE: Duration = Duration::from_secs(10);
const CTRL_CHANNEL_SIZE: usize = 64;

// ── session context ───────────────────────────────────────────────────────────

/// Bundles the per-session immutable references that are needed throughout
/// `main_loop` and `handle_client_message` to keep function argument counts
/// within the lint limit.
struct SessionCtx<'a> {
    session_id: Uuid,
    core: &'a Arc<TunnelCore>,
    config: &'a Arc<ServerConfig>,
    audit_tx: &'a AuditTx,
}

// ── public entry point ────────────────────────────────────────────────────────

pub async fn handle_session<S>(
    ws: WebSocketStream<S>,
    peer_addr: SocketAddr,
    core: Arc<TunnelCore>,
    config: Arc<ServerConfig>,
    audit_tx: AuditTx,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    match run_session(ws, peer_addr, &core, &config, &audit_tx).await {
        Ok(()) => tracing::info!(%peer_addr, "session ended cleanly"),
        Err(e) => tracing::warn!(%peer_addr, "session error: {e}"),
    }
}

// ── session driver ────────────────────────────────────────────────────────────

async fn run_session<S>(
    ws: WebSocketStream<S>,
    peer_addr: SocketAddr,
    core: &Arc<TunnelCore>,
    config: &Arc<ServerConfig>,
    audit_tx: &AuditTx,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Create the control channel up-front so we only register_session once.
    let (ctrl_tx, mut ctrl_rx) = mpsc::channel::<ControlMessage>(CTRL_CHANNEL_SIZE);

    // Auth.
    let (mut ws, session_id) =
        auth_handshake(ws, peer_addr, core, config, ctrl_tx, audit_tx).await?;

    tracing::info!(%peer_addr, %session_id, "session authenticated");

    // Heartbeat channels.
    let (ping_out_tx, mut ping_out_rx) = mpsc::channel::<u64>(4);
    let (pong_in_tx, pong_in_rx) = mpsc::channel::<u64>(4);
    let (hb_stop_tx, hb_stop_rx) = oneshot::channel::<()>();

    tokio::spawn(heartbeat_task(
        ping_out_tx,
        pong_in_rx,
        hb_stop_rx,
        session_id,
    ));

    let mut mux = MuxSession::start_detached();

    // Store the loopback peer end so the data-plane bridge task can pick it
    // up when the client's /_data/<session_id> WebSocket arrives.
    if let Some(pipe) = mux.take_pipe_client() {
        core.set_data_pipe(&session_id, pipe);
    }

    // Extract the raw yamux Connection and hand it to a dedicated driver task.
    //
    // yamux 0.13 uses lazy SYNs and requires the Connection to be polled
    // continuously to flush outbound frames to the underlying IO (the duplex
    // pipe that is bridged to the real data WebSocket).  The driver task:
    //   • Accepts open-stream requests from the main loop via `open_rx`.
    //   • For each request: opens an outbound stream (Mode::Client), writes
    //     the 16-byte conn_id to force the SYN+DATA to be flushed, then hands
    //     the stream to the edge task via `core.resolve_pending_conn`.
    //   • Continuously polls `poll_next_inbound` (which also drives all
    //     outbound IO) between requests.
    let conn = mux.into_conn();
    let (open_tx, mut open_rx) = mpsc::channel::<uuid::Uuid>(16);
    let core_for_driver = Arc::clone(core);
    tokio::spawn(async move {
        let mut conn = conn;
        loop {
            tokio::select! {
                req = open_rx.recv() => {
                    let conn_id = match req { None => break, Some(id) => id };
                    match poll_fn(|cx| conn.poll_new_outbound(cx)).await {
                        Ok(mut stream) => {
                            // Writing the conn_id bytes immediately forces yamux
                            // to emit a SYN + DATA frame, which the driver task's
                            // next poll_next_inbound call will flush to the pipe.
                            if stream.write_all(conn_id.as_bytes()).await.is_err()
                                || stream.flush().await.is_err()
                            {
                                tracing::warn!(%conn_id, "failed to write/flush yamux stream");
                                continue;
                            }
                            if !core_for_driver.resolve_pending_conn(&conn_id, stream) {
                                tracing::warn!(%conn_id, "no edge task waiting for this conn_id");
                            }
                        }
                        Err(e) => tracing::warn!(%conn_id, "yamux open_stream: {e}"),
                    }
                }

                // poll_next_inbound drives ALL yamux IO (including flushing
                // outbound frames written above).  In Mode::Client the server
                // will never receive genuine inbound streams, so any that
                // arrive are simply discarded.
                result = poll_fn(|cx| conn.poll_next_inbound(cx)) => {
                    match result {
                        Some(Ok(_)) => tracing::debug!("unexpected inbound yamux stream — ignored"),
                        Some(Err(e)) => { tracing::debug!("yamux driver error: {e}"); break; }
                        None => { tracing::debug!("yamux connection closed"); break; }
                    }
                }
            }
        }
    });

    let ctx = SessionCtx {
        session_id,
        core,
        config,
        audit_tx,
    };
    let result = main_loop(
        &mut ws,
        &mut ctrl_rx,
        &mut ping_out_rx,
        pong_in_tx,
        &ctx,
        open_tx,
    )
    .await;

    let _ = hb_stop_tx.send(());
    core.remove_session(&session_id);
    tracing::debug!(%session_id, "session removed");

    result
}

// ── auth handshake ────────────────────────────────────────────────────────────

async fn auth_handshake<S>(
    mut ws: WebSocketStream<S>,
    peer_addr: SocketAddr,
    core: &Arc<TunnelCore>,
    config: &Arc<ServerConfig>,
    ctrl_tx: mpsc::Sender<ControlMessage>,
    audit_tx: &AuditTx,
) -> Result<(WebSocketStream<S>, Uuid)>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let raw = timeout(AUTH_TIMEOUT, ws.next())
        .await
        .map_err(|_| Error::Auth("auth timeout".into()))?
        .ok_or_else(|| Error::Auth("connection closed before auth".into()))?
        .map_err(|e| Error::Auth(e.to_string()))?;

    let frame = parse_binary(raw)?;

    let (token, _client_version) = match frame {
        ControlFrame::Auth {
            token,
            client_version,
        } => (token, client_version),
        other => {
            let _ = send_frame(
                &mut ws,
                &ControlFrame::AuthError {
                    message: "expected Auth frame".into(),
                },
            )
            .await;
            let _ = audit_tx.try_send(AuditEvent::AuthAttempt {
                peer: peer_addr.to_string(),
                success: false,
                token_id: None,
            });
            return Err(Error::Auth(format!("unexpected frame: {other:?}")));
        }
    };

    let authed = !config.auth.require_auth || token == config.auth.admin_token;
    if !authed {
        let _ = send_frame(
            &mut ws,
            &ControlFrame::AuthError {
                message: "invalid token".into(),
            },
        )
        .await;
        let _ = audit_tx.try_send(AuditEvent::AuthAttempt {
            peer: peer_addr.to_string(),
            success: false,
            token_id: None,
        });
        return Err(Error::Auth("invalid token".into()));
    }

    let token_id = token.clone();
    let session_id = core.register_session(peer_addr, token, ctrl_tx);

    let _ = audit_tx.try_send(AuditEvent::AuthAttempt {
        peer: peer_addr.to_string(),
        success: true,
        token_id: Some(token_id),
    });

    send_frame(
        &mut ws,
        &ControlFrame::AuthOk {
            session_id,
            server_version: env!("CARGO_PKG_VERSION").to_string(),
        },
    )
    .await?;

    Ok((ws, session_id))
}

// ── main loop ─────────────────────────────────────────────────────────────────

async fn main_loop<S>(
    ws: &mut WebSocketStream<S>,
    ctrl_rx: &mut mpsc::Receiver<ControlMessage>,
    ping_out_rx: &mut mpsc::Receiver<u64>,
    pong_in_tx: mpsc::Sender<u64>,
    ctx: &SessionCtx<'_>,
    open_tx: mpsc::Sender<uuid::Uuid>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let session_id = ctx.session_id;
    loop {
        tokio::select! {
            // Outbound Ping queued by the heartbeat task.
            ts = ping_out_rx.recv() => {
                match ts {
                    None => return Ok(()),
                    Some(timestamp) => {
                        send_frame(ws, &ControlFrame::Ping { timestamp }).await?;
                        tracing::trace!(%session_id, timestamp, "ping sent");
                    }
                }
            }

            // Inbound WebSocket frame.
            msg = ws.next() => {
                match msg {
                    None => {
                        tracing::debug!(%session_id, "peer closed WebSocket");
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        tracing::warn!(%session_id, "ws error: {e}");
                        return Err(Error::Io(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe, e.to_string())));
                    }
                    Some(Ok(msg)) => {
                        handle_client_message(msg, ws, ctx, &pong_in_tx).await?;
                    }
                }
            }

            // Control message from the router.
            ctrl = ctrl_rx.recv() => {
                match ctrl {
                    None | Some(ControlMessage::Shutdown) => {
                        tracing::info!(%session_id, "shutdown");
                        let _ = ws.close(None).await;
                        return Ok(());
                    }
                    Some(ControlMessage::NewConnection { conn_id, client_addr, protocol }) => {
                        tracing::debug!(%session_id, %conn_id, %client_addr, "NewConnection: opening yamux stream");
                        // Ask the yamux driver task to open an outbound stream,
                        // write the conn_id bytes (forcing SYN), and hand the
                        // stream to the waiting edge task.
                        let _ = open_tx.send(conn_id).await;
                        // Notify the client so it can correlate the arriving
                        // yamux stream with the local service to proxy.
                        send_frame(ws, &ControlFrame::NewConnection {
                            conn_id,
                            client_addr: client_addr.to_string(),
                            protocol,
                        }).await?;
                    }
                }
            }
        }
    }
}

// ── frame dispatch ────────────────────────────────────────────────────────────

async fn handle_client_message<S>(
    msg: Message,
    ws: &mut WebSocketStream<S>,
    ctx: &SessionCtx<'_>,
    pong_in_tx: &mpsc::Sender<u64>,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let SessionCtx {
        session_id,
        core,
        config,
        audit_tx,
    } = ctx;
    let session_id = *session_id;
    let frame = match parse_binary(msg) {
        Ok(f) => f,
        Err(_) => return Ok(()),
    };

    match frame {
        ControlFrame::RegisterTunnel {
            request_id,
            protocol,
            subdomain,
            local_addr: _,
        } => {
            tracing::debug!(%session_id, %request_id, ?protocol, "register tunnel");
            match &protocol {
                TunnelProtocol::Http | TunnelProtocol::Https => {
                    match core.register_http_tunnel(&session_id, subdomain, protocol.clone()) {
                        Ok((tunnel_id, sub)) => {
                            let scheme = if protocol == TunnelProtocol::Https {
                                "https"
                            } else {
                                "http"
                            };
                            let public_url = format!("{scheme}://{}.{}", sub, config.server.domain);
                            let _ = audit_tx.try_send(AuditEvent::TunnelRegistered {
                                session_id: session_id.to_string(),
                                tunnel_id: tunnel_id.to_string(),
                                protocol: format!("{protocol:?}").to_lowercase(),
                                label: sub.clone(),
                            });
                            send_frame(
                                ws,
                                &ControlFrame::TunnelRegistered {
                                    request_id,
                                    tunnel_id,
                                    public_url,
                                    assigned_port: None,
                                },
                            )
                            .await?;
                        }
                        Err(e) => {
                            send_frame(
                                ws,
                                &ControlFrame::TunnelError {
                                    request_id,
                                    message: e.to_string(),
                                },
                            )
                            .await?;
                        }
                    }
                }
                TunnelProtocol::Tcp => match core.register_tcp_tunnel(&session_id) {
                    Ok((tunnel_id, port)) => {
                        let public_url = format!("tcp://{}:{port}", config.server.domain);
                        let _ = audit_tx.try_send(AuditEvent::TunnelRegistered {
                            session_id: session_id.to_string(),
                            tunnel_id: tunnel_id.to_string(),
                            protocol: "tcp".into(),
                            label: port.to_string(),
                        });
                        send_frame(
                            ws,
                            &ControlFrame::TunnelRegistered {
                                request_id,
                                tunnel_id,
                                public_url,
                                assigned_port: Some(port),
                            },
                        )
                        .await?;
                    }
                    Err(e) => {
                        send_frame(
                            ws,
                            &ControlFrame::TunnelError {
                                request_id,
                                message: e.to_string(),
                            },
                        )
                        .await?;
                    }
                },
            }
        }

        ControlFrame::UnregisterTunnel { tunnel_id } => {
            tracing::debug!(%session_id, %tunnel_id, "unregister tunnel");
            let _ = audit_tx.try_send(AuditEvent::TunnelRemoved {
                tunnel_id: tunnel_id.to_string(),
                label: String::new(),
            });
            core.remove_tunnel(&tunnel_id);
        }

        ControlFrame::Ping { timestamp } => {
            send_frame(ws, &ControlFrame::Pong { timestamp }).await?;
        }

        ControlFrame::Pong { timestamp } => {
            tracing::trace!(%session_id, timestamp, "pong");
            let _ = pong_in_tx.try_send(timestamp);
        }

        other => {
            tracing::warn!(%session_id, ?other, "unexpected frame — ignored");
        }
    }
    Ok(())
}

// ── heartbeat task ────────────────────────────────────────────────────────────

async fn heartbeat_task(
    ping_out_tx: mpsc::Sender<u64>,
    mut pong_in_rx: mpsc::Receiver<u64>,
    mut stop: oneshot::Receiver<()>,
    session_id: Uuid,
) {
    let mut ticker = interval(PING_INTERVAL);
    ticker.tick().await; // skip immediate first tick

    let mut pending: Option<Instant> = None;

    loop {
        tokio::select! {
            _ = &mut stop => break,

            _ = ticker.tick() => {
                if let Some(sent_at) = pending {
                    if sent_at.elapsed() > PONG_DEADLINE {
                        tracing::warn!(%session_id, "heartbeat timeout");
                        break;
                    }
                }
                let ts = now_ms();
                if ping_out_tx.send(ts).await.is_err() {
                    break;
                }
                pending = Some(Instant::now());
            }

            pong = pong_in_rx.recv() => {
                match pong {
                    None => break,
                    Some(_) => {
                        pending = None;
                        tracing::trace!(%session_id, "heartbeat ok");
                    }
                }
            }
        }
    }
}

// ── data-plane bridge ─────────────────────────────────────────────────────────

/// Bridge the client's data WebSocket to the session's loopback pipe.
///
/// The yamux `Connection` inside `MuxSession` is backed by one end of an
/// in-process `tokio::io::duplex` pair.  This function takes the other
/// (client) end of that pair and bidirectionally copies bytes between it and
/// the real data WebSocket, making yamux frames from the remote client flow
/// transparently into the server-side yamux `Connection`.
pub async fn handle_data_connection<S>(
    ws: WebSocketStream<S>,
    session_id: uuid::Uuid,
    core: Arc<TunnelCore>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Retrieve the loopback pipe end that `run_session` stored after creating
    // the `MuxSession`.  A brief retry loop handles the unlikely race where
    // the data WebSocket arrives before `run_session` has called `set_data_pipe`.
    let mut pipe = None;
    for _ in 0..40 {
        pipe = core.take_data_pipe(&session_id);
        if pipe.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let Some(mut pipe) = pipe else {
        tracing::warn!(%session_id, "data connection arrived but no pipe found (session unknown?)");
        return;
    };

    tracing::info!(%session_id, "data WebSocket connected, bridging to yamux session");

    // Convert the WebSocket (message-oriented) to a byte-stream then to
    // tokio AsyncRead+AsyncWrite, then copy bidirectionally with the pipe.
    let mut ws_bytes = crate::control::mux::WsCompat::new(ws).compat();

    if let Err(e) = tokio::io::copy_bidirectional(&mut ws_bytes, &mut pipe).await {
        tracing::debug!(%session_id, "data bridge closed: {e}");
    }

    tracing::debug!(%session_id, "data WebSocket bridge ended");
}

// ── helpers ───────────────────────────────────────────────────────────────────

async fn send_frame<S>(ws: &mut WebSocketStream<S>, frame: &ControlFrame) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let bytes = encode_frame(frame);
    ws.send(Message::Binary(bytes)).await.map_err(|e| {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            e.to_string(),
        ))
    })
}

fn parse_binary(msg: Message) -> Result<ControlFrame> {
    match msg {
        Message::Binary(data) => decode_frame(&data).map_err(Error::Protocol),
        other => Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("expected binary frame, got {other:?}"),
        ))),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
