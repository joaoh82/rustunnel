-- Daily usage rollup for the admin stats API (TUNNEL-50).
--
-- Version 20261103143000 is a timestamp later than 20261103120000
-- (directional byte columns). Version 15 stays free for platform-api's
-- 0015_payg_nudge_events.sql, which that service applies with raw_sql
-- and does not record in public._sqlx_migrations.
--
-- Idempotent: CREATE TABLE IF NOT EXISTS and CREATE INDEX IF NOT EXISTS.
-- Safe to replay. sqlx still records the file once, by checksum.
--
-- Indexes are not CREATE INDEX CONCURRENTLY. sqlx 0.8 runs a migration
-- inside one transaction unless the file starts with `-- no-transaction`
-- (sqlx-core migrate source, Postgres Migrate::apply). PostgreSQL rejects
-- CREATE INDEX CONCURRENTLY inside a transaction block, so the opt-out
-- would be required, and a failure mid-file would leave a partial schema.
-- This file stays transactional.
--
-- A plain CREATE INDEX takes SHARE on the table: reads continue, and
-- INSERT, UPDATE, and DELETE block until the build commits. That blocks
-- tunnel registration, meter flushes, and closes on tunnel_log for the
-- length of the build. No row count for tunnel_log is stored in this
-- repo, so the lock time is unknown until someone checks
-- pg_class.reltuples (or a real count) on the target database. tokens is
-- a much smaller table. usage_daily is created empty here, so its indexes
-- are instant. Operators who want to avoid the write pause can build the
-- two tunnel_log indexes CONCURRENTLY by hand before this migration runs;
-- CREATE INDEX IF NOT EXISTS then becomes a no-op.
--
-- Grain is one row per UTC day, user, token, region, and protocol.
-- Empty string stands in for a missing token_id or region_id. The nil
-- UUID stands in for a missing user_id. There is no foreign key: a
-- deleted user or token must not block the rollup, and the nil user is
-- not a row in users.
--
-- peak_concurrent_tunnels is the max overlap inside this slice.
-- token_peak_concurrent is the max overlap for this user and token that
-- day, across regions and protocols. user_peak_concurrent is the max
-- overlap for this user that day, across tokens, regions, and protocols.
-- Leaderboard and per-user reads take MAX(user_peak_concurrent) over the
-- window. That equals the window peak, because a peak instant falls on
-- one UTC day. Summing peaks would be wrong.
--
-- bytes_proxied is the total attributed to the day. bytes_direction_unknown
-- is the part of that total that came from legacy rows (bytes_in and
-- bytes_out still 0, bytes_proxied > 0), which have no direction.
-- The rollup job in db/usage_rollup.rs fills the table. This file does not.

CREATE TABLE IF NOT EXISTS usage_daily (
    day                       DATE        NOT NULL,
    user_id                   UUID        NOT NULL,
    token_id                  TEXT        NOT NULL DEFAULT '',
    region_id                 TEXT        NOT NULL DEFAULT '',
    protocol                  TEXT        NOT NULL,
    sessions                  INTEGER     NOT NULL DEFAULT 0,
    tunnel_seconds            BIGINT      NOT NULL DEFAULT 0,
    bytes_in                  BIGINT      NOT NULL DEFAULT 0,
    bytes_out                 BIGINT      NOT NULL DEFAULT 0,
    bytes_proxied             BIGINT      NOT NULL DEFAULT 0,
    bytes_direction_unknown   BIGINT      NOT NULL DEFAULT 0,
    requests                  BIGINT      NOT NULL DEFAULT 0,
    peak_concurrent_tunnels   INTEGER     NOT NULL DEFAULT 0,
    token_peak_concurrent     INTEGER     NOT NULL DEFAULT 0,
    user_peak_concurrent      INTEGER     NOT NULL DEFAULT 0,
    updated_at                TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (day, user_id, token_id, region_id, protocol)
);

CREATE INDEX IF NOT EXISTS idx_usage_daily_user_day
    ON usage_daily (user_id, day);
CREATE INDEX IF NOT EXISTS idx_usage_daily_token_day
    ON usage_daily (token_id, day);

-- Overlap scans: closed rows by end time, open rows by start time.
-- token_id lookups are the per-token path the admin API used to scan.
CREATE INDEX IF NOT EXISTS idx_tunnel_log_unregistered
    ON tunnel_log (unregistered_at);
CREATE INDEX IF NOT EXISTS idx_tunnel_log_token
    ON tunnel_log (token_id);
CREATE INDEX IF NOT EXISTS idx_tunnel_log_open_registered
    ON tunnel_log (registered_at)
    WHERE unregistered_at IS NULL;

-- Token stats read tokens, not tunnel_log. These keep "created per day"
-- and "active in the last 7 days" off a sequential scan as the table grows.
CREATE INDEX IF NOT EXISTS idx_tokens_created_at
    ON tokens (created_at);
CREATE INDEX IF NOT EXISTS idx_tokens_last_used_at
    ON tokens (last_used_at);
