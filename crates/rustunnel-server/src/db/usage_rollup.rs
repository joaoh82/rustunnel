//! Daily usage rollup and the admin reads that sit on top of it.
//!
//! Three regional edges run this binary against one Postgres. The recompute
//! deletes and reinserts every slice for the days it owns, so two writers
//! at once can wipe each other's rows. A transaction-scoped advisory lock
//! (`pg_try_advisory_xact_lock`) makes one edge the writer. The other two
//! skip the tick instead of waiting. The lock is released on commit, and
//! on disconnect if the process dies mid-job.
//!
//! Region-scoped rollups were the other option. They lose rows whose
//! `region_id` is NULL (everything from before multi-region), and they
//! make three edges each scan `tunnel_log` on the shared primary. One
//! locked pass covers every region, including the NULL ones.
//!
//! Byte rule, so a recompute is idempotent without a snapshot table:
//! a closed tunnel's counters land on the UTC day of its last open
//! microsecond (`unregistered_at` minus 1us, so a close at exactly
//! midnight stays on the day that had the session). An open tunnel's
//! flushed counters land on the UTC day of `now` minus 1us. Tunnel
//! seconds always use the overlap with the UTC day, and an open row
//! counts only up to `now`. Requests follow the byte rule.
//!
//! Legacy rows (before directional columns) have `bytes_in = 0`,
//! `bytes_out = 0`, and `bytes_proxied > 0`. That total is stored as
//! `bytes_direction_unknown`. The admin API returns in and out as JSON
//! null when a whole bucket is unknown.
//!
//! The timer recomputes today and yesterday (UTC) only. Older days are
//! filled by `rustunnel-server backfill`, which is not started from the
//! server path.

use chrono::{DateTime, Days, NaiveDate, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::{Error, Result};

/// Nil UUID stored when `tunnel_log.user_id` is NULL.
/// Not a row in `users`. Admin user counts and the leaderboard skip it.
pub const ANONYMOUS_USER_ID: Uuid = Uuid::nil();

/// `pg_try_advisory_xact_lock` key. Distinct from sqlx's migrator lock,
/// which is a hash of the database name. Only has to be stable.
pub const USAGE_ROLLUP_LOCK_KEY: i64 = 0x5254_5553_4147_4501;

/// How a leaderboard is ordered. The string values are the query param.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaderboardSort {
    Bandwidth,
    TunnelHours,
    PeakConcurrent,
}

impl LeaderboardSort {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "bandwidth" => Some(Self::Bandwidth),
            "tunnel_hours" => Some(Self::TunnelHours),
            "peak_concurrent" => Some(Self::PeakConcurrent),
            _ => None,
        }
    }

    fn order_sql(self) -> &'static str {
        match self {
            Self::Bandwidth => "bytes_total DESC, user_id ASC",
            Self::TunnelHours => "tunnel_seconds DESC, user_id ASC",
            Self::PeakConcurrent => "peak_concurrent_tunnels DESC, user_id ASC",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollupApplied {
    pub days: i32,
    pub rows: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollupOutcome {
    Applied(RollupApplied),
    /// Another edge (or a backfill) holds [`USAGE_ROLLUP_LOCK_KEY`].
    SkippedBusy,
}

/// UTC date window the timer recomputes. Lookback is clamped to 2..=90
/// so the timer always refreshes today and yesterday, and a bad config
/// cannot scan a decade every tick. The backfill command does not use
/// this clamp.
pub fn rollup_window(now: DateTime<Utc>, lookback_days: u32) -> (NaiveDate, NaiveDate) {
    let days = lookback_days.clamp(2, 90);
    let end = now.date_naive();
    let start = end
        .checked_sub_days(Days::new(u64::from(days - 1)))
        .unwrap_or(end);
    (start, end)
}

/// Recompute the timer window ending at `now`.
pub async fn rollup_recent(
    pool: &PgPool,
    lookback_days: u32,
    now: DateTime<Utc>,
) -> Result<RollupOutcome> {
    let (from, to) = rollup_window(now, lookback_days);
    rollup_usage_between(pool, from, to, now).await
}

/// Delete and reinsert `usage_daily` for each UTC day in `from..=to`.
///
/// One transaction, one advisory lock, all regions. Running it twice
/// with the same `tunnel_log` contents yields the same rows.
pub async fn rollup_usage_between(
    pool: &PgPool,
    from: NaiveDate,
    to: NaiveDate,
    now: DateTime<Utc>,
) -> Result<RollupOutcome> {
    if from > to {
        return Err(Error::Config(format!(
            "usage rollup range starts {from} after it ends {to}"
        )));
    }

    let mut tx = pool.begin().await?;
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1::bigint)")
        .bind(USAGE_ROLLUP_LOCK_KEY)
        .fetch_one(&mut *tx)
        .await?;
    if !locked {
        tx.rollback().await?;
        return Ok(RollupOutcome::SkippedBusy);
    }

    sqlx::query("DELETE FROM usage_daily WHERE day >= $1 AND day <= $2")
        .bind(from)
        .bind(to)
        .execute(&mut *tx)
        .await?;

    let rows = sqlx::query(ROLLUP_INSERT_SQL)
        .bind(from)
        .bind(to)
        .bind(now)
        .bind(ANONYMOUS_USER_ID)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    tx.commit().await?;

    let days = i32::try_from((to - from).num_days() + 1).unwrap_or(i32::MAX);
    Ok(RollupOutcome::Applied(RollupApplied { days, rows }))
}

/// In and out are unknown when the whole bucket is legacy proxied bytes.
pub fn direction_pair(bytes_in: i64, bytes_out: i64, unknown: i64) -> (Option<i64>, Option<i64>) {
    if unknown > 0 && bytes_in == 0 && bytes_out == 0 {
        (None, None)
    } else {
        (Some(bytes_in), Some(bytes_out))
    }
}

fn test_filter_sql(exclude_test: bool) -> &'static str {
    if exclude_test {
        "(u.email IS NULL OR NOT (u.email ILIKE 'joaoh82+%' OR lower(u.email) = 'joao.silva@carv.com'))"
    } else {
        "TRUE"
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct DailyAggRow {
    day: NaiveDate,
    sessions: i64,
    tunnel_seconds: i64,
    active_users: i64,
    requests: i64,
    bytes_in: i64,
    bytes_out: i64,
    bytes_total: i64,
    bytes_direction_unknown: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UsageDailyPoint {
    pub day: NaiveDate,
    pub sessions: i64,
    pub tunnel_seconds: i64,
    pub tunnel_hours: f64,
    pub active_users: i64,
    pub requests: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UsageBandwidthPoint {
    pub day: NaiveDate,
    pub bytes_in: Option<i64>,
    pub bytes_out: Option<i64>,
    pub bytes_total: i64,
    pub bytes_direction_unknown: i64,
}

async fn load_daily(
    pool: &PgPool,
    from: NaiveDate,
    to: NaiveDate,
    exclude_test: bool,
) -> Result<Vec<DailyAggRow>> {
    let sql = format!(
        "WITH agg AS (
            SELECT
                d.day AS day,
                COALESCE(SUM(d.sessions), 0)::bigint AS sessions,
                COALESCE(SUM(d.tunnel_seconds), 0)::bigint AS tunnel_seconds,
                COUNT(DISTINCT d.user_id) FILTER (
                    WHERE d.user_id <> $3 AND d.sessions > 0
                )::bigint AS active_users,
                COALESCE(SUM(d.requests), 0)::bigint AS requests,
                COALESCE(SUM(d.bytes_in), 0)::bigint AS bytes_in,
                COALESCE(SUM(d.bytes_out), 0)::bigint AS bytes_out,
                COALESCE(SUM(d.bytes_proxied), 0)::bigint AS bytes_total,
                COALESCE(SUM(d.bytes_direction_unknown), 0)::bigint AS bytes_direction_unknown
            FROM usage_daily d
            LEFT JOIN users u ON u.id = d.user_id
            LEFT JOIN tokens t ON t.id = d.token_id
            WHERE d.day >= $1 AND d.day <= $2
              AND {test}
              AND (t.system IS NOT TRUE)
            GROUP BY d.day
        )
        SELECT
            series.day::date AS day,
            COALESCE(agg.sessions, 0)::bigint AS sessions,
            COALESCE(agg.tunnel_seconds, 0)::bigint AS tunnel_seconds,
            COALESCE(agg.active_users, 0)::bigint AS active_users,
            COALESCE(agg.requests, 0)::bigint AS requests,
            COALESCE(agg.bytes_in, 0)::bigint AS bytes_in,
            COALESCE(agg.bytes_out, 0)::bigint AS bytes_out,
            COALESCE(agg.bytes_total, 0)::bigint AS bytes_total,
            COALESCE(agg.bytes_direction_unknown, 0)::bigint AS bytes_direction_unknown
        FROM generate_series($1::timestamp, $2::timestamp, interval '1 day') AS series(day)
        LEFT JOIN agg ON agg.day = series.day::date
        ORDER BY series.day",
        test = test_filter_sql(exclude_test)
    );
    let rows = sqlx::query_as(&sql)
        .bind(from)
        .bind(to)
        .bind(ANONYMOUS_USER_ID)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

fn hours(seconds: i64) -> f64 {
    seconds as f64 / 3600.0
}

pub async fn usage_daily_series(
    pool: &PgPool,
    from: NaiveDate,
    to: NaiveDate,
    exclude_test: bool,
) -> Result<Vec<UsageDailyPoint>> {
    let rows = load_daily(pool, from, to, exclude_test).await?;
    Ok(rows
        .into_iter()
        .map(|row| UsageDailyPoint {
            day: row.day,
            sessions: row.sessions,
            tunnel_seconds: row.tunnel_seconds,
            tunnel_hours: hours(row.tunnel_seconds),
            active_users: row.active_users,
            requests: row.requests,
        })
        .collect())
}

pub async fn usage_bandwidth_series(
    pool: &PgPool,
    from: NaiveDate,
    to: NaiveDate,
    exclude_test: bool,
) -> Result<Vec<UsageBandwidthPoint>> {
    let rows = load_daily(pool, from, to, exclude_test).await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let (bytes_in, bytes_out) =
                direction_pair(row.bytes_in, row.bytes_out, row.bytes_direction_unknown);
            UsageBandwidthPoint {
                day: row.day,
                bytes_in,
                bytes_out,
                bytes_total: row.bytes_total,
                bytes_direction_unknown: row.bytes_direction_unknown,
            }
        })
        .collect())
}

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct TokenCreatedPoint {
    pub day: NaiveDate,
    pub count: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TokenUsageStats {
    pub total: i64,
    pub active_last_7_days: i64,
    /// "Active" means `tokens.last_used_at` is within the last 7 days
    /// (a successful token verify), not that the token opened a tunnel.
    pub created_per_day: Vec<TokenCreatedPoint>,
}

pub async fn token_usage_stats(
    pool: &PgPool,
    from: NaiveDate,
    to: NaiveDate,
    exclude_test: bool,
) -> Result<TokenUsageStats> {
    let test = test_filter_sql(exclude_test);
    let totals_sql = format!(
        "SELECT
            COUNT(*)::bigint AS total,
            COUNT(*) FILTER (
                WHERE t.last_used_at >= now() - interval '7 days'
            )::bigint AS active_last_7_days
         FROM tokens t
         LEFT JOIN users u ON u.id = t.user_id
         WHERE t.system = FALSE
           AND {test}"
    );
    let (total, active_last_7_days): (i64, i64) =
        sqlx::query_as(&totals_sql).fetch_one(pool).await?;

    let series_sql = format!(
        "WITH created AS (
            SELECT (t.created_at AT TIME ZONE 'UTC')::date AS day, COUNT(*)::bigint AS count
            FROM tokens t
            LEFT JOIN users u ON u.id = t.user_id
            WHERE t.system = FALSE
              AND {test}
              AND t.created_at >= ($1::timestamp AT TIME ZONE 'UTC')
              AND t.created_at < (($2::date + 1)::timestamp AT TIME ZONE 'UTC')
            GROUP BY 1
        )
        SELECT series.day::date AS day, COALESCE(created.count, 0)::bigint AS count
        FROM generate_series($1::timestamp, $2::timestamp, interval '1 day') AS series(day)
        LEFT JOIN created ON created.day = series.day::date
        ORDER BY series.day"
    );
    let created_per_day = sqlx::query_as(&series_sql)
        .bind(from)
        .bind(to)
        .fetch_all(pool)
        .await?;

    Ok(TokenUsageStats {
        total,
        active_last_7_days,
        created_per_day,
    })
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct UserAggRaw {
    user_id: Uuid,
    email: String,
    sessions: i64,
    tunnel_seconds: i64,
    bytes_in: i64,
    bytes_out: i64,
    bytes_total: i64,
    bytes_direction_unknown: i64,
    requests: i64,
    peak_concurrent_tunnels: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UsageByUser {
    pub user_id: Uuid,
    pub email: String,
    pub sessions: i64,
    pub tunnel_seconds: i64,
    pub tunnel_hours: f64,
    pub bytes_in: Option<i64>,
    pub bytes_out: Option<i64>,
    pub bytes_total: i64,
    pub bytes_direction_unknown: i64,
    pub requests: i64,
    pub peak_concurrent_tunnels: i64,
}

fn map_user(raw: UserAggRaw) -> UsageByUser {
    let (bytes_in, bytes_out) =
        direction_pair(raw.bytes_in, raw.bytes_out, raw.bytes_direction_unknown);
    UsageByUser {
        user_id: raw.user_id,
        email: raw.email,
        sessions: raw.sessions,
        tunnel_seconds: raw.tunnel_seconds,
        tunnel_hours: hours(raw.tunnel_seconds),
        bytes_in,
        bytes_out,
        bytes_total: raw.bytes_total,
        bytes_direction_unknown: raw.bytes_direction_unknown,
        requests: raw.requests,
        peak_concurrent_tunnels: raw.peak_concurrent_tunnels,
    }
}

fn user_group_sql(exclude_test: bool) -> String {
    format!(
        "SELECT
            d.user_id,
            u.email,
            COALESCE(SUM(d.sessions), 0)::bigint AS sessions,
            COALESCE(SUM(d.tunnel_seconds), 0)::bigint AS tunnel_seconds,
            COALESCE(SUM(d.bytes_in), 0)::bigint AS bytes_in,
            COALESCE(SUM(d.bytes_out), 0)::bigint AS bytes_out,
            COALESCE(SUM(d.bytes_proxied), 0)::bigint AS bytes_total,
            COALESCE(SUM(d.bytes_direction_unknown), 0)::bigint AS bytes_direction_unknown,
            COALESCE(SUM(d.requests), 0)::bigint AS requests,
            COALESCE(MAX(d.user_peak_concurrent), 0)::bigint AS peak_concurrent_tunnels
         FROM usage_daily d
         JOIN users u ON u.id = d.user_id
         LEFT JOIN tokens t ON t.id = d.token_id
         WHERE d.day >= $1 AND d.day <= $2
           AND {test}
           AND (t.system IS NOT TRUE)
         GROUP BY d.user_id, u.email",
        test = test_filter_sql(exclude_test)
    )
}

pub async fn usage_by_user(
    pool: &PgPool,
    from: NaiveDate,
    to: NaiveDate,
    exclude_test: bool,
    limit: i64,
    offset: i64,
) -> Result<(Vec<UsageByUser>, i64)> {
    let grouped = user_group_sql(exclude_test);
    let list_sql = format!("{grouped} ORDER BY bytes_total DESC, d.user_id ASC LIMIT $3 OFFSET $4");
    let count_sql = format!("SELECT COUNT(*)::bigint FROM ({grouped}) counted");
    let (rows, total) = tokio::join!(
        sqlx::query_as::<_, UserAggRaw>(&list_sql)
            .bind(from)
            .bind(to)
            .bind(limit)
            .bind(offset)
            .fetch_all(pool),
        sqlx::query_scalar::<_, i64>(&count_sql)
            .bind(from)
            .bind(to)
            .fetch_one(pool),
    );
    Ok((rows?.into_iter().map(map_user).collect(), total?))
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct TokenAggRaw {
    token_id: String,
    token_label: String,
    user_id: Option<Uuid>,
    email: Option<String>,
    sessions: i64,
    tunnel_seconds: i64,
    bytes_in: i64,
    bytes_out: i64,
    bytes_total: i64,
    bytes_direction_unknown: i64,
    requests: i64,
    peak_concurrent_tunnels: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UsageByToken {
    pub token_id: String,
    pub token_label: String,
    pub user_id: Option<Uuid>,
    pub email: Option<String>,
    pub sessions: i64,
    pub tunnel_seconds: i64,
    pub tunnel_hours: f64,
    pub bytes_in: Option<i64>,
    pub bytes_out: Option<i64>,
    pub bytes_total: i64,
    pub bytes_direction_unknown: i64,
    pub requests: i64,
    pub peak_concurrent_tunnels: i64,
}

fn token_group_sql(exclude_test: bool) -> String {
    format!(
        "SELECT
            d.token_id,
            COALESCE(t.label, '') AS token_label,
            t.user_id AS user_id,
            u.email AS email,
            COALESCE(SUM(d.sessions), 0)::bigint AS sessions,
            COALESCE(SUM(d.tunnel_seconds), 0)::bigint AS tunnel_seconds,
            COALESCE(SUM(d.bytes_in), 0)::bigint AS bytes_in,
            COALESCE(SUM(d.bytes_out), 0)::bigint AS bytes_out,
            COALESCE(SUM(d.bytes_proxied), 0)::bigint AS bytes_total,
            COALESCE(SUM(d.bytes_direction_unknown), 0)::bigint AS bytes_direction_unknown,
            COALESCE(SUM(d.requests), 0)::bigint AS requests,
            COALESCE(MAX(d.token_peak_concurrent), 0)::bigint AS peak_concurrent_tunnels
         FROM usage_daily d
         JOIN tokens t ON t.id = d.token_id
         LEFT JOIN users u ON u.id = t.user_id
         WHERE d.day >= $1 AND d.day <= $2
           AND t.system = FALSE
           AND d.token_id <> ''
           AND {test}
         GROUP BY d.token_id, t.label, t.user_id, u.email",
        test = test_filter_sql(exclude_test)
    )
}

pub async fn usage_by_token(
    pool: &PgPool,
    from: NaiveDate,
    to: NaiveDate,
    exclude_test: bool,
    limit: i64,
    offset: i64,
) -> Result<(Vec<UsageByToken>, i64)> {
    let grouped = token_group_sql(exclude_test);
    let list_sql =
        format!("{grouped} ORDER BY bytes_total DESC, d.token_id ASC LIMIT $3 OFFSET $4");
    let count_sql = format!("SELECT COUNT(*)::bigint FROM ({grouped}) counted");
    let (rows, total) = tokio::join!(
        sqlx::query_as::<_, TokenAggRaw>(&list_sql)
            .bind(from)
            .bind(to)
            .bind(limit)
            .bind(offset)
            .fetch_all(pool),
        sqlx::query_scalar::<_, i64>(&count_sql)
            .bind(from)
            .bind(to)
            .fetch_one(pool),
    );
    let entries = rows?
        .into_iter()
        .map(|raw| {
            let (bytes_in, bytes_out) =
                direction_pair(raw.bytes_in, raw.bytes_out, raw.bytes_direction_unknown);
            UsageByToken {
                token_id: raw.token_id,
                token_label: raw.token_label,
                user_id: raw.user_id,
                email: raw.email,
                sessions: raw.sessions,
                tunnel_seconds: raw.tunnel_seconds,
                tunnel_hours: hours(raw.tunnel_seconds),
                bytes_in,
                bytes_out,
                bytes_total: raw.bytes_total,
                bytes_direction_unknown: raw.bytes_direction_unknown,
                requests: raw.requests,
                peak_concurrent_tunnels: raw.peak_concurrent_tunnels,
            }
        })
        .collect();
    Ok((entries, total?))
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UsageLeaderboardEntry {
    pub user_id: Uuid,
    pub email: String,
    pub sessions: i64,
    pub tunnel_seconds: i64,
    pub tunnel_hours: f64,
    pub bytes_in: Option<i64>,
    pub bytes_out: Option<i64>,
    pub bytes_total: i64,
    pub bytes_direction_unknown: i64,
    pub requests: i64,
    pub peak_concurrent_tunnels: i64,
}

pub async fn usage_leaderboard(
    pool: &PgPool,
    from: NaiveDate,
    to: NaiveDate,
    exclude_test: bool,
    sort: LeaderboardSort,
    limit: i64,
) -> Result<Vec<UsageLeaderboardEntry>> {
    let grouped = user_group_sql(exclude_test);
    let sql = format!(
        "{grouped} ORDER BY {order} LIMIT $3",
        order = sort.order_sql()
    );
    let rows = sqlx::query_as::<_, UserAggRaw>(&sql)
        .bind(from)
        .bind(to)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|raw| {
            let user = map_user(raw);
            UsageLeaderboardEntry {
                user_id: user.user_id,
                email: user.email,
                sessions: user.sessions,
                tunnel_seconds: user.tunnel_seconds,
                tunnel_hours: user.tunnel_hours,
                bytes_in: user.bytes_in,
                bytes_out: user.bytes_out,
                bytes_total: user.bytes_total,
                bytes_direction_unknown: user.bytes_direction_unknown,
                requests: user.requests,
                peak_concurrent_tunnels: user.peak_concurrent_tunnels,
            }
        })
        .collect())
}

const ROLLUP_INSERT_SQL: &str = "\
WITH bounds AS (
    SELECT
        gs::date AS day,
        (gs::timestamp AT TIME ZONE 'UTC') AS start_at,
        ((gs::date + 1)::timestamp AT TIME ZONE 'UTC') AS end_at
    FROM generate_series($1::timestamp, $2::timestamp, interval '1 day') AS gs
),
clipped AS (
    SELECT
        b.day,
        COALESCE(tl.user_id, $4) AS user_id,
        COALESCE(tl.token_id, '') AS token_id,
        COALESCE(tl.region_id, '') AS region_id,
        tl.protocol,
        GREATEST(tl.registered_at, b.start_at) AS overlap_start,
        LEAST(COALESCE(tl.unregistered_at, $3), b.end_at, $3) AS overlap_end,
        (
            (
                tl.unregistered_at IS NOT NULL
                AND ((tl.unregistered_at - interval '1 microsecond') AT TIME ZONE 'UTC')::date = b.day
            )
            OR (
                tl.unregistered_at IS NULL
                AND (($3 - interval '1 microsecond') AT TIME ZONE 'UTC')::date = b.day
            )
        ) AS attribute_bytes,
        tl.bytes_in,
        tl.bytes_out,
        tl.bytes_proxied,
        tl.request_count
    FROM tunnel_log tl
    JOIN bounds b
      ON tl.registered_at < b.end_at
     AND COALESCE(tl.unregistered_at, $3) > b.start_at
    WHERE tl.registered_at < (($2::date + 1)::timestamp AT TIME ZONE 'UTC')
      AND COALESCE(tl.unregistered_at, $3) > ($1::timestamp AT TIME ZONE 'UTC')
      AND (tl.unregistered_at IS NULL OR tl.unregistered_at > tl.registered_at)
),
overlapping AS (
    SELECT * FROM clipped WHERE overlap_end > overlap_start
),
slice_events AS (
    SELECT day, user_id, token_id, region_id, protocol, overlap_start AS ts, 1 AS delta
    FROM overlapping
    UNION ALL
    SELECT day, user_id, token_id, region_id, protocol, overlap_end, -1
    FROM overlapping
),
slice_peaks AS (
    SELECT day, user_id, token_id, region_id, protocol,
           MAX(concurrent)::int AS peak_concurrent_tunnels
    FROM (
        SELECT day, user_id, token_id, region_id, protocol,
               SUM(delta) OVER (
                   PARTITION BY day, user_id, token_id, region_id, protocol
                   ORDER BY ts, delta
                   ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
               ) AS concurrent
        FROM slice_events
    ) running
    GROUP BY 1, 2, 3, 4, 5
),
token_peaks AS (
    SELECT day, user_id, token_id, MAX(concurrent)::int AS token_peak_concurrent
    FROM (
        SELECT day, user_id, token_id,
               SUM(delta) OVER (
                   PARTITION BY day, user_id, token_id
                   ORDER BY ts, delta
                   ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
               ) AS concurrent
        FROM (
            SELECT day, user_id, token_id, overlap_start AS ts, 1 AS delta FROM overlapping
            UNION ALL
            SELECT day, user_id, token_id, overlap_end, -1 FROM overlapping
        ) events
    ) running
    GROUP BY 1, 2, 3
),
user_peaks AS (
    SELECT day, user_id, MAX(concurrent)::int AS user_peak_concurrent
    FROM (
        SELECT day, user_id,
               SUM(delta) OVER (
                   PARTITION BY day, user_id
                   ORDER BY ts, delta
                   ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
               ) AS concurrent
        FROM (
            SELECT day, user_id, overlap_start AS ts, 1 AS delta
            FROM overlapping
            WHERE user_id <> $4
            UNION ALL
            SELECT day, user_id, overlap_end, -1
            FROM overlapping
            WHERE user_id <> $4
        ) events
    ) running
    GROUP BY 1, 2
),
agg AS (
    SELECT
        day, user_id, token_id, region_id, protocol,
        COUNT(*)::int AS sessions,
        COALESCE(SUM(EXTRACT(EPOCH FROM (overlap_end - overlap_start))), 0)::bigint AS tunnel_seconds,
        COALESCE(SUM(CASE WHEN attribute_bytes THEN bytes_in ELSE 0 END), 0)::bigint AS bytes_in,
        COALESCE(SUM(CASE WHEN attribute_bytes THEN bytes_out ELSE 0 END), 0)::bigint AS bytes_out,
        COALESCE(SUM(CASE WHEN attribute_bytes THEN bytes_proxied ELSE 0 END), 0)::bigint AS bytes_proxied,
        COALESCE(SUM(CASE
            WHEN attribute_bytes AND bytes_in = 0 AND bytes_out = 0 AND bytes_proxied > 0
            THEN bytes_proxied ELSE 0 END), 0)::bigint AS bytes_direction_unknown,
        COALESCE(SUM(CASE WHEN attribute_bytes THEN request_count ELSE 0 END), 0)::bigint AS requests
    FROM overlapping
    GROUP BY day, user_id, token_id, region_id, protocol
)
INSERT INTO usage_daily (
    day, user_id, token_id, region_id, protocol,
    sessions, tunnel_seconds,
    bytes_in, bytes_out, bytes_proxied, bytes_direction_unknown,
    requests, peak_concurrent_tunnels, token_peak_concurrent, user_peak_concurrent,
    updated_at
)
SELECT
    a.day, a.user_id, a.token_id, a.region_id, a.protocol,
    a.sessions, a.tunnel_seconds,
    a.bytes_in, a.bytes_out, a.bytes_proxied, a.bytes_direction_unknown,
    a.requests,
    p.peak_concurrent_tunnels,
    tp.token_peak_concurrent,
    COALESCE(up.user_peak_concurrent, 0),
    $3
FROM agg a
JOIN slice_peaks p USING (day, user_id, token_id, region_id, protocol)
JOIN token_peaks tp USING (day, user_id, token_id)
LEFT JOIN user_peaks up USING (day, user_id)
";

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use chrono::TimeZone;
    use http_body_util::BodyExt;
    use sqlx::Acquire;
    use tokio::sync::RwLock;
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::*;
    use crate::audit::noop_audit;
    use crate::config::{DatabaseSection, RegionSection};
    use crate::core::TunnelCore;
    use crate::dashboard::api::{router, ApiState};
    use crate::db;

    /// Rollup tests share one advisory lock. Keep them off each other.
    static ROLLUP_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn ts(y: i32, m: u32, d: u32, hh: u32, mm: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, hh, mm, 0).unwrap()
    }

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    async fn pool() -> sqlx::PgPool {
        let url = std::env::var("TEST_DATABASE_URL")
            .unwrap_or_else(|_| "postgres://rustunnel:test@127.0.0.1:5432/rustunnel_test".into());
        let db = db::init_db(&DatabaseSection {
            url,
            captured_path: ":memory:".into(),
        })
        .await
        .expect("init_db");
        db.pg
    }

    async fn full_db() -> db::Db {
        let url = std::env::var("TEST_DATABASE_URL")
            .unwrap_or_else(|_| "postgres://rustunnel:test@127.0.0.1:5432/rustunnel_test".into());
        db::init_db(&DatabaseSection {
            url,
            captured_path: ":memory:".into(),
        })
        .await
        .expect("init_db")
    }

    #[test]
    fn timer_window_always_includes_yesterday() {
        let now = ts(2001, 3, 2, 6, 30);
        assert_eq!(rollup_window(now, 2), (day(2001, 3, 1), day(2001, 3, 2)));
        assert_eq!(rollup_window(now, 0), (day(2001, 3, 1), day(2001, 3, 2)));
        assert_eq!(rollup_window(now, 1), (day(2001, 3, 1), day(2001, 3, 2)));
        assert_eq!(rollup_window(now, 3), (day(2001, 2, 28), day(2001, 3, 2)));
        let (from, to) = rollup_window(now, 10_000);
        assert_eq!(to, day(2001, 3, 2));
        assert_eq!((to - from).num_days(), 89);
    }

    #[test]
    fn unknown_direction_is_null_only_when_the_whole_bucket_is_legacy() {
        assert_eq!(direction_pair(0, 0, 500), (None, None));
        assert_eq!(direction_pair(0, 0, 0), (Some(0), Some(0)));
        assert_eq!(direction_pair(10, 4, 0), (Some(10), Some(4)));
        assert_eq!(direction_pair(10, 0, 500), (Some(10), Some(0)));
    }

    #[tokio::test]
    async fn migration_is_idempotent_and_concurrent_index_cannot_run_in_a_transaction() {
        let pool = pool().await;
        let sql = include_str!("../../migrations/pg/20261103143000_usage_daily.sql");
        assert!(
            !sql.starts_with("-- no-transaction"),
            "the file must stay inside sqlx's transaction"
        );

        let (version,): (i64,) = sqlx::query_as(
            "SELECT version FROM _sqlx_migrations WHERE version = 20261103143000 AND success",
        )
        .fetch_one(&pool)
        .await
        .expect("migration recorded");
        assert_eq!(version, 20_261_103_143_000);

        sqlx::raw_sql(sql)
            .execute(&pool)
            .await
            .expect("replay once");
        sqlx::raw_sql(sql)
            .execute(&pool)
            .await
            .expect("replay twice");

        let names: Vec<String> = sqlx::query_scalar(
            "SELECT indexname FROM pg_indexes WHERE schemaname = 'public' AND indexname = ANY($1)",
        )
        .bind(vec![
            "idx_tunnel_log_unregistered",
            "idx_tunnel_log_token",
            "idx_tunnel_log_open_registered",
            "idx_tokens_created_at",
            "idx_tokens_last_used_at",
            "idx_usage_daily_user_day",
            "idx_usage_daily_token_day",
            "usage_daily_pkey",
        ])
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(names.len(), 8, "{names:?}");

        let mut tx = pool.begin().await.unwrap();
        let err = sqlx::query(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_usage_daily_concurrent_probe \
             ON usage_daily (updated_at)",
        )
        .execute(&mut *tx)
        .await
        .expect_err("CONCURRENTLY inside a transaction");
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains("concurrently"),
            "expected a transaction-block rejection, got {msg}"
        );
    }

    #[derive(sqlx::FromRow)]
    struct Slice {
        token_id: String,
        region_id: String,
        protocol: String,
        sessions: i32,
        tunnel_seconds: i64,
        bytes_in: i64,
        bytes_out: i64,
        bytes_proxied: i64,
        bytes_direction_unknown: i64,
        requests: i64,
        peak_concurrent_tunnels: i32,
        token_peak_concurrent: i32,
        user_peak_concurrent: i32,
    }

    async fn slices(pool: &sqlx::PgPool, on: NaiveDate, user: Uuid) -> Vec<Slice> {
        sqlx::query_as(
            "SELECT token_id, region_id, protocol, sessions, tunnel_seconds, \
                    bytes_in, bytes_out, bytes_proxied, bytes_direction_unknown, requests, \
                    peak_concurrent_tunnels, token_peak_concurrent, user_peak_concurrent \
             FROM usage_daily \
             WHERE day = $1 AND user_id = $2 \
             ORDER BY token_id, region_id, protocol",
        )
        .bind(on)
        .bind(user)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_log(
        pool: &sqlx::PgPool,
        user: Option<Uuid>,
        token: Option<&str>,
        region: Option<&str>,
        protocol: &str,
        start: DateTime<Utc>,
        end: Option<DateTime<Utc>>,
        bytes_in: i64,
        bytes_out: i64,
        bytes_proxied: i64,
        requests: i64,
    ) -> String {
        let id = Uuid::new_v4().to_string();
        if let Some(token_id) = token {
            sqlx::query(
                "INSERT INTO tokens (id, token_hash, label, created_at) \
                 VALUES ($1, $1, 'rollup', now()) \
                 ON CONFLICT (id) DO NOTHING",
            )
            .bind(token_id)
            .execute(pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO tunnel_log (
                id, tunnel_id, protocol, label, session_id, token_id,
                registered_at, unregistered_at, region_id, user_id,
                bytes_in, bytes_out, bytes_proxied, request_count
             ) VALUES (
                $1, $1, $2, 'rollup', $1, $3, $4, $5, $6, $7, $8, $9, $10, $11
             )",
        )
        .bind(&id)
        .bind(protocol)
        .bind(token)
        .bind(start)
        .bind(end)
        .bind(region)
        .bind(user)
        .bind(bytes_in)
        .bind(bytes_out)
        .bind(bytes_proxied)
        .bind(requests)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn rollup_handles_open_tunnels_day_boundary_and_is_idempotent() {
        let _guard = ROLLUP_TEST_LOCK.lock().await;
        let pool = pool().await;
        let user_a = Uuid::new_v4();
        let user_c = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users (id, email, password_hash) VALUES ($1, $2, 'x'), ($3, $4, 'x')",
        )
        .bind(user_a)
        .bind(format!("rollup-a-{user_a}@example.com"))
        .bind(user_c)
        .bind(format!("rollup-c-{user_c}@example.com"))
        .execute(&pool)
        .await
        .unwrap();

        let from = day(2001, 3, 1);
        let to = day(2001, 3, 2);
        let now = ts(2001, 3, 2, 6, 30);
        sqlx::query("DELETE FROM usage_daily WHERE day >= $1 AND day <= $2")
            .bind(from)
            .bind(to)
            .execute(&pool)
            .await
            .unwrap();
        // These dates are in the past so live open rows (registered_at = now)
        // fall outside the window. Drop leftovers from an earlier run of this test.
        sqlx::query(
            "DELETE FROM tunnel_log \
             WHERE registered_at >= ($1::timestamp AT TIME ZONE 'UTC') \
               AND registered_at < (($2::date + 1)::timestamp AT TIME ZONE 'UTC')",
        )
        .bind(from)
        .bind(to)
        .execute(&pool)
        .await
        .unwrap();

        // Open across the boundary. Seconds stop at `now`. Bytes land on today.
        let open_id = insert_log(
            &pool,
            Some(user_a),
            Some("ta"),
            Some("eu"),
            "http",
            ts(2001, 3, 1, 22, 0),
            None,
            10,
            4,
            14,
            2,
        )
        .await;
        // Closed across midnight. Bytes on the close day.
        let cross_id = insert_log(
            &pool,
            Some(user_a),
            Some("ta"),
            Some("eu"),
            "http",
            ts(2001, 3, 1, 23, 0),
            Some(ts(2001, 3, 2, 1, 0)),
            100,
            25,
            125,
            5,
        )
        .await;
        // Legacy row: direction unknown, same user, other slice, no overlap with the pair above.
        let legacy_id = insert_log(
            &pool,
            Some(user_a),
            Some("ta"),
            Some("us"),
            "tcp",
            ts(2001, 3, 1, 10, 0),
            Some(ts(2001, 3, 1, 12, 0)),
            0,
            0,
            500,
            8,
        )
        .await;
        // Closes at exactly the next midnight. Bytes stay on the day that had the session.
        // NULL region and NULL user must still be rolled up.
        let midnight_id = insert_log(
            &pool,
            None,
            None,
            None,
            "udp",
            ts(2001, 3, 1, 0, 0),
            Some(ts(2001, 3, 2, 0, 0)),
            1,
            1,
            2,
            1,
        )
        .await;
        // Two regions at once. Slice peak is 1. User and token peak are 2.
        let eu_id = insert_log(
            &pool,
            Some(user_c),
            Some("tc"),
            Some("eu"),
            "http",
            ts(2001, 3, 1, 15, 0),
            Some(ts(2001, 3, 1, 16, 0)),
            1,
            1,
            2,
            1,
        )
        .await;
        let us_id = insert_log(
            &pool,
            Some(user_c),
            Some("tc"),
            Some("us"),
            "http",
            ts(2001, 3, 1, 15, 0),
            Some(ts(2001, 3, 1, 16, 0)),
            1,
            1,
            2,
            1,
        )
        .await;

        let applied = match rollup_usage_between(&pool, from, to, now).await.unwrap() {
            RollupOutcome::Applied(applied) => applied,
            RollupOutcome::SkippedBusy => panic!("lock should be free"),
        };
        assert_eq!(applied.days, 2);
        assert!(applied.rows >= 4, "rows {}", applied.rows);

        let day1 = slices(&pool, from, user_a).await;
        let http = day1.iter().find(|s| s.protocol == "http").unwrap();
        assert_eq!(http.sessions, 2);
        assert_eq!(http.tunnel_seconds, 7_200 + 3_600);
        assert_eq!(
            http.bytes_proxied, 0,
            "open and not-yet-closed bytes wait for the close day"
        );
        assert_eq!(http.peak_concurrent_tunnels, 2);
        assert_eq!(http.user_peak_concurrent, 2);
        let legacy = day1.iter().find(|s| s.protocol == "tcp").unwrap();
        assert_eq!(legacy.tunnel_seconds, 7_200);
        assert_eq!(legacy.bytes_in, 0);
        assert_eq!(legacy.bytes_out, 0);
        assert_eq!(legacy.bytes_proxied, 500);
        assert_eq!(legacy.bytes_direction_unknown, 500);
        assert_eq!(legacy.requests, 8);
        assert_eq!(legacy.peak_concurrent_tunnels, 1);

        let anon = slices(&pool, from, ANONYMOUS_USER_ID).await;
        assert_eq!(anon.len(), 1);
        assert_eq!(anon[0].region_id, "");
        assert_eq!(anon[0].token_id, "");
        assert_eq!(anon[0].tunnel_seconds, 86_400);
        assert_eq!(anon[0].bytes_proxied, 2);
        assert_eq!(anon[0].bytes_direction_unknown, 0);
        assert_eq!(anon[0].user_peak_concurrent, 0);
        assert!(
            slices(&pool, to, ANONYMOUS_USER_ID).await.is_empty(),
            "a close at midnight does not open a session on the next day"
        );

        let day2 = slices(&pool, to, user_a).await;
        assert_eq!(day2.len(), 1);
        assert_eq!(day2[0].sessions, 2);
        // Open tunnel: 00:00 to 06:30. Closed tunnel: 00:00 to 01:00.
        assert_eq!(day2[0].tunnel_seconds, 23_400 + 3_600);
        assert_eq!(day2[0].bytes_in, 110);
        assert_eq!(day2[0].bytes_out, 29);
        assert_eq!(day2[0].bytes_proxied, 139);
        assert_eq!(day2[0].bytes_direction_unknown, 0);
        assert_eq!(day2[0].requests, 7);
        assert_eq!(day2[0].peak_concurrent_tunnels, 2);

        let both = slices(&pool, from, user_c).await;
        assert_eq!(both.len(), 2);
        assert!(both.iter().all(|s| s.peak_concurrent_tunnels == 1));
        assert!(both.iter().all(|s| s.token_peak_concurrent == 2));
        assert!(both.iter().all(|s| s.user_peak_concurrent == 2));

        let again = rollup_usage_between(&pool, from, to, now).await.unwrap();
        let RollupOutcome::Applied(again) = again else {
            panic!("second run should apply");
        };
        assert_eq!(
            again.rows, applied.rows,
            "recompute must replace, not append"
        );
        assert_eq!(slices(&pool, to, user_a).await[0].tunnel_seconds, 27_000);

        // Drop the legacy tunnel and recompute. Its slice disappears.
        sqlx::query("DELETE FROM tunnel_log WHERE tunnel_id = $1")
            .bind(&legacy_id)
            .execute(&pool)
            .await
            .unwrap();
        rollup_usage_between(&pool, from, to, now).await.unwrap();
        let day1 = slices(&pool, from, user_a).await;
        assert!(day1.iter().all(|s| s.protocol != "tcp"));
        assert_eq!(day1.len(), 1);

        sqlx::query("DELETE FROM tunnel_log WHERE tunnel_id = ANY($1)")
            .bind(vec![open_id, cross_id, midnight_id, eu_id, us_id])
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM usage_daily WHERE day >= $1 AND day <= $2")
            .bind(from)
            .bind(to)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM users WHERE id = ANY($1)")
            .bind(vec![user_a, user_c])
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn advisory_lock_skips_the_second_writer_and_does_not_double_counts() {
        let _guard = ROLLUP_TEST_LOCK.lock().await;
        let pool = pool().await;
        let on = day(2001, 6, 1);
        let now = ts(2001, 6, 1, 12, 0);
        sqlx::query("DELETE FROM usage_daily WHERE day = $1")
            .bind(on)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "DELETE FROM tunnel_log \
             WHERE registered_at >= ($1::timestamp AT TIME ZONE 'UTC') \
               AND registered_at < (($1::date + 1)::timestamp AT TIME ZONE 'UTC')",
        )
        .bind(on)
        .execute(&pool)
        .await
        .unwrap();
        let id = insert_log(
            &pool,
            None,
            Some("lock-tok"),
            Some("eu"),
            "tcp",
            ts(2001, 6, 1, 1, 0),
            Some(ts(2001, 6, 1, 3, 0)),
            8,
            8,
            16,
            1,
        )
        .await;

        let mut conn = pool.acquire().await.unwrap();
        let mut held = conn.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock($1::bigint)")
            .bind(USAGE_ROLLUP_LOCK_KEY)
            .execute(&mut *held)
            .await
            .unwrap();
        let skipped = rollup_usage_between(&pool, on, on, now).await.unwrap();
        assert_eq!(skipped, RollupOutcome::SkippedBusy);
        let (n,): (i64,) =
            sqlx::query_as("SELECT COUNT(*)::bigint FROM usage_daily WHERE day = $1")
                .bind(on)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 0, "a skipped run must not delete or insert");
        held.rollback().await.unwrap();

        let (left, right) = tokio::join!(
            rollup_usage_between(&pool, on, on, now),
            rollup_usage_between(&pool, on, on, now),
        );
        left.unwrap();
        right.unwrap();
        let (sessions, bytes): (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(sessions), 0)::bigint, COALESCE(SUM(bytes_proxied), 0)::bigint \
             FROM usage_daily WHERE day = $1",
        )
        .bind(on)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(sessions, 1);
        assert_eq!(bytes, 16);

        rollup_usage_between(&pool, on, on, now).await.unwrap();
        let (sessions,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(sessions), 0)::bigint FROM usage_daily WHERE day = $1",
        )
        .bind(on)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(sessions, 1, "a later recompute stays at one session");

        sqlx::query("DELETE FROM tunnel_log WHERE tunnel_id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM usage_daily WHERE day = $1")
            .bind(on)
            .execute(&pool)
            .await
            .unwrap();
    }

    async fn json_get(
        app: &axum::Router,
        path: &str,
        token: &str,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .uri(path)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let value = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                serde_json::Value::String(String::from_utf8_lossy(&bytes).into())
            })
        };
        (status, value)
    }

    fn app(db: db::Db) -> axum::Router {
        router(ApiState {
            core: Arc::new(TunnelCore::new([0, 0], [0, 0], 4, 4, 0, 10)),
            db,
            capture: Arc::new(RwLock::new(HashMap::new()))
                as crate::dashboard::capture::CaptureStore,
            admin_token: "test-admin-token".into(),
            audit_tx: noop_audit(),
            region: RegionSection::default(),
        })
    }

    async fn insert_user(pool: &sqlx::PgPool, email: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, email, password_hash) VALUES ($1, $2, 'x')")
            .bind(id)
            .bind(email)
            .execute(pool)
            .await
            .unwrap();
        id
    }

    async fn insert_token(
        pool: &sqlx::PgPool,
        user: Option<Uuid>,
        label: &str,
        system: bool,
        created_at: DateTime<Utc>,
        last_used_at: Option<DateTime<Utc>>,
    ) -> String {
        let id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO tokens (id, token_hash, label, created_at, user_id, system, last_used_at) \
             VALUES ($1, $1, $2, $3, $4, $5, $6)",
        )
        .bind(&id)
        .bind(label)
        .bind(created_at)
        .bind(user)
        .bind(system)
        .bind(last_used_at)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_daily(
        pool: &sqlx::PgPool,
        on: NaiveDate,
        user: Uuid,
        token: &str,
        protocol: &str,
        sessions: i32,
        seconds: i64,
        bytes_in: i64,
        bytes_out: i64,
        proxied: i64,
        unknown: i64,
        requests: i64,
        slice_peak: i32,
        token_peak: i32,
        user_peak: i32,
    ) {
        sqlx::query(
            "INSERT INTO usage_daily (
                day, user_id, token_id, region_id, protocol,
                sessions, tunnel_seconds, bytes_in, bytes_out, bytes_proxied,
                bytes_direction_unknown, requests, peak_concurrent_tunnels,
                token_peak_concurrent, user_peak_concurrent
             ) VALUES (
                $1, $2, $3, 'eu', $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14
             )",
        )
        .bind(on)
        .bind(user)
        .bind(token)
        .bind(protocol)
        .bind(sessions)
        .bind(seconds)
        .bind(bytes_in)
        .bind(bytes_out)
        .bind(proxied)
        .bind(unknown)
        .bind(requests)
        .bind(slice_peak)
        .bind(token_peak)
        .bind(user_peak)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn admin_usage_endpoints_shape_and_test_account_exclusion() {
        let db = full_db().await;
        let pool = db.pg.clone();
        let today = Utc::now().date_naive();
        let start = today.checked_sub_days(Days::new(89)).unwrap();
        sqlx::query("DELETE FROM usage_daily WHERE day >= $1 AND day <= $2")
            .bind(start)
            .bind(today)
            .execute(&pool)
            .await
            .unwrap();
        // The carv address is a fixed string, so a failed earlier run leaves it behind.
        sqlx::query(
            "DELETE FROM tokens WHERE user_id IN (SELECT id FROM users WHERE lower(email) = 'joao.silva@carv.com')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "DELETE FROM tunnel_log WHERE user_id IN (SELECT id FROM users WHERE lower(email) = 'joao.silva@carv.com')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM usage_events WHERE user_id IN (SELECT id FROM users WHERE lower(email) = 'joao.silva@carv.com')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM users WHERE lower(email) = 'joao.silva@carv.com'")
            .execute(&pool)
            .await
            .unwrap();

        let suffix = Uuid::new_v4().simple().to_string();
        let real_a = insert_user(&pool, &format!("real-a-{suffix}@example.com")).await;
        let real_b = insert_user(&pool, &format!("real-b-{suffix}@example.com")).await;
        let test_plus = insert_user(&pool, &format!("joaoh82+dash-{suffix}@example.com")).await;
        // Mixed case must still match the exact address.
        let test_carv = insert_user(&pool, "Joao.Silva@carv.com").await;
        // A lookalike in the same domain is not the test pattern.
        let lookalike = insert_user(&pool, &format!("joao.silva.{suffix}@carv.com")).await;
        let _prefix = insert_user(&pool, &format!("joaoh82-{suffix}@example.com")).await;

        let now = Utc::now();
        let old = now - chrono::Duration::days(40);
        let tok_a = insert_token(&pool, Some(real_a), "a", false, now, Some(now)).await;
        let tok_b = insert_token(&pool, Some(real_b), "b", false, now, Some(now)).await;
        let tok_old = insert_token(&pool, Some(real_a), "old", false, old, Some(old)).await;
        let tok_test = insert_token(&pool, Some(test_plus), "test", false, now, Some(now)).await;
        let tok_sys = insert_token(&pool, Some(real_a), "sys", true, now, Some(now)).await;
        let tok_look = insert_token(&pool, Some(lookalike), "look", false, now, Some(now)).await;

        // Today: A has legacy bytes plus a system-token row that must be ignored.
        // B has a known direction split and more seconds, so sort order flips.
        insert_daily(
            &pool, today, real_a, &tok_a, "http", 4, 7_200, 0, 0, 500, 500, 8, 1, 1, 5,
        )
        .await;
        insert_daily(
            &pool, today, real_a, &tok_sys, "http", 9, 9, 9, 9, 18, 0, 9, 1, 1, 5,
        )
        .await;
        insert_daily(
            &pool, today, real_b, &tok_b, "tcp", 1, 20_000, 100, 50, 150, 0, 2, 1, 1, 1,
        )
        .await;
        insert_daily(
            &pool, today, test_plus, &tok_test, "http", 7, 100, 1, 1, 9_999, 0, 1, 1, 1, 9,
        )
        .await;
        insert_daily(
            &pool, today, test_carv, "carv-tok", "http", 2, 20, 1, 1, 30, 0, 1, 1, 1, 4,
        )
        .await;
        insert_daily(
            &pool, today, lookalike, &tok_look, "http", 1, 60, 5, 5, 10, 0, 1, 1, 1, 1,
        )
        .await;
        insert_daily(
            &pool,
            today,
            ANONYMOUS_USER_ID,
            "",
            "udp",
            3,
            10,
            0,
            0,
            0,
            0,
            0,
            1,
            1,
            0,
        )
        .await;
        // A day that is only legacy bytes, so in and out are null.
        let legacy_day = today.checked_sub_days(Days::new(40)).unwrap();
        insert_daily(
            &pool, legacy_day, real_a, &tok_a, "http", 1, 60, 0, 0, 80, 80, 1, 1, 1, 1,
        )
        .await;

        let app = app(db);
        let (status, denied) = json_get(&app, "/api/admin/usage/daily", "nope").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(denied.get("error").is_some(), "{denied}");

        let (status, daily) =
            json_get(&app, "/api/admin/usage/daily?days=7", "test-admin-token").await;
        assert_eq!(status, StatusCode::OK, "{daily}");
        assert_eq!(daily["days"], 7);
        assert_eq!(daily["exclude_test"], true);
        let series = daily["series"].as_array().unwrap();
        assert_eq!(series.len(), 7);
        let today_row = series
            .iter()
            .find(|p| p["day"] == today.to_string())
            .unwrap();
        for key in [
            "sessions",
            "tunnel_seconds",
            "tunnel_hours",
            "active_users",
            "requests",
        ] {
            assert!(today_row.get(key).is_some(), "missing {key} in {today_row}");
        }
        // A 4 + B 1 + lookalike 1 + anonymous 3. Test emails and the system token are out.
        assert_eq!(today_row["sessions"], 9);
        assert_eq!(today_row["tunnel_seconds"], 7_200 + 20_000 + 60 + 10);
        assert_eq!(today_row["active_users"], 3);
        assert_eq!(today_row["requests"], 8 + 2 + 1);

        let (status, omitted) = json_get(&app, "/api/admin/usage/daily", "test-admin-token").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(omitted["series"].as_array().unwrap().len(), 30);
        assert_eq!(omitted["exclude_test"], true);

        let (status, included) = json_get(
            &app,
            "/api/admin/usage/daily?days=7&exclude_test=false",
            "test-admin-token",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let included_today = included["series"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["day"] == today.to_string())
            .unwrap();
        // Plus joaoh82+ (7) and the exact carv address (2). The dotted lookalike was already in.
        assert_eq!(included_today["sessions"], 9 + 7 + 2);
        assert_eq!(included_today["active_users"], 5);

        let (status, bw) = json_get(
            &app,
            "/api/admin/usage/bandwidth?days=90",
            "test-admin-token",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{bw}");
        assert_eq!(bw["series"].as_array().unwrap().len(), 90);
        let legacy = bw["series"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["day"] == legacy_day.to_string())
            .unwrap();
        assert!(legacy["bytes_in"].is_null(), "{legacy}");
        assert!(legacy["bytes_out"].is_null(), "{legacy}");
        assert_eq!(legacy["bytes_total"], 80);
        assert_eq!(legacy["bytes_direction_unknown"], 80);
        let mixed = bw["series"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["day"] == today.to_string())
            .unwrap();
        // Known direction is B (100/50) plus lookalike (5/5). A's 500 is unknown.
        assert_eq!(mixed["bytes_in"], 105);
        assert_eq!(mixed["bytes_out"], 55);
        assert_eq!(mixed["bytes_total"], 500 + 150 + 10);
        assert_eq!(mixed["bytes_direction_unknown"], 500);

        // Token totals share the database with other tests. Compare to the spec
        // under a share lock so an insert cannot land between the two reads.
        let mut conn = pool.acquire().await.unwrap();
        let mut locked = conn.begin().await.unwrap();
        sqlx::query("SET LOCAL lock_timeout = '15s'")
            .execute(&mut *locked)
            .await
            .unwrap();
        sqlx::query("LOCK TABLE tokens IN SHARE MODE")
            .execute(&mut *locked)
            .await
            .unwrap();
        let (status, tokens) =
            json_get(&app, "/api/admin/usage/tokens?days=7", "test-admin-token").await;
        assert_eq!(status, StatusCode::OK, "{tokens}");
        assert_eq!(tokens["created_per_day"].as_array().unwrap().len(), 7);
        assert!(tokens.get("total").is_some());
        assert!(tokens.get("active_last_7_days").is_some());
        let expected_total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM tokens t \
             LEFT JOIN users u ON u.id = t.user_id \
             WHERE t.system = FALSE \
               AND (u.email IS NULL OR NOT (u.email ILIKE 'joaoh82+%' OR lower(u.email) = 'joao.silva@carv.com'))",
        )
        .fetch_one(&mut *locked)
        .await
        .unwrap();
        let expected_active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM tokens t \
             LEFT JOIN users u ON u.id = t.user_id \
             WHERE t.system = FALSE \
               AND t.last_used_at >= now() - interval '7 days' \
               AND (u.email IS NULL OR NOT (u.email ILIKE 'joaoh82+%' OR lower(u.email) = 'joao.silva@carv.com'))",
        )
        .fetch_one(&mut *locked)
        .await
        .unwrap();
        let expected_created: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM tokens t \
             LEFT JOIN users u ON u.id = t.user_id \
             WHERE t.system = FALSE \
               AND (t.created_at AT TIME ZONE 'UTC')::date = $1 \
               AND (u.email IS NULL OR NOT (u.email ILIKE 'joaoh82+%' OR lower(u.email) = 'joao.silva@carv.com'))",
        )
        .bind(today)
        .fetch_one(&mut *locked)
        .await
        .unwrap();
        assert_eq!(tokens["total"], expected_total);
        assert_eq!(tokens["active_last_7_days"], expected_active);
        let created_today = tokens["created_per_day"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["day"] == today.to_string())
            .unwrap();
        assert_eq!(created_today["count"], expected_created);
        let (status, tokens_all) = json_get(
            &app,
            "/api/admin/usage/tokens?days=7&exclude_test=false",
            "test-admin-token",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let test_tokens: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM tokens t \
             JOIN users u ON u.id = t.user_id \
             WHERE t.system = FALSE \
               AND (u.email ILIKE 'joaoh82+%' OR lower(u.email) = 'joao.silva@carv.com')",
        )
        .fetch_one(&mut *locked)
        .await
        .unwrap();
        assert!(test_tokens >= 1);
        assert_eq!(
            tokens_all["total"].as_i64().unwrap() - tokens["total"].as_i64().unwrap(),
            test_tokens
        );
        // The 40-day-old token is in the total and not in the 7-day active count.
        assert!(tokens["total"].as_i64().unwrap() > tokens["active_last_7_days"].as_i64().unwrap());
        locked.rollback().await.unwrap();
        let _ = tok_old;

        let (status, by_user) =
            json_get(&app, "/api/admin/usage/by-user?days=7", "test-admin-token").await;
        assert_eq!(status, StatusCode::OK, "{by_user}");
        let entries = by_user["entries"].as_array().unwrap();
        assert_eq!(by_user["total"], 3);
        let user_a = entries
            .iter()
            .find(|e| e["user_id"] == real_a.to_string())
            .unwrap();
        assert_eq!(
            user_a["sessions"], 4,
            "system-token sessions must not be added"
        );
        assert_eq!(user_a["bytes_in"], serde_json::Value::Null);
        assert_eq!(user_a["bytes_total"], 500);
        assert_eq!(user_a["peak_concurrent_tunnels"], 5);
        assert_eq!(user_a["tunnel_seconds"], 7_200);
        assert!(entries
            .iter()
            .all(|e| e["email"].as_str().unwrap_or("").to_lowercase() != "joao.silva@carv.com"));
        assert!(entries.iter().all(|e| {
            !e["email"].as_str().unwrap_or("").starts_with("joaoh82+")
                && !e["email"].as_str().unwrap_or("").starts_with("JOAOH82+")
        }));

        let (status, by_user_all) = json_get(
            &app,
            "/api/admin/usage/by-user?days=7&exclude_test=false",
            "test-admin-token",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(by_user_all["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| { e["email"].as_str().unwrap_or("").contains("joaoh82+") }));
        assert!(by_user_all["entries"].as_array().unwrap().iter().any(|e| {
            e["email"]
                .as_str()
                .unwrap_or("")
                .eq_ignore_ascii_case("joao.silva@carv.com")
        }));
        // The dotted address is not the test pattern, even in the same domain.
        assert!(entries
            .iter()
            .any(|e| e["user_id"] == lookalike.to_string()));

        let (status, by_token) =
            json_get(&app, "/api/admin/usage/by-token?days=7", "test-admin-token").await;
        assert_eq!(status, StatusCode::OK, "{by_token}");
        let token_entries = by_token["entries"].as_array().unwrap();
        assert!(token_entries.iter().any(|e| e["token_id"] == tok_a));
        assert!(token_entries.iter().any(|e| e["token_id"] == tok_b));
        assert!(token_entries.iter().all(|e| e["token_id"] != tok_sys));
        assert!(token_entries.iter().all(|e| e["token_id"] != tok_test));
        let token_a = token_entries
            .iter()
            .find(|e| e["token_id"] == tok_a)
            .unwrap();
        assert_eq!(token_a["token_label"], "a");
        assert_eq!(token_a["sessions"], 4);
        assert_eq!(token_a["peak_concurrent_tunnels"], 1);

        let (status, bad_days) = json_get(
            &app,
            "/api/admin/usage/leaderboard?days=14",
            "test-admin-token",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad_days}");
        let (status, bad_sort) = json_get(
            &app,
            "/api/admin/usage/leaderboard?days=7&sort=concurrent",
            "test-admin-token",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad_sort}");

        let (status, by_bw) = json_get(
            &app,
            "/api/admin/usage/leaderboard?days=7&sort=bandwidth",
            "test-admin-token",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{by_bw}");
        let board = by_bw["entries"].as_array().unwrap();
        assert_eq!(board[0]["user_id"], real_a.to_string());
        assert_eq!(board[0]["bytes_total"], 500);
        assert!(board.iter().all(|e| {
            e["email"].as_str().unwrap_or("").to_lowercase() != "joao.silva@carv.com"
        }));

        let (status, by_hours) = json_get(
            &app,
            "/api/admin/usage/leaderboard?days=30&sort=tunnel_hours",
            "test-admin-token",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{by_hours}");
        assert_eq!(by_hours["entries"][0]["user_id"], real_b.to_string());
        assert_eq!(by_hours["entries"][0]["tunnel_seconds"], 20_000);

        let (status, by_peak) = json_get(
            &app,
            "/api/admin/usage/leaderboard?days=90&sort=peak_concurrent",
            "test-admin-token",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{by_peak}");
        assert_eq!(by_peak["sort"], "peak_concurrent");
        assert_eq!(by_peak["entries"][0]["user_id"], real_a.to_string());
        assert_eq!(by_peak["entries"][0]["peak_concurrent_tunnels"], 5);

        let (status, with_test) = json_get(
            &app,
            "/api/admin/usage/leaderboard?days=7&sort=bandwidth&exclude_test=false",
            "test-admin-token",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(with_test["entries"][0]["email"]
            .as_str()
            .unwrap()
            .contains("joaoh82+"));

        sqlx::query("DELETE FROM usage_daily WHERE day >= $1 AND day <= $2")
            .bind(start)
            .bind(today)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM tokens WHERE id = ANY($1)")
            .bind(vec![tok_a, tok_b, tok_old, tok_test, tok_sys, tok_look])
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "DELETE FROM users WHERE email LIKE $1 OR lower(email) = 'joao.silva@carv.com' OR id = ANY($2)",
        )
        .bind(format!("%{suffix}%"))
        .bind(vec![real_a, real_b, test_plus, test_carv, lookalike])
        .execute(&pool)
        .await
        .unwrap();
    }
}
