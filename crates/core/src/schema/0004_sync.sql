-- Scheduled sync (workstream W3, docs/sync.md): per-recipe state and run
-- history. These tables record what the hub did, not what it derived from
-- the vault, so `process --rebuild` leaves them alone. Credentials are not
-- here; they live sealed in <home>/sync-secrets.json.

CREATE TABLE sync_state (
  recipe          TEXT PRIMARY KEY,
  cursor          TEXT,               -- UTC end of the last successful window (RFC 3339)
  last_run_at     TEXT,               -- last attempt, successful or not
  last_success_at TEXT,
  last_error      TEXT,               -- cleared by the next success
  last_error_at   TEXT,
  paused          INTEGER NOT NULL DEFAULT 0,
  paused_reason   TEXT,
  paused_at       TEXT,
  tool            TEXT,               -- MCP tool the pin belongs to
  schema_sha256   TEXT,               -- pinned input schema of that tool
  pinned_at       TEXT
);

CREATE TABLE sync_runs (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  recipe      TEXT NOT NULL,
  started_at  TEXT NOT NULL,
  finished_at TEXT NOT NULL,
  window_from TEXT,
  window_to   TEXT,
  status      TEXT NOT NULL CHECK (status IN ('ok', 'error', 'refused', 'paused')),
  raw_id      TEXT,                   -- the stored response
  created     INTEGER,                -- 1 when the response was new to the vault
  bytes       INTEGER,
  error       TEXT
);
CREATE INDEX sync_runs_recipe ON sync_runs(recipe, id);

CREATE VIEW v_sync_status AS
SELECT recipe, cursor, last_run_at, last_success_at, last_error, last_error_at,
       paused, paused_reason, tool, schema_sha256 AS pinned_schema_sha256
FROM sync_state;

CREATE VIEW v_sync_runs AS
SELECT id, recipe, started_at, finished_at, window_from, window_to, status,
       raw_id, created, bytes, error
FROM sync_runs;
