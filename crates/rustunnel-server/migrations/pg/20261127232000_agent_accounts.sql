-- The edge runs this file at startup. sqlx 0.8 wraps it in one transaction
-- unless the file starts with `-- no-transaction`
-- (sqlx-core migrate/source.rs). This file does not, so SET LOCAL lasts
-- for every statement below and ends when that transaction commits.
-- ALTER on users or tokens takes ACCESS EXCLUSIVE. A waiter in that lock
-- queue blocks every token lookup. Five seconds fails the migration
-- instead of holding the queue open.
SET LOCAL lock_timeout = '5s';

-- Agent-native accounts (signup ticket 1, database half).
--
-- Version 20261127232000 is a timestamp after 20261103143000, the newest
-- sqlx migration in this directory. Version 15 stays free for
-- platform-api's 0015_payg_nudge_events.sql, which that service applies
-- with raw_sql and does not record in public._sqlx_migrations.
-- platform-api's later raw_sql files (20261103140000, 20261103180000)
-- are also not recorded there. A timestamp cannot collide with version
-- 15 or 16.
--
-- Idempotent: ADD COLUMN IF NOT EXISTS, backfill, DROP CONSTRAINT IF
-- EXISTS before ADD, CREATE INDEX IF NOT EXISTS, INSERT ... ON CONFLICT.
-- Safe to replay with raw_sql. sqlx still records the file once, by
-- checksum.
--
-- Stays inside sqlx's transaction. Indexes are not CREATE INDEX
-- CONCURRENTLY (PostgreSQL rejects that inside a transaction). `users`
-- is the small shared account table, not tunnel_log.
--
-- Backward compatible with the currently deployed server:
-- * New user columns are nullable or NOT NULL with a default. Existing
--   INSERT statements that omit them keep working. Existing rows become
--   kind = 'human'.
-- * tokens.expires_at already exists (0006) and stays nullable. NULL
--   means no expiry, which is every human token. This file re-adds the
--   column with IF NOT EXISTS for a database that skipped 0006. It does
--   not set a default and does not rewrite existing rows.
-- * plans.name is unconstrained text, not an enum and not a CHECK.
--   Adding a name CHECK would reject plan names this repo does not seed
--   (platform-api may insert more). The agent plan is a new row.
-- * New plan columns have defaults, so existing plan rows and old
--   INSERT statements stay valid. free and payg are not rewritten,
--   except that they gain the default protocol list and a soft cap.
-- * No DELETE of users. No new ON DELETE CASCADE. Expired unclaimed
--   accounts are marked with deactivated_at and must stay visible to
--   admin analytics. This file installs no cleanup job.
--
-- Edge enforcement of these limits is ticket 2. The deployed server only
-- hobby-gates plans.billing_model = 'free'. The agent row uses
-- billing_model = 'agent', so a server that has not shipped ticket 2
-- does not apply Hobby upgrade copy or the soft gigabyte warning.

-- ── users ────────────────────────────────────────────────────────────────────
-- kind: 'human' (every existing account) or 'agent'.
-- claimed_at: set when a human claims the account. NULL while unclaimed.
--   Unclaimed agent tokens expire 7 days after users.created_at; claim
--   clears tokens.expires_at. That write is application code, not a default,
--   because human tokens must stay NULL.
-- signup_ip_hash: salted hash of the signup IP. Never a raw address.
-- source: signup surface. Recommended values are 'cli', 'mcp', 'api'.
--   Left unconstrained so a new surface does not need a migration.
-- terms_version: terms text the signup accepted. NULL if none was recorded.
-- deactivated_at: set when an expired unclaimed account is deactivated.
--   The row stays. status is not changed by this column.

ALTER TABLE users ADD COLUMN IF NOT EXISTS kind TEXT;
UPDATE users SET kind = 'human' WHERE kind IS NULL;
ALTER TABLE users ALTER COLUMN kind SET DEFAULT 'human';
ALTER TABLE users ALTER COLUMN kind SET NOT NULL;

ALTER TABLE users DROP CONSTRAINT IF EXISTS users_kind_check;
ALTER TABLE users ADD CONSTRAINT users_kind_check
    CHECK (kind IN ('human', 'agent'));

ALTER TABLE users ADD COLUMN IF NOT EXISTS claimed_at TIMESTAMPTZ;
ALTER TABLE users ADD COLUMN IF NOT EXISTS signup_ip_hash TEXT;
ALTER TABLE users ADD COLUMN IF NOT EXISTS source TEXT;
ALTER TABLE users ADD COLUMN IF NOT EXISTS terms_version TEXT;
ALTER TABLE users ADD COLUMN IF NOT EXISTS deactivated_at TIMESTAMPTZ;

COMMENT ON COLUMN users.kind IS
    'human (default, every pre-existing account) or agent.';
COMMENT ON COLUMN users.claimed_at IS
    'When a human claimed an agent account. NULL while unclaimed.';
COMMENT ON COLUMN users.signup_ip_hash IS
    'Salted hash of the signup IP. Never store the raw address.';
COMMENT ON COLUMN users.source IS
    'Signup surface. Recommended: cli, mcp, api. Not a closed set.';
COMMENT ON COLUMN users.terms_version IS
    'Terms version accepted at signup. NULL when none was recorded.';
COMMENT ON COLUMN users.deactivated_at IS
    'Set when an expired unclaimed account is deactivated. The row is kept for admin analytics and is never deleted by a cleanup job.';

-- Admin breakdowns. Plain b-trees so both sides of the filter are usable
-- (kind = human and kind = agent, deactivated and not).
CREATE INDEX IF NOT EXISTS idx_users_kind
    ON users (kind);
CREATE INDEX IF NOT EXISTS idx_users_source
    ON users (source);
CREATE INDEX IF NOT EXISTS idx_users_deactivated_at
    ON users (deactivated_at);

-- Count signups for one salted IP over a window:
-- WHERE signup_ip_hash = $1 AND created_at >= $2
-- Partial: human rows have a NULL hash and are not signup-rate traffic.
CREATE INDEX IF NOT EXISTS idx_users_signup_ip_hash_created_at
    ON users (signup_ip_hash, created_at)
    WHERE signup_ip_hash IS NOT NULL;

-- ── tokens.expires_at ────────────────────────────────────────────────────────
-- Nullable. NULL = no expiry (human PAYG and admin tokens).
-- platform-api writes expires_at = users.created_at + interval '7 days'
-- on the agent token, and sets it back to NULL when users.claimed_at is set.
-- There is no column default: a default of 7 days would expire human tokens.

ALTER TABLE tokens ADD COLUMN IF NOT EXISTS expires_at TIMESTAMPTZ;

COMMENT ON COLUMN tokens.expires_at IS
    'NULL means no expiry. Unclaimed agent tokens are users.created_at + 7 days and are cleared on claim. Deactivation sets users.deactivated_at; it does not delete the token or the user.';

-- ── plans: columns the existing gigabyte / protocol shape cannot store ──────
-- allowed_protocols: wire names from TunnelProtocol (snake_case):
--   http, https, tcp, udp, p2p.
--   http and https are the HTTP edge (https only changes the public URL
--   scheme). Default is every protocol, so free and payg stay unrestricted.
-- bandwidth_hard_cap + bandwidth_limit_bytes: a hard monthly cap on
--   tunnel_log.bytes_proxied. bandwidth_limit_gb stays the soft Hobby
--   warning and is whole binary gigabytes, so it cannot store 250 MB.
--   Hard cap is off and the byte column is NULL unless both are set.

ALTER TABLE plans ADD COLUMN IF NOT EXISTS allowed_protocols TEXT[];
UPDATE plans
   SET allowed_protocols = ARRAY['http', 'https', 'tcp', 'udp', 'p2p']::text[]
 WHERE allowed_protocols IS NULL;
ALTER TABLE plans
    ALTER COLUMN allowed_protocols SET DEFAULT ARRAY['http', 'https', 'tcp', 'udp', 'p2p']::text[];
ALTER TABLE plans ALTER COLUMN allowed_protocols SET NOT NULL;

ALTER TABLE plans DROP CONSTRAINT IF EXISTS plans_allowed_protocols_check;
ALTER TABLE plans ADD CONSTRAINT plans_allowed_protocols_check
    CHECK (
        cardinality(allowed_protocols) > 0
        AND allowed_protocols <@ ARRAY['http', 'https', 'tcp', 'udp', 'p2p']::text[]
    );

ALTER TABLE plans ADD COLUMN IF NOT EXISTS bandwidth_hard_cap BOOLEAN;
UPDATE plans SET bandwidth_hard_cap = false WHERE bandwidth_hard_cap IS NULL;
ALTER TABLE plans ALTER COLUMN bandwidth_hard_cap SET DEFAULT false;
ALTER TABLE plans ALTER COLUMN bandwidth_hard_cap SET NOT NULL;

ALTER TABLE plans ADD COLUMN IF NOT EXISTS bandwidth_limit_bytes BIGINT;

ALTER TABLE plans DROP CONSTRAINT IF EXISTS plans_bandwidth_cap_check;
ALTER TABLE plans ADD CONSTRAINT plans_bandwidth_cap_check
    CHECK (
        (NOT bandwidth_hard_cap AND bandwidth_limit_bytes IS NULL)
        OR (
            bandwidth_hard_cap
            AND bandwidth_limit_bytes IS NOT NULL
            AND bandwidth_limit_bytes > 0
        )
    );

COMMENT ON COLUMN plans.allowed_protocols IS
    'TunnelProtocol wire names this plan may register: http, https, tcp, udp, p2p.';
COMMENT ON COLUMN plans.bandwidth_hard_cap IS
    'When true, bandwidth_limit_bytes is a hard monthly cap. When false, bandwidth_limit_gb remains the soft Hobby warning.';
COMMENT ON COLUMN plans.bandwidth_limit_bytes IS
    'Hard monthly cap in bytes. NULL when bandwidth_hard_cap is false. The agent plan stores the advertised 250 MB as 250000000.';

-- The agent plan. name is the plan value. There is no plans enum.
-- Limits:
--   max_tunnels = 1              one concurrent tunnel
--   allowed_protocols            http + https only (HTTP edge)
--   allow_custom_subdomains      false = random subdomain only
--   bandwidth_hard_cap           true
--   bandwidth_limit_bytes        250000000 (250 MB decimal, the advertised cap)
--   bandwidth_limit_gb           NULL (soft gigabyte warning is not used)
--   billing_model                'agent' (not 'free', so the deployed
--                                hobby gate does not apply)
-- max_connections, rate_limit_rps, and history_days are the table
-- defaults (100, 100, 7). They are not extra agent limits.
INSERT INTO plans (
    name,
    billing_model,
    monthly_price_cents,
    max_tunnels,
    max_connections,
    rate_limit_rps,
    bandwidth_limit_gb,
    history_days,
    is_active,
    allow_custom_subdomains,
    monthly_minimum_cents,
    allowed_protocols,
    bandwidth_hard_cap,
    bandwidth_limit_bytes
) VALUES (
    'agent',
    'agent',
    0,
    1,
    100,
    100,
    NULL,
    7,
    true,
    false,
    0,
    ARRAY['http', 'https']::text[],
    true,
    250000000
)
-- ON CONFLICT DO UPDATE resets this row to the canonical values on every
-- replay. That overwrites a manual admin edit of the agent plan. It is
-- intentional: the advertised limits live in this file.
ON CONFLICT (name) DO UPDATE SET
    billing_model = EXCLUDED.billing_model,
    monthly_price_cents = EXCLUDED.monthly_price_cents,
    max_tunnels = EXCLUDED.max_tunnels,
    max_connections = EXCLUDED.max_connections,
    rate_limit_rps = EXCLUDED.rate_limit_rps,
    bandwidth_limit_gb = EXCLUDED.bandwidth_limit_gb,
    history_days = EXCLUDED.history_days,
    is_active = EXCLUDED.is_active,
    allow_custom_subdomains = EXCLUDED.allow_custom_subdomains,
    monthly_minimum_cents = EXCLUDED.monthly_minimum_cents,
    allowed_protocols = EXCLUDED.allowed_protocols,
    bandwidth_hard_cap = EXCLUDED.bandwidth_hard_cap,
    bandwidth_limit_bytes = EXCLUDED.bandwidth_limit_bytes;
