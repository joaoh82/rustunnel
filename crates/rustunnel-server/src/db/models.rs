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
