//! UDP edge proxy.
//!
//! Listens on one UDP port per active UDP tunnel.  Incoming datagrams are
//! keyed by remote address to create logical "sessions".  For each new
//! session the edge sends `ControlMessage::NewConnection` to the client,
//! waits for a yamux stream, then bridges datagrams bidirectionally using
//! a 4-byte length-prefixed framing over the yamux byte stream.
//!
//! Dynamic listener management
//! ───────────────────────────
//! `run_udp_edge` subscribes to `UdpTunnelEvent` from `TunnelCore`.
//! * `Registered { port }`   → spawn a per-port listener task.
//! * `Unregistered { port }` → abort the per-port listener task.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tracing::{debug, info, warn};
use uuid::Uuid;

use rustunnel_protocol::TunnelProtocol;

use crate::core::{ControlMessage, TunnelCore, UdpTunnelEvent};

// ── constants ────────────────────────────────────────────────────────────────

/// Maximum time to wait for the remote client to open the yamux data stream.
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);

/// UDP session idle timeout — sessions with no traffic are cleaned up.
const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum number of datagrams buffered while waiting for the client
/// to open the yamux stream for a new session.
const MAX_BUFFERED_PACKETS: usize = 64;

/// Maximum UDP datagram size we accept.
const MAX_DATAGRAM_SIZE: usize = 65535;

/// How often the session reaper runs.
const REAPER_INTERVAL: Duration = Duration::from_secs(10);

// ── per-session state ────────────────────────────────────────────────────────

struct UdpSession {
    /// Sender to forward datagrams into the session's proxy task.
    tx: mpsc::Sender<Bytes>,
    last_activity: Arc<std::sync::atomic::AtomicU64>,
}

impl UdpSession {
    fn touch(&self) {
        let now = Instant::now().elapsed().as_secs(); // monotonic
                                                      // We store a relative "seconds since some epoch" — only ordering matters.
        self.last_activity.store(now, Ordering::Relaxed);
    }
}

// ── public entry point ───────────────────────────────────────────────────────

/// Watch for UDP tunnel lifecycle events and manage per-port listeners.
///
/// This function runs forever; spawn it as a background task.
pub async fn run_udp_edge(core: Arc<TunnelCore>) {
    let mut events = core.subscribe_udp_events();
    let mut join_set: JoinSet<()> = JoinSet::new();
    let mut handles: HashMap<u16, tokio::task::AbortHandle> = HashMap::new();

    // Bootstrap: start listeners for any UDP tunnels already active.
    for entry in core.udp_routes.iter() {
        let port = *entry.key();
        let handle = spawn_port_listener(port, core.clone(), &mut join_set);
        handles.insert(port, handle);
    }

    info!("UDP edge manager started");

    loop {
        match events.recv().await {
            Ok(UdpTunnelEvent::Registered { tunnel_id, port }) => {
                info!(%tunnel_id, port, "starting UDP listener");
                let handle = spawn_port_listener(port, core.clone(), &mut join_set);
                if let Some(old) = handles.insert(port, handle) {
                    old.abort();
                }
            }
            Ok(UdpTunnelEvent::Unregistered { port }) => {
                info!(port, "stopping UDP listener");
                if let Some(handle) = handles.remove(&port) {
                    handle.abort();
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                warn!("UDP event channel lagged by {n} events — resyncing");
                resync_listeners(&mut handles, &core, &mut join_set);
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                info!("UDP event channel closed — UDP edge manager exiting");
                break;
            }
        }
    }
}

// ── per-port listener ────────────────────────────────────────────────────────

fn spawn_port_listener(
    port: u16,
    core: Arc<TunnelCore>,
    join_set: &mut JoinSet<()>,
) -> tokio::task::AbortHandle {
    join_set.spawn(async move {
        if let Err(e) = port_listener(port, core).await {
            warn!(port, "UDP listener exited with error: {e}");
        }
    })
}

async fn port_listener(port: u16, core: Arc<TunnelCore>) -> crate::error::Result<()> {
    let addr: SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
    let socket = Arc::new(
        UdpSocket::bind(addr)
            .await
            .map_err(crate::error::Error::Io)?,
    );
    info!(port, %addr, "UDP port listener bound");

    // Active sessions keyed by remote address.
    let sessions: Arc<DashMap<SocketAddr, UdpSession>> = Arc::new(DashMap::new());

    // Spawn a background reaper for idle sessions.
    let sessions_reaper = sessions.clone();
    let _reaper = tokio::spawn(async move {
        let mut interval = tokio::time::interval(REAPER_INTERVAL);
        loop {
            interval.tick().await;
            sessions_reaper.retain(|_addr, session| {
                // Check if the sender is closed (proxy task exited).
                !session.tx.is_closed()
            });
        }
    });

    let mut buf = vec![0u8; MAX_DATAGRAM_SIZE];
    loop {
        let (len, peer) = match socket.recv_from(&mut buf).await {
            Ok(pair) => pair,
            Err(e) => {
                warn!(port, "UDP recv error: {e}");
                continue;
            }
        };

        // IP rate limit.
        if !core.ip_limiter.check(peer.ip()) {
            debug!(port, %peer, "IP rate limit exceeded — dropping UDP datagram");
            continue;
        }

        let data = Bytes::copy_from_slice(&buf[..len]);

        // Existing session? Forward the datagram.
        if let Some(session) = sessions.get(&peer) {
            session.touch();
            // Non-blocking send — drop if the proxy task is behind.
            let _ = session.tx.try_send(data);
            continue;
        }

        // New session — resolve tunnel, create session, spawn proxy task.
        let (tunnel_info, control_tx) = match core.resolve_udp(port) {
            Some(pair) => pair,
            None => {
                debug!(port, %peer, "no UDP tunnel on port — dropping");
                continue;
            }
        };

        // Acquire connection semaphore.
        let permit = match tunnel_info.conn_semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                debug!(port, %peer, "too many concurrent UDP sessions — dropping");
                continue;
            }
        };

        let conn_id = Uuid::new_v4();
        info!(
            %conn_id, %peer, port,
            tunnel_id = %tunnel_info.tunnel_id,
            "new UDP session"
        );

        // Create a channel for forwarding datagrams to the proxy task.
        let (tx, rx) = mpsc::channel::<Bytes>(MAX_BUFFERED_PACKETS);

        let last_activity = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let session = UdpSession {
            tx: tx.clone(),
            last_activity: last_activity.clone(),
        };
        sessions.insert(peer, session);

        // Send the first datagram into the session channel.
        let _ = tx.try_send(data);

        // Spawn the proxy task.
        let core_clone = core.clone();
        let socket_clone = socket.clone();
        let sessions_clone = sessions.clone();
        tokio::spawn(async move {
            let _permit = permit; // held until this task exits
            proxy_udp_session(
                conn_id,
                peer,
                port,
                rx,
                socket_clone,
                tunnel_info,
                control_tx,
                core_clone,
            )
            .await;
            // Clean up session entry.
            sessions_clone.remove(&peer);
            debug!(%conn_id, %peer, "UDP session ended");
        });
    }
}

// ── UDP session proxy ────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn proxy_udp_session(
    conn_id: Uuid,
    peer: SocketAddr,
    port: u16,
    mut datagram_rx: mpsc::Receiver<Bytes>,
    public_socket: Arc<UdpSocket>,
    tunnel_info: crate::core::TunnelInfo,
    control_tx: mpsc::Sender<ControlMessage>,
    core: Arc<TunnelCore>,
) {
    // Register pending stream & notify client.
    let stream_rx = core.register_pending_conn(conn_id);

    if control_tx
        .send(ControlMessage::NewConnection {
            conn_id,
            client_addr: peer,
            protocol: TunnelProtocol::Udp,
        })
        .await
        .is_err()
    {
        core.cancel_pending_conn(&conn_id);
        return;
    }

    // Wait for the client to open the yamux stream.
    let yamux_stream = match timeout(STREAM_TIMEOUT, stream_rx).await {
        Ok(Ok(s)) => s,
        Ok(Err(_)) => {
            warn!(%conn_id, port, "pending-conn sender dropped for UDP");
            return;
        }
        Err(_) => {
            warn!(%conn_id, port, "timed out waiting for UDP data stream");
            core.cancel_pending_conn(&conn_id);
            return;
        }
    };

    let mut upstream = yamux_stream.compat();
    let mut bytes_in: u64 = 0;
    let mut bytes_out: u64 = 0;

    // Bidirectional bridge: datagrams ↔ framed yamux stream.
    // Framing: [4-byte big-endian length][payload]
    let mut read_buf = BytesMut::with_capacity(MAX_DATAGRAM_SIZE + 4);

    loop {
        tokio::select! {
            // Inbound datagram from the public socket (via channel).
            datagram = datagram_rx.recv() => {
                let Some(data) = datagram else { break };
                let len = data.len() as u32;
                // Write length prefix + payload to yamux stream, then flush
                // to ensure the data is actually sent (yamux buffers internally).
                if upstream.write_all(&len.to_be_bytes()).await.is_err() {
                    break;
                }
                if upstream.write_all(&data).await.is_err() {
                    break;
                }
                if upstream.flush().await.is_err() {
                    break;
                }
                bytes_in += data.len() as u64;
            }

            // Outbound datagram from the client via yamux stream.
            result = read_framed_datagram(&mut upstream, &mut read_buf) => {
                match result {
                    Ok(data) => {
                        if public_socket.send_to(&data, peer).await.is_err() {
                            break;
                        }
                        bytes_out += data.len() as u64;
                    }
                    Err(_) => break,
                }
            }

            // Session idle timeout.
            _ = tokio::time::sleep(SESSION_IDLE_TIMEOUT) => {
                debug!(%conn_id, %peer, "UDP session idle timeout");
                break;
            }
        }
    }

    tunnel_info.add_transfer(bytes_in, bytes_out);
    info!(
        %conn_id, port, %peer,
        bytes_in, bytes_out,
        "UDP session proxy done"
    );
}

/// Read a single length-prefixed datagram from a yamux stream.
async fn read_framed_datagram<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    _buf: &mut BytesMut,
) -> Result<Bytes, std::io::Error> {
    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes).await?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len > MAX_DATAGRAM_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "datagram too large",
        ));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    Ok(Bytes::from(payload))
}

// ── resync after broadcast lag ───────────────────────────────────────────────

fn resync_listeners(
    handles: &mut HashMap<u16, tokio::task::AbortHandle>,
    core: &Arc<TunnelCore>,
    join_set: &mut JoinSet<()>,
) {
    let active: std::collections::HashSet<u16> = core.udp_routes.iter().map(|e| *e.key()).collect();

    handles.retain(|port, handle| {
        if active.contains(port) {
            true
        } else {
            handle.abort();
            false
        }
    });

    for port in &active {
        if !handles.contains_key(port) {
            let handle = spawn_port_listener(*port, core.clone(), join_set);
            handles.insert(*port, handle);
        }
    }
}
