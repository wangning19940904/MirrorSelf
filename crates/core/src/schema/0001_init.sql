-- MirrorSelf index, schema v1 (architecture §6).
-- Everything here except ops_log, grants and audit_log is derived from the
-- raw vault and can be rebuilt with `mirrorself process --rebuild`.

CREATE TABLE raw_objects (
  id          TEXT PRIMARY KEY,           -- keyed content hash
  source      TEXT NOT NULL,
  kind        TEXT NOT NULL,
  mime        TEXT NOT NULL,
  name        TEXT,
  received_at TEXT NOT NULL,
  origin_ref  TEXT,
  storage_uri TEXT NOT NULL,
  key_id      TEXT NOT NULL,
  bytes       INTEGER NOT NULL,
  meta        TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE processor_runs (
  id          TEXT PRIMARY KEY,
  processor   TEXT NOT NULL,
  version     TEXT NOT NULL,
  raw_id      TEXT,                       -- NULL for whole-index stages
  status      TEXT NOT NULL CHECK (status IN ('ok', 'error')),
  started_at  TEXT NOT NULL,
  finished_at TEXT,
  outputs     TEXT,
  error       TEXT
);
CREATE INDEX processor_runs_raw ON processor_runs(raw_id, processor);

CREATE TABLE categories (
  id        TEXT PRIMARY KEY,
  parent_id TEXT REFERENCES categories(id),
  name      TEXT NOT NULL,
  name_zh   TEXT NOT NULL
);
INSERT INTO categories (id, parent_id, name, name_zh) VALUES
  ('sleep',     NULL, 'Sleep',       '睡眠'),
  ('activity',  NULL, 'Activity',    '活动'),
  ('cardio',    NULL, 'Cardio',      '心血管'),
  ('metabolic', NULL, 'Metabolic',   '代谢'),
  ('labs',      NULL, 'Labs',        '化验'),
  ('nutrition', NULL, 'Nutrition',   '营养'),
  ('symptoms',  NULL, 'Symptoms',    '症状'),
  ('meds',      NULL, 'Medications', '用药');

-- Code list (ms: internal codes now, LOINC lab analytes from M2). Seeded
-- from codes.rs on every open.
CREATE TABLE codes (
  code        TEXT PRIMARY KEY,
  code_system TEXT NOT NULL,
  display     TEXT NOT NULL,
  display_zh  TEXT,
  category_id TEXT NOT NULL REFERENCES categories(id),
  unit        TEXT NOT NULL,
  aggregation TEXT NOT NULL CHECK (aggregation IN ('sum', 'mean')),
  minmax      INTEGER NOT NULL DEFAULT 0,
  notable     INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE observations (
  id          TEXT PRIMARY KEY,
  fp          TEXT NOT NULL UNIQUE,       -- source-independent fingerprint; dedupes overlapping exports
  code_system TEXT NOT NULL,
  code        TEXT NOT NULL,
  value_num   REAL,
  value_text  TEXT,
  unit        TEXT,
  start_ts    TEXT NOT NULL,              -- UTC, RFC 3339
  end_ts      TEXT NOT NULL,
  tz_offset   INTEGER NOT NULL,           -- original offset, seconds east of UTC
  local_date  TEXT NOT NULL,              -- the day this value counts toward
  device      TEXT,
  source_name TEXT,
  source_tier INTEGER NOT NULL DEFAULT 2, -- 0 watch, 1 phone, 2 other
  ref_low     REAL,
  ref_high    REAL,
  flag        TEXT,
  document_id TEXT,
  raw_id      TEXT NOT NULL REFERENCES raw_objects(id),
  run_id      TEXT NOT NULL,
  confidence  REAL NOT NULL DEFAULT 1.0,
  status      TEXT NOT NULL DEFAULT 'auto'
              CHECK (status IN ('auto', 'confirmed', 'rejected', 'superseded', 'duplicate'))
);
CREATE INDEX observations_code_date ON observations(code, local_date);
CREATE INDEX observations_raw ON observations(raw_id);

CREATE TABLE events (
  id          TEXT PRIMARY KEY,
  fp          TEXT NOT NULL UNIQUE,
  type        TEXT NOT NULL,              -- workout, sleep, visit, checkup, symptom, medication, note
  category_id TEXT REFERENCES categories(id),
  start_ts    TEXT NOT NULL,
  end_ts      TEXT NOT NULL,
  tz_offset   INTEGER NOT NULL,
  local_date  TEXT NOT NULL,
  title       TEXT NOT NULL,
  body        TEXT,
  data        TEXT NOT NULL DEFAULT '{}',
  place_id    TEXT,
  raw_id      TEXT REFERENCES raw_objects(id),
  run_id      TEXT NOT NULL,
  derived     INTEGER NOT NULL DEFAULT 0  -- 1 when built by a whole-index stage
);
CREATE INDEX events_start ON events(start_ts);
CREATE INDEX events_type_date ON events(type, local_date);

CREATE TABLE documents (
  id          TEXT PRIMARY KEY,
  raw_id      TEXT NOT NULL REFERENCES raw_objects(id),
  doc_type    TEXT NOT NULL,
  issued_at   TEXT,
  provider_id TEXT,
  language    TEXT,
  pages       INTEGER,
  text        TEXT
);

CREATE TABLE entities (
  id      TEXT PRIMARY KEY,
  kind    TEXT NOT NULL,                  -- person, provider, place, condition, medication, device, activity_type, food
  name    TEXT NOT NULL,
  aliases TEXT NOT NULL DEFAULT '[]',
  codes   TEXT NOT NULL DEFAULT '{}',
  UNIQUE (kind, name)
);

CREATE TABLE links (
  from_type  TEXT NOT NULL,
  from_id    TEXT NOT NULL,
  to_type    TEXT NOT NULL,
  to_id      TEXT NOT NULL,
  relation   TEXT NOT NULL,               -- occurred_at, involves, measured_by, part_of, same_as, mentions
  confidence REAL NOT NULL DEFAULT 1.0,
  method     TEXT NOT NULL CHECK (method IN ('rule', 'ai', 'user')),
  PRIMARY KEY (from_type, from_id, to_type, to_id, relation)
);
CREATE INDEX links_to ON links(to_type, to_id);

CREATE TABLE episodes (
  id       TEXT PRIMARY KEY,
  kind     TEXT NOT NULL,                 -- illness, training_block, trip, checkup_cycle, medication_course
  start_ts TEXT NOT NULL,
  end_ts   TEXT,
  title    TEXT NOT NULL,
  summary  TEXT
);

-- User edits (review decisions, notes, merges). Synced; replayed on rebuild.
CREATE TABLE ops_log (
  id      TEXT PRIMARY KEY,
  hlc     TEXT NOT NULL,
  actor   TEXT NOT NULL,
  op      TEXT NOT NULL,
  target  TEXT NOT NULL,
  payload TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE grants (
  id         TEXT PRIMARY KEY,
  client_id  TEXT NOT NULL,
  scopes     TEXT NOT NULL,
  filters    TEXT NOT NULL DEFAULT '{}',
  redaction  TEXT NOT NULL DEFAULT 'names',
  created_at TEXT NOT NULL,
  expires_at TEXT,
  revoked_at TEXT
);

CREATE TABLE audit_log (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  ts           TEXT NOT NULL,
  actor        TEXT NOT NULL,
  client_id    TEXT NOT NULL,
  action       TEXT NOT NULL,
  scope        TEXT NOT NULL,
  query_digest TEXT NOT NULL,
  rows         INTEGER NOT NULL,
  outcome      TEXT NOT NULL DEFAULT 'ok' CHECK (outcome IN ('ok', 'denied', 'error'))
);

CREATE TABLE daily_metrics (
  date         TEXT NOT NULL,
  code         TEXT NOT NULL,
  stat         TEXT NOT NULL,             -- sum, mean, min, max
  value        REAL NOT NULL,
  n            INTEGER NOT NULL,          -- samples behind the value
  baseline_28d REAL,                      -- mean of the previous 28 days (>= 7 days of data)
  sd_28d       REAL,
  z            REAL,
  notable      INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (code, stat, date)
);

CREATE VIRTUAL TABLE search_fts USING fts5(
  kind UNINDEXED,
  ref_id UNINDEXED,
  local_date UNINDEXED,
  title,
  body,
  tokenize = 'unicode61 remove_diacritics 2'
);

-- Agent-facing views: a stable, read-only contract. Base tables can change
-- between versions; these cannot without a major version bump.

CREATE VIEW v_metrics_daily AS
SELECT d.date, d.code, c.display, c.category_id AS category, d.stat, d.value, c.unit,
       d.n, d.baseline_28d, d.sd_28d, d.z, d.notable
FROM daily_metrics d
LEFT JOIN codes c ON c.code = d.code;

CREATE VIEW v_timeline AS
SELECT 'event' AS kind, e.id, e.type, e.category_id AS category, e.start_ts, e.end_ts,
       e.local_date, e.title, e.data, e.raw_id
FROM events e
UNION ALL
SELECT 'notable', 'dm:' || d.code || ':' || d.stat || ':' || d.date, 'metric_deviation',
       c.category_id, d.date || 'T12:00:00Z', d.date || 'T12:00:00Z', d.date,
       c.display || ' ' || printf('%.1f', d.value) || ' ' || c.unit || ' ('
         || printf('%+.1f', d.z) || ' SD vs 28-day baseline '
         || printf('%.1f', d.baseline_28d) || ')',
       json_object('code', d.code, 'stat', d.stat, 'value', d.value, 'unit', c.unit,
                   'baseline_28d', d.baseline_28d, 'sd_28d', d.sd_28d, 'z', d.z, 'n', d.n),
       NULL
FROM daily_metrics d
JOIN codes c ON c.code = d.code
WHERE d.notable = 1;

CREATE VIEW v_labs AS
SELECT o.id, o.code, c.display, o.value_num, o.value_text, o.unit, o.ref_low, o.ref_high,
       o.flag, o.start_ts AS observed_at, o.local_date, o.document_id, o.raw_id,
       o.confidence, o.status
FROM observations o
LEFT JOIN codes c ON c.code = o.code
WHERE o.code_system = 'loinc' AND o.status IN ('auto', 'confirmed');

CREATE VIEW v_documents AS
SELECT id, doc_type, issued_at, provider_id, language, pages, raw_id
FROM documents;

CREATE VIEW v_entities AS
SELECT id, kind, name, aliases
FROM entities;

CREATE VIEW v_links AS
SELECT from_type, from_id, to_type, to_id, relation, confidence, method
FROM links;

CREATE VIEW v_codes AS
SELECT code, code_system, display, display_zh, category_id AS category, unit, aggregation
FROM codes;
