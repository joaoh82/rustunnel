use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::Mutex;
use rand::seq::IteratorRandom;
use tokio::io::DuplexStream;
use tokio::sync::{broadcast, mpsc, oneshot};
use uuid::Uuid;
use yamux::Stream as YamuxStream;

use rustunnel_protocol::TunnelProtocol;

use crate::error::{Error, Result};

use super::ip_limiter::IpRateLimiter;
use super::limiter::RateLimiter;
use super::tunnel::{
    ControlMessage, GroupAlertPayload, GroupMember, GroupSpec, SessionInfo, TcpTunnelEvent,
    TunnelGroup, TunnelInfo, UdpTunnelEvent, ZeroHealthyAlert,
};
use crate::auth::AuthFailureLimiter;

/// Broadcast channel capacity for TCP/UDP tunnel lifecycle events.
const TCP_EVENT_CAPACITY: usize = 64;
const UDP_EVENT_CAPACITY: usize = 64;
/// Capacity of the load-balancing group-event broadcast channel. Sized
/// for bursty flapping; lagged subscribers fall back to the polling
/// `/api/groups` endpoint to resync.
const GROUP_EVENT_CAPACITY: usize = 256;

// ── TunnelCore ────────────────────────────────────────────────────────────────

/// Central routing table for the server.
///
/// All public methods are designed to be called from many async tasks concurrently;
/// interior mutability is provided by `DashMap` and `parking_lot::Mutex`.
pub struct TunnelCore {
    /// subdomain → group of HTTP/HTTPS members.
    ///
    /// In Phase 1 every group has exactly one member (degenerate case);
    /// Phase 2 of TUNNEL-7 lifts the cap so multiple clients can register the
    /// same subdomain with a shared `group_key` and share traffic.
    pub http_routes: DashMap<String, Arc<TunnelGroup>>,
    /// port → group of TCP members. Same shape as `http_routes` — Phase 3
    /// allows multiple clients on the same port via groups.
    pub tcp_routes: DashMap<u16, Arc<TunnelGroup>>,
    /// port → group of UDP members. UDP grouping isn't on the roadmap yet
    /// (see plan §6); the `Arc<TunnelGroup>` wrapper keeps the dispatch path
    /// uniform with HTTP/TCP.
    pub udp_routes: DashMap<u16, Arc<TunnelGroup>>,
    /// `(group_name, key_hash) → port` lookup for TCP groups (TUNNEL-7 Phase 3).
    ///
    /// HTTP groups dispatch off the user-supplied subdomain, so the routing
    /// key is in the registration request. TCP ports are server-allocated, so
    /// the second member of `group="ssh"` doesn't know which port to ask for
    /// — this index lets the server resolve `(group, key_hash)` to the port
    /// the first registration claimed. The entry is only present while the
    /// group has at least one member; `remove_tunnel` cleans it up alongside
    /// the route entry.
    tcp_group_index: DashMap<(String, String), u16>,
    /// name → P2pPublisher  (P2P tunnels registered by publishers)
    pub p2p_tunnels: DashMap<String, P2pPublisher>,
    /// session_id → SessionInfo
    pub sessions: DashMap<Uuid, SessionInfo>,
    /// Pool of TCP ports not yet allocated; populated from the configured range.
    available_tcp_ports: Mutex<Vec<u16>>,
    /// Pool of UDP ports not yet allocated; populated from the configured range.
    available_udp_ports: Mutex<Vec<u16>>,
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
    /// Notifies the UDP edge layer whenever a UDP tunnel is added/removed.
    udp_events: broadcast::Sender<UdpTunnelEvent>,
    /// Per-tunnel token-bucket rate limiter (keyed by tunnel_id).
    pub rate_limiter: Arc<RateLimiter>,
    /// Per-source-IP sliding-window rate limiter.
    pub ip_limiter: Arc<IpRateLimiter>,
    /// Per-source-IP throttle on failed authentication attempts (control-plane
    /// handshake and dashboard API). See [`crate::auth::AuthFailureLimiter`].
    pub auth_limiter: Arc<AuthFailureLimiter>,
    /// Broadcast channel for `GroupEvent`s emitted on every member health
    /// transition. Drives the SSE endpoint
    /// `GET /api/groups/:label/events`. A lagged subscriber gets a
    /// `Lagged(n)` error and is expected to resync via `/api/groups`.
    /// Capacity 256 keeps memory bounded under flapping while still
    /// absorbing reasonable bursts.
    group_events: broadcast::Sender<crate::core::tunnel::GroupEvent>,
}

/// A registered P2P publisher — subscribers connect to this by name.
#[derive(Debug, Clone)]
pub struct P2pPublisher {
    pub tunnel_info: TunnelInfo,
    pub secret_hash: String,
    pub name: String,
    /// NAT type reported by the publisher client (set via P2pNatInfo).
    pub nat_type: Option<String>,
    /// Public mapped addresses from STUN probing.
    pub mapped_addrs: Vec<String>,
}

/// Classify a NAT pair and return the hole-punching strategy.
///
/// Returns `("strategy_name", should_attempt_direct)`.
pub fn classify_nat_pair(pub_nat: Option<&str>, sub_nat: Option<&str>) -> (&'static str, bool) {
    match (pub_nat, sub_nat) {
        // Both cone or open — direct exchange, high success rate.
        (Some("open" | "cone"), Some("open" | "cone")) => ("direct_exchange", true),
        // One cone + one symmetric — port prediction, moderate success.
        (Some("cone" | "open"), Some("symmetric")) | (Some("symmetric"), Some("cone" | "open")) => {
            ("port_prediction", true)
        }
        // Both symmetric — skip, use relay.
        (Some("symmetric"), Some("symmetric")) => ("relay", false),
        // Unknown or missing — skip, use relay.
        _ => ("relay", false),
    }
}

/// Identifies where a tunnel lives in the routing tables.
#[derive(Debug, Clone)]
enum TunnelKey {
    Http(String),
    Tcp(u16),
    Udp(u16),
    P2p(String),
}

impl TunnelCore {
    /// Create a new router pre-seeded with TCP and UDP port ranges `[low, high]` (inclusive).
    pub fn new(
        tcp_port_range: [u16; 2],
        udp_port_range: [u16; 2],
        max_tunnels_per_session: usize,
        max_connections_per_tunnel: usize,
        ip_rate_limit_rps: u32,
        max_failed_auth_per_minute: u32,
    ) -> Self {
        let [tcp_low, tcp_high] = tcp_port_range;
        let tcp_ports: Vec<u16> = (tcp_low..=tcp_high).collect();
        let [udp_low, udp_high] = udp_port_range;
        let udp_ports: Vec<u16> = if udp_low == 0 && udp_high == 0 {
            Vec::new() // UDP disabled
        } else {
            (udp_low..=udp_high).collect()
        };
        let (tcp_events, _) = broadcast::channel(TCP_EVENT_CAPACITY);
        let (udp_events, _) = broadcast::channel(UDP_EVENT_CAPACITY);
        let (group_events, _) = broadcast::channel(GROUP_EVENT_CAPACITY);
        Self {
            http_routes: DashMap::new(),
            tcp_routes: DashMap::new(),
            udp_routes: DashMap::new(),
            tcp_group_index: DashMap::new(),
            p2p_tunnels: DashMap::new(),
            sessions: DashMap::new(),
            available_tcp_ports: Mutex::new(tcp_ports),
            available_udp_ports: Mutex::new(udp_ports),
            tunnel_index: DashMap::new(),
            max_tunnels_per_session,
            max_connections_per_tunnel,
            pending_conns: DashMap::new(),
            tcp_events,
            udp_events,
            rate_limiter: Arc::new(RateLimiter::new()),
            ip_limiter: Arc::new(IpRateLimiter::new(ip_rate_limit_rps)),
            auth_limiter: Arc::new(AuthFailureLimiter::new(max_failed_auth_per_minute)),
            group_events,
        }
    }

    /// Subscribe to the live load-balancing group event stream.
    ///
    /// Returns a `broadcast::Receiver<GroupEvent>` that receives every
    /// member health-bit transition across every group. Subscribers
    /// filter on `(protocol, label)` themselves. Lagged subscribers
    /// receive a `Lagged(n)` error and are expected to resync via
    /// `/api/groups`. See `dashboard/api.rs::group_events_sse` for the
    /// SSE endpoint that consumes this channel.
    pub fn subscribe_group_events(&self) -> broadcast::Receiver<crate::core::tunnel::GroupEvent> {
        self.group_events.subscribe()
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

    /// Subscribe to UDP tunnel lifecycle events.
    pub fn subscribe_udp_events(&self) -> broadcast::Receiver<UdpTunnelEvent> {
        self.udp_events.subscribe()
    }

    // ── data-plane pipe handoff ───────────────────────────────────────────────

    /// Store the loopback pipe client end in the session so the data-plane
    /// bridge task can retrieve it when the `/_data/<session_id>` WS arrives.
    pub fn set_data_pipe(&self, session_id: &Uuid, pipe: DuplexStream) {
        if let Some(mut s) = self.sessions.get_mut(session_id) {
            s.data_pipe = Some(pipe);
        }
    }

    /// Take the loopback pipe client end out of the session.
    /// Returns `None` if the session is unknown or the pipe was already taken.
    pub fn take_data_pipe(&self, session_id: &Uuid) -> Option<DuplexStream> {
        self.sessions
            .get_mut(session_id)
            .and_then(|mut s| s.data_pipe.take())
    }

    // ── session management ────────────────────────────────────────────────────

    /// Register a new client session and return its generated `session_id`.
    pub fn register_session(
        &self,
        addr: SocketAddr,
        token_id: String,
        db_token_id: Option<String>,
        user_id: Option<Uuid>,
        control_tx: mpsc::Sender<ControlMessage>,
    ) -> Uuid {
        let session_id = Uuid::new_v4();
        self.sessions.insert(
            session_id,
            SessionInfo::new(addr, token_id, db_token_id, user_id, control_tx),
        );
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
    ///
    /// When `group` is `None` (the only case before TUNNEL-7 Phase 2): the
    /// subdomain is owned by exactly one tunnel, mirroring historical
    /// behaviour. A duplicate subdomain registration is rejected.
    ///
    /// When `group` is `Some`: the subdomain becomes a load-balanced pool.
    /// The first registration with a given `(subdomain, key_hash)` creates
    /// the group; subsequent registrations join it iff their `key_hash`
    /// matches and their `protocol` (Http/Https) matches the existing
    /// members. A solo (no-group) registration on top of an existing group
    /// — or a grouped registration on top of an existing solo — is
    /// rejected. See plan §4.3.
    ///
    /// Returns `(tunnel_id, public_subdomain)`.
    pub fn register_http_tunnel(
        &self,
        session_id: &Uuid,
        subdomain: Option<String>,
        protocol: TunnelProtocol,
        group: Option<GroupSpec>,
    ) -> Result<(Uuid, String)> {
        self.check_session_limit(session_id)?;

        let subdomain = match subdomain {
            Some(s) => {
                validate_subdomain(&s)?;
                s
            }
            None => random_subdomain(),
        };

        let tunnel_id = Uuid::new_v4();
        let info = TunnelInfo::new(
            *session_id,
            tunnel_id,
            protocol.clone(),
            Some(subdomain.clone()),
            None,
            self.max_connections_per_tunnel,
        );

        // Atomic upsert via the entry API — closes the get-then-insert race
        // that two concurrent first-registrations could otherwise hit (plan
        // §7 risk #4). The shard lock is held for the duration of this
        // match block.
        use dashmap::mapref::entry::Entry;
        match self.http_routes.entry(subdomain.clone()) {
            Entry::Vacant(vac) => {
                let new_group = match group {
                    None => TunnelGroup::new_solo(subdomain.clone(), info),
                    Some(spec) => TunnelGroup::new_with_member(
                        spec.group_name,
                        Some(spec.key_hash),
                        GroupMember::with_health_spec(info, spec.health_check),
                    ),
                };
                vac.insert(new_group);
            }
            Entry::Occupied(occ) => {
                let existing = occ.get();
                let Some(spec) = group else {
                    // Solo registration on a subdomain that's already
                    // registered (solo or grouped) — historical behaviour.
                    return Err(Error::Tunnel(format!(
                        "subdomain '{subdomain}' is already in use"
                    )));
                };
                // Grouped registration: existing must already be a group
                // with a matching key.
                let existing_key = existing.key_hash.as_deref();
                if existing_key != Some(spec.key_hash.as_str()) {
                    return Err(Error::Tunnel(format!(
                        "group key does not match existing group for subdomain '{subdomain}'"
                    )));
                }
                // Identity check: protocol (Http vs Https) must match the
                // existing members. FRP enforces the same on
                // customDomains/subdomain/locations; protocol is the only
                // field where mismatch is meaningful for us.
                let existing_protocol = existing
                    .members
                    .iter()
                    .next()
                    .map(|m| m.info.protocol.clone());
                if existing_protocol.as_ref() != Some(&protocol) {
                    return Err(Error::Tunnel(format!(
                        "group member protocol mismatch for subdomain '{subdomain}': existing={:?}, new={:?}",
                        existing_protocol, protocol,
                    )));
                }
                existing.members.insert(
                    tunnel_id,
                    GroupMember::with_health_spec(info, spec.health_check),
                );
            }
        }

        self.tunnel_index
            .insert(tunnel_id, TunnelKey::Http(subdomain.clone()));
        self.add_tunnel_to_session(session_id, tunnel_id);

        Ok((tunnel_id, subdomain))
    }

    /// Register a TCP tunnel for `session_id`.
    ///
    /// `group = None` (the historical case): allocate a fresh port and create
    /// a solo group, mirroring the HTTP path.
    ///
    /// `group = Some` (TUNNEL-7 Phase 3): the first registration of a given
    /// `(group_name, key_hash)` allocates a port; subsequent registrations
    /// look up that port via `tcp_group_index` and reuse it. The TCP edge
    /// listener fires `TcpTunnelEvent::Registered` only on the first member
    /// — joins are no-ops at the listener layer (the port is already bound
    /// from the first member's event).
    ///
    /// Returns `(tunnel_id, port)`.
    pub fn register_tcp_tunnel(
        &self,
        session_id: &Uuid,
        group: Option<GroupSpec>,
    ) -> Result<(Uuid, u16)> {
        self.check_session_limit(session_id)?;

        let tunnel_id = Uuid::new_v4();
        let build_info = |port: u16| {
            TunnelInfo::new(
                *session_id,
                tunnel_id,
                TunnelProtocol::Tcp,
                None,
                Some(port),
                self.max_connections_per_tunnel,
            )
        };

        use dashmap::mapref::entry::Entry;
        match group {
            None => {
                // Solo path — historical behaviour.
                let port = self
                    .available_tcp_ports
                    .lock()
                    .pop()
                    .ok_or(Error::NoPortsAvailable)?;
                let info = build_info(port);
                let new_group = TunnelGroup::new_solo(port.to_string(), info);
                self.tcp_routes.insert(port, new_group);
                self.tunnel_index.insert(tunnel_id, TunnelKey::Tcp(port));
                self.add_tunnel_to_session(session_id, tunnel_id);
                let _ = self
                    .tcp_events
                    .send(TcpTunnelEvent::Registered { tunnel_id, port });
                Ok((tunnel_id, port))
            }
            Some(spec) => {
                let idx_key = (spec.group_name.clone(), spec.key_hash.clone());
                // The index entry guard serialises every "creation or join"
                // for this `(name, key_hash)` against every other one — and
                // against the matching `remove_tunnel` path, which acquires
                // the same entry to evict the index. Closes the
                // concurrent-first-registration race for TCP groups.
                match self.tcp_group_index.entry(idx_key) {
                    Entry::Vacant(idx_vac) => {
                        // First member of a brand-new TCP group.
                        let port = self
                            .available_tcp_ports
                            .lock()
                            .pop()
                            .ok_or(Error::NoPortsAvailable)?;
                        let info = build_info(port);
                        let new_group = TunnelGroup::new_with_member(
                            spec.group_name,
                            Some(spec.key_hash),
                            GroupMember::with_health_spec(info, spec.health_check),
                        );
                        self.tcp_routes.insert(port, new_group);
                        idx_vac.insert(port);
                        self.tunnel_index.insert(tunnel_id, TunnelKey::Tcp(port));
                        self.add_tunnel_to_session(session_id, tunnel_id);
                        let _ = self
                            .tcp_events
                            .send(TcpTunnelEvent::Registered { tunnel_id, port });
                        Ok((tunnel_id, port))
                    }
                    Entry::Occupied(idx_occ) => {
                        // Subsequent member: reuse the existing port. No new
                        // allocation, no `Registered` event (the listener is
                        // already up from the first member).
                        let port = *idx_occ.get();
                        let info = build_info(port);
                        // Sanity: the route must exist while the index says
                        // it does. If it doesn't, the index is stale — bail.
                        let Some(existing) = self.tcp_routes.get(&port) else {
                            return Err(Error::Tunnel(format!(
                                "TCP group index points to missing port {port}; please retry"
                            )));
                        };
                        existing.members.insert(
                            tunnel_id,
                            GroupMember::with_health_spec(info, spec.health_check),
                        );
                        drop(existing);
                        self.tunnel_index.insert(tunnel_id, TunnelKey::Tcp(port));
                        self.add_tunnel_to_session(session_id, tunnel_id);
                        Ok((tunnel_id, port))
                    }
                }
            }
        }
    }

    /// Register a UDP tunnel for `session_id`, allocating the next available port.
    /// Returns `(tunnel_id, port)`.
    pub fn register_udp_tunnel(&self, session_id: &Uuid) -> Result<(Uuid, u16)> {
        self.check_session_limit(session_id)?;

        let port = self
            .available_udp_ports
            .lock()
            .pop()
            .ok_or(Error::NoPortsAvailable)?;

        let tunnel_id = Uuid::new_v4();
        let info = TunnelInfo::new(
            *session_id,
            tunnel_id,
            TunnelProtocol::Udp,
            None,
            Some(port),
            self.max_connections_per_tunnel,
        );

        let group = TunnelGroup::new_solo(port.to_string(), info);
        self.udp_routes.insert(port, group);
        self.tunnel_index.insert(tunnel_id, TunnelKey::Udp(port));
        self.add_tunnel_to_session(session_id, tunnel_id);
        let _ = self
            .udp_events
            .send(UdpTunnelEvent::Registered { tunnel_id, port });

        Ok((tunnel_id, port))
    }

    /// Register a P2P publisher tunnel for `session_id`.
    /// Returns `(tunnel_id, name)`.
    pub fn register_p2p_tunnel(
        &self,
        session_id: &Uuid,
        name: String,
        secret_hash: String,
    ) -> Result<(Uuid, String)> {
        self.check_session_limit(session_id)?;

        if self.p2p_tunnels.contains_key(&name) {
            return Err(Error::Tunnel(format!(
                "P2P tunnel name '{name}' is already in use"
            )));
        }

        let tunnel_id = Uuid::new_v4();
        let info = TunnelInfo::new(
            *session_id,
            tunnel_id,
            TunnelProtocol::P2p,
            None,
            None,
            self.max_connections_per_tunnel,
        );

        let publisher = P2pPublisher {
            tunnel_info: info,
            secret_hash,
            name: name.clone(),
            nat_type: None,
            mapped_addrs: Vec::new(),
        };

        self.p2p_tunnels.insert(name.clone(), publisher);
        self.tunnel_index
            .insert(tunnel_id, TunnelKey::P2p(name.clone()));
        self.add_tunnel_to_session(session_id, tunnel_id);

        Ok((tunnel_id, name))
    }

    /// Look up a P2P publisher by name and return it with the session control channel.
    pub fn resolve_p2p(&self, name: &str) -> Option<(P2pPublisher, mpsc::Sender<ControlMessage>)> {
        let publisher = self.p2p_tunnels.get(name)?.clone();
        let tx = self
            .sessions
            .get(&publisher.tunnel_info.session_id)?
            .control_tx
            .clone();
        publisher
            .tunnel_info
            .request_count
            .fetch_add(1, Ordering::Relaxed);
        Some((publisher, tx))
    }

    /// Update the NAT info for a P2P publisher tunnel.
    pub fn update_p2p_nat_info(
        &self,
        tunnel_id: &Uuid,
        nat_type: String,
        mapped_addrs: Vec<String>,
    ) {
        if let Some(TunnelKey::P2p(name)) = self.tunnel_index.get(tunnel_id).as_deref().cloned() {
            if let Some(mut publisher) = self.p2p_tunnels.get_mut(&name) {
                publisher.nat_type = Some(nat_type);
                publisher.mapped_addrs = mapped_addrs;
            }
        }
    }

    /// Remove a tunnel by ID, returning any allocated TCP/UDP port to the pool.
    ///
    /// For grouped routes the member is removed from its `TunnelGroup`; the
    /// group itself (and the associated route entry, port allocation, and
    /// edge listener) only goes away when the *last* member leaves.
    /// Phase 1 has one-member-per-group, so every removal is the last one;
    /// this shape keeps Phase 2/3 from having to revisit the removal path.
    pub fn remove_tunnel(&self, tunnel_id: &Uuid) {
        let Some((_, key)) = self.tunnel_index.remove(tunnel_id) else {
            return;
        };
        match key {
            TunnelKey::Http(subdomain) => {
                let group_empty = self
                    .http_routes
                    .get(&subdomain)
                    .map(|g| {
                        g.members.remove(tunnel_id);
                        g.members.is_empty()
                    })
                    .unwrap_or(true);
                if group_empty {
                    self.http_routes.remove(&subdomain);
                }
            }
            TunnelKey::Tcp(port) => {
                // Snapshot the group's identity *before* mutating its members
                // — we need (name, key_hash) to find the index entry to evict
                // when the last member leaves. We always remove the member;
                // tcp_group_index cleanup only happens for grouped (key_hash
                // is Some) and only when the group is now empty.
                let (group_empty, idx_key) = self
                    .tcp_routes
                    .get(&port)
                    .map(|g| {
                        let idx = g.key_hash.as_ref().map(|kh| (g.name.clone(), kh.clone()));
                        g.members.remove(tunnel_id);
                        (g.members.is_empty(), idx)
                    })
                    .unwrap_or((true, None));
                if group_empty {
                    self.tcp_routes.remove(&port);
                    if let Some(k) = idx_key {
                        self.tcp_group_index.remove(&k);
                    }
                    self.available_tcp_ports.lock().push(port);
                    let _ = self.tcp_events.send(TcpTunnelEvent::Unregistered { port });
                }
            }
            TunnelKey::Udp(port) => {
                let group_empty = self
                    .udp_routes
                    .get(&port)
                    .map(|g| {
                        g.members.remove(tunnel_id);
                        g.members.is_empty()
                    })
                    .unwrap_or(true);
                if group_empty {
                    self.udp_routes.remove(&port);
                    self.available_udp_ports.lock().push(port);
                    let _ = self.udp_events.send(UdpTunnelEvent::Unregistered { port });
                }
            }
            TunnelKey::P2p(name) => {
                self.p2p_tunnels.remove(&name);
            }
        }
    }

    /// Return the current proxied-request count for the *specific* member
    /// identified by `tunnel_id`. Per-member counters keep billing and
    /// dashboards honest; aggregate at the group level when displaying.
    /// Returns 0 if the tunnel is unknown.
    pub fn get_tunnel_request_count(&self, tunnel_id: &Uuid) -> u64 {
        match self.tunnel_index.get(tunnel_id).as_deref() {
            Some(TunnelKey::Http(sub)) => self
                .http_routes
                .get(sub)
                .and_then(|g| {
                    g.members
                        .get(tunnel_id)
                        .map(|m| m.info.request_count.load(Ordering::Relaxed))
                })
                .unwrap_or(0),
            Some(TunnelKey::Tcp(port)) => self
                .tcp_routes
                .get(port)
                .and_then(|g| {
                    g.members
                        .get(tunnel_id)
                        .map(|m| m.info.request_count.load(Ordering::Relaxed))
                })
                .unwrap_or(0),
            Some(TunnelKey::Udp(port)) => self
                .udp_routes
                .get(port)
                .and_then(|g| {
                    g.members
                        .get(tunnel_id)
                        .map(|m| m.info.request_count.load(Ordering::Relaxed))
                })
                .unwrap_or(0),
            Some(TunnelKey::P2p(name)) => self
                .p2p_tunnels
                .get(name)
                .map(|p| p.tunnel_info.request_count.load(Ordering::Relaxed))
                .unwrap_or(0),
            None => 0,
        }
    }

    /// Absolute counters for `tunnel_id`.
    ///
    /// Missing tunnels return zeros so a close can still mark the row. A zero
    /// snapshot does not decrease counters already flushed, because persistence
    /// uses `GREATEST`.
    pub fn tunnel_meter(&self, tunnel_id: &Uuid) -> crate::core::LiveTunnelMeter {
        let found = match self.tunnel_index.get(tunnel_id).as_deref() {
            Some(TunnelKey::Http(sub)) => self
                .http_routes
                .get(sub)
                .and_then(|g| g.members.get(tunnel_id).map(|m| m.info.meter_snapshot())),
            Some(TunnelKey::Tcp(port)) => self
                .tcp_routes
                .get(port)
                .and_then(|g| g.members.get(tunnel_id).map(|m| m.info.meter_snapshot())),
            Some(TunnelKey::Udp(port)) => self
                .udp_routes
                .get(port)
                .and_then(|g| g.members.get(tunnel_id).map(|m| m.info.meter_snapshot())),
            Some(TunnelKey::P2p(name)) => self
                .p2p_tunnels
                .get(name)
                .map(|p| p.tunnel_info.meter_snapshot()),
            None => None,
        };
        found.unwrap_or_else(|| crate::core::LiveTunnelMeter::zeros(*tunnel_id))
    }

    /// Snapshot every live tunnel. Used by the periodic flush and by
    /// graceful shutdown, before rows are closed.
    pub fn live_tunnel_meters(&self) -> Vec<crate::core::LiveTunnelMeter> {
        let mut rows = Vec::new();
        for group in self.http_routes.iter() {
            for member in group.members.iter() {
                rows.push(member.info.meter_snapshot());
            }
        }
        for group in self.tcp_routes.iter() {
            for member in group.members.iter() {
                rows.push(member.info.meter_snapshot());
            }
        }
        for group in self.udp_routes.iter() {
            for member in group.members.iter() {
                rows.push(member.info.meter_snapshot());
            }
        }
        for entry in self.p2p_tunnels.iter() {
            rows.push(entry.tunnel_info.meter_snapshot());
        }
        rows
    }

    /// Return the current bytes-proxied counter for a member.
    /// Returns 0 if the tunnel is unknown.
    pub fn get_tunnel_bytes_proxied(&self, tunnel_id: &Uuid) -> u64 {
        match self.tunnel_index.get(tunnel_id).as_deref() {
            Some(TunnelKey::Http(sub)) => self
                .http_routes
                .get(sub)
                .and_then(|g| {
                    g.members
                        .get(tunnel_id)
                        .map(|m| m.info.bytes_proxied.load(Ordering::Relaxed))
                })
                .unwrap_or(0),
            Some(TunnelKey::Tcp(port)) => self
                .tcp_routes
                .get(port)
                .and_then(|g| {
                    g.members
                        .get(tunnel_id)
                        .map(|m| m.info.bytes_proxied.load(Ordering::Relaxed))
                })
                .unwrap_or(0),
            Some(TunnelKey::Udp(port)) => self
                .udp_routes
                .get(port)
                .and_then(|g| {
                    g.members
                        .get(tunnel_id)
                        .map(|m| m.info.bytes_proxied.load(Ordering::Relaxed))
                })
                .unwrap_or(0),
            Some(TunnelKey::P2p(name)) => self
                .p2p_tunnels
                .get(name)
                .map(|p| p.tunnel_info.bytes_proxied.load(Ordering::Relaxed))
                .unwrap_or(0),
            None => 0,
        }
    }

    // ── health-check bit (TUNNEL-7 Phase 4) ───────────────────────────────────

    /// Mark `tunnel_id` healthy and reset its consecutive-failure counter.
    /// Called from the session frame handler when a `TunnelHealthy` arrives.
    /// Returns `true` if the tunnel was found and updated, `false` otherwise.
    /// The caller is expected to gate this on session ownership — see the
    /// auth check in `control/session.rs`.
    pub fn set_tunnel_healthy(&self, tunnel_id: &Uuid) -> bool {
        let mut transitioned = false;
        let updated = self.with_member(tunnel_id, |member| {
            // Record only on the rising edge — steady-state Healthy
            // reports would flood the ring.
            let was_unhealthy = !member.healthy.swap(true, Ordering::AcqRel);
            member.consecutive_failures.store(0, Ordering::Release);
            if was_unhealthy {
                member.record_health_event(true, "recovered");
                transitioned = true;
            }
        });
        if updated {
            if let Some(group) = self.group_for_tunnel(tunnel_id) {
                // Recovery means the group has at least one healthy member
                // again, so re-arm the "0 healthy" debounce.
                group.zero_healthy_alerted.store(false, Ordering::Release);
                if transitioned {
                    self.broadcast_group_event(tunnel_id, &group, true, "recovered");
                }
            }
        }
        updated
    }

    /// Mark `tunnel_id` unhealthy and bump its failure counters.
    /// `reason` is a free-form string from the client used for dashboards.
    /// Returns `true` if the tunnel was found and updated.
    pub fn set_tunnel_unhealthy(&self, tunnel_id: &Uuid, reason: &str) -> bool {
        let mut transitioned = false;
        let updated = self.with_member(tunnel_id, |member| {
            let was_healthy = member.healthy.swap(false, Ordering::AcqRel);
            member.consecutive_failures.fetch_add(1, Ordering::Release);
            member.total_health_failures.fetch_add(1, Ordering::Relaxed);
            // Record on the falling edge only. A repeated Unhealthy
            // (server already had it as down) bumps the counter but
            // doesn't add a new timeline entry.
            if was_healthy {
                member.record_health_event(false, reason);
                transitioned = true;
            }
            tracing::debug!(%tunnel_id, %reason, "tunnel marked unhealthy");
        });
        if updated && transitioned {
            if let Some(group) = self.group_for_tunnel(tunnel_id) {
                self.broadcast_group_event(tunnel_id, &group, false, reason);
            }
        }
        updated
    }

    /// Push a `GroupEvent` onto the broadcast channel. Best-effort —
    /// `send` returns an error iff there are no live subscribers, which
    /// is fine. The send-failure path doesn't propagate up.
    fn broadcast_group_event(
        &self,
        tunnel_id: &Uuid,
        group: &Arc<TunnelGroup>,
        healthy: bool,
        reason: &str,
    ) {
        let key = match self.tunnel_index.get(tunnel_id).as_deref().cloned() {
            Some(k) => k,
            None => return,
        };
        let (protocol, label) = match key {
            TunnelKey::Http(sub) => ("http", sub),
            TunnelKey::Tcp(port) => ("tcp", port.to_string()),
            TunnelKey::Udp(port) => ("udp", port.to_string()),
            TunnelKey::P2p(_) => return,
        };
        let event = crate::core::tunnel::GroupEvent {
            protocol: protocol.to_string(),
            label,
            tunnel_id: *tunnel_id,
            healthy,
            reason: reason.to_string(),
            healthy_count: group.healthy_count(),
            member_count: group.members.len(),
        };
        let _ = self.group_events.send(event);
    }

    /// Resolve `tunnel_id` to its `GroupMember` (across http/tcp/udp routes)
    /// and run `f` on it. Returns `true` if the tunnel + member were found.
    fn with_member<F>(&self, tunnel_id: &Uuid, f: F) -> bool
    where
        F: FnOnce(&GroupMember),
    {
        let key = match self.tunnel_index.get(tunnel_id).as_deref().cloned() {
            Some(k) => k,
            None => return false,
        };
        let group = match key {
            TunnelKey::Http(sub) => self.http_routes.get(&sub).map(|g| Arc::clone(&g)),
            TunnelKey::Tcp(port) => self.tcp_routes.get(&port).map(|g| Arc::clone(&g)),
            TunnelKey::Udp(port) => self.udp_routes.get(&port).map(|g| Arc::clone(&g)),
            TunnelKey::P2p(_) => None, // health checks not supported for P2P
        };
        let Some(group) = group else { return false };
        let Some(member) = group.members.get(tunnel_id) else {
            return false;
        };
        f(&member);
        true
    }

    /// Resolve `tunnel_id` to its owning `Arc<TunnelGroup>` (HTTP/TCP/UDP).
    /// Returns `None` for unknown tunnels and for P2P (which doesn't
    /// participate in load-balancing groups).
    pub fn group_for_tunnel(&self, tunnel_id: &Uuid) -> Option<Arc<TunnelGroup>> {
        let key = self.tunnel_index.get(tunnel_id).as_deref().cloned()?;
        match key {
            TunnelKey::Http(sub) => self.http_routes.get(&sub).map(|g| Arc::clone(&g)),
            TunnelKey::Tcp(port) => self.tcp_routes.get(&port).map(|g| Arc::clone(&g)),
            TunnelKey::Udp(port) => self.udp_routes.get(&port).map(|g| Arc::clone(&g)),
            TunnelKey::P2p(_) => None,
        }
    }

    /// If the group containing `tunnel_id` has just transitioned to 0
    /// healthy members AND we haven't already fired an alert for this
    /// transition, return a payload describing the affected group.
    /// Subsequent calls return `None` until any member becomes healthy
    /// again (which clears the debounce in `set_tunnel_healthy`).
    ///
    /// `region_id` is supplied by the caller because the router doesn't
    /// know what region it's running in.
    pub fn pop_zero_healthy_alert(
        &self,
        tunnel_id: &Uuid,
        region_id: &str,
    ) -> Option<ZeroHealthyAlert> {
        let group = self.group_for_tunnel(tunnel_id)?;
        // Only multi-member groups can meaningfully have "0 healthy" —
        // a solo tunnel going unhealthy isn't an LB transition.
        group.key_hash.as_ref()?;
        if group.healthy_count() != 0 {
            return None;
        }
        // Atomically arm the debounce. If we lose the race (another flip
        // already armed it), don't re-fire.
        if group
            .zero_healthy_alerted
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        // Determine protocol + label from the routing table key.
        let key = self.tunnel_index.get(tunnel_id).as_deref().cloned()?;
        let (protocol, label) = match key {
            TunnelKey::Http(sub) => ("http", sub),
            TunnelKey::Tcp(port) => ("tcp", port.to_string()),
            TunnelKey::Udp(port) => ("udp", port.to_string()),
            TunnelKey::P2p(_) => return None,
        };
        let key_hash_short = group
            .key_hash
            .as_deref()
            .map(|kh| kh.chars().take(8).collect::<String>())
            .unwrap_or_default();
        // Collect unique per-tenant webhook URLs from members' specs.
        // Insertion order is preserved so we don't shuffle delivery on
        // every alert (helps log correlation).
        let mut tenant_webhook_urls: Vec<String> = Vec::new();
        for member in group.members.iter() {
            if let Some(spec) = member.health_spec.as_ref() {
                if let Some(url) = spec.alert_webhook_url.as_deref() {
                    if !tenant_webhook_urls.iter().any(|u| u == url) {
                        tenant_webhook_urls.push(url.to_string());
                    }
                }
            }
        }
        Some(ZeroHealthyAlert {
            payload: GroupAlertPayload {
                region_id: region_id.to_string(),
                protocol: protocol.to_string(),
                label,
                group_name: group.name.clone(),
                key_hash_short,
                member_count: group.members.len(),
            },
            tenant_webhook_urls,
        })
    }

    /// Look up the session that owns `tunnel_id`. Used by the frame handler
    /// to authorise health-bit toggles — only the owning session may flip
    /// the flag for its own tunnels.
    pub fn tunnel_session(&self, tunnel_id: &Uuid) -> Option<Uuid> {
        let key = self.tunnel_index.get(tunnel_id).as_deref().cloned()?;
        let group = match key {
            TunnelKey::Http(sub) => self.http_routes.get(&sub).map(|g| Arc::clone(&g))?,
            TunnelKey::Tcp(port) => self.tcp_routes.get(&port).map(|g| Arc::clone(&g))?,
            TunnelKey::Udp(port) => self.udp_routes.get(&port).map(|g| Arc::clone(&g))?,
            TunnelKey::P2p(name) => {
                return self
                    .p2p_tunnels
                    .get(&name)
                    .map(|p| p.tunnel_info.session_id);
            }
        };
        group.members.get(tunnel_id).map(|m| m.info.session_id)
    }

    // ── resolution (hot path) ─────────────────────────────────────────────────

    /// Look up the tunnel and its session's control channel by subdomain.
    ///
    /// Picks a healthy member uniformly at random. In Phase 1 every group
    /// has exactly one (always-healthy) member, so this degenerates to the
    /// previous behaviour. Once Phase 2 lands, the random pick gives us
    /// FRP-style group dispatch with no further routing-layer changes.
    pub fn resolve_http(
        &self,
        subdomain: &str,
    ) -> Option<(TunnelInfo, mpsc::Sender<ControlMessage>)> {
        let group = self.http_routes.get(subdomain)?.clone();
        self.dispatch_member(&group)
    }

    /// Side-effect-free existence check for an HTTP route. Unlike
    /// `resolve_http` this does not dispatch a member (no request-count
    /// increment, no random pick) — use it for gating decisions.
    pub fn has_http_route(&self, subdomain: &str) -> bool {
        self.http_routes.contains_key(subdomain)
    }

    /// Look up the tunnel and its session's control channel by TCP port.
    pub fn resolve_tcp(&self, port: u16) -> Option<(TunnelInfo, mpsc::Sender<ControlMessage>)> {
        let group = self.tcp_routes.get(&port)?.clone();
        self.dispatch_member(&group)
    }

    /// Look up the tunnel and its session's control channel by UDP port.
    pub fn resolve_udp(&self, port: u16) -> Option<(TunnelInfo, mpsc::Sender<ControlMessage>)> {
        let group = self.udp_routes.get(&port)?.clone();
        self.dispatch_member(&group)
    }

    /// Pick one healthy member of `group` and return its tunnel info plus
    /// the owning session's control channel. Returns `None` when no member
    /// is healthy or the picked member's session has gone away.
    fn dispatch_member(
        &self,
        group: &TunnelGroup,
    ) -> Option<(TunnelInfo, mpsc::Sender<ControlMessage>)> {
        // Pick uniformly at random among healthy members. We snapshot the
        // chosen member's TunnelInfo (cheap — it's mostly Arc-wrapped
        // counters) and drop the DashMap iter so we don't hold a shard lock
        // across the session lookup.
        let mut rng = rand::thread_rng();
        let info = group
            .members
            .iter()
            .filter(|m| m.healthy.load(Ordering::Acquire))
            .map(|m| m.info.clone())
            .choose(&mut rng)?;

        let tx = self.sessions.get(&info.session_id)?.control_tx.clone();
        info.request_count.fetch_add(1, Ordering::Relaxed);
        Some((info, tx))
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
        TunnelCore::new([20000, 20009], [0, 0], 5, 100, 1000, 10)
    }

    fn dummy_session(core: &TunnelCore) -> (Uuid, mpsc::Receiver<ControlMessage>) {
        let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let (tx, rx) = mpsc::channel(16);
        let session_id = core.register_session(addr, "token-1".to_string(), None, None, tx);
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
            .register_http_tunnel(
                &session_id,
                Some("myapp".to_string()),
                TunnelProtocol::Http,
                None,
            )
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
            .register_http_tunnel(&session_id, None, TunnelProtocol::Http, None)
            .unwrap();

        assert_eq!(subdomain.len(), 8);
        assert!(subdomain.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn duplicate_subdomain_is_rejected() {
        let core = make_core();
        let (session_id, _rx) = dummy_session(&core);

        core.register_http_tunnel(
            &session_id,
            Some("clash".to_string()),
            TunnelProtocol::Http,
            None,
        )
        .unwrap();

        let result = core.register_http_tunnel(
            &session_id,
            Some("clash".to_string()),
            TunnelProtocol::Http,
            None,
        );

        assert!(matches!(result, Err(Error::Tunnel(_))));
    }

    #[test]
    fn remove_http_tunnel() {
        let core = make_core();
        let (session_id, _rx) = dummy_session(&core);

        let (tunnel_id, _) = core
            .register_http_tunnel(
                &session_id,
                Some("gone".to_string()),
                TunnelProtocol::Http,
                None,
            )
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

        let (tunnel_id, port) = core.register_tcp_tunnel(&session_id, None).unwrap();

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
        let core = TunnelCore::new([30000, 30000], [0, 0], 5, 100, 1000, 10); // single-port range
        let (session_id, _rx) = dummy_session(&core);

        let (tunnel_id, port) = core.register_tcp_tunnel(&session_id, None).unwrap();
        assert_eq!(port, 30000);

        // Pool is now empty — next allocation must fail.
        let (session2_id, _rx2) = dummy_session(&core);
        assert!(matches!(
            core.register_tcp_tunnel(&session2_id, None),
            Err(Error::NoPortsAvailable)
        ));

        // Return the port.
        core.remove_tunnel(&tunnel_id);

        // Now allocation succeeds again.
        let (_id2, port2) = core.register_tcp_tunnel(&session2_id, None).unwrap();
        assert_eq!(port2, 30000);
    }

    #[test]
    fn no_ports_available_error() {
        let core = TunnelCore::new([40000, 40000], [0, 0], 10, 100, 1000, 10);
        let (sid1, _rx1) = dummy_session(&core);
        let (sid2, _rx2) = dummy_session(&core);

        core.register_tcp_tunnel(&sid1, None).unwrap();

        assert!(matches!(
            core.register_tcp_tunnel(&sid2, None),
            Err(Error::NoPortsAvailable)
        ));
    }

    // ── resolution ────────────────────────────────────────────────────────────

    #[test]
    fn resolve_http_returns_tunnel_and_sender() {
        let core = make_core();
        let (session_id, _rx) = dummy_session(&core);

        core.register_http_tunnel(
            &session_id,
            Some("web".to_string()),
            TunnelProtocol::Http,
            None,
        )
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

        let (_, port) = core.register_tcp_tunnel(&session_id, None).unwrap();

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
            .register_http_tunnel(
                &session_id,
                Some("bye".to_string()),
                TunnelProtocol::Http,
                None,
            )
            .unwrap();
        let (_, port) = core.register_tcp_tunnel(&session_id, None).unwrap();

        core.remove_session(&session_id);

        assert!(!core.sessions.contains_key(&session_id));
        assert!(!core.tunnel_index.contains_key(&tid));
        assert!(!core.http_routes.contains_key("bye"));
        assert!(!core.tcp_routes.contains_key(&port));
    }

    // ── per-session tunnel limit ──────────────────────────────────────────────

    #[test]
    fn tunnel_limit_is_enforced() {
        let core = TunnelCore::new([50000, 50009], [0, 0], 2, 100, 1000, 10);
        let (session_id, _rx) = dummy_session(&core);

        core.register_http_tunnel(&session_id, None, TunnelProtocol::Http, None)
            .unwrap();
        core.register_http_tunnel(&session_id, None, TunnelProtocol::Http, None)
            .unwrap();

        let result = core.register_http_tunnel(&session_id, None, TunnelProtocol::Http, None);
        assert!(matches!(result, Err(Error::LimitExceeded(_))));
    }

    #[test]
    fn session_not_found_error() {
        let core = make_core();
        let ghost = Uuid::new_v4();

        assert!(matches!(
            core.register_http_tunnel(&ghost, None, TunnelProtocol::Http, None),
            Err(Error::SessionNotFound(_))
        ));
    }

    // ── subdomain validation ──────────────────────────────────────────────────

    #[test]
    fn valid_subdomains_are_accepted() {
        let core = make_core();
        let (sid, _rx) = dummy_session(&core);
        for s in &["abc", "my-app", "foo123", "a-b-c", "aaa"] {
            let r =
                core.register_http_tunnel(&sid, Some(s.to_string()), TunnelProtocol::Http, None);
            assert!(r.is_ok(), "expected '{s}' to be valid, got {r:?}");
        }
    }

    #[test]
    fn subdomain_too_short_is_rejected() {
        let core = make_core();
        let (sid, _rx) = dummy_session(&core);
        assert!(matches!(
            core.register_http_tunnel(&sid, Some("ab".to_string()), TunnelProtocol::Http, None),
            Err(Error::Tunnel(_))
        ));
    }

    #[test]
    fn subdomain_leading_hyphen_is_rejected() {
        let core = make_core();
        let (sid, _rx) = dummy_session(&core);
        assert!(matches!(
            core.register_http_tunnel(&sid, Some("-bad".to_string()), TunnelProtocol::Http, None),
            Err(Error::Tunnel(_))
        ));
    }

    #[test]
    fn subdomain_trailing_hyphen_is_rejected() {
        let core = make_core();
        let (sid, _rx) = dummy_session(&core);
        assert!(matches!(
            core.register_http_tunnel(&sid, Some("bad-".to_string()), TunnelProtocol::Http, None),
            Err(Error::Tunnel(_))
        ));
    }

    #[test]
    fn subdomain_invalid_chars_are_rejected() {
        let core = make_core();
        let (sid, _rx) = dummy_session(&core);
        assert!(matches!(
            core.register_http_tunnel(
                &sid,
                Some("bad_name".to_string()),
                TunnelProtocol::Http,
                None
            ),
            Err(Error::Tunnel(_))
        ));
        assert!(matches!(
            core.register_http_tunnel(
                &sid,
                Some("bad.name".to_string()),
                TunnelProtocol::Http,
                None
            ),
            Err(Error::Tunnel(_))
        ));
    }

    // ── NAT classification ───────────────────────────────────────────────

    #[test]
    fn classify_cone_cone_is_direct() {
        let (strategy, attempt) = classify_nat_pair(Some("cone"), Some("cone"));
        assert_eq!(strategy, "direct_exchange");
        assert!(attempt);
    }

    #[test]
    fn classify_open_cone_is_direct() {
        let (strategy, attempt) = classify_nat_pair(Some("open"), Some("cone"));
        assert_eq!(strategy, "direct_exchange");
        assert!(attempt);
    }

    #[test]
    fn classify_cone_symmetric_is_port_prediction() {
        let (strategy, attempt) = classify_nat_pair(Some("cone"), Some("symmetric"));
        assert_eq!(strategy, "port_prediction");
        assert!(attempt);
    }

    #[test]
    fn classify_symmetric_symmetric_is_relay() {
        let (strategy, attempt) = classify_nat_pair(Some("symmetric"), Some("symmetric"));
        assert_eq!(strategy, "relay");
        assert!(!attempt);
    }

    #[test]
    fn classify_unknown_is_relay() {
        let (strategy, attempt) = classify_nat_pair(Some("unknown"), Some("cone"));
        assert_eq!(strategy, "relay");
        assert!(!attempt);
    }

    #[test]
    fn classify_none_is_relay() {
        let (strategy, attempt) = classify_nat_pair(None, None);
        assert_eq!(strategy, "relay");
        assert!(!attempt);
    }

    // ── group sanity (Phase 1 of TUNNEL-7) ───────────────────────────────

    /// Every solo registration is wrapped in a degenerate one-member group;
    /// dispatch must still return that single member.
    #[test]
    fn http_solo_registration_creates_one_member_group() {
        let core = make_core();
        let (sid, _rx) = dummy_session(&core);
        let (tid, _) = core
            .register_http_tunnel(&sid, Some("solo".into()), TunnelProtocol::Http, None)
            .unwrap();

        let group = core.http_routes.get("solo").unwrap();
        assert_eq!(group.members.len(), 1);
        assert!(group.members.contains_key(&tid));
        assert!(
            group.key_hash.is_none(),
            "no group_key on solo registrations"
        );
        assert_eq!(group.name, "solo");
    }

    /// Removing the only member of a group also removes the group itself
    /// (and, for TCP, returns the port to the pool — exercised by the
    /// existing `remove_tcp_tunnel_returns_port_to_pool` test).
    #[test]
    fn last_member_removal_evicts_group() {
        let core = make_core();
        let (sid, _rx) = dummy_session(&core);
        let (tid, _) = core
            .register_http_tunnel(&sid, Some("ephemeral".into()), TunnelProtocol::Http, None)
            .unwrap();
        assert!(core.http_routes.contains_key("ephemeral"));

        core.remove_tunnel(&tid);
        assert!(!core.http_routes.contains_key("ephemeral"));
    }

    /// Members marked unhealthy must be skipped by dispatch even though
    /// they're still in the routing table. Phase 4 will be the first place
    /// this bit gets flipped on a real `TunnelUnhealthy` frame; the dispatch
    /// path is already wired so we lock it in with a test now.
    #[test]
    fn unhealthy_member_is_excluded_from_dispatch() {
        let core = make_core();
        let (sid, _rx) = dummy_session(&core);
        let (tid, _) = core
            .register_http_tunnel(&sid, Some("toggle".into()), TunnelProtocol::Http, None)
            .unwrap();

        // Flip the lone member to unhealthy.
        {
            let group = core.http_routes.get("toggle").unwrap();
            let member = group.members.get(&tid).unwrap();
            member.healthy.store(false, Ordering::Release);
        }

        assert!(
            core.resolve_http("toggle").is_none(),
            "no healthy members → resolve must return None"
        );

        // Mark healthy again → dispatch resumes.
        {
            let group = core.http_routes.get("toggle").unwrap();
            let member = group.members.get(&tid).unwrap();
            member.healthy.store(true, Ordering::Release);
        }
        assert!(core.resolve_http("toggle").is_some());
    }

    // ── HTTP group registration (Phase 2 of TUNNEL-7) ────────────────────
    //
    // These tests pin down the four cells of plan §4.3's truth table:
    //   subdomain free + solo  →  create solo group  (already covered above)
    //   subdomain free + group →  create new group with key_hash
    //   subdomain taken + solo →  reject (legacy "in use")
    //   subdomain taken + group + matching key + matching protocol →  join
    //   subdomain taken + group + mismatched key →  reject
    //   subdomain taken + group + mismatched protocol →  reject
    // Plus dispatch-distributes-across-members which is the whole point.

    fn solo_group_spec(name: &str, key: &str) -> GroupSpec {
        GroupSpec {
            group_name: name.to_string(),
            key_hash: key.to_string(),
            health_check: None,
        }
    }

    /// First grouped registration creates a multi-member-shaped group
    /// (still one member at this point) with the supplied key_hash.
    #[test]
    fn http_group_first_registration_sets_key_hash() {
        let core = make_core();
        let (sid, _rx) = dummy_session(&core);

        let (tid, _) = core
            .register_http_tunnel(
                &sid,
                Some("pool".into()),
                TunnelProtocol::Http,
                Some(solo_group_spec("web", "hash-A")),
            )
            .unwrap();

        let group = core.http_routes.get("pool").unwrap();
        assert_eq!(group.name, "web");
        assert_eq!(group.key_hash.as_deref(), Some("hash-A"));
        assert_eq!(group.members.len(), 1);
        assert!(group.members.contains_key(&tid));
    }

    /// Two clients with the same `(subdomain, key_hash)` form one pool.
    /// Sessions are distinct; tunnels are distinct; the routing entry is shared.
    #[test]
    fn http_group_second_member_with_matching_key_joins() {
        let core = make_core();
        let (sid_a, _rx_a) = dummy_session(&core);
        let (sid_b, _rx_b) = dummy_session(&core);

        let (tid_a, _) = core
            .register_http_tunnel(
                &sid_a,
                Some("pool".into()),
                TunnelProtocol::Http,
                Some(solo_group_spec("web", "hash-A")),
            )
            .unwrap();
        let (tid_b, _) = core
            .register_http_tunnel(
                &sid_b,
                Some("pool".into()),
                TunnelProtocol::Http,
                Some(solo_group_spec("web", "hash-A")),
            )
            .unwrap();

        let group = core.http_routes.get("pool").unwrap();
        assert_eq!(group.members.len(), 2);
        assert!(group.members.contains_key(&tid_a));
        assert!(group.members.contains_key(&tid_b));
        // tunnel_index points each tunnel_id to the same Http(subdomain) key,
        // so per-tunnel counter lookup keeps working.
        assert_eq!(core.get_tunnel_request_count(&tid_a), 0);
        assert_eq!(core.get_tunnel_request_count(&tid_b), 0);
    }

    /// A second registration with the right subdomain but wrong key is
    /// rejected — this is the auth check that prevents one tenant from
    /// hijacking another's pool on a shared edge.
    #[test]
    fn http_group_mismatched_key_is_rejected() {
        let core = make_core();
        let (sid_a, _rx_a) = dummy_session(&core);
        let (sid_b, _rx_b) = dummy_session(&core);

        core.register_http_tunnel(
            &sid_a,
            Some("pool".into()),
            TunnelProtocol::Http,
            Some(solo_group_spec("web", "hash-A")),
        )
        .unwrap();

        let result = core.register_http_tunnel(
            &sid_b,
            Some("pool".into()),
            TunnelProtocol::Http,
            Some(solo_group_spec("web", "hash-B")),
        );

        assert!(
            matches!(result, Err(Error::Tunnel(ref msg)) if msg.contains("group key does not match"))
        );
        // Group must still hold only the original member.
        assert_eq!(core.http_routes.get("pool").unwrap().members.len(), 1);
    }

    /// Joining an existing group with a matching key but a mismatched
    /// protocol (Http vs Https) is rejected.
    #[test]
    fn http_group_protocol_mismatch_is_rejected() {
        let core = make_core();
        let (sid_a, _rx_a) = dummy_session(&core);
        let (sid_b, _rx_b) = dummy_session(&core);

        core.register_http_tunnel(
            &sid_a,
            Some("pool".into()),
            TunnelProtocol::Http,
            Some(solo_group_spec("web", "hash-A")),
        )
        .unwrap();

        let result = core.register_http_tunnel(
            &sid_b,
            Some("pool".into()),
            TunnelProtocol::Https,
            Some(solo_group_spec("web", "hash-A")),
        );

        assert!(matches!(result, Err(Error::Tunnel(ref msg)) if msg.contains("protocol mismatch")));
        assert_eq!(core.http_routes.get("pool").unwrap().members.len(), 1);
    }

    /// A solo registration on top of an existing grouped pool is rejected
    /// (preserves historical "subdomain in use" semantics for non-group
    /// callers — and prevents accidentally bypassing the group key check).
    #[test]
    fn http_solo_on_existing_group_is_rejected() {
        let core = make_core();
        let (sid_a, _rx_a) = dummy_session(&core);
        let (sid_b, _rx_b) = dummy_session(&core);

        core.register_http_tunnel(
            &sid_a,
            Some("pool".into()),
            TunnelProtocol::Http,
            Some(solo_group_spec("web", "hash-A")),
        )
        .unwrap();

        let result =
            core.register_http_tunnel(&sid_b, Some("pool".into()), TunnelProtocol::Http, None);

        assert!(matches!(result, Err(Error::Tunnel(ref msg)) if msg.contains("already in use")));
    }

    /// Conversely: a grouped registration on top of an existing solo
    /// registration is rejected (`key_hash = None` on the existing group
    /// won't match anything the joiner can supply).
    #[test]
    fn http_group_on_existing_solo_is_rejected() {
        let core = make_core();
        let (sid_a, _rx_a) = dummy_session(&core);
        let (sid_b, _rx_b) = dummy_session(&core);

        core.register_http_tunnel(&sid_a, Some("pool".into()), TunnelProtocol::Http, None)
            .unwrap();

        let result = core.register_http_tunnel(
            &sid_b,
            Some("pool".into()),
            TunnelProtocol::Http,
            Some(solo_group_spec("web", "hash-A")),
        );

        assert!(
            matches!(result, Err(Error::Tunnel(ref msg)) if msg.contains("group key does not match"))
        );
    }

    /// Removing one member of a multi-member group leaves the route in
    /// place with the remaining members — only the *last* leave evicts it.
    #[test]
    fn http_group_removing_one_of_two_members_keeps_route() {
        let core = make_core();
        let (sid_a, _rx_a) = dummy_session(&core);
        let (sid_b, _rx_b) = dummy_session(&core);

        let (tid_a, _) = core
            .register_http_tunnel(
                &sid_a,
                Some("pool".into()),
                TunnelProtocol::Http,
                Some(solo_group_spec("web", "hash-A")),
            )
            .unwrap();
        let (tid_b, _) = core
            .register_http_tunnel(
                &sid_b,
                Some("pool".into()),
                TunnelProtocol::Http,
                Some(solo_group_spec("web", "hash-A")),
            )
            .unwrap();

        core.remove_tunnel(&tid_a);

        // Route still exists, group has one member left.
        let group = core.http_routes.get("pool").unwrap();
        assert_eq!(group.members.len(), 1);
        assert!(group.members.contains_key(&tid_b));
        assert!(core.resolve_http("pool").is_some());
    }

    /// Random dispatch across two healthy members must hit each one with
    /// non-trivial frequency over many resolves. This is the load-balancing
    /// promise of Phase 2.
    #[test]
    fn http_group_random_dispatch_distributes_across_members() {
        let core = make_core();
        let (sid_a, _rx_a) = dummy_session(&core);
        let (sid_b, _rx_b) = dummy_session(&core);

        let (tid_a, _) = core
            .register_http_tunnel(
                &sid_a,
                Some("lbpool".into()),
                TunnelProtocol::Http,
                Some(solo_group_spec("web", "hash-A")),
            )
            .unwrap();
        let (tid_b, _) = core
            .register_http_tunnel(
                &sid_b,
                Some("lbpool".into()),
                TunnelProtocol::Http,
                Some(solo_group_spec("web", "hash-A")),
            )
            .unwrap();

        // Drive resolve_http() many times. resolve_http internally bumps
        // the chosen member's request_count, so we read those counters.
        const N: u64 = 1_000;
        for _ in 0..N {
            assert!(core.resolve_http("lbpool").is_some());
        }

        let count_a = core.get_tunnel_request_count(&tid_a);
        let count_b = core.get_tunnel_request_count(&tid_b);
        assert_eq!(count_a + count_b, N);

        // With uniform random over 1000 trials and p=0.5, both should land
        // well inside [200, 800] — this gives effectively zero flake rate
        // while still catching a "always picks the same one" regression.
        assert!(
            (200..=800).contains(&count_a),
            "expected ~500 hits on member A, got {count_a} (B got {count_b})"
        );
        assert!(
            (200..=800).contains(&count_b),
            "expected ~500 hits on member B, got {count_b} (A got {count_a})"
        );
    }

    // ── TCP group registration (Phase 3 of TUNNEL-7) ─────────────────────
    //
    // Same shape as the HTTP truth table, plus the port-pool wrinkle:
    // first member of a group allocates a port; subsequent members reuse
    // it; only when the *last* member leaves does the port go back to the
    // pool and the listener-Unregistered event fire.
    //
    // For these tests we use the dedicated event subscriber to assert the
    // edge-listener event semantics — silence is the contract for joiners.

    /// First TCP-grouped registration allocates a port from the pool;
    /// `tcp_group_index` records the mapping; the listener gets a
    /// `Registered` event so the edge knows to bind.
    #[test]
    fn tcp_group_first_registration_allocates_port_and_indexes() {
        let core = make_core();
        let (sid, _rx) = dummy_session(&core);
        let mut events = core.subscribe_tcp_events();

        let (tid, port) = core
            .register_tcp_tunnel(&sid, Some(solo_group_spec("ssh", "hash-A")))
            .unwrap();

        // Routing entry is in place with the right key_hash.
        let group = core.tcp_routes.get(&port).unwrap();
        assert_eq!(group.name, "ssh");
        assert_eq!(group.key_hash.as_deref(), Some("hash-A"));
        assert_eq!(group.members.len(), 1);
        assert!(group.members.contains_key(&tid));
        drop(group);

        // Index resolves the (name, key_hash) → port.
        let idx_port = core
            .tcp_group_index
            .get(&("ssh".to_string(), "hash-A".to_string()))
            .map(|v| *v);
        assert_eq!(idx_port, Some(port));

        // Edge listener got the Registered event for the first member.
        let evt = events
            .try_recv()
            .expect("Registered event for first member");
        match evt {
            TcpTunnelEvent::Registered { tunnel_id, port: p } => {
                assert_eq!(tunnel_id, tid);
                assert_eq!(p, port);
            }
            other => panic!("expected Registered, got {other:?}"),
        }
    }

    /// Second registration with the same `(group, key_hash)` reuses the
    /// existing port and does NOT fire a new Registered event — the
    /// listener is already bound.
    #[test]
    fn tcp_group_second_member_reuses_port_no_extra_event() {
        let core = make_core();
        let (sid_a, _rx_a) = dummy_session(&core);
        let (sid_b, _rx_b) = dummy_session(&core);
        let mut events = core.subscribe_tcp_events();

        let (_tid_a, port_a) = core
            .register_tcp_tunnel(&sid_a, Some(solo_group_spec("ssh", "hash-A")))
            .unwrap();
        let (_tid_b, port_b) = core
            .register_tcp_tunnel(&sid_b, Some(solo_group_spec("ssh", "hash-A")))
            .unwrap();

        assert_eq!(
            port_a, port_b,
            "second member must reuse the first member's port"
        );

        let group = core.tcp_routes.get(&port_a).unwrap();
        assert_eq!(group.members.len(), 2);
        drop(group);

        // First Registered was for tid_a; nothing else.
        let _first = events.try_recv().expect("Registered for first member");
        assert!(
            events.try_recv().is_err(),
            "no Registered event must fire for join — the listener is already up"
        );
    }

    /// Mismatched key on the same group name → rejected.
    #[test]
    fn tcp_group_mismatched_key_creates_a_separate_pool() {
        let core = make_core();
        let (sid_a, _rx_a) = dummy_session(&core);
        let (sid_b, _rx_b) = dummy_session(&core);

        let (_tid_a, port_a) = core
            .register_tcp_tunnel(&sid_a, Some(solo_group_spec("ssh", "hash-A")))
            .unwrap();
        // Different key → different index entry → fresh port allocation.
        // (FRP allows this since `(name, key)` is the pool identity.)
        let (_tid_b, port_b) = core
            .register_tcp_tunnel(&sid_b, Some(solo_group_spec("ssh", "hash-B")))
            .unwrap();

        assert_ne!(
            port_a, port_b,
            "different keys → different pools → different ports"
        );
        assert_eq!(core.tcp_routes.get(&port_a).unwrap().members.len(), 1);
        assert_eq!(core.tcp_routes.get(&port_b).unwrap().members.len(), 1);
    }

    /// Removing one of two members keeps the route + the index entry; only
    /// the *last* leave evicts everything and returns the port to the pool.
    #[test]
    fn tcp_group_removing_one_of_two_keeps_port_allocated() {
        let core = TunnelCore::new([60000, 60001], [0, 0], 5, 100, 1000, 10);
        let (sid_a, _rx_a) = dummy_session(&core);
        let (sid_b, _rx_b) = dummy_session(&core);
        let mut events = core.subscribe_tcp_events();

        let (tid_a, port) = core
            .register_tcp_tunnel(&sid_a, Some(solo_group_spec("ssh", "hash-A")))
            .unwrap();
        let (tid_b, port_b) = core
            .register_tcp_tunnel(&sid_b, Some(solo_group_spec("ssh", "hash-A")))
            .unwrap();
        assert_eq!(port, port_b);

        // Drain the Registered event from creation.
        let _ = events.try_recv();

        // Remove one member — port stays, index stays, no Unregistered event.
        core.remove_tunnel(&tid_a);
        assert!(core.tcp_routes.contains_key(&port));
        assert!(core
            .tcp_group_index
            .contains_key(&("ssh".to_string(), "hash-A".to_string())));
        assert!(events.try_recv().is_err());

        // Remove the last member — port returns to pool, index clears,
        // Unregistered fires.
        core.remove_tunnel(&tid_b);
        assert!(!core.tcp_routes.contains_key(&port));
        assert!(!core
            .tcp_group_index
            .contains_key(&("ssh".to_string(), "hash-A".to_string())));
        let evt = events.try_recv().expect("Unregistered for last leave");
        assert!(matches!(evt, TcpTunnelEvent::Unregistered { port: p } if p == port));
    }

    /// Random dispatch across two TCP members — same property the HTTP test
    /// pins down, exercised through `resolve_tcp(port)`.
    #[test]
    fn tcp_group_random_dispatch_distributes_across_members() {
        let core = make_core();
        let (sid_a, _rx_a) = dummy_session(&core);
        let (sid_b, _rx_b) = dummy_session(&core);

        let (tid_a, port) = core
            .register_tcp_tunnel(&sid_a, Some(solo_group_spec("ssh", "hash-A")))
            .unwrap();
        let (tid_b, _) = core
            .register_tcp_tunnel(&sid_b, Some(solo_group_spec("ssh", "hash-A")))
            .unwrap();

        const N: u64 = 1_000;
        for _ in 0..N {
            assert!(core.resolve_tcp(port).is_some());
        }

        let count_a = core.get_tunnel_request_count(&tid_a);
        let count_b = core.get_tunnel_request_count(&tid_b);
        assert_eq!(count_a + count_b, N);
        assert!(
            (200..=800).contains(&count_a),
            "expected ~500 hits on member A, got {count_a} (B got {count_b})"
        );
        assert!(
            (200..=800).contains(&count_b),
            "expected ~500 hits on member B, got {count_b} (A got {count_a})"
        );
    }

    /// Port pool is finite — when a brand-new TCP group can't allocate a
    /// fresh port, registration fails with `NoPortsAvailable` (same as the
    /// solo path).
    #[test]
    fn tcp_group_first_registration_respects_port_pool() {
        let core = TunnelCore::new([60100, 60100], [0, 0], 5, 100, 1000, 10);
        let (sid_a, _rx_a) = dummy_session(&core);
        let (sid_b, _rx_b) = dummy_session(&core);

        // First grouped registration takes the only port.
        core.register_tcp_tunnel(&sid_a, Some(solo_group_spec("ssh", "hash-A")))
            .unwrap();

        // A *different* group needs a different port — pool is empty.
        assert!(matches!(
            core.register_tcp_tunnel(&sid_b, Some(solo_group_spec("ssh", "hash-B"))),
            Err(Error::NoPortsAvailable)
        ));
    }

    // ── set_tunnel_healthy / set_tunnel_unhealthy (Phase 4) ──────────────

    /// `set_tunnel_unhealthy` flips the bit; subsequent dispatch routes
    /// around the unhealthy member. `set_tunnel_healthy` brings it back.
    /// `consecutive_failures` resets to 0 on healthy, increments on
    /// unhealthy.
    #[test]
    fn set_tunnel_health_toggles_bit_and_failures() {
        let core = make_core();
        let (sid, _rx) = dummy_session(&core);
        let (tid, _) = core
            .register_http_tunnel(&sid, Some("hcheck".into()), TunnelProtocol::Http, None)
            .unwrap();

        // Solo member starts healthy.
        assert!(core.resolve_http("hcheck").is_some());

        // Mark unhealthy → dispatch excludes it; failure counter bumps.
        assert!(core.set_tunnel_unhealthy(&tid, "probe-1 failed"));
        assert!(core.resolve_http("hcheck").is_none());
        let group = core.http_routes.get("hcheck").unwrap();
        let member = group.members.get(&tid).unwrap();
        assert_eq!(member.consecutive_failures.load(Ordering::Acquire), 1);
        drop(member);
        drop(group);

        // Another unhealthy → counter to 2.
        core.set_tunnel_unhealthy(&tid, "probe-2 failed");
        let group = core.http_routes.get("hcheck").unwrap();
        let member = group.members.get(&tid).unwrap();
        assert_eq!(member.consecutive_failures.load(Ordering::Acquire), 2);
        drop(member);
        drop(group);

        // Healthy resets counter to 0 + restores dispatch.
        assert!(core.set_tunnel_healthy(&tid));
        let group = core.http_routes.get("hcheck").unwrap();
        let member = group.members.get(&tid).unwrap();
        assert_eq!(member.consecutive_failures.load(Ordering::Acquire), 0);
        drop(member);
        drop(group);
        assert!(core.resolve_http("hcheck").is_some());
    }

    /// `set_tunnel_healthy` / `set_tunnel_unhealthy` return `false` for
    /// unknown tunnel IDs — the frame handler relies on this for the
    /// "unknown tunnel" path.
    #[test]
    fn set_tunnel_health_returns_false_for_unknown() {
        let core = make_core();
        let ghost = Uuid::new_v4();
        assert!(!core.set_tunnel_healthy(&ghost));
        assert!(!core.set_tunnel_unhealthy(&ghost, "doesn't matter"));
    }

    /// `tunnel_session` resolves the owning session for HTTP, TCP, and UDP
    /// tunnels. The frame handler uses this to gate health-bit toggles to
    /// the owning session only.
    #[test]
    fn tunnel_session_resolves_owner_across_protocols() {
        let core = make_core();
        let (sid_a, _rx_a) = dummy_session(&core);
        let (sid_b, _rx_b) = dummy_session(&core);

        let (tid_http, _) = core
            .register_http_tunnel(&sid_a, Some("owner".into()), TunnelProtocol::Http, None)
            .unwrap();
        let (tid_tcp, _) = core.register_tcp_tunnel(&sid_b, None).unwrap();

        assert_eq!(core.tunnel_session(&tid_http), Some(sid_a));
        assert_eq!(core.tunnel_session(&tid_tcp), Some(sid_b));
        assert_eq!(core.tunnel_session(&Uuid::new_v4()), None);
    }

    /// In a multi-member HTTP group, marking one member unhealthy must
    /// route all subsequent connections to the other. This is the
    /// load-balancing-with-health invariant — Phase 4's whole point.
    #[test]
    fn unhealthy_member_in_group_routes_around() {
        let core = make_core();
        let (sid_a, _rx_a) = dummy_session(&core);
        let (sid_b, _rx_b) = dummy_session(&core);

        let (tid_a, _) = core
            .register_http_tunnel(
                &sid_a,
                Some("pool".into()),
                TunnelProtocol::Http,
                Some(solo_group_spec("web", "hash-A")),
            )
            .unwrap();
        let (tid_b, _) = core
            .register_http_tunnel(
                &sid_b,
                Some("pool".into()),
                TunnelProtocol::Http,
                Some(solo_group_spec("web", "hash-A")),
            )
            .unwrap();

        // Mark A unhealthy. All N dispatches must land on B.
        core.set_tunnel_unhealthy(&tid_a, "probe failed");
        for _ in 0..50 {
            assert!(core.resolve_http("pool").is_some());
        }
        assert_eq!(core.get_tunnel_request_count(&tid_a), 0);
        assert_eq!(core.get_tunnel_request_count(&tid_b), 50);

        // Bring A back. Now dispatch distributes again.
        core.set_tunnel_healthy(&tid_a);
        for _ in 0..200 {
            core.resolve_http("pool");
        }
        // Both should have grown — A from 0, B beyond 50. Loose bounds
        // for flake-resistance; the point is "A serves requests again".
        assert!(core.get_tunnel_request_count(&tid_a) > 30);
        assert!(core.get_tunnel_request_count(&tid_b) > 80);
    }

    /// A member registered with a `health_check` starts unhealthy and is
    /// excluded from dispatch until the first `TunnelHealthy` flips the
    /// bit. Phase 4 wires the frame plumbing; the bit semantics belong here.
    #[test]
    fn http_group_member_with_health_check_starts_unhealthy() {
        let core = make_core();
        let (sid, _rx) = dummy_session(&core);

        let spec = GroupSpec {
            group_name: "web".into(),
            key_hash: "hash-A".into(),
            health_check: Some(rustunnel_protocol::HealthCheckSpec {
                kind: rustunnel_protocol::HealthCheckKind::Tcp,
                interval_secs: 10,
                timeout_secs: 3,
                max_failed: 3,
                http_path: None,
                http_expect_2xx: true,
                alert_webhook_url: None,
            }),
        };
        let (tid, _) = core
            .register_http_tunnel(&sid, Some("await".into()), TunnelProtocol::Http, Some(spec))
            .unwrap();

        // Member exists but is unhealthy → no healthy member to dispatch to.
        let group = core.http_routes.get("await").unwrap();
        let member = group.members.get(&tid).unwrap();
        assert!(!member.healthy.load(Ordering::Acquire));
        drop(member);
        drop(group);
        assert!(core.resolve_http("await").is_none());
    }
}
