use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelProtocol {
    Http,
    Https,
    Tcp,
    Udp,
    P2p,
}

// ── Health-check spec (load balancing — TUNNEL-7) ─────────────────────────────

/// Probe type for client-side health checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthCheckKind {
    /// Open a TCP connection to the local service; success = connect within timeout.
    Tcp,
    /// `GET <http_path>` on the local service; success = 2xx within timeout.
    Http,
}

/// Health-check configuration sent by the client at registration.
///
/// The server stores this per member so dashboards can surface what's being
/// probed, but probes themselves run on the client (FRP-style trust model).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthCheckSpec {
    pub kind: HealthCheckKind,
    /// Probe interval in seconds.
    pub interval_secs: u32,
    /// Per-probe timeout in seconds.
    pub timeout_secs: u32,
    /// Consecutive failures required before reporting `TunnelUnhealthy`.
    pub max_failed: u32,
    /// Path used by HTTP probes (required when `kind = Http`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_path: Option<String>,
    /// When true (default), only HTTP 2xx responses count as healthy.
    #[serde(default = "default_http_expect_2xx")]
    pub http_expect_2xx: bool,
    /// Optional per-tunnel webhook URL the server should POST to when
    /// the *group* this member belongs to transitions to 0 healthy
    /// members (TUNNEL-8 Phase 5 follow-up).
    ///
    /// Distinct from the operator-side `[load_balancing] alert_webhook_url`
    /// in `server.toml` — that one fires for every group on the edge so
    /// the operator can see "something on my fleet went down". This one
    /// fires only for the group containing this member, so each *tenant*
    /// gets their own page-the-on-call destination.
    ///
    /// Hashed-by-uniqueness server-side: if multiple members of the
    /// same group all carry the same URL (typical — one tenant, one
    /// destination), the server fires it once per transition. Different
    /// URLs across members all fire (rare — multiple tenants sharing a
    /// pool, see plan §4.6).
    ///
    /// Older clients (< 0.7.x w/ this field) never send it. Older
    /// servers (< current) deserialise the field as `None` and drop it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alert_webhook_url: Option<String>,
}

fn default_http_expect_2xx() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlFrame {
    Auth {
        token: String,
        client_version: String,
    },
    AuthOk {
        session_id: Uuid,
        server_version: String,
    },
    AuthError {
        message: String,
    },
    RegisterTunnel {
        request_id: String,
        protocol: TunnelProtocol,
        subdomain: Option<String>,
        local_addr: String,
        /// SHA-256 hash of the shared secret (P2P publisher only).
        #[serde(skip_serializing_if = "Option::is_none")]
        p2p_secret_hash: Option<String>,
        /// Human-readable tunnel name for P2P discovery (P2P publisher only).
        #[serde(skip_serializing_if = "Option::is_none")]
        p2p_name: Option<String>,
        /// Load-balancing group name. Tunnels sharing the same group + key
        /// form one logical pool dispatched at random. Omitted by older clients.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<String>,
        /// SHA-256 of the shared `group_key` (never the raw key). Required when
        /// `group` is set — used by the server to authorise group joins.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group_key_hash: Option<String>,
        /// Optional client-side health-check spec. When present, the client
        /// probes its local service and reports `TunnelHealthy`/`TunnelUnhealthy`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        health_check: Option<HealthCheckSpec>,
        /// Who is driving this registration. `"mcp"` selects the MCP upgrade
        /// URL. Omitted by released CLIs; the server treats absence as `cli`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_surface: Option<String>,
    },
    TunnelRegistered {
        request_id: String,
        tunnel_id: Uuid,
        public_url: String,
        assigned_port: Option<u16>,
        /// The assigned P2P tunnel name (P2P publisher only).
        #[serde(skip_serializing_if = "Option::is_none")]
        p2p_tunnel_name: Option<String>,
        /// Optional notice, currently the Hobby bandwidth warning at >= 80%.
        /// Released clients ignore the field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        notice: Option<String>,
    },
    TunnelError {
        request_id: String,
        message: String,
    },
    UnregisterTunnel {
        tunnel_id: Uuid,
    },
    NewConnection {
        conn_id: Uuid,
        client_addr: String,
        protocol: TunnelProtocol,
    },
    DataStreamOpen {
        conn_id: Uuid,
    },
    Ping {
        timestamp: u64,
    },
    Pong {
        timestamp: u64,
    },

    // ── Load-balancing health reports (TUNNEL-7) ─────────────────────────
    /// Client reports that a previously-registered tunnel's upstream is healthy
    /// again (sent on the first probe success after a failure streak, and as
    /// the initial signal for tunnels that registered with a `health_check`).
    /// New clients should only emit this when the server's advertised version
    /// supports load balancing; older servers will log a decode error otherwise.
    TunnelHealthy {
        tunnel_id: Uuid,
    },
    /// Client reports that a tunnel's upstream has failed `max_failed`
    /// consecutive probes. The server excludes the member from dispatch until
    /// a `TunnelHealthy` arrives.
    TunnelUnhealthy {
        tunnel_id: Uuid,
        reason: String,
    },

    // ── P2P frames ───────────────────────────────────────────────────────
    /// Subscriber requests a P2P connection to a named publisher tunnel.
    P2pConnect {
        request_id: String,
        target_tunnel_name: String,
        secret_hash: String,
    },

    /// Server confirms a P2P connection has been established (sent to subscriber).
    P2pConnected {
        request_id: String,
        conn_id: Uuid,
    },

    /// Server rejects a P2P connection request (sent to subscriber).
    P2pError {
        request_id: String,
        message: String,
    },

    // ── P2P direct (Phase 3) ─────────────────────────────────────────────
    /// Client reports its NAT type and mapped addresses after STUN probing.
    /// Sent by both publisher and subscriber during P2P setup.
    P2pNatInfo {
        tunnel_id: Uuid,
        nat_type: String,
        mapped_addrs: Vec<String>,
        local_addrs: Vec<String>,
    },

    /// Server sends each peer the other's NAT info and hole-punch instructions.
    P2pPunchInstructions {
        conn_id: Uuid,
        peer_addrs: Vec<String>,
        strategy: String,
        punch_timeout_ms: u32,
    },

    /// Client reports hole-punch result back to the server.
    P2pPunchResult {
        conn_id: Uuid,
        success: bool,
        direct_addr: Option<String>,
    },

    /// Client periodically reports bandwidth for direct P2P connections.
    P2pMetrics {
        tunnel_id: Uuid,
        bytes_sent: u64,
        bytes_received: u64,
    },
}

pub fn encode_frame(frame: &ControlFrame) -> Vec<u8> {
    serde_json::to_vec(frame).expect("ControlFrame serialization is infallible")
}

pub fn decode_frame(data: &[u8]) -> Result<ControlFrame> {
    serde_json::from_slice(data).map_err(|e| Error::Protocol(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Old clients (pre-load-balancing) never emit the new fields. The wire
    /// shape they produce — and parse — must continue to round-trip exactly.
    #[test]
    fn legacy_register_tunnel_round_trip_unchanged() {
        // The literal payload an old client produced before this change.
        let legacy = r#"{"type":"register_tunnel","request_id":"req-1","protocol":"http","subdomain":"foo","local_addr":"127.0.0.1:3000"}"#;
        let frame = decode_frame(legacy.as_bytes()).expect("decode legacy");
        match &frame {
            ControlFrame::RegisterTunnel {
                group,
                group_key_hash,
                health_check,
                client_surface,
                ..
            } => {
                assert!(group.is_none());
                assert!(group_key_hash.is_none());
                assert!(health_check.is_none());
                assert!(client_surface.is_none());
            }
            other => panic!("unexpected frame: {other:?}"),
        }
        // Re-encoding must not introduce keys for the optional new fields —
        // older servers must keep parsing it cleanly.
        let reencoded = String::from_utf8(encode_frame(&frame)).unwrap();
        assert!(!reencoded.contains("\"group\""));
        assert!(!reencoded.contains("\"group_key_hash\""));
        assert!(!reencoded.contains("\"health_check\""));
        assert!(!reencoded.contains("\"client_surface\""));
    }

    #[test]
    fn register_tunnel_with_group_round_trip() {
        let frame = ControlFrame::RegisterTunnel {
            request_id: "req-2".into(),
            protocol: TunnelProtocol::Tcp,
            subdomain: None,
            local_addr: "127.0.0.1:8080".into(),
            p2p_secret_hash: None,
            p2p_name: None,
            group: Some("web".into()),
            group_key_hash: Some("a".repeat(64)),
            health_check: Some(HealthCheckSpec {
                kind: HealthCheckKind::Http,
                interval_secs: 10,
                timeout_secs: 3,
                max_failed: 3,
                http_path: Some("/status".into()),
                http_expect_2xx: true,
                alert_webhook_url: Some("https://hooks.example.com/tenant-A".into()),
            }),
            client_surface: None,
        };
        let bytes = encode_frame(&frame);
        let decoded = decode_frame(&bytes).expect("decode round-trip");
        match decoded {
            ControlFrame::RegisterTunnel {
                group,
                group_key_hash,
                health_check,
                ..
            } => {
                assert_eq!(group.as_deref(), Some("web"));
                assert_eq!(group_key_hash.as_deref(), Some(&*"a".repeat(64)));
                let spec = health_check.expect("health spec preserved");
                assert_eq!(spec.kind, HealthCheckKind::Http);
                assert_eq!(spec.http_path.as_deref(), Some("/status"));
                // Round-trip the per-tenant webhook URL too (added in
                // the Phase 5 webhook redesign).
                assert_eq!(
                    spec.alert_webhook_url.as_deref(),
                    Some("https://hooks.example.com/tenant-A")
                );
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    #[test]
    fn tunnel_healthy_unhealthy_round_trip() {
        let id = uuid::Uuid::new_v4();
        let healthy = ControlFrame::TunnelHealthy { tunnel_id: id };
        let unhealthy = ControlFrame::TunnelUnhealthy {
            tunnel_id: id,
            reason: "connection refused".into(),
        };
        for f in [healthy, unhealthy] {
            let bytes = encode_frame(&f);
            // Re-decoding must not fail.
            decode_frame(&bytes).expect("decode round-trip");
        }
    }

    /// An old server receiving a payload that *omits* `health_check` must
    /// still parse it; the new field defaults to `None` even though we do not
    /// emit `#[serde(default)]` (Option<T> is implicitly defaulting in serde).
    #[test]
    fn forward_compat_old_server_decodes_new_field_absence() {
        // Construct a minimal payload as a byte string (server's perspective).
        let payload =
            r#"{"type":"tunnel_healthy","tunnel_id":"00000000-0000-0000-0000-000000000000"}"#;
        let frame = decode_frame(payload.as_bytes()).expect("decode tunnel_healthy");
        assert!(matches!(frame, ControlFrame::TunnelHealthy { .. }));
    }

    /// An old edge omits `notice`. New clients must still decode the frame.
    #[test]
    fn tunnel_registered_without_notice_decodes() {
        let payload = r#"{"type":"tunnel_registered","request_id":"r","tunnel_id":"00000000-0000-0000-0000-000000000000","public_url":"https://x.example","assigned_port":null}"#;
        let frame = decode_frame(payload.as_bytes()).expect("decode tunnel_registered");
        match frame {
            ControlFrame::TunnelRegistered {
                notice, public_url, ..
            } => {
                assert!(notice.is_none());
                assert_eq!(public_url, "https://x.example");
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    #[test]
    fn register_tunnel_client_surface_round_trip() {
        let payload = r#"{"type":"register_tunnel","request_id":"req-1","protocol":"http","subdomain":"foo","local_addr":"127.0.0.1:3000","client_surface":"mcp"}"#;
        let frame = decode_frame(payload.as_bytes()).expect("decode");
        match frame {
            ControlFrame::RegisterTunnel { client_surface, .. } => {
                assert_eq!(client_surface.as_deref(), Some("mcp"));
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}
