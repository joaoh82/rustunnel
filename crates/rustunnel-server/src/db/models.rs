//! Database model types for dashboard persistence.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::FromRow;
use uuid::Uuid;

/// A provisioned API token (hashed for storage).
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Token {
    pub id: String,
    /// SHA-256 hex digest of the raw token value.
    pub token_hash: String,
    /// Human-readable label for the token.
    pub label: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    /// Optional comma-separated list of subdomain patterns this token may use.
    /// `None` means unrestricted (token may register any subdomain / protocol).
    pub scope: Option<String>,

    // ── Platform billing fields (Phase 1) ─────────────────────────────────
    /// User that owns this token. `None` for admin and legacy tokens.
    /// Limit checks are skipped when this is `None` for backwards compatibility.
    pub user_id: Option<Uuid>,
    /// Hard expiry timestamp. `None` means no expiry (human PAYG tokens).
    ///
    /// Unclaimed agent tokens are inserted with `users.created_at` plus
    /// [`AgentPlanLimits::TOKEN_TTL_DAYS`] days. Claim (`users.claimed_at`)
    /// clears this back to `None`. There is no column default: a default of
    /// 7 days would expire human tokens.
    pub expires_at: Option<DateTime<Utc>>,
    /// Tier identifier: "free" | "payg" | "micro" | "standard" | "project".
    pub tier: Option<String>,
    /// Maximum number of simultaneously open tunnels. `None` means unlimited.
    pub tunnel_limit: Option<i32>,
    /// Token lifecycle status. `"active"` | `"suspended"` | `"revoked"`.
    pub status: String,
    /// When `true`: bypasses `expires_at` and `tunnel_limit` checks entirely.
    /// `status` is still enforced. Used for admin/testing/early-adopter tokens.
    pub unlimited: bool,
}

/// One lifecycle record per tunnel registration.
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct TunnelLog {
    pub id: String,
    pub tunnel_id: String,
    /// "http" or "tcp"
    pub protocol: String,
    /// Subdomain (HTTP) or port string (TCP).
    pub label: String,
    pub session_id: String,
    /// DB token ID that opened this tunnel; `None` for admin-token sessions.
    pub token_id: Option<String>,
    pub registered_at: DateTime<Utc>,
    pub unregistered_at: Option<DateTime<Utc>>,
}

/// A token record with its historical tunnel registration count.
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct TokenWithCount {
    pub id: String,
    pub token_hash: String,
    pub label: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub scope: Option<String>,
    pub tunnel_count: i64,
    // Platform billing fields
    pub user_id: Option<Uuid>,
    pub expires_at: Option<DateTime<Utc>>,
    pub tier: Option<String>,
    pub tunnel_limit: Option<i32>,
    pub status: String,
    pub unlimited: bool,
}

/// A tunnel_log row joined with the owning token's label.
/// Returned by the `GET /api/history` endpoint.
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct TunnelLogEntry {
    pub id: String,
    pub tunnel_id: String,
    /// "http" or "tcp"
    pub protocol: String,
    /// Subdomain (HTTP) or port string (TCP).
    pub label: String,
    pub session_id: String,
    /// DB token id — `None` for admin-token sessions.
    pub token_id: Option<String>,
    /// Human-readable label from the `tokens` table (populated by LEFT JOIN).
    pub token_label: Option<String>,
    pub registered_at: DateTime<Utc>,
    pub unregistered_at: Option<DateTime<Utc>>,
    /// Region that hosted this tunnel (e.g. "eu", "us"). `None` for pre-Phase-3 rows.
    pub region_id: Option<String>,
}

/// Canonical `plans` row for agent-native accounts.
///
/// Seeded by `migrations/pg/20261127232000_agent_accounts.sql`.
/// `plans.name` is unconstrained text (no enum, no name CHECK). The plan
/// value is the row whose `name` is [`Self::NAME`].
///
/// platform-api `POST /agent/signup` must insert an active `subscriptions`
/// row pointing at this plan, set `tokens.expires_at` to
/// `users.created_at + TOKEN_TTL_DAYS`, and store a salted IP hash in
/// `users.signup_ip_hash`. The edge does not enforce these limits until
/// ticket 2. `billing_model` is [`Self::BILLING_MODEL`], not `"free"`, so
/// the deployed hobby gate does not apply.
pub struct AgentPlanLimits;

impl AgentPlanLimits {
    pub const NAME: &'static str = "agent";
    pub const BILLING_MODEL: &'static str = "agent";
    /// One concurrent tunnel (`plans.max_tunnels`).
    pub const MAX_TUNNELS: i32 = 1;
    /// Random subdomain only (`plans.allow_custom_subdomains`).
    pub const ALLOW_CUSTOM_SUBDOMAINS: bool = false;
    /// HTTP edge only. `http` is the CLI protocol; `https` is the same edge
    /// with an https public URL. `tcp`, `udp`, and `p2p` are excluded.
    pub const ALLOWED_PROTOCOLS: &'static [&'static str] = &["http", "https"];
    pub const BANDWIDTH_HARD_CAP: bool = true;
    /// 250 MiB. Binary units, the same 1024-based family as the Hobby
    /// gigabyte cap (`250 * 1024 * 1024`). Not 250_000_000 decimal bytes,
    /// and not `plans.bandwidth_limit_gb` (that column is whole GiB and is
    /// a soft warning).
    pub const BANDWIDTH_LIMIT_BYTES: i64 = 250 * 1024 * 1024;
    /// Unclaimed agent API tokens expire this many days after
    /// `users.created_at`. Cleared to NULL when `users.claimed_at` is set.
    pub const TOKEN_TTL_DAYS: i64 = 7;
}

/// A plan row with its live subscriber count.
/// Returned by `GET /api/admin/plans`.
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct AdminPlan {
    pub id: Uuid,
    pub name: String,
    pub billing_model: String,
    pub monthly_price_cents: i32,
    pub max_tunnels: Option<i32>,
    pub max_connections: i32,
    pub rate_limit_rps: i32,
    pub bandwidth_limit_gb: Option<i32>,
    pub history_days: i32,
    pub is_active: bool,
    /// When false, registration must use a random subdomain.
    pub allow_custom_subdomains: bool,
    /// TunnelProtocol wire names this plan may register.
    pub allowed_protocols: Vec<String>,
    /// When true, [`Self::bandwidth_limit_bytes`] is a hard monthly cap.
    pub bandwidth_hard_cap: bool,
    /// Hard monthly cap in bytes. `None` when [`Self::bandwidth_hard_cap`]
    /// is false.
    pub bandwidth_limit_bytes: Option<i64>,
    /// Number of subscriptions with `status = 'active'` on this plan.
    pub subscriber_count: i64,
}

/// A tunnel_log row enriched with usage counters for admin inspection.
/// Extends `TunnelLogEntry` with `bytes_proxied`, `request_count`, and `user_id`.
/// Returned by `GET /api/admin/users/:id/tunnels`.
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct AdminTunnelEntry {
    pub id: String,
    pub tunnel_id: String,
    pub protocol: String,
    pub label: String,
    pub session_id: String,
    pub token_id: Option<String>,
    pub token_label: Option<String>,
    pub registered_at: DateTime<Utc>,
    pub unregistered_at: Option<DateTime<Utc>>,
    pub region_id: Option<String>,
    /// Combined total. Equal to `bytes_in + bytes_out` for rows written by
    /// the meter flush. Older rows may have this set and the split at 0.
    pub bytes_proxied: i64,
    pub bytes_in: i64,
    pub bytes_out: i64,
    pub request_count: i64,
    pub user_id: Option<Uuid>,
}

/// A row from the `regions` table.
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Region {
    pub id: String,
    pub name: String,
    pub location: String,
    pub host: String,
    pub control_port: i32,
    pub active: bool,
}

/// A single captured HTTP request/response pair.
#[derive(Debug, Clone, Serialize, FromRow)]
pub struct CapturedRequest {
    pub id: String,
    pub tunnel_id: String,
    pub conn_id: String,
    pub method: String,
    pub path: String,
    pub status: i64,
    pub request_bytes: i64,
    pub response_bytes: i64,
    pub duration_ms: i64,
    pub captured_at: DateTime<Utc>,
    /// Full request headers + body stored as JSON (may be None for large bodies).
    pub request_body: Option<String>,
    /// Full response headers + body stored as JSON (may be None for large bodies).
    pub response_body: Option<String>,
}

impl CapturedRequest {
    /// Synthetic UUID from the id string field.
    pub fn uuid(&self) -> Option<Uuid> {
        self.id.parse().ok()
    }
}
