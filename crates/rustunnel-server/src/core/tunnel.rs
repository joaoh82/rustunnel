use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use tokio::io::DuplexStream;
use tokio::sync::{mpsc, Semaphore};
use uuid::Uuid;

use rustunnel_protocol::{HealthCheckSpec, TunnelProtocol};

/// Maximum number of recent health-state transitions kept per member.
/// Capped so a flapping backend can't grow the ring unboundedly. Older
/// events are evicted FIFO.
pub const HEALTH_EVENT_RING_SIZE: usize = 50;

/// One health-state transition for a `GroupMember`. Timestamps are
/// captured at the moment the bit flips on the server side. `reason` is
/// the string carried by `TunnelUnhealthy` (or `"recovered"` for a flip
/// to healthy, or `"registered"` for the very first event when a member
/// with a `health_check` spec comes online).
#[derive(Debug, Clone)]
pub struct HealthEvent {
    /// `SystemTime` at the moment the flip happened. Carried alongside
    /// `Instant` because `Instant` isn't serialisable / human-readable;
    /// `SystemTime` is what dashboards display.
    pub at: SystemTime,
    pub healthy: bool,
    pub reason: String,
}

// ── TCP tunnel events (edge ↔ core) ───────────────────────────────────────────

/// Broadcast by `TunnelCore` whenever a TCP tunnel is added or removed.
/// The TCP edge layer subscribes to this to manage per-port listeners.
#[derive(Debug, Clone)]
pub enum TcpTunnelEvent {
    Registered { tunnel_id: Uuid, port: u16 },
    Unregistered { port: u16 },
}

/// Broadcast by `TunnelCore` whenever a UDP tunnel is added or removed.
/// The UDP edge layer subscribes to this to manage per-port listeners.
#[derive(Debug, Clone)]
pub enum UdpTunnelEvent {
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
    /// Bytes from the public side toward the local service (requests, uploads).
    pub bytes_in: Arc<AtomicU64>,
    /// Bytes from the local service toward the public side (responses, downloads).
    pub bytes_out: Arc<AtomicU64>,
    /// `bytes_in + bytes_out`. Kept as its own counter so existing readers
    /// (dashboard, Prometheus) keep seeing the combined total.
    pub bytes_proxied: Arc<AtomicU64>,
    /// Limits concurrent proxied connections for this tunnel.
    /// Shared across all clones so every proxy task draws from the same pool.
    pub conn_semaphore: Arc<Semaphore>,
}

/// Absolute in-memory counters for one live tunnel.
///
/// Persisted as absolute values (not deltas). A later flush or close with
/// smaller numbers does not decrease what is already stored.
#[derive(Debug, Clone, Copy)]
pub struct LiveTunnelMeter {
    pub tunnel_id: Uuid,
    pub request_count: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

impl LiveTunnelMeter {
    pub fn bytes_proxied(self) -> u64 {
        self.bytes_in.saturating_add(self.bytes_out)
    }

    /// Zero counters for `tunnel_id`. Used when the routing entry is already
    /// gone so a close still marks the row, without wiping a previous flush.
    pub fn zeros(tunnel_id: Uuid) -> Self {
        Self {
            tunnel_id,
            request_count: 0,
            bytes_in: 0,
            bytes_out: 0,
        }
    }
}

/// Shared directional counters. Cloning shares the same atomics.
#[derive(Debug, Clone)]
pub struct TransferMeter {
    bytes_in: Arc<AtomicU64>,
    bytes_out: Arc<AtomicU64>,
    bytes_proxied: Arc<AtomicU64>,
}

impl TransferMeter {
    /// Record one transfer. `inbound` is public toward local, `outbound` is
    /// local toward public. `bytes_proxied` increases by the same sum.
    pub fn add(&self, inbound: u64, outbound: u64) {
        if inbound > 0 {
            self.bytes_in.fetch_add(inbound, Ordering::Relaxed);
        }
        if outbound > 0 {
            self.bytes_out.fetch_add(outbound, Ordering::Relaxed);
        }
        let total = inbound.saturating_add(outbound);
        if total > 0 {
            self.bytes_proxied.fetch_add(total, Ordering::Relaxed);
        }
    }
}

impl TunnelInfo {
    pub fn new(
        session_id: Uuid,
        tunnel_id: Uuid,
        protocol: TunnelProtocol,
        subdomain: Option<String>,
        assigned_port: Option<u16>,
        max_connections: usize,
    ) -> Self {
        Self {
            session_id,
            tunnel_id,
            protocol,
            subdomain,
            assigned_port,
            created_at: Instant::now(),
            request_count: Arc::new(AtomicU64::new(0)),
            bytes_in: Arc::new(AtomicU64::new(0)),
            bytes_out: Arc::new(AtomicU64::new(0)),
            bytes_proxied: Arc::new(AtomicU64::new(0)),
            conn_semaphore: Arc::new(Semaphore::new(max_connections)),
        }
    }

    pub fn meter(&self) -> TransferMeter {
        TransferMeter {
            bytes_in: Arc::clone(&self.bytes_in),
            bytes_out: Arc::clone(&self.bytes_out),
            bytes_proxied: Arc::clone(&self.bytes_proxied),
        }
    }

    pub fn add_transfer(&self, inbound: u64, outbound: u64) {
        self.meter().add(inbound, outbound);
    }

    pub fn meter_snapshot(&self) -> LiveTunnelMeter {
        LiveTunnelMeter {
            tunnel_id: self.tunnel_id,
            request_count: self.request_count.load(Ordering::Relaxed),
            bytes_in: self.bytes_in.load(Ordering::Relaxed),
            bytes_out: self.bytes_out.load(Ordering::Relaxed),
        }
    }
}

// ── Load-balancing groups (TUNNEL-7) ──────────────────────────────────────────
//
// A `TunnelGroup` is the routing entry for an HTTP subdomain or a TCP/UDP
// port. It holds one or more `GroupMember`s — each backed by a separate
// `RegisterTunnel` from a separate client. Phase 1 only ever has a single
// member per group (the "degenerate" case); multi-member registration arrives
// in Phase 2/3.
//
// Storing this on the routing-table value side (instead of a separate map of
// pools) makes resolution a single DashMap lookup and lets remove-on-last-leave
// happen atomically next to the existing route-removal code.

/// One backend in a `TunnelGroup`.
///
/// `healthy` defaults to `true` when no `health_spec` is set (we trust the
/// client's presence) and to `false` when a spec exists, until the first
/// `TunnelHealthy` arrives.
#[derive(Debug)]
pub struct GroupMember {
    pub info: TunnelInfo,
    pub healthy: AtomicBool,
    /// Health-check spec from `RegisterTunnel.health_check`; `None` if the
    /// client didn't ask for probes.
    pub health_spec: Option<HealthCheckSpec>,
    /// Current consecutive-failure streak — resets to 0 each time a
    /// `TunnelHealthy` arrives. Surfaced on the dashboard so operators can
    /// see "this member is currently down on its 3rd straight failure".
    pub consecutive_failures: AtomicU32,
    /// Cumulative count of `TunnelUnhealthy` frames received since this
    /// member registered. Drives the
    /// `rustunnel_group_health_failures_total{group, kind}` Prometheus
    /// counter (Phase 5). Never resets while the member is registered;
    /// goes away when the member leaves the group.
    pub total_health_failures: AtomicU64,
    /// Ring buffer of recent health-state transitions (TUNNEL-8 Phase 5).
    /// Capped at `HEALTH_EVENT_RING_SIZE`; older events are evicted FIFO.
    /// Only edges are recorded — not every `TunnelHealthy` probe report.
    /// Surfaced via `GET /api/tunnels/:id/health-events` and the per-tunnel
    /// health timeline on both dashboards.
    pub health_events: Mutex<VecDeque<HealthEvent>>,
}

impl GroupMember {
    /// Create a member that's immediately considered healthy. Used for the
    /// Phase 1 path where no health-check spec is attached.
    pub fn healthy_with(info: TunnelInfo) -> Self {
        Self {
            info,
            healthy: AtomicBool::new(true),
            health_spec: None,
            consecutive_failures: AtomicU32::new(0),
            total_health_failures: AtomicU64::new(0),
            health_events: Mutex::new(VecDeque::with_capacity(HEALTH_EVENT_RING_SIZE)),
        }
    }

    /// Create a member with an optional health-check spec.
    ///
    /// Initial `healthy` follows §4.5 of the plan: a member with no spec is
    /// trusted (presence ⇒ healthy); a member that *did* opt into probes
    /// starts unhealthy and only flips to healthy on the first
    /// `TunnelHealthy` frame from the client. This prevents routing real
    /// traffic to a backend whose upstream we haven't probed yet.
    pub fn with_health_spec(info: TunnelInfo, spec: Option<HealthCheckSpec>) -> Self {
        let initially_healthy = spec.is_none();
        let mut events = VecDeque::with_capacity(HEALTH_EVENT_RING_SIZE);
        // Seed an initial event so the timeline always shows when the
        // member came online and what state it started in. "registered"
        // is the synthetic reason for this T0 event.
        events.push_back(HealthEvent {
            at: SystemTime::now(),
            healthy: initially_healthy,
            reason: if initially_healthy {
                "registered".into()
            } else {
                "registered (awaiting first probe)".into()
            },
        });
        Self {
            info,
            healthy: AtomicBool::new(initially_healthy),
            health_spec: spec,
            consecutive_failures: AtomicU32::new(0),
            total_health_failures: AtomicU64::new(0),
            health_events: Mutex::new(events),
        }
    }

    /// Append a transition to the ring buffer. Called from
    /// `set_tunnel_healthy` / `set_tunnel_unhealthy` only when the bit
    /// actually flipped — steady-state probe reports don't generate
    /// events. Evicts the oldest entry when the buffer is full.
    pub fn record_health_event(&self, healthy: bool, reason: impl Into<String>) {
        let mut ring = self.health_events.lock();
        if ring.len() >= HEALTH_EVENT_RING_SIZE {
            ring.pop_front();
        }
        ring.push_back(HealthEvent {
            at: SystemTime::now(),
            healthy,
            reason: reason.into(),
        });
    }

    /// Snapshot the current ring contents (newest last). Returned as a
    /// fresh `Vec` so the lock is released before the caller serialises.
    pub fn health_events_snapshot(&self) -> Vec<HealthEvent> {
        self.health_events.lock().iter().cloned().collect()
    }
}

/// A pool of one or more members serving the same subdomain or port.
#[derive(Debug)]
pub struct TunnelGroup {
    /// Display name (== subdomain for HTTP, port-as-string for TCP/UDP, or
    /// the user-supplied `group` from `RegisterTunnel` once Phase 2 lands).
    pub name: String,
    /// SHA-256 of the user-supplied `group_key`. `None` for ungrouped /
    /// degenerate single-member registrations (Phase 1).
    pub key_hash: Option<String>,
    /// Members keyed by their `tunnel_id`.
    pub members: DashMap<Uuid, GroupMember>,
    /// Debounce flag for the "0 healthy members" webhook alert
    /// (TUNNEL-8 Phase 5). Set to `true` after we fire an alert; reset
    /// to `false` when any member becomes healthy again. Prevents the
    /// alert from re-firing on every additional `TunnelUnhealthy` while
    /// the group is already known-down.
    pub zero_healthy_alerted: AtomicBool,
}

impl TunnelGroup {
    pub fn new_solo(name: String, info: TunnelInfo) -> Arc<Self> {
        let group = Arc::new(Self {
            name,
            key_hash: None,
            members: DashMap::new(),
            zero_healthy_alerted: AtomicBool::new(false),
        });
        group
            .members
            .insert(info.tunnel_id, GroupMember::healthy_with(info));
        group
    }

    /// Create a fresh group seeded with `member`. Phase 2 of TUNNEL-7 uses
    /// this for the first registration of a multi-member HTTP group; Phase
    /// 3 will reuse it for TCP. `key_hash` is `Some` for grouped pools and
    /// `None` for ungrouped/solo registrations (use `new_solo` in that
    /// case for clarity).
    pub fn new_with_member(
        name: String,
        key_hash: Option<String>,
        member: GroupMember,
    ) -> Arc<Self> {
        let tunnel_id = member.info.tunnel_id;
        let group = Arc::new(Self {
            name,
            key_hash,
            members: DashMap::new(),
            zero_healthy_alerted: AtomicBool::new(false),
        });
        group.members.insert(tunnel_id, member);
        group
    }

    /// Count the members currently marked healthy. Lock-free; fine to
    /// call on the hot path.
    pub fn healthy_count(&self) -> usize {
        self.members
            .iter()
            .filter(|m| m.healthy.load(std::sync::atomic::Ordering::Acquire))
            .count()
    }
}

/// Payload for the "group went 0/N healthy" webhook alert
/// (TUNNEL-8 Phase 5). Serialised as JSON in the POST body sent to each
/// configured webhook destination.
#[derive(Debug, Clone)]
pub struct GroupAlertPayload {
    pub region_id: String,
    pub protocol: String,
    pub label: String,
    pub group_name: String,
    pub key_hash_short: String,
    pub member_count: usize,
}

/// Output of `TunnelCore::pop_zero_healthy_alert` — the alert payload
/// plus the deduped list of per-tenant webhook destinations gathered
/// from the affected group's members. The frame handler also adds the
/// operator-side `[load_balancing] alert_webhook_url` when firing, so
/// this struct doesn't include it.
#[derive(Debug, Clone)]
pub struct ZeroHealthyAlert {
    pub payload: GroupAlertPayload,
    /// Unique per-tenant webhook URLs from `member.health_spec.alert_webhook_url`,
    /// in insertion order. Typically one URL (one tenant per group); rare
    /// multi-tenant pools (plan §4.6) yield several. Each URL gets one
    /// POST per transition.
    pub tenant_webhook_urls: Vec<String>,
}

/// One push event on the live group-event stream
/// (`GET /api/groups/:label/events`). Emitted whenever a member's health
/// bit flips. Subscribers filter on `(protocol, label)` server-side, so a
/// dashboard tab open on `pool` doesn't see events for other groups.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GroupEvent {
    /// `http` / `tcp` / `udp` — matches the routing-table the group lives in.
    pub protocol: String,
    /// Subdomain (HTTP) or port-as-string (TCP/UDP) — the routing key.
    pub label: String,
    /// `tunnel_id` of the affected member.
    pub tunnel_id: Uuid,
    /// New health state of the member after the transition.
    pub healthy: bool,
    /// Free-form reason carried by `TunnelUnhealthy`, or `"recovered"` for
    /// the upward edge. Same string that lands in the per-member ring buffer.
    pub reason: String,
    /// Healthy member count *after* the transition, so dashboards can
    /// render `2/3 healthy` without a follow-up GET.
    pub healthy_count: usize,
    /// Total member count after the transition.
    pub member_count: usize,
}

/// User-supplied parameters for joining or creating a load-balancing group.
///
/// Built by the session frame handler from `RegisterTunnel.group` /
/// `RegisterTunnel.group_key_hash` / `RegisterTunnel.health_check` once the
/// `[load_balancing] enabled = true` kill switch is on. Passed into
/// `TunnelCore::register_http_tunnel` (Phase 2) and
/// `register_tcp_tunnel` (Phase 3, future).
#[derive(Debug, Clone)]
pub struct GroupSpec {
    /// User-supplied display name (sets `TunnelGroup.name` on the first
    /// registration; subsequent joiners can supply any value, the existing
    /// name wins — same as FRP).
    pub group_name: String,
    /// SHA-256 hash of the user-supplied `group_key`. Must match across
    /// every member of the group.
    pub key_hash: String,
    /// Optional health-check spec for this member.
    pub health_check: Option<HealthCheckSpec>,
}

// ── per-session state ─────────────────────────────────────────────────────────

/// Live state for a connected client session.
pub struct SessionInfo {
    /// Remote address the client connected from.
    pub client_addr: SocketAddr,
    /// Opaque identifier of the auth token used (empty string when auth is disabled).
    pub auth_token_id: String,
    /// The `tokens.id` (UUID) of the authenticated token, if it came from the DB.
    /// `None` for the admin token or when auth is disabled.
    pub db_token_id: Option<String>,
    /// `tokens.user_id` of the authenticated token. `None` for the admin
    /// token, for legacy / admin-issued DB tokens with no owning user, and
    /// when auth is disabled. Cached at session creation so the dashboard
    /// API can scope `/api/tunnels` and `/api/groups` to a single tenant
    /// without a per-request DB lookup.
    pub user_id: Option<Uuid>,
    /// Channel for sending control messages to the session handler task.
    pub control_tx: mpsc::Sender<ControlMessage>,
    /// Tunnel IDs owned by this session.
    pub tunnels: Vec<Uuid>,
    pub connected_at: Instant,
    /// Updated on every Ping/Pong exchange.
    pub last_heartbeat: RwLock<Instant>,
    /// Loopback pipe endpoint stored here until the `/_data/<session_id>`
    /// WebSocket connection arrives and takes it for bridging.
    pub data_pipe: Option<DuplexStream>,
}

impl SessionInfo {
    pub fn new(
        client_addr: SocketAddr,
        auth_token_id: String,
        db_token_id: Option<String>,
        user_id: Option<Uuid>,
        control_tx: mpsc::Sender<ControlMessage>,
    ) -> Self {
        let now = Instant::now();
        Self {
            client_addr,
            auth_token_id,
            db_token_id,
            user_id,
            control_tx,
            tunnels: Vec::new(),
            connected_at: now,
            last_heartbeat: RwLock::new(now),
            data_pipe: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_keeps_bytes_proxied_equal_to_in_plus_out() {
        let info = TunnelInfo::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            TunnelProtocol::Http,
            None,
            None,
            1,
        );
        info.add_transfer(100, 40);
        info.add_transfer(5, 0);
        info.meter().add(0, 7);
        let snap = info.meter_snapshot();
        assert_eq!(snap.bytes_in, 105);
        assert_eq!(snap.bytes_out, 47);
        assert_eq!(
            info.bytes_proxied.load(Ordering::Relaxed),
            snap.bytes_proxied()
        );
        assert_eq!(snap.bytes_proxied(), 152);
    }
}
