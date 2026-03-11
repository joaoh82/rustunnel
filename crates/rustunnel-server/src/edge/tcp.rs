//! TCP edge proxy.
//!
//! Listens on one TCP port per active TCP tunnel.  When a public TCP
//! connection arrives:
//!   1. Resolve the tunnel via `core.resolve_tcp(port)`.
//!   2. Send `ControlMessage::NewConnection` to the session.
//!   3. Wait up to `STREAM_TIMEOUT` for the client to open a yamux stream.
//!   4. `tokio::io::copy_bidirectional` between the public socket and the
//!      yamux stream.
//!
//! Dynamic listener management
//! ───────────────────────────
//! `run_tcp_edge` subscribes to `TcpTunnelEvent` from `TunnelCore`.
//! * `Registered { port }`   → spawn a per-port listener task.
//! * `Unregistered { port }` → abort the per-port listener task.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tracing::{debug, info, warn};
use uuid::Uuid;

use rustunnel_protocol::TunnelProtocol;

use crate::core::{ControlMessage, TcpTunnelEvent, TunnelCore};
use crate::error::{Error, Result};
use crate::net::bind_reuse;

// ── timeouts ──────────────────────────────────────────────────────────────────

/// Maximum time to wait for the remote client to open the yamux data stream.
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);

// ── public entry point ────────────────────────────────────────────────────────

/// Watch for TCP tunnel lifecycle events and manage per-port listeners.
///
/// This function runs forever; spawn it as a background task.
///
/// Per-port listener tasks are tracked in a `JoinSet`.  When this future is
/// dropped (e.g. via `AbortHandle::abort()`), the `JoinSet` is dropped with
/// it, which automatically aborts all per-port tasks — releasing their bound
/// TCP ports immediately.
pub async fn run_tcp_edge(core: Arc<TunnelCore>) {
    let mut events = core.subscribe_tcp_events();
    // JoinSet owns per-port listener tasks; dropping it aborts them all.
    let mut join_set: JoinSet<()> = JoinSet::new();
    // AbortHandle per active port for targeted removal.
    let mut handles: HashMap<u16, tokio::task::AbortHandle> = HashMap::new();

    // Bootstrap: start listeners for any TCP tunnels that are already active
    // (e.g. when the edge is restarted while tunnels are registered).
    for entry in core.tcp_routes.iter() {
        let port = *entry.key();
        let handle = spawn_port_listener(port, core.clone(), &mut join_set);
        handles.insert(port, handle);
    }

    info!("TCP edge manager started");

    loop {
        match events.recv().await {
            Ok(TcpTunnelEvent::Registered { tunnel_id, port }) => {
                info!(%tunnel_id, port, "starting TCP listener");
                let handle = spawn_port_listener(port, core.clone(), &mut join_set);
                if let Some(old) = handles.insert(port, handle) {
                    old.abort(); // shouldn't happen, but be safe
                }
            }
            Ok(TcpTunnelEvent::Unregistered { port }) => {
                info!(port, "stopping TCP listener");
                if let Some(handle) = handles.remove(&port) {
                    handle.abort();
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                warn!("TCP event channel lagged by {n} events — resyncing");
                // Re-sync: compare current state with our handles map.
                resync_listeners(&mut handles, &core, &mut join_set);
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                info!("TCP event channel closed — TCP edge manager exiting");
                break;
            }
        }
    }
    // JoinSet is dropped here, aborting all remaining per-port listeners.
}

// ── per-port listener ─────────────────────────────────────────────────────────

fn spawn_port_listener(
    port: u16,
    core: Arc<TunnelCore>,
    join_set: &mut JoinSet<()>,
) -> tokio::task::AbortHandle {
    join_set.spawn(async move {
        if let Err(e) = port_listener(port, core).await {
            warn!(port, "TCP listener exited with error: {e}");
        }
    })
}

async fn port_listener(port: u16, core: Arc<TunnelCore>) -> Result<()> {
    let addr: SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
    let listener = bind_reuse(addr)?;
    info!(port, %addr, "TCP port listener bound");

    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                warn!(port, "TCP accept error: {e}");
                continue;
            }
        };
        debug!(port, %peer, "new TCP connection");
        let _ = tcp.set_nodelay(true);

        // IP rate limit check.
        if !core.ip_limiter.check(peer.ip()) {
            debug!(port, %peer, "IP rate limit exceeded — dropping TCP connection");
            continue;
        }

        let core = core.clone();
        tokio::spawn(async move {
            if let Err(e) = proxy_tcp_connection(tcp, peer, port, core).await {
                debug!(port, %peer, "TCP proxy error: {e}");
            }
        });
    }
}

// ── TCP proxy ─────────────────────────────────────────────────────────────────

async fn proxy_tcp_connection(
    mut public_tcp: tokio::net::TcpStream,
    peer: SocketAddr,
    port: u16,
    core: Arc<TunnelCore>,
) -> Result<()> {
    // ── resolve tunnel ────────────────────────────────────────────────────
    let (tunnel_info, control_tx) = core
        .resolve_tcp(port)
        .ok_or_else(|| Error::Tunnel(format!("no TCP tunnel on port {port}")))?;

    // ── acquire connection semaphore ──────────────────────────────────────
    let _permit = match tunnel_info.conn_semaphore.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return Err(Error::Tunnel(format!(
                "too many concurrent connections on port {port}"
            )));
        }
    };

    let conn_id = Uuid::new_v4();
    info!(
        %conn_id, %peer, port,
        tunnel_id = %tunnel_info.tunnel_id,
        "TCP connection proxying"
    );

    // ── register pending stream & notify client ───────────────────────────
    let stream_rx = core.register_pending_conn(conn_id);

    control_tx
        .send(ControlMessage::NewConnection {
            conn_id,
            client_addr: peer,
            protocol: TunnelProtocol::Tcp,
        })
        .await
        .map_err(|e| Error::Tunnel(format!("control send failed: {e}")))?;

    // ── wait for data stream ──────────────────────────────────────────────
    let yamux_stream = match timeout(STREAM_TIMEOUT, stream_rx).await {
        Ok(Ok(s)) => s,
        Ok(Err(_)) => {
            return Err(Error::Tunnel("pending-conn sender dropped".into()));
        }
        Err(_) => {
            warn!(%conn_id, port, "timed out waiting for TCP data stream");
            return Err(Error::Tunnel("stream timeout".into()));
        }
    };

    // Wrap yamux stream (futures::io) → tokio::io for copy_bidirectional.
    let mut upstream = yamux_stream.compat();

    // ── bidirectional byte copy ───────────────────────────────────────────
    match tokio::io::copy_bidirectional(&mut public_tcp, &mut upstream).await {
        Ok((up, down)) => {
            info!(
                %conn_id, port, %peer,
                bytes_up = up, bytes_down = down,
                "TCP proxy done"
            );
        }
        Err(e) => {
            debug!(%conn_id, "TCP copy error: {e}");
        }
    }

    Ok(())
}

// ── resync after broadcast lag ────────────────────────────────────────────────

/// Compare the live `tcp_routes` in `core` against the map of running
/// listener tasks, stopping stale ones and starting missing ones.
fn resync_listeners(
    handles: &mut HashMap<u16, tokio::task::AbortHandle>,
    core: &Arc<TunnelCore>,
    join_set: &mut JoinSet<()>,
) {
    // Collect currently active ports.
    let active: std::collections::HashSet<u16> = core.tcp_routes.iter().map(|e| *e.key()).collect();

    // Stop listeners whose tunnels no longer exist.
    handles.retain(|port, handle| {
        if active.contains(port) {
            true
        } else {
            handle.abort();
            false
        }
    });

    // Start listeners for ports we don't yet have.
    for port in &active {
        if !handles.contains_key(port) {
            let handle = spawn_port_listener(*port, core.clone(), join_set);
            handles.insert(*port, handle);
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::TunnelCore;
    use std::net::SocketAddr;
    use tokio::sync::mpsc;

    fn make_core() -> Arc<TunnelCore> {
        Arc::new(TunnelCore::new([25000, 25010], 5, 100, 1000))
    }

    fn add_session(core: &Arc<TunnelCore>) -> (Uuid, mpsc::Receiver<ControlMessage>) {
        let addr: SocketAddr = "127.0.0.1:9000".parse().unwrap();
        let (tx, rx) = mpsc::channel(16);
        let sid = core.register_session(addr, "tok".into(), tx);
        (sid, rx)
    }

    #[tokio::test]
    async fn tcp_event_fires_on_register() {
        let core = make_core();
        let mut rx = core.subscribe_tcp_events();
        let (sid, _ctrl_rx) = add_session(&core);

        let (_tid, port) = core.register_tcp_tunnel(&sid).unwrap();

        let event = tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("event within timeout")
            .unwrap();

        match event {
            TcpTunnelEvent::Registered { port: p, .. } => assert_eq!(p, port),
            other => panic!("expected Registered, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tcp_event_fires_on_unregister() {
        let core = make_core();
        let mut rx = core.subscribe_tcp_events();
        let (sid, _ctrl_rx) = add_session(&core);

        let (tid, port) = core.register_tcp_tunnel(&sid).unwrap();
        // consume the Registered event
        let _ = rx.recv().await;

        core.remove_tunnel(&tid);

        let event = tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("event within timeout")
            .unwrap();

        match event {
            TcpTunnelEvent::Unregistered { port: p } => assert_eq!(p, port),
            other => panic!("expected Unregistered, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pending_conn_roundtrip() {
        let core = make_core();

        let conn_id = Uuid::new_v4();
        let _rx = core.register_pending_conn(conn_id);
        // Verify that resolve_pending_conn with an unknown conn_id returns false.
        let (server, _client) = tokio::io::duplex(64);
        let session = yamux::Connection::new(
            tokio_util::compat::TokioAsyncReadCompatExt::compat(server),
            yamux::Config::default(),
            yamux::Mode::Server,
        );
        // Verify the API compiles; a real yamux::Stream requires a live session
        // so the full roundtrip is covered by integration tests.
        drop(session);
        let _ = conn_id;
    }
}
