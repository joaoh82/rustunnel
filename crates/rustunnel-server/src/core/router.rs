use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::sync::{broadcast, mpsc, oneshot, Semaphore};
use uuid::Uuid;
use yamux::Stream as YamuxStream;

use rustunnel_protocol::TunnelProtocol;

use crate::error::{Error, Result};

use super::ip_limiter::IpRateLimiter;
use super::limiter::RateLimiter;
use super::tunnel::{ControlMessage, SessionInfo, TcpTunnelEvent, TunnelInfo};

/// Broadcast channel capacity for TCP tunnel lifecycle events.
const TCP_EVENT_CAPACITY: usize = 64;

// ── TunnelCore ────────────────────────────────────────────────────────────────

/// Central routing table for the server.
///
/// All public methods are designed to be called from many async tasks concurrently;
/// interior mutability is provided by `DashMap` and `parking_lot::Mutex`.
pub struct TunnelCore {
    /// subdomain → TunnelInfo  (HTTP / HTTPS tunnels)
    pub http_routes: DashMap<String, TunnelInfo>,
    /// port → TunnelInfo  (TCP tunnels)
    pub tcp_routes: DashMap<u16, TunnelInfo>,
    /// session_id → SessionInfo
    pub sessions: DashMap<Uuid, SessionInfo>,
    /// Pool of TCP ports not yet allocated; populated from the configured range.
    available_tcp_ports: Mutex<Vec<u16>>,
    /// Reverse index: tunnel_id → subdomain/port, used for O(1) removal.
    tunnel_index: DashMap<Uuid, TunnelKey>,
    /// Maximum tunnels allowed per session (enforced at registration time).
    max_tunnels_per_session: usize,
    /// Maximum concurrent proxied connections per tunnel (used to init semaphores).
    max_connections_per_tunnel: usize,
    /// Pending proxy connections: conn_id → oneshot sender that delivers the
    /// yamux data stream once the remote client opens it.
    pending_conns: DashMap<Uuid, oneshot::Sender<YamuxStream>>,
    /// Notifies the TCP edge layer whenever a TCP tunnel is added/removed.
    tcp_events: broadcast::Sender<TcpTunnelEvent>,
    /// Per-tunnel token-bucket rate limiter (keyed by tunnel_id).
    pub rate_limiter: Arc<RateLimiter>,
    /// Per-source-IP sliding-window rate limiter.
    pub ip_limiter: Arc<IpRateLimiter>,
}

/// Identifies where a tunnel lives in the routing tables.
#[derive(Debug, Clone)]
enum TunnelKey {
    Http(String),
    Tcp(u16),
}

impl TunnelCore {
    /// Create a new router pre-seeded with the TCP port range `[low, high]` (inclusive).
    pub fn new(
        tcp_port_range: [u16; 2],
        max_tunnels_per_session: usize,
        max_connections_per_tunnel: usize,
        ip_rate_limit_rps: u32,
    ) -> Self {
        let [low, high] = tcp_port_range;
        let ports: Vec<u16> = (low..=high).collect();
        let (tcp_events, _) = broadcast::channel(TCP_EVENT_CAPACITY);
        Self {
            http_routes: DashMap::new(),
            tcp_routes: DashMap::new(),
            sessions: DashMap::new(),
            available_tcp_ports: Mutex::new(ports),
            tunnel_index: DashMap::new(),
            max_tunnels_per_session,
            max_connections_per_tunnel,
            pending_conns: DashMap::new(),
            tcp_events,
            rate_limiter: Arc::new(RateLimiter::new()),
            ip_limiter: Arc::new(IpRateLimiter::new(ip_rate_limit_rps)),
        }
    }

    // ── pending connection registry ───────────────────────────────────────────

    /// Register a pending proxy connection.  Returns the receiver end that
    /// will be resolved with a yamux stream once the client opens one.
    pub fn register_pending_conn(&self, conn_id: Uuid) -> oneshot::Receiver<YamuxStream> {
        let (tx, rx) = oneshot::channel();
        self.pending_conns.insert(conn_id, tx);
        rx
    }

    /// Resolve a pending connection by delivering the yamux stream to the
    /// waiting edge task.  Returns `false` when `conn_id` is unknown.
    pub fn resolve_pending_conn(&self, conn_id: &Uuid, stream: YamuxStream) -> bool {
        if let Some((_, tx)) = self.pending_conns.remove(conn_id) {
            tx.send(stream).is_ok()
        } else {
            false
        }
    }

    /// Cancel a pending connection by removing its registration.
    /// The waiting edge task's `oneshot::Receiver` will get `Err(RecvError)`.
    pub fn cancel_pending_conn(&self, conn_id: &Uuid) {
        self.pending_conns.remove(conn_id);
    }

    /// Subscribe to TCP tunnel lifecycle events.
    pub fn subscribe_tcp_events(&self) -> broadcast::Receiver<TcpTunnelEvent> {
        self.tcp_events.subscribe()
    }

    // ── session management ────────────────────────────────────────────────────

    /// Register a new client session and return its generated `session_id`.
    pub fn register_session(
        &self,
        addr: SocketAddr,
        token_id: String,
        control_tx: mpsc::Sender<ControlMessage>,
    ) -> Uuid {
        let session_id = Uuid::new_v4();
        self.sessions
            .insert(session_id, SessionInfo::new(addr, token_id, control_tx));
        session_id
    }

    /// Remove a session **and** all tunnels it owns.
    pub fn remove_session(&self, session_id: &Uuid) {
        if let Some((_, session)) = self.sessions.remove(session_id) {
            for tunnel_id in &session.tunnels {
                self.remove_tunnel(tunnel_id);
            }
        }
    }

    // ── tunnel registration ───────────────────────────────────────────────────

    /// Register an HTTP tunnel for `session_id`.
    ///
    /// If `subdomain` is `None` an 8-character random hex label is generated.
    /// User-supplied subdomains are validated: alphanumeric + hyphens only,
    /// 3–63 characters, no leading or trailing hyphens.
    /// Returns `(tunnel_id, public_subdomain)`.
    pub fn register_http_tunnel(
        &self,
        session_id: &Uuid,
        subdomain: Option<String>,
        protocol: TunnelProtocol,
    ) -> Result<(Uuid, String)> {
        self.check_session_limit(session_id)?;

        let subdomain = match subdomain {
            Some(s) => {
                validate_subdomain(&s)?;
                s
            }
            None => random_subdomain(),
        };

        // Reject duplicate subdomain registrations.
        if self.http_routes.contains_key(&subdomain) {
            return Err(Error::Tunnel(format!(
                "subdomain '{subdomain}' is already in use"
            )));
        }

        let tunnel_id = Uuid::new_v4();
        let info = TunnelInfo {
            session_id: *session_id,
            tunnel_id,
            protocol,
            subdomain: Some(subdomain.clone()),
            assigned_port: None,
            created_at: std::time::Instant::now(),
            request_count: Arc::new(AtomicU64::new(0)),
            conn_semaphore: Arc::new(Semaphore::new(self.max_connections_per_tunnel)),
        };

        self.http_routes.insert(subdomain.clone(), info);
        self.tunnel_index
            .insert(tunnel_id, TunnelKey::Http(subdomain.clone()));
        self.add_tunnel_to_session(session_id, tunnel_id);

        Ok((tunnel_id, subdomain))
    }

    /// Register a TCP tunnel for `session_id`, allocating the next available port.
    /// Returns `(tunnel_id, port)`.
    pub fn register_tcp_tunnel(&self, session_id: &Uuid) -> Result<(Uuid, u16)> {
        self.check_session_limit(session_id)?;

        let port = self
            .available_tcp_ports
            .lock()
            .pop()
            .ok_or(Error::NoPortsAvailable)?;

        let tunnel_id = Uuid::new_v4();
        let info = TunnelInfo {
            session_id: *session_id,
            tunnel_id,
            protocol: TunnelProtocol::Tcp,
            subdomain: None,
            assigned_port: Some(port),
            created_at: std::time::Instant::now(),
            request_count: Arc::new(AtomicU64::new(0)),
            conn_semaphore: Arc::new(Semaphore::new(self.max_connections_per_tunnel)),
        };

        self.tcp_routes.insert(port, info);
        self.tunnel_index.insert(tunnel_id, TunnelKey::Tcp(port));
        self.add_tunnel_to_session(session_id, tunnel_id);
        let _ = self
            .tcp_events
            .send(TcpTunnelEvent::Registered { tunnel_id, port });

        Ok((tunnel_id, port))
    }

    /// Remove a tunnel by ID, returning any allocated TCP port to the pool.
    pub fn remove_tunnel(&self, tunnel_id: &Uuid) {
        let Some((_, key)) = self.tunnel_index.remove(tunnel_id) else {
            return;
        };
        match key {
            TunnelKey::Http(subdomain) => {
                self.http_routes.remove(&subdomain);
            }
            TunnelKey::Tcp(port) => {
                self.tcp_routes.remove(&port);
                self.available_tcp_ports.lock().push(port);
                let _ = self.tcp_events.send(TcpTunnelEvent::Unregistered { port });
            }
        }
    }

    // ── resolution (hot path) ─────────────────────────────────────────────────

    /// Look up the tunnel and its session's control channel by subdomain.
    pub fn resolve_http(
        &self,
        subdomain: &str,
    ) -> Option<(TunnelInfo, mpsc::Sender<ControlMessage>)> {
        let tunnel = self.http_routes.get(subdomain)?.clone();
        let tx = self.sessions.get(&tunnel.session_id)?.control_tx.clone();
        tunnel.request_count.fetch_add(1, Ordering::Relaxed);
        Some((tunnel, tx))
    }

    /// Look up the tunnel and its session's control channel by TCP port.
    pub fn resolve_tcp(&self, port: u16) -> Option<(TunnelInfo, mpsc::Sender<ControlMessage>)> {
        let tunnel = self.tcp_routes.get(&port)?.clone();
        let tx = self.sessions.get(&tunnel.session_id)?.control_tx.clone();
        tunnel.request_count.fetch_add(1, Ordering::Relaxed);
        Some((tunnel, tx))
    }

    // ── helpers ───────────────────────────────────────────────────────────────

    fn check_session_limit(&self, session_id: &Uuid) -> Result<()> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| Error::SessionNotFound(session_id.to_string()))?;

        if session.tunnels.len() >= self.max_tunnels_per_session {
            return Err(Error::LimitExceeded(format!(
                "session {} already has {} tunnels (max {})",
                session_id,
                session.tunnels.len(),
                self.max_tunnels_per_session
            )));
        }
        Ok(())
    }

    fn add_tunnel_to_session(&self, session_id: &Uuid, tunnel_id: Uuid) {
        if let Some(mut session) = self.sessions.get_mut(session_id) {
            session.tunnels.push(tunnel_id);
        }
    }
}

// ── utility ───────────────────────────────────────────────────────────────────

/// Validate a user-supplied subdomain label.
///
/// Rules:
/// * Length: 3–63 characters.
/// * Characters: ASCII alphanumeric or hyphens only.
/// * No leading or trailing hyphens.
fn validate_subdomain(s: &str) -> Result<()> {
    if !(3..=63).contains(&s.len()) {
        return Err(Error::Tunnel(format!(
            "subdomain '{s}' must be 3–63 characters long"
        )));
    }
    if s.starts_with('-') || s.ends_with('-') {
        return Err(Error::Tunnel(format!(
            "subdomain '{s}' must not start or end with a hyphen"
        )));
    }
    if !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(Error::Tunnel(format!(
            "subdomain '{s}' may only contain letters, digits, and hyphens"
        )));
    }
    Ok(())
}

/// Generate an 8-character lowercase hex subdomain.
fn random_subdomain() -> String {
    let id = Uuid::new_v4();
    // Take the first 4 bytes (8 hex chars) of the UUID.
    let bytes = id.as_bytes();
    format!(
        "{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3]
    )
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_core() -> TunnelCore {
        TunnelCore::new([20000, 20009], 5, 100, 1000)
    }

    fn dummy_session(core: &TunnelCore) -> (Uuid, mpsc::Receiver<ControlMessage>) {
        let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let (tx, rx) = mpsc::channel(16);
        let session_id = core.register_session(addr, "token-1".to_string(), tx);
        (session_id, rx)
    }

    // ── session ───────────────────────────────────────────────────────────────

    #[test]
    fn register_and_remove_session() {
        let core = make_core();
        let (session_id, _rx) = dummy_session(&core);

        assert!(core.sessions.contains_key(&session_id));

        core.remove_session(&session_id);
        assert!(!core.sessions.contains_key(&session_id));
    }

    #[test]
    fn remove_nonexistent_session_is_noop() {
        let core = make_core();
        core.remove_session(&Uuid::new_v4()); // must not panic
    }

    // ── HTTP tunnel ───────────────────────────────────────────────────────────

    #[test]
    fn register_http_tunnel_with_explicit_subdomain() {
        let core = make_core();
        let (session_id, _rx) = dummy_session(&core);

        let (tunnel_id, subdomain) = core
            .register_http_tunnel(&session_id, Some("myapp".to_string()), TunnelProtocol::Http)
            .unwrap();

        assert_eq!(subdomain, "myapp");
        assert!(core.http_routes.contains_key("myapp"));
        assert!(core
            .sessions
            .get(&session_id)
            .unwrap()
            .tunnels
            .contains(&tunnel_id));
    }

    #[test]
    fn register_http_tunnel_auto_subdomain() {
        let core = make_core();
        let (session_id, _rx) = dummy_session(&core);

        let (_, subdomain) = core
            .register_http_tunnel(&session_id, None, TunnelProtocol::Http)
            .unwrap();

        assert_eq!(subdomain.len(), 8);
        assert!(subdomain.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn duplicate_subdomain_is_rejected() {
        let core = make_core();
        let (session_id, _rx) = dummy_session(&core);

        core.register_http_tunnel(&session_id, Some("clash".to_string()), TunnelProtocol::Http)
            .unwrap();

        let result =
            core.register_http_tunnel(&session_id, Some("clash".to_string()), TunnelProtocol::Http);

        assert!(matches!(result, Err(Error::Tunnel(_))));
    }

    #[test]
    fn remove_http_tunnel() {
        let core = make_core();
        let (session_id, _rx) = dummy_session(&core);

        let (tunnel_id, _) = core
            .register_http_tunnel(&session_id, Some("gone".to_string()), TunnelProtocol::Http)
            .unwrap();

        core.remove_tunnel(&tunnel_id);

        assert!(!core.http_routes.contains_key("gone"));
        assert!(!core.tunnel_index.contains_key(&tunnel_id));
    }

    // ── TCP tunnel ────────────────────────────────────────────────────────────

    #[test]
    fn register_tcp_tunnel_allocates_port() {
        let core = make_core();
        let (session_id, _rx) = dummy_session(&core);

        let (tunnel_id, port) = core.register_tcp_tunnel(&session_id).unwrap();

        assert!((20000..=20009).contains(&port));
        assert!(core.tcp_routes.contains_key(&port));
        assert!(core
            .sessions
            .get(&session_id)
            .unwrap()
            .tunnels
            .contains(&tunnel_id));
    }

    #[test]
    fn remove_tcp_tunnel_returns_port_to_pool() {
        let core = TunnelCore::new([30000, 30000], 5, 100, 1000); // single-port range
        let (session_id, _rx) = dummy_session(&core);

        let (tunnel_id, port) = core.register_tcp_tunnel(&session_id).unwrap();
        assert_eq!(port, 30000);

        // Pool is now empty — next allocation must fail.
        let (session2_id, _rx2) = dummy_session(&core);
        assert!(matches!(
            core.register_tcp_tunnel(&session2_id),
            Err(Error::NoPortsAvailable)
        ));

        // Return the port.
        core.remove_tunnel(&tunnel_id);

        // Now allocation succeeds again.
        let (_id2, port2) = core.register_tcp_tunnel(&session2_id).unwrap();
        assert_eq!(port2, 30000);
    }

    #[test]
    fn no_ports_available_error() {
        let core = TunnelCore::new([40000, 40000], 10, 100, 1000);
        let (sid1, _rx1) = dummy_session(&core);
        let (sid2, _rx2) = dummy_session(&core);

        core.register_tcp_tunnel(&sid1).unwrap();

        assert!(matches!(
            core.register_tcp_tunnel(&sid2),
            Err(Error::NoPortsAvailable)
        ));
    }

    // ── resolution ────────────────────────────────────────────────────────────

    #[test]
    fn resolve_http_returns_tunnel_and_sender() {
        let core = make_core();
        let (session_id, _rx) = dummy_session(&core);

        core.register_http_tunnel(&session_id, Some("web".to_string()), TunnelProtocol::Http)
            .unwrap();

        let (info, _tx) = core.resolve_http("web").unwrap();
        assert_eq!(info.subdomain.as_deref(), Some("web"));
        // request_count was incremented by resolve_http
        assert_eq!(info.request_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn resolve_http_unknown_subdomain_returns_none() {
        let core = make_core();
        assert!(core.resolve_http("no-such").is_none());
    }

    #[test]
    fn resolve_tcp_returns_tunnel_and_sender() {
        let core = make_core();
        let (session_id, _rx) = dummy_session(&core);

        let (_, port) = core.register_tcp_tunnel(&session_id).unwrap();

        let (info, _tx) = core.resolve_tcp(port).unwrap();
        assert_eq!(info.assigned_port, Some(port));
        assert_eq!(info.request_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn resolve_tcp_unknown_port_returns_none() {
        let core = make_core();
        assert!(core.resolve_tcp(9999).is_none());
    }

    // ── session removal cascades to tunnels ───────────────────────────────────

    #[test]
    fn remove_session_cleans_up_tunnels() {
        let core = make_core();
        let (session_id, _rx) = dummy_session(&core);

        let (tid, _) = core
            .register_http_tunnel(&session_id, Some("bye".to_string()), TunnelProtocol::Http)
            .unwrap();
        let (_, port) = core.register_tcp_tunnel(&session_id).unwrap();

        core.remove_session(&session_id);

        assert!(!core.sessions.contains_key(&session_id));
        assert!(!core.tunnel_index.contains_key(&tid));
        assert!(!core.http_routes.contains_key("bye"));
        assert!(!core.tcp_routes.contains_key(&port));
    }

    // ── per-session tunnel limit ──────────────────────────────────────────────

    #[test]
    fn tunnel_limit_is_enforced() {
        let core = TunnelCore::new([50000, 50009], 2, 100, 1000);
        let (session_id, _rx) = dummy_session(&core);

        core.register_http_tunnel(&session_id, None, TunnelProtocol::Http)
            .unwrap();
        core.register_http_tunnel(&session_id, None, TunnelProtocol::Http)
            .unwrap();

        let result = core.register_http_tunnel(&session_id, None, TunnelProtocol::Http);
        assert!(matches!(result, Err(Error::LimitExceeded(_))));
    }

    #[test]
    fn session_not_found_error() {
        let core = make_core();
        let ghost = Uuid::new_v4();

        assert!(matches!(
            core.register_http_tunnel(&ghost, None, TunnelProtocol::Http),
            Err(Error::SessionNotFound(_))
        ));
    }

    // ── subdomain validation ──────────────────────────────────────────────────

    #[test]
    fn valid_subdomains_are_accepted() {
        let core = make_core();
        let (sid, _rx) = dummy_session(&core);
        for s in &["abc", "my-app", "foo123", "a-b-c", "aaa"] {
            let r = core.register_http_tunnel(&sid, Some(s.to_string()), TunnelProtocol::Http);
            assert!(r.is_ok(), "expected '{s}' to be valid, got {r:?}");
        }
    }

    #[test]
    fn subdomain_too_short_is_rejected() {
        let core = make_core();
        let (sid, _rx) = dummy_session(&core);
        assert!(matches!(
            core.register_http_tunnel(&sid, Some("ab".to_string()), TunnelProtocol::Http),
            Err(Error::Tunnel(_))
        ));
    }

    #[test]
    fn subdomain_leading_hyphen_is_rejected() {
        let core = make_core();
        let (sid, _rx) = dummy_session(&core);
        assert!(matches!(
            core.register_http_tunnel(&sid, Some("-bad".to_string()), TunnelProtocol::Http),
            Err(Error::Tunnel(_))
        ));
    }

    #[test]
    fn subdomain_trailing_hyphen_is_rejected() {
        let core = make_core();
        let (sid, _rx) = dummy_session(&core);
        assert!(matches!(
            core.register_http_tunnel(&sid, Some("bad-".to_string()), TunnelProtocol::Http),
            Err(Error::Tunnel(_))
        ));
    }

    #[test]
    fn subdomain_invalid_chars_are_rejected() {
        let core = make_core();
        let (sid, _rx) = dummy_session(&core);
        assert!(matches!(
            core.register_http_tunnel(&sid, Some("bad_name".to_string()), TunnelProtocol::Http),
            Err(Error::Tunnel(_))
        ));
        assert!(matches!(
            core.register_http_tunnel(&sid, Some("bad.name".to_string()), TunnelProtocol::Http),
            Err(Error::Tunnel(_))
        ));
    }
}
