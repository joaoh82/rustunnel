-- Directional byte counters for tunnel metering.
--
-- bytes_proxied stays the combined total (bytes_in + bytes_out) so every
-- existing SUM(bytes_proxied) reader keeps the same number: Hobby cap
-- checks, PAYG spend, usage_events slices, and the admin API.
--
-- Version 20261103120000 is a timestamp, not the next integer after 0014.
-- rustunnel-server is the only writer of public._sqlx_migrations
-- (sqlx::migrate!, default table, ignore_missing left at false).
-- platform-api does not run a Migrator. It applies
-- platform-api/migrations/0015_payg_nudge_events.sql with sqlx::raw_sql
-- and the comment there says this repo is expected to adopt that file.
-- Version 15 is left free for that file. Version 16 is the next integer
-- either repo is likely to take. A timestamp cannot collide with either.
--
-- Idempotent: ADD COLUMN IF NOT EXISTS. Historical rows keep bytes_proxied
-- as written and the new columns at 0. New flushes set
-- bytes_proxied = bytes_in + bytes_out.

ALTER TABLE tunnel_log ADD COLUMN IF NOT EXISTS bytes_in  BIGINT NOT NULL DEFAULT 0;
ALTER TABLE tunnel_log ADD COLUMN IF NOT EXISTS bytes_out BIGINT NOT NULL DEFAULT 0;

-- Per-tunnel billing slices. platform-api reads bytes_proxied by name.
-- The new columns default to 0 so existing inserts that list columns
-- explicitly keep working, and SUM(bytes_proxied) is unchanged.
ALTER TABLE usage_events ADD COLUMN IF NOT EXISTS bytes_in  BIGINT NOT NULL DEFAULT 0;
ALTER TABLE usage_events ADD COLUMN IF NOT EXISTS bytes_out BIGINT NOT NULL DEFAULT 0;
