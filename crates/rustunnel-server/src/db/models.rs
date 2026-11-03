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
    /// User that owns this token. `None` for admin and agent tokens (legacy).
    /// Limit checks are skipped when this is `None` for backwards compatibility.
    pub user_id: Option<Uuid>,
    /// Hard expiry timestamp. `None` means no expiry (human PAYG tokens).
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
