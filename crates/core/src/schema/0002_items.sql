-- MirrorSelf index, schema v2: categories beyond health, and `items`, the
-- metadata envelope for anything ingested that has no typed table of its own
-- (uploaded files, generic sync responses). See docs/demo-plan.md.

INSERT INTO categories (id, parent_id, name, name_zh) VALUES
  ('finance',   NULL, 'Finance',   '财务'),
  ('work',      NULL, 'Work',      '工作'),
  ('documents', NULL, 'Documents', '文档'),
  ('media',     NULL, 'Photos and media', '照片与媒体');

CREATE TABLE items (
  id                TEXT PRIMARY KEY,     -- stable_id("item", [raw_id, part])
  raw_id            TEXT NOT NULL REFERENCES raw_objects(id),
  part              TEXT NOT NULL DEFAULT '',  -- path or index inside the raw object; '' is the whole object
  type              TEXT NOT NULL,        -- document, photo, record, message, unknown, …
  subtype           TEXT,                 -- checkup_report, receipt, github_commit, …
  category_id       TEXT REFERENCES categories(id),
  title             TEXT,
  text              TEXT,                 -- extracted text
  event_start       TEXT,                 -- when it happened: UTC, RFC 3339
  event_end         TEXT,
  tz_offset         INTEGER,              -- original offset, seconds east of UTC
  local_date        TEXT,                 -- the day it counts toward
  received_at       TEXT NOT NULL,        -- upload or fetch time, from the raw object
  source_updated_at TEXT,                 -- last-modified time reported by the source; never the processing time
  lat               REAL,                 -- WGS-84
  lon               REAL,
  place             TEXT,
  data              TEXT NOT NULL DEFAULT '{}',
  run_id            TEXT NOT NULL,
  confidence        REAL NOT NULL DEFAULT 1.0,
  method            TEXT NOT NULL DEFAULT 'rule' CHECK (method IN ('rule', 'ai', 'user')),
  status            TEXT NOT NULL DEFAULT 'auto'
                    CHECK (status IN ('auto', 'needs_review', 'confirmed', 'rejected')),
  UNIQUE (raw_id, part)
);
CREATE INDEX items_type_date ON items(type, local_date);
CREATE INDEX items_event_start ON items(event_start);

CREATE VIEW v_items AS
SELECT i.id, i.type, i.subtype, i.category_id AS category, i.title, i.text,
       i.event_start, i.event_end, i.tz_offset, i.local_date,
       i.received_at, i.source_updated_at, i.lat, i.lon, i.place, i.data,
       i.confidence, i.method, i.status, i.raw_id,
       r.source, r.kind AS raw_kind, r.name AS file_name, r.mime
FROM items i
JOIN raw_objects r ON r.id = i.raw_id
WHERE i.status != 'rejected';
