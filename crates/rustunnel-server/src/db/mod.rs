//! SQLite database pool and schema initialisation.

pub mod models;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;

use crate::error::Result;

/// Initialise the SQLite connection pool and run the embedded migrations.
pub async fn init_pool(database_url: &str) -> Result<SqlitePool> {
    // Support both bare file paths and sqlite:// URIs.
    let url = if database_url.starts_with("sqlite:") {
        database_url.to_string()
    } else {
        format!("sqlite:{database_url}")
    };

    let opts = SqliteConnectOptions::from_str(&url)?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await?;

    migrate(&pool).await?;
    Ok(pool)
}

/// Create tables if they do not yet exist.
async fn migrate(pool: &SqlitePool) -> Result<()> {
    // Idempotent schema — safe to run on every startup.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS tokens (
            id           TEXT PRIMARY KEY,
            token_hash   TEXT NOT NULL UNIQUE,
            label        TEXT NOT NULL,
            created_at   TEXT NOT NULL,
            last_used_at TEXT,
            scope        TEXT
        );

        CREATE TABLE IF NOT EXISTS tunnel_log (
            id               TEXT PRIMARY KEY,
            tunnel_id        TEXT NOT NULL,
            protocol         TEXT NOT NULL,
            label            TEXT NOT NULL,
            session_id       TEXT NOT NULL,
            registered_at    TEXT NOT NULL,
            unregistered_at  TEXT
        );

        CREATE TABLE IF NOT EXISTS captured_requests (
            id             TEXT PRIMARY KEY,
            tunnel_id      TEXT NOT NULL,
            conn_id        TEXT NOT NULL,
            method         TEXT NOT NULL,
            path           TEXT NOT NULL,
            status         INTEGER NOT NULL,
            request_bytes  INTEGER NOT NULL DEFAULT 0,
            response_bytes INTEGER NOT NULL DEFAULT 0,
            duration_ms    INTEGER NOT NULL DEFAULT 0,
            captured_at    TEXT NOT NULL,
            request_body   TEXT,
            response_body  TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_captured_tunnel ON captured_requests (tunnel_id, captured_at DESC);
        CREATE INDEX IF NOT EXISTS idx_tunnel_log_id   ON tunnel_log (tunnel_id);
        "#,
    )
    .execute(pool)
    .await?;

    // Additive column migrations — errors are ignored because the column may
    // already exist on databases created after these were added.
    let _ = sqlx::query("ALTER TABLE tokens ADD COLUMN scope TEXT")
        .execute(pool)
        .await;
    let _ = sqlx::query("ALTER TABLE tunnel_log ADD COLUMN token_id TEXT")
        .execute(pool)
        .await;

    Ok(())
}

// ── token helpers ─────────────────────────────────────────────────────────────

use chrono::Utc;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use models::{Token, TokenWithCount, TunnelLogEntry};

/// Hash a raw token value with SHA-256.
pub fn hash_token(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    hex::encode(hasher.finalize())
}

/// Insert a new token record.  Returns the raw (unhashed) token string.
///
/// `scope` is an optional comma-separated list of subdomain patterns.
/// `None` means the token is unrestricted.
pub async fn create_token(
    pool: &SqlitePool,
    label: &str,
    scope: Option<&str>,
) -> Result<(Token, String)> {
    let raw = Uuid::new_v4().to_string();
    let hash = hash_token(&raw);
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();

    sqlx::query(
        "INSERT INTO tokens (id, token_hash, label, created_at, scope) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&hash)
    .bind(label)
    .bind(now.to_rfc3339())
    .bind(scope)
    .execute(pool)
    .await?;

    let token = Token {
        id,
        token_hash: hash,
        label: label.to_string(),
        created_at: now,
        last_used_at: None,
        scope: scope.map(str::to_string),
    };
    Ok((token, raw))
}

/// Return `Some(Token)` if the hash matches a known token, updating `last_used_at`.
pub async fn verify_token(pool: &SqlitePool, raw: &str) -> Result<Option<Token>> {
    let hash = hash_token(raw);
    let token: Option<Token> = sqlx::query_as(
        "SELECT id, token_hash, label, created_at, last_used_at, scope \
         FROM tokens WHERE token_hash = ?",
    )
    .bind(&hash)
    .fetch_optional(pool)
    .await?;

    if let Some(ref t) = token {
        sqlx::query("UPDATE tokens SET last_used_at = ? WHERE id = ?")
            .bind(Utc::now().to_rfc3339())
            .bind(&t.id)
            .execute(pool)
            .await?;
    }

    Ok(token)
}

/// Delete a token by id.
pub async fn delete_token(pool: &SqlitePool, id: &str) -> Result<bool> {
    let rows = sqlx::query("DELETE FROM tokens WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(rows > 0)
}

/// List all tokens with their historical tunnel registration counts.
pub async fn list_tokens_with_counts(pool: &SqlitePool) -> Result<Vec<TokenWithCount>> {
    let rows: Vec<TokenWithCount> = sqlx::query_as(
        "SELECT t.id, t.token_hash, t.label, t.created_at, t.last_used_at, t.scope, \
                COALESCE(COUNT(tl.id), 0) AS tunnel_count \
         FROM tokens t \
         LEFT JOIN tunnel_log tl ON tl.token_id = t.id \
         GROUP BY t.id \
         ORDER BY t.created_at DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// ── tunnel log helpers ────────────────────────────────────────────────────────

/// Insert a tunnel_log row when a tunnel is registered.
pub async fn log_tunnel_registered(
    pool: &SqlitePool,
    tunnel_id: &str,
    protocol: &str,
    label: &str,
    session_id: &str,
    token_id: Option<&str>,
) -> Result<()> {
    let id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO tunnel_log (id, tunnel_id, protocol, label, session_id, token_id, registered_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(tunnel_id)
    .bind(protocol)
    .bind(label)
    .bind(session_id)
    .bind(token_id)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

/// Set `unregistered_at` on the tunnel_log row for `tunnel_id`.
pub async fn log_tunnel_unregistered(pool: &SqlitePool, tunnel_id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE tunnel_log SET unregistered_at = ? \
         WHERE tunnel_id = ? AND unregistered_at IS NULL",
    )
    .bind(Utc::now().to_rfc3339())
    .bind(tunnel_id)
    .execute(pool)
    .await?;
    Ok(())
}

// ── tunnel history helpers ────────────────────────────────────────────────────

/// Return a page of tunnel history rows, newest first.
///
/// `protocol` filters to `"http"` or `"tcp"` when provided.
pub async fn list_tunnel_history(
    pool: &SqlitePool,
    limit: i64,
    offset: i64,
    protocol: Option<&str>,
) -> Result<Vec<TunnelLogEntry>> {
    let rows: Vec<TunnelLogEntry> = if let Some(proto) = protocol {
        sqlx::query_as(
            "SELECT tl.id, tl.tunnel_id, tl.protocol, tl.label, tl.session_id, \
                    tl.token_id, t.label AS token_label, \
                    tl.registered_at, tl.unregistered_at \
             FROM tunnel_log tl \
             LEFT JOIN tokens t ON t.id = tl.token_id \
             WHERE tl.protocol = ? \
             ORDER BY tl.registered_at DESC \
             LIMIT ? OFFSET ?",
        )
        .bind(proto)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as(
            "SELECT tl.id, tl.tunnel_id, tl.protocol, tl.label, tl.session_id, \
                    tl.token_id, t.label AS token_label, \
                    tl.registered_at, tl.unregistered_at \
             FROM tunnel_log tl \
             LEFT JOIN tokens t ON t.id = tl.token_id \
             ORDER BY tl.registered_at DESC \
             LIMIT ? OFFSET ?",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?
    };
    Ok(rows)
}

/// Total number of tunnel_log rows matching an optional protocol filter.
pub async fn count_tunnel_history(
    pool: &SqlitePool,
    protocol: Option<&str>,
) -> Result<i64> {
    let count: (i64,) = if let Some(proto) = protocol {
        sqlx::query_as("SELECT COUNT(*) FROM tunnel_log WHERE protocol = ?")
            .bind(proto)
            .fetch_one(pool)
            .await?
    } else {
        sqlx::query_as("SELECT COUNT(*) FROM tunnel_log")
            .fetch_one(pool)
            .await?
    };
    Ok(count.0)
}
