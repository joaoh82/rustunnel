//! Database pools and schema initialisation.
//!
//! Two pools:
//!   - `pg`    — PostgreSQL, shared across all regions: tokens, tunnel_log
//!   - `local` — SQLite, per-region: captured_requests

pub mod models;

use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;

use crate::config::DatabaseSection;
use crate::error::Result;

/// Dual-pool database handle.  Cheap to clone (both pools are Arc-backed).
#[derive(Clone)]
pub struct Db {
    /// Shared PostgreSQL pool — tokens, tunnel_log.
    pub pg: PgPool,
    /// Local SQLite pool — captured_requests.
    pub local: SqlitePool,
}

/// Initialise both pools and run migrations on each.
pub async fn init_db(config: &DatabaseSection) -> Result<Db> {
    // ── PostgreSQL ────────────────────────────────────────────────────────────
    let pg = PgPoolOptions::new()
        .max_connections(10)
        .connect(&config.url)
        .await?;

    sqlx::migrate!("migrations/pg").run(&pg).await?;

    // ── SQLite (local captured_requests) ──────────────────────────────────────
    let sqlite_url =
        if config.captured_path.starts_with("sqlite:") || config.captured_path == ":memory:" {
            config.captured_path.clone()
        } else {
            format!("sqlite:{}", config.captured_path)
        };

    let local = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(
            SqliteConnectOptions::from_str(&sqlite_url)?
                .create_if_missing(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                .foreign_keys(true),
        )
        .await?;

    sqlx::migrate!("migrations/local").run(&local).await?;

    Ok(Db { pg, local })
}

// ── token helpers ─────────────────────────────────────────────────────────────

use chrono::Utc;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use models::{Region, Token, TokenWithCount, TunnelLogEntry};

/// Hash a raw token value with SHA-256.
pub fn hash_token(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    hex::encode(hasher.finalize())
}

/// Insert a new token record.  Returns the raw (unhashed) token string.
pub async fn create_token(
    pool: &PgPool,
    label: &str,
    scope: Option<&str>,
) -> Result<(Token, String)> {
    let raw = Uuid::new_v4().to_string();
    let hash = hash_token(&raw);
    let id = Uuid::new_v4().to_string();
    let now = Utc::now();

    sqlx::query(
        "INSERT INTO tokens (id, token_hash, label, created_at, scope) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&id)
    .bind(&hash)
    .bind(label)
    .bind(now)
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
pub async fn verify_token(pool: &PgPool, raw: &str) -> Result<Option<Token>> {
    let hash = hash_token(raw);
    let token: Option<Token> = sqlx::query_as(
        "SELECT id, token_hash, label, created_at, last_used_at, scope \
         FROM tokens WHERE token_hash = $1",
    )
    .bind(&hash)
    .fetch_optional(pool)
    .await?;

    if let Some(ref t) = token {
        sqlx::query("UPDATE tokens SET last_used_at = $1 WHERE id = $2")
            .bind(Utc::now())
            .bind(&t.id)
            .execute(pool)
            .await?;
    }

    Ok(token)
}

/// Delete a token by id.
pub async fn delete_token(pool: &PgPool, id: &str) -> Result<bool> {
    let rows = sqlx::query("DELETE FROM tokens WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(rows > 0)
}

/// List all tokens with their historical tunnel registration counts.
pub async fn list_tokens_with_counts(pool: &PgPool) -> Result<Vec<TokenWithCount>> {
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
    pool: &PgPool,
    tunnel_id: &str,
    protocol: &str,
    label: &str,
    session_id: &str,
    token_id: Option<&str>,
    region_id: &str,
) -> Result<()> {
    let id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO tunnel_log \
             (id, tunnel_id, protocol, label, session_id, token_id, registered_at, region_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(&id)
    .bind(tunnel_id)
    .bind(protocol)
    .bind(label)
    .bind(session_id)
    .bind(token_id)
    .bind(Utc::now())
    .bind(region_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Close all tunnel_log rows that are still open (unregistered_at IS NULL).
///
/// Called once on server startup to mark tunnels from previous runs as closed,
/// since their WebSocket connections no longer exist.
pub async fn close_stale_tunnels(pool: &PgPool) -> Result<u64> {
    let rows =
        sqlx::query("UPDATE tunnel_log SET unregistered_at = $1 WHERE unregistered_at IS NULL")
            .bind(Utc::now())
            .execute(pool)
            .await?
            .rows_affected();
    Ok(rows)
}

/// Set `unregistered_at` on the tunnel_log row for `tunnel_id`.
pub async fn log_tunnel_unregistered(pool: &PgPool, tunnel_id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE tunnel_log SET unregistered_at = $1 \
         WHERE tunnel_id = $2 AND unregistered_at IS NULL",
    )
    .bind(Utc::now())
    .bind(tunnel_id)
    .execute(pool)
    .await?;
    Ok(())
}

// ── tunnel history helpers ────────────────────────────────────────────────────

/// Return a page of tunnel history rows.
///
/// Filters: `protocol` ("http"/"tcp"), `token_id`, `active` (true = open, false = closed).
/// Sorting: `sort_by` ("started"/"duration"/"protocol") × `sort_dir` ("asc"/"desc").
///
/// The ORDER BY clause is built from validated match arms — not raw user input —
/// so format! here does not introduce SQL injection risk.
#[allow(clippy::too_many_arguments)]
pub async fn list_tunnel_history(
    pool: &PgPool,
    limit: i64,
    offset: i64,
    protocol: Option<&str>,
    token_id: Option<&str>,
    active: Option<bool>,
    sort_by: &str,
    sort_dir: &str,
) -> Result<Vec<TunnelLogEntry>> {
    let order_col = match sort_by {
        "duration" => "COALESCE(tl.unregistered_at, now()) - tl.registered_at",
        "protocol" => "tl.protocol",
        _ => "tl.registered_at", // "started" is the default
    };
    let order_dir = if sort_dir == "asc" { "ASC" } else { "DESC" };

    let sql = format!(
        "SELECT tl.id, tl.tunnel_id, tl.protocol, tl.label, tl.session_id, \
                tl.token_id, t.label AS token_label, \
                tl.registered_at, tl.unregistered_at \
         FROM tunnel_log tl \
         LEFT JOIN tokens t ON t.id = tl.token_id \
         WHERE ($1::text IS NULL OR tl.protocol = $1) \
           AND ($2::text IS NULL OR tl.token_id = $2) \
           AND ($3::boolean IS NULL \
                OR ($3 = true  AND tl.unregistered_at IS NULL) \
                OR ($3 = false AND tl.unregistered_at IS NOT NULL)) \
         ORDER BY {order_col} {order_dir} \
         LIMIT $4 OFFSET $5"
    );

    let rows: Vec<TunnelLogEntry> = sqlx::query_as(&sql)
        .bind(protocol)
        .bind(token_id)
        .bind(active)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

// ── region helpers ────────────────────────────────────────────────────────────

/// Return all active regions ordered by id.
pub async fn list_regions(pool: &PgPool) -> Result<Vec<Region>> {
    let rows: Vec<Region> = sqlx::query_as(
        "SELECT id, name, location, host, control_port, active \
         FROM regions \
         WHERE active = true \
         ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Total number of tunnel_log rows matching the given filters.
pub async fn count_tunnel_history(
    pool: &PgPool,
    protocol: Option<&str>,
    token_id: Option<&str>,
    active: Option<bool>,
) -> Result<i64> {
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM tunnel_log tl \
         WHERE ($1::text IS NULL OR tl.protocol = $1) \
           AND ($2::text IS NULL OR tl.token_id = $2) \
           AND ($3::boolean IS NULL \
                OR ($3 = true  AND tl.unregistered_at IS NULL) \
                OR ($3 = false AND tl.unregistered_at IS NOT NULL))",
    )
    .bind(protocol)
    .bind(token_id)
    .bind(active)
    .fetch_one(pool)
    .await?;
    Ok(count.0)
}
