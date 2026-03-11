use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::RwLock;
use tokio::sync::{mpsc, Semaphore};
use uuid::Uuid;

use rustunnel_protocol::TunnelProtocol;

// ── TCP tunnel events (edge ↔ core) ───────────────────────────────────────────

/// Broadcast by `TunnelCore` whenever a TCP tunnel is added or removed.
/// The TCP edge layer subscribes to this to manage per-port listeners.
#[derive(Debug, Clone)]
pub enum TcpTunnelEvent {
    Registered { tunnel_id: Uuid, port: u16 },
    Unregistered { port: u16 },
}

// ── control-plane message ─────────────────────────────────────────────────────

/// Messages the router sends down a session's control channel.
#[derive(Debug)]
pub enum ControlMessage {
    /// A new public connection has arrived and must be proxied.
    NewConnection {
        conn_id: Uuid,
        client_addr: SocketAddr,
        protocol: TunnelProtocol,
    },
    /// Instruct the session handler to tear down cleanly.
    Shutdown,
}

// ── per-tunnel state ──────────────────────────────────────────────────────────

/// Lightweight, clone-able view of a registered tunnel.
#[derive(Debug, Clone)]
pub struct TunnelInfo {
    pub session_id: Uuid,
    pub tunnel_id: Uuid,
    pub protocol: TunnelProtocol,
    /// Present for HTTP/HTTPS tunnels; `None` for TCP.
    pub subdomain: Option<String>,
    /// Present for TCP tunnels; `None` for HTTP/HTTPS.
    pub assigned_port: Option<u16>,
    pub created_at: Instant,
    /// Monotonically-increasing counter of proxied requests/connections.
    pub request_count: Arc<AtomicU64>,
    /// Limits concurrent proxied connections for this tunnel.
    /// Shared across all clones so every proxy task draws from the same pool.
    pub conn_semaphore: Arc<Semaphore>,
}

// ── per-session state ─────────────────────────────────────────────────────────

/// Live state for a connected client session.
pub struct SessionInfo {
    /// Remote address the client connected from.
    pub client_addr: SocketAddr,
    /// Opaque identifier of the auth token used (empty string when auth is disabled).
    pub auth_token_id: String,
    /// Channel for sending control messages to the session handler task.
    pub control_tx: mpsc::Sender<ControlMessage>,
    /// Tunnel IDs owned by this session.
    pub tunnels: Vec<Uuid>,
    pub connected_at: Instant,
    /// Updated on every Ping/Pong exchange.
    pub last_heartbeat: RwLock<Instant>,
}

impl SessionInfo {
    pub fn new(
        client_addr: SocketAddr,
        auth_token_id: String,
        control_tx: mpsc::Sender<ControlMessage>,
    ) -> Self {
        let now = Instant::now();
        Self {
            client_addr,
            auth_token_id,
            control_tx,
            tunnels: Vec::new(),
            connected_at: now,
            last_heartbeat: RwLock::new(now),
        }
    }
}
