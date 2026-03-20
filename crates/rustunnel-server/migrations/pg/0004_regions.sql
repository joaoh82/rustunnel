-- Region registry: one row per active server region.
-- Seeded at deploy time; read by the client for region discovery and
-- by the dashboard for fan-out queries.

CREATE TABLE IF NOT EXISTS regions (
    id           TEXT PRIMARY KEY,          -- short identifier, e.g. "eu", "us", "ap"
    name         TEXT NOT NULL,             -- human-readable, e.g. "Europe"
    location     TEXT NOT NULL,             -- city/DC, e.g. "Falkenstein, DE"
    host         TEXT NOT NULL,             -- public hostname, e.g. "eu.edge.rustunnel.com"
    control_port INTEGER NOT NULL DEFAULT 4040,
    active       BOOLEAN NOT NULL DEFAULT true,
    added_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Seed the three planned regions.  INSERT ... ON CONFLICT DO NOTHING so
-- re-running migrations on an existing database is a no-op.
INSERT INTO regions (id, name, location, host) VALUES
    ('eu', 'Europe',       'Helsinki, FI',  'eu.edge.rustunnel.com'),
    ('us', 'US East',      'Ashburn, VA',   'us.edge.rustunnel.com'),
    ('ap', 'Asia Pacific', 'Singapore',     'ap.edge.rustunnel.com')
ON CONFLICT (id) DO NOTHING;
