-- Record which region each tunnel was registered on.
-- NULL for existing rows (tunnels registered before multi-region was deployed).
--
-- Intentionally no FK constraint: region_id is an informational label.
-- Dev/single-server deployments may use values like "default" or "local"
-- that don't appear in the regions table, and that's fine.

ALTER TABLE tunnel_log
    ADD COLUMN IF NOT EXISTS region_id TEXT;

CREATE INDEX IF NOT EXISTS idx_tunnel_log_region ON tunnel_log (region_id);
