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
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS tokens (
            id           TEXT PRIMARY KEY,
            token_hash   TEXT NOT NULL UNIQUE,
            label        TEXT NOT NULL,
            created_at   TEXT NOT NULL,
            last_used_at TEXT
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

    Ok(())
}

// ── token helpers ─────────────────────────────────────────────────────────────

use chrono::Utc;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use models::Token;

/// Hash a raw token value with SHA-256.
pub fn hash_token(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    hex::encode(hasher.finalize())
}

/// Insert a new token record.  Returns the raw (unhashed) token string.
pub async fn create_token(pool: &SqlitePool, label: &str) -> Result<(Token, String)> {
    let raw = Uuid::new_v4().to_string();
    let hash = hash_token(&raw);
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();

    sqlx::query(
        "INSERT INTO tokens (id, token_hash, label, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&hash)
    .bind(label)
    .bind(now.to_rfc3339())
    .execute(pool)
    .await?;

    let token = Token {
        id,
        token_hash: hash,
        label: label.to_string(),
        created_at: now,
        last_used_at: None,
    };
    Ok((token, raw))
}

/// Return `Some(Token)` if the hash matches a known token, updating `last_used_at`.
pub async fn verify_token(pool: &SqlitePool, raw: &str) -> Result<Option<Token>> {
    let hash = hash_token(raw);
    let token: Option<Token> = sqlx::query_as(
        "SELECT id, token_hash, label, created_at, last_used_at FROM tokens WHERE token_hash = ?",
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

/// List all tokens.
pub async fn list_tokens(pool: &SqlitePool) -> Result<Vec<Token>> {
    let rows: Vec<Token> =
        sqlx::query_as("SELECT id, token_hash, label, created_at, last_used_at FROM tokens ORDER BY created_at DESC")
            .fetch_all(pool)
            .await?;
    Ok(rows)
}
