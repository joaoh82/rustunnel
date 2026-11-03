//! Database pools and schema initialisation.
//!
//! Two pools:
//!   - `pg`    — PostgreSQL, shared across all regions: tokens, tunnel_log
//!   - `local` — SQLite, per-region: captured_requests

mod metering;
pub mod models;
mod usage_rollup;

pub use metering::{
    close_stale_tunnels, flush_and_close_live_meters, flush_live_meters, flush_tunnel_meters,
    log_tunnel_unregistered,
};
pub use usage_rollup::{
    rollup_recent, rollup_usage_between, token_usage_stats, usage_bandwidth_series, usage_by_token,
    usage_by_user, usage_daily_series, usage_leaderboard, LeaderboardSort, RollupOutcome,
    TokenUsageStats, UsageBandwidthPoint, UsageByToken, UsageByUser, UsageDailyPoint,
    UsageLeaderboardEntry,
};

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

pub use models::{AdminPlan, AdminTunnelEntry};
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
    let raw = format!("rt_live_{}", Uuid::new_v4());
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
        user_id: None,
        expires_at: None,
        tier: None,
        tunnel_limit: None,
        status: "active".to_string(),
        unlimited: false,
    };
    Ok((token, raw))
}

/// Return `Some(Token)` if the hash matches a known token, updating `last_used_at`.
pub async fn verify_token(pool: &PgPool, raw: &str) -> Result<Option<Token>> {
    let hash = hash_token(raw);
    let token: Option<Token> = sqlx::query_as(
        "SELECT id, token_hash, label, created_at, last_used_at, scope, \
                user_id, expires_at, tier, tunnel_limit, status, unlimited \
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
                t.user_id, t.expires_at, t.tier, t.tunnel_limit, t.status, t.unlimited, \
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

/// Set the `unlimited` flag on a token. Returns `true` if the token was found.
pub async fn set_token_unlimited(pool: &PgPool, id: &str, unlimited: bool) -> Result<bool> {
    let rows = sqlx::query("UPDATE tokens SET unlimited = $1 WHERE id = $2")
        .bind(unlimited)
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(rows > 0)
}

// ── tunnel log helpers ────────────────────────────────────────────────────────

/// Arguments for [`log_tunnel_registered`].
pub struct TunnelRegistration<'a> {
    pub tunnel_id: &'a str,
    pub protocol: &'a str,
    pub label: &'a str,
    pub session_id: &'a str,
    pub token_id: Option<&'a str>,
    pub region_id: &'a str,
    pub user_id: Option<uuid::Uuid>,
}

/// Insert a tunnel_log row when a tunnel is registered.
pub async fn log_tunnel_registered(pool: &PgPool, reg: TunnelRegistration<'_>) -> Result<()> {
    let id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO tunnel_log \
             (id, tunnel_id, protocol, label, session_id, token_id, registered_at, region_id, user_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(&id)
    .bind(reg.tunnel_id)
    .bind(reg.protocol)
    .bind(reg.label)
    .bind(reg.session_id)
    .bind(reg.token_id)
    .bind(Utc::now())
    .bind(reg.region_id)
    .bind(reg.user_id)
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
                tl.registered_at, tl.unregistered_at, tl.region_id \
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

// ── admin user helpers ────────────────────────────────────────────────────────

/// A minimal user row returned by admin list/detail endpoints.
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct AdminUser {
    pub id: uuid::Uuid,
    pub email: String,
    pub display_name: Option<String>,
    pub email_verified: bool,
    pub status: String,
    pub auth_method: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Number of API tokens belonging to this user.
    pub token_count: i64,
    /// The user's *effective* plan. A canceled subscription drops the user back
    /// to "free", so this reads "free" even though the underlying row still
    /// points at the paid plan they used to be on.
    pub plan_name: Option<String>,
    /// Raw status of the user's latest subscription ("active", "past_due",
    /// "canceled", …). Kept separate from `plan_name` so the UI can show both
    /// "free" and *why* it is free.
    pub subscription_status: Option<String>,
    /// The paid plan this user churned off, set only once they have canceled a
    /// non-free plan. `None` for users who never paid. Drives win-back filters.
    pub previous_plan_name: Option<String>,
    /// When the paid subscription ended.
    pub canceled_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Most recent use across all of the user's tokens.
    pub last_active_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Optional filters for the admin user list.
#[derive(Debug, Default, Clone, Copy)]
pub struct AdminUserFilters<'a> {
    /// Case-insensitive substring match on email.
    pub search: Option<&'a str>,
    /// Match on *effective* plan, so "free" also matches churned paid users.
    pub plan: Option<&'a str>,
    /// "google" | "password".
    pub auth_method: Option<&'a str>,
    /// User account status, e.g. "active" | "banned".
    pub status: Option<&'a str>,
    /// Only users who were on a paid plan and have since canceled.
    pub previously_paid: bool,
}

/// Sort orders accepted by the admin user list.
#[derive(Debug, Default, Clone, Copy)]
pub enum AdminUserSort {
    /// Newest signups first.
    #[default]
    Created,
    /// Most recently active first; users who never used a token sort last.
    LastActive,
    /// Alphabetical by email.
    Email,
}

impl AdminUserSort {
    /// Fixed ORDER BY clause. Never interpolates caller input.
    fn order_by(self) -> &'static str {
        match self {
            Self::Created => "ORDER BY u.created_at DESC",
            Self::LastActive => "ORDER BY tc.last_active_at DESC NULLS LAST, u.created_at DESC",
            Self::Email => "ORDER BY u.email ASC",
        }
    }
}

impl std::str::FromStr for AdminUserSort {
    type Err = ();
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "created" => Ok(Self::Created),
            "last_active" => Ok(Self::LastActive),
            "email" => Ok(Self::Email),
            _ => Err(()),
        }
    }
}

/// The user's most recent subscription, with the plan joined in.
///
/// Effective plan has to be *derived* rather than read: subscribing UPGRADES
/// the existing free row in place (see platform-api `billing::subscribe`), so
/// once that row is canceled there is no surviving free row to fall back to.
/// "free" is synthesised in the projection below.
const SUB_LATERAL: &str = "
    LEFT JOIN LATERAL (
        SELECT p.name AS plan_name, p.billing_model, s.status, s.canceled_at
          FROM subscriptions s JOIN plans p ON p.id = s.plan_id
         WHERE s.user_id = u.id
         ORDER BY s.created_at DESC
         LIMIT 1
    ) sub ON TRUE";

/// Token count and last-activity, as a lateral so no GROUP BY is needed.
const TOKEN_LATERAL: &str = "
    LEFT JOIN LATERAL (
        SELECT COUNT(*)::bigint AS token_count, MAX(t.last_used_at) AS last_active_at
          FROM tokens t WHERE t.user_id = u.id
    ) tc ON TRUE";

/// Projection shared by the list and detail queries.
const ADMIN_USER_COLS: &str = "
    u.id, u.email, u.display_name, u.email_verified, u.status, u.auth_method, u.created_at,
    tc.token_count,
    CASE WHEN sub.status = 'canceled' THEN 'free' ELSE sub.plan_name END AS plan_name,
    sub.status AS subscription_status,
    CASE WHEN sub.status = 'canceled' AND sub.billing_model <> 'free'
         THEN sub.plan_name END AS previous_plan_name,
    CASE WHEN sub.status = 'canceled' THEN sub.canceled_at END AS canceled_at,
    tc.last_active_at";

/// Filter predicates. Bind order: $1 search, $2 plan, $3 auth_method,
/// $4 status, $5 previously_paid.
const ADMIN_USER_FILTERS: &str = "
    WHERE ($1::text IS NULL OR u.email ILIKE '%' || $1 || '%')
      AND ($2::text IS NULL OR (CASE WHEN sub.status = 'canceled'
                                     THEN 'free' ELSE sub.plan_name END) = $2)
      AND ($3::text IS NULL OR u.auth_method = $3)
      AND ($4::text IS NULL OR u.status = $4)
      AND ($5::bool IS NOT TRUE OR (sub.status = 'canceled'
                                    AND sub.billing_model <> 'free'))";

/// Paginated, filterable list of users.
pub async fn list_admin_users(
    pool: &PgPool,
    limit: i64,
    offset: i64,
    filters: AdminUserFilters<'_>,
    sort: AdminUserSort,
) -> Result<Vec<AdminUser>> {
    let sql = format!(
        "SELECT {cols} FROM users u {tokens} {sub} {filters} {order} LIMIT $6 OFFSET $7",
        cols = ADMIN_USER_COLS,
        tokens = TOKEN_LATERAL,
        sub = SUB_LATERAL,
        filters = ADMIN_USER_FILTERS,
        order = sort.order_by(),
    );
    let rows: Vec<AdminUser> = sqlx::query_as(&sql)
        .bind(filters.search)
        .bind(filters.plan)
        .bind(filters.auth_method)
        .bind(filters.status)
        .bind(filters.previously_paid)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Total user count matching the same filters (for pagination).
///
/// Keeps the subscription lateral so the plan/previously-paid filters mean the
/// same thing here as they do in `list_admin_users`.
pub async fn count_admin_users(pool: &PgPool, filters: AdminUserFilters<'_>) -> Result<i64> {
    let sql = format!(
        "SELECT COUNT(*)::bigint FROM users u {sub} {filters}",
        sub = SUB_LATERAL,
        filters = ADMIN_USER_FILTERS,
    );
    let (count,): (i64,) = sqlx::query_as(&sql)
        .bind(filters.search)
        .bind(filters.plan)
        .bind(filters.auth_method)
        .bind(filters.status)
        .bind(filters.previously_paid)
        .fetch_one(pool)
        .await?;
    Ok(count)
}

/// Single user detail.
pub async fn get_admin_user(pool: &PgPool, user_id: &uuid::Uuid) -> Result<Option<AdminUser>> {
    let sql = format!(
        "SELECT {cols} FROM users u {tokens} {sub} WHERE u.id = $1",
        cols = ADMIN_USER_COLS,
        tokens = TOKEN_LATERAL,
        sub = SUB_LATERAL,
    );
    let row: Option<AdminUser> = sqlx::query_as(&sql)
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Update a user's status (e.g. "active" → "banned").
/// Returns `true` if the row was found and updated.
pub async fn set_user_status(pool: &PgPool, user_id: &uuid::Uuid, status: &str) -> Result<bool> {
    let rows = sqlx::query("UPDATE users SET status = $1 WHERE id = $2")
        .bind(status)
        .bind(user_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(rows > 0)
}

// ── admin plan helpers ────────────────────────────────────────────────────────

/// All plans with their live active-subscriber count.
pub async fn list_admin_plans(pool: &PgPool) -> Result<Vec<AdminPlan>> {
    let rows: Vec<AdminPlan> = sqlx::query_as(
        "SELECT p.id, p.name, p.billing_model, p.monthly_price_cents,
                p.max_tunnels, p.max_connections, p.rate_limit_rps,
                p.bandwidth_limit_gb, p.history_days, p.is_active,
                COUNT(s.id)::bigint AS subscriber_count
         FROM plans p
         LEFT JOIN subscriptions s ON s.plan_id = p.id AND s.status = 'active'
         GROUP BY p.id
         ORDER BY p.monthly_price_cents ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// ── admin metrics helpers ─────────────────────────────────────────────────────

/// One data point for the "users created over time" time-series.
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct UserOverTimeEntry {
    /// Date in `YYYY-MM-DD` format.
    pub date: String,
    /// Number of users created on that date.
    pub count: i64,
}

/// Return daily user-registration counts for the last `days` days (inclusive today).
///
/// Uses `generate_series` to fill in days with zero registrations, so the
/// caller always receives exactly `days` ascending data points.
pub async fn get_users_over_time(pool: &PgPool, days: i64) -> Result<Vec<UserOverTimeEntry>> {
    let rows: Vec<UserOverTimeEntry> = sqlx::query_as(
        "SELECT
             series.day::text AS date,
             COALESCE(COUNT(u.id), 0)::bigint AS count
         FROM generate_series(
             CURRENT_DATE - ($1 - 1) * INTERVAL '1 day',
             CURRENT_DATE,
             INTERVAL '1 day'
         ) AS series(day)
         LEFT JOIN users u
             ON DATE_TRUNC('day', u.created_at AT TIME ZONE 'UTC') = series.day
         GROUP BY series.day
         ORDER BY series.day ASC",
    )
    .bind(days)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// ── admin platform usage helpers ──────────────────────────────────────────────

/// Platform-wide aggregate metrics drawn from the shared tunnel_log and users tables.
/// Queries all regions via the shared PostgreSQL — no per-region fan-out needed.
#[derive(Debug, serde::Serialize)]
pub struct PlatformUsage {
    /// Open tunnels across all regions right now.
    pub active_tunnels_global: i64,
    /// Tunnel registrations since midnight UTC today (all regions).
    pub tunnels_today: i64,
    /// Distinct users with at least one open tunnel right now.
    pub active_users: i64,
    /// Bytes proxied since midnight UTC today (all regions).
    pub bandwidth_today_bytes: i64,
    /// Total registered user accounts.
    pub total_users: i64,
}

pub async fn get_platform_usage(pool: &PgPool) -> Result<PlatformUsage> {
    let (active_tunnels, tunnels_today, active_users, bandwidth_today, total_users) = tokio::join!(
        sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*)::bigint FROM tunnel_log WHERE unregistered_at IS NULL"
        )
        .fetch_one(pool),
        sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*)::bigint FROM tunnel_log WHERE registered_at >= CURRENT_DATE"
        )
        .fetch_one(pool),
        sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(DISTINCT user_id)::bigint FROM tunnel_log \
             WHERE unregistered_at IS NULL AND user_id IS NOT NULL"
        )
        .fetch_one(pool),
        sqlx::query_as::<_, (i64,)>(
            "SELECT COALESCE(SUM(bytes_proxied), 0)::bigint FROM tunnel_log \
             WHERE registered_at >= CURRENT_DATE"
        )
        .fetch_one(pool),
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*)::bigint FROM users").fetch_one(pool),
    );
    Ok(PlatformUsage {
        active_tunnels_global: active_tunnels?.0,
        tunnels_today: tunnels_today?.0,
        active_users: active_users?.0,
        bandwidth_today_bytes: bandwidth_today?.0,
        total_users: total_users?.0,
    })
}

// ── admin per-user tunnel / token helpers ─────────────────────────────────────

/// Paginated tunnel history for a single user (newest first).
pub async fn list_user_tunnels(
    pool: &PgPool,
    user_id: &uuid::Uuid,
    limit: i64,
    offset: i64,
) -> Result<Vec<AdminTunnelEntry>> {
    let rows: Vec<AdminTunnelEntry> = sqlx::query_as(
        "SELECT tl.id, tl.tunnel_id, tl.protocol, tl.label, tl.session_id,
                tl.token_id, t.label AS token_label,
                tl.registered_at, tl.unregistered_at, tl.region_id,
                tl.bytes_proxied, tl.bytes_in, tl.bytes_out, tl.request_count, tl.user_id
         FROM tunnel_log tl
         LEFT JOIN tokens t ON t.id::text = tl.token_id
         WHERE tl.user_id = $1
         ORDER BY tl.registered_at DESC
         LIMIT $2 OFFSET $3",
    )
    .bind(user_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Total tunnel_log rows for a given user (for pagination).
pub async fn count_user_tunnels(pool: &PgPool, user_id: &uuid::Uuid) -> Result<i64> {
    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*)::bigint FROM tunnel_log WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(pool)
            .await?;
    Ok(count)
}

/// All tokens owned by a user with their historical tunnel registration counts.
pub async fn list_user_tokens(pool: &PgPool, user_id: &uuid::Uuid) -> Result<Vec<TokenWithCount>> {
    let rows: Vec<TokenWithCount> = sqlx::query_as(
        "SELECT t.id, t.token_hash, t.label, t.created_at, t.last_used_at, t.scope,
                t.user_id, t.expires_at, t.tier, t.tunnel_limit, t.status, t.unlimited,
                COALESCE(COUNT(tl.id), 0)::bigint AS tunnel_count
         FROM tokens t
         LEFT JOIN tunnel_log tl ON tl.token_id = t.id::text
         WHERE t.user_id = $1
         GROUP BY t.id
         ORDER BY t.created_at DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
