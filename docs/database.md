# Database Reference

rustunnel-server uses an embedded SQLite database for persistent state.
This document covers the schema, configuration, direct query patterns, and
maintenance procedures.

---

## Location and configuration

The database path is set in the server config file:

```toml
[database]
path = "/var/lib/rustunnel/tunnel.db"
```

Bare file paths and `sqlite://` URIs are both accepted.  Use `:memory:` for
ephemeral in-process testing.

The server opens a pool of up to 5 connections with WAL journal mode and
foreign-key enforcement enabled.  The file is created automatically on first
startup.

To open the database interactively:

```bash
sqlite3 /var/lib/rustunnel/tunnel.db
```

---

## Schema

### `tokens`

Stores API tokens used to authenticate the CLI client and the REST API.

```sql
CREATE TABLE tokens (
    id           TEXT PRIMARY KEY,   -- UUID v4
    token_hash   TEXT NOT NULL UNIQUE, -- SHA-256 hex of the raw token
    label        TEXT NOT NULL,
    created_at   TEXT NOT NULL,      -- RFC 3339 UTC
    last_used_at TEXT,               -- RFC 3339 UTC, updated on every verify
    scope        TEXT                -- comma-separated subdomain patterns, NULL = unrestricted
);
```

> **Note**: The raw token value is **never stored**.  Only the SHA-256 hash is
> persisted.  The raw value is returned once at creation time and cannot be
> recovered from the database.

### `tunnel_log`

Append-only log of every tunnel that has been registered and unregistered.

```sql
CREATE TABLE tunnel_log (
    id               TEXT PRIMARY KEY,
    tunnel_id        TEXT NOT NULL,
    protocol         TEXT NOT NULL,  -- "http" | "tcp"
    label            TEXT NOT NULL,  -- subdomain (HTTP) or port string (TCP)
    session_id       TEXT NOT NULL,
    registered_at    TEXT NOT NULL,  -- RFC 3339 UTC
    unregistered_at  TEXT            -- RFC 3339 UTC, NULL while tunnel is active
);

CREATE INDEX idx_tunnel_log_id ON tunnel_log (tunnel_id);
```

### `captured_requests`

HTTP request/response pairs captured by the edge proxy for the request
inspector in the dashboard.

```sql
CREATE TABLE captured_requests (
    id             TEXT PRIMARY KEY,
    tunnel_id      TEXT NOT NULL,
    conn_id        TEXT NOT NULL,
    method         TEXT NOT NULL,
    path           TEXT NOT NULL,
    status         INTEGER NOT NULL,
    request_bytes  INTEGER NOT NULL DEFAULT 0,
    response_bytes INTEGER NOT NULL DEFAULT 0,
    duration_ms    INTEGER NOT NULL DEFAULT 0,
    captured_at    TEXT NOT NULL,    -- RFC 3339 UTC
    request_body   TEXT,             -- JSON: {headers, body}; NULL if body exceeded limit
    response_body  TEXT              -- JSON: {headers, body}; NULL if body exceeded limit
);

CREATE INDEX idx_captured_tunnel ON captured_requests (tunnel_id, captured_at DESC);
```

The `request_body` and `response_body` columns contain JSON of the form:

```json
{
  "headers": { "content-type": ["application/json"] },
  "body": "<raw body string>"
}
```

---

## Common queries

### Tokens

```sql
-- List all tokens (newest first)
SELECT id, label, scope, created_at, last_used_at
FROM tokens
ORDER BY created_at DESC;

-- Find a token by label
SELECT * FROM tokens WHERE label = 'ci-deploy';

-- Check when a token was last used
SELECT label, last_used_at FROM tokens WHERE id = '<uuid>';

-- Tokens that have never been used
SELECT id, label, created_at
FROM tokens
WHERE last_used_at IS NULL
ORDER BY created_at;

-- Tokens unused for more than 90 days
SELECT id, label, last_used_at
FROM tokens
WHERE last_used_at < datetime('now', '-90 days')
   OR last_used_at IS NULL;

-- Delete a token by label (use id in production to be precise)
DELETE FROM tokens WHERE label = 'old-token';

-- Delete a token by id
DELETE FROM tokens WHERE id = '<uuid>';
```

> **Warning**: Deleting a token immediately revokes access for any client
> using it.  There is no grace period.

### Tunnel history

```sql
-- All tunnels ever registered (newest first)
SELECT tunnel_id, protocol, label, registered_at, unregistered_at
FROM tunnel_log
ORDER BY registered_at DESC;

-- Currently active tunnels (no unregistered_at yet)
SELECT tunnel_id, protocol, label, registered_at
FROM tunnel_log
WHERE unregistered_at IS NULL;

-- Tunnel lifetime for a specific tunnel
SELECT registered_at, unregistered_at,
       round((julianday(COALESCE(unregistered_at, 'now')) -
              julianday(registered_at)) * 86400) AS duration_seconds
FROM tunnel_log
WHERE tunnel_id = '<tunnel-id>';

-- All tunnels from the last 24 hours
SELECT * FROM tunnel_log
WHERE registered_at > datetime('now', '-1 day')
ORDER BY registered_at DESC;

-- Count of tunnels per protocol
SELECT protocol, COUNT(*) AS total FROM tunnel_log GROUP BY protocol;
```

### Captured requests

```sql
-- Recent requests for a tunnel (newest first)
SELECT method, path, status, duration_ms, captured_at
FROM captured_requests
WHERE tunnel_id = '<tunnel-id>'
ORDER BY captured_at DESC
LIMIT 50;

-- Slow requests (over 1 second)
SELECT tunnel_id, method, path, status, duration_ms, captured_at
FROM captured_requests
WHERE duration_ms > 1000
ORDER BY duration_ms DESC;

-- Error responses (5xx)
SELECT tunnel_id, method, path, status, captured_at
FROM captured_requests
WHERE status >= 500
ORDER BY captured_at DESC;

-- Request volume per tunnel
SELECT tunnel_id, COUNT(*) AS requests,
       AVG(duration_ms) AS avg_ms,
       MAX(duration_ms) AS max_ms
FROM captured_requests
GROUP BY tunnel_id
ORDER BY requests DESC;

-- Requests in the last hour
SELECT COUNT(*) FROM captured_requests
WHERE captured_at > datetime('now', '-1 hour');

-- Delete captured requests older than 30 days
DELETE FROM captured_requests
WHERE captured_at < datetime('now', '-30 days');
```

---

## Manually creating a token

The server hashes tokens with SHA-256.  You can insert a token directly if
you need to bootstrap access without the running server or CLI.

```bash
# 1. Generate a random token value
TOKEN=$(uuidgen | tr '[:upper:]' '[:lower:]')
echo "Raw token (save this): $TOKEN"

# 2. Compute the SHA-256 hash
HASH=$(echo -n "$TOKEN" | sha256sum | awk '{print $1}')

# 3. Insert into the database
sqlite3 /var/lib/rustunnel/tunnel.db <<SQL
INSERT INTO tokens (id, token_hash, label, created_at)
VALUES (lower(hex(randomblob(16))), '$HASH', 'bootstrap', datetime('now'));
SQL
```

Use the raw `$TOKEN` value with the CLI (`--token`) or dashboard.

---

## Backup and restore

```bash
# Hot backup (safe while server is running — WAL mode)
sqlite3 /var/lib/rustunnel/tunnel.db ".backup /tmp/tunnel-backup.db"

# Or simply copy the file while the server is stopped
cp /var/lib/rustunnel/tunnel.db /tmp/tunnel-backup.db

# Restore
systemctl stop rustunnel.service
cp /tmp/tunnel-backup.db /var/lib/rustunnel/tunnel.db
systemctl start rustunnel.service
```

---

## Maintenance

```bash
# Check database integrity
sqlite3 /var/lib/rustunnel/tunnel.db "PRAGMA integrity_check;"

# Show table sizes (approximate row counts)
sqlite3 /var/lib/rustunnel/tunnel.db "
  SELECT 'tokens' AS tbl, COUNT(*) FROM tokens UNION ALL
  SELECT 'tunnel_log',     COUNT(*) FROM tunnel_log UNION ALL
  SELECT 'captured_requests', COUNT(*) FROM captured_requests;
"

# Reclaim space after bulk deletes
sqlite3 /var/lib/rustunnel/tunnel.db "VACUUM;"

# Show database file size
ls -lh /var/lib/rustunnel/tunnel.db
```

### Purging old captured requests

Captured request bodies can accumulate quickly on busy tunnels.  A weekly
cron job to trim old data:

```bash
# /etc/cron.weekly/rustunnel-trim
sqlite3 /var/lib/rustunnel/tunnel.db \
  "DELETE FROM captured_requests WHERE captured_at < datetime('now', '-30 days');"
sqlite3 /var/lib/rustunnel/tunnel.db "VACUUM;"
```

---

## Schema migrations

The server applies migrations automatically on startup via an idempotent
`migrate()` function.  All `CREATE TABLE` / `CREATE INDEX` statements use
`IF NOT EXISTS`, and additive column changes use `ALTER TABLE … ADD COLUMN`
(which is a no-op if the column already exists).

There is no need to run migrations manually.  Downgrading the server binary
to an older version is safe as long as you do not need the new columns — the
old binary will simply ignore them.
