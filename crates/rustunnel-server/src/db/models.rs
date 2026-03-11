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
    pub registered_at: DateTime<Utc>,
    pub unregistered_at: Option<DateTime<Utc>>,
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
