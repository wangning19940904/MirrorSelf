-- MirrorSelf index, workstream W5: model annotations on items and the
-- effective values after review. See docs/model.md.
--
-- Both tables are derived. `item_annotations` is replayed from
-- `model_output` raw objects; review decisions live in `ops_log` (and in
-- `review_decision` raw objects, so they survive deleting the index), and
-- the `files.review_apply` stage rebuilds `item_effective` from them.

-- One row per stored model response. Model results never overwrite the
-- rule-made `items` row; they wait here until the user confirms them.
CREATE TABLE item_annotations (
  id             TEXT PRIMARY KEY,      -- stable_id("ann", [output_raw_id])
  item_id        TEXT NOT NULL,         -- the item the input file produced
  input_raw_id   TEXT NOT NULL,         -- the file that was sent
  output_raw_id  TEXT NOT NULL REFERENCES raw_objects(id),  -- the stored response
  model          TEXT NOT NULL,
  prompt_version TEXT NOT NULL,
  valid          INTEGER NOT NULL,      -- 1 when the response matched the schema
  error          TEXT,                  -- why it did not
  type           TEXT,
  subtype        TEXT,
  title          TEXT,
  text           TEXT,
  event_time     TEXT,                  -- as the model wrote it
  event_start    TEXT,                  -- UTC, when the model gave an offset
  tz_offset      INTEGER,
  local_date     TEXT,
  place          TEXT,
  confidence     REAL,
  method         TEXT NOT NULL DEFAULT 'ai' CHECK (method = 'ai'),
  status         TEXT NOT NULL CHECK (status IN ('needs_review', 'confirmed', 'rejected', 'invalid')),
  created_at     TEXT NOT NULL,         -- received_at of the stored response
  run_id         TEXT NOT NULL
);
CREATE INDEX item_annotations_item ON item_annotations(item_id);

-- Effective values for items that have annotations or review decisions.
-- Precedence per field: user edit, then the rule value, then a confirmed
-- model value (a title taken from the file name counts as no rule value).
CREATE TABLE item_effective (
  item_id       TEXT PRIMARY KEY,
  type          TEXT NOT NULL,
  subtype       TEXT,
  category_id   TEXT,
  title         TEXT,
  text          TEXT,
  event_start   TEXT,
  event_end     TEXT,
  tz_offset     INTEGER,
  local_date    TEXT,
  lat           REAL,
  lon           REAL,
  place         TEXT,
  confidence    REAL NOT NULL,
  method        TEXT NOT NULL CHECK (method IN ('rule', 'ai', 'user')),
  status        TEXT NOT NULL CHECK (status IN ('auto', 'needs_review', 'confirmed', 'rejected')),
  annotation_id TEXT,                   -- the confirmed annotation in use
  pending       INTEGER NOT NULL DEFAULT 0,  -- annotations awaiting review
  edited        INTEGER NOT NULL DEFAULT 0   -- 1 when the user edited a field
);

-- Every item with its effective values, rejected ones included.
CREATE VIEW v_items_all AS
SELECT i.id,
       CASE WHEN e.item_id IS NULL THEN i.type ELSE e.type END AS type,
       CASE WHEN e.item_id IS NULL THEN i.subtype ELSE e.subtype END AS subtype,
       CASE WHEN e.item_id IS NULL THEN i.category_id ELSE e.category_id END AS category,
       CASE WHEN e.item_id IS NULL THEN i.title ELSE e.title END AS title,
       CASE WHEN e.item_id IS NULL THEN i.text ELSE e.text END AS text,
       CASE WHEN e.item_id IS NULL THEN i.event_start ELSE e.event_start END AS event_start,
       CASE WHEN e.item_id IS NULL THEN i.event_end ELSE e.event_end END AS event_end,
       CASE WHEN e.item_id IS NULL THEN i.tz_offset ELSE e.tz_offset END AS tz_offset,
       CASE WHEN e.item_id IS NULL THEN i.local_date ELSE e.local_date END AS local_date,
       i.received_at, i.source_updated_at,
       CASE WHEN e.item_id IS NULL THEN i.lat ELSE e.lat END AS lat,
       CASE WHEN e.item_id IS NULL THEN i.lon ELSE e.lon END AS lon,
       CASE WHEN e.item_id IS NULL THEN i.place ELSE e.place END AS place,
       i.data,
       CASE WHEN e.item_id IS NULL THEN i.confidence ELSE e.confidence END AS confidence,
       CASE WHEN e.item_id IS NULL THEN i.method ELSE e.method END AS method,
       CASE WHEN e.item_id IS NULL THEN i.status ELSE e.status END AS status,
       i.raw_id,
       r.source, r.kind AS raw_kind, r.name AS file_name, r.mime,
       e.annotation_id,
       coalesce(e.pending, 0) AS pending_annotations,
       coalesce(e.edited, 0) AS edited
FROM items i
JOIN raw_objects r ON r.id = i.raw_id
LEFT JOIN item_effective e ON e.item_id = i.id;

-- v_items keeps every column it had, in the same order, and now shows
-- effective values. New columns are appended.
DROP VIEW v_items;
CREATE VIEW v_items AS
SELECT * FROM v_items_all WHERE status != 'rejected';

-- What models said about items, including responses that failed
-- validation (status 'invalid', with the reason in `error`).
CREATE VIEW v_item_annotations AS
SELECT a.id, a.item_id, a.method, a.model, a.prompt_version, a.status, a.confidence,
       a.type, a.subtype, a.title, a.text, a.event_time, a.event_start, a.tz_offset,
       a.local_date, a.place, a.error, a.created_at, a.input_raw_id, a.output_raw_id
FROM item_annotations a;
