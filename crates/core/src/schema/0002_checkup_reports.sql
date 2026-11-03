-- MirrorSelf index, schema v2: checkup reports (architecture §5 stage 5b, §6).
-- Lab results are observations with their source page, box and review
-- state; documents gain pages and provenance. Every v1 column of the v_*
-- views keeps its name and position; new columns are appended.

DROP VIEW v_labs;
DROP VIEW v_documents;

-- `pending` (waiting in the review queue) needs a new CHECK constraint, and
-- SQLite can only change one by rebuilding the table.
CREATE TABLE observations_v2 (
  id              TEXT PRIMARY KEY,
  fp              TEXT NOT NULL UNIQUE,
  code_system     TEXT NOT NULL,
  code            TEXT NOT NULL,
  value_num       REAL,
  value_text      TEXT,
  unit            TEXT,
  start_ts        TEXT NOT NULL,
  end_ts          TEXT NOT NULL,
  tz_offset       INTEGER NOT NULL,
  local_date      TEXT NOT NULL,
  device          TEXT,
  source_name     TEXT,
  source_tier     INTEGER NOT NULL DEFAULT 2,
  ref_low         REAL,
  ref_high        REAL,
  flag            TEXT,                     -- as printed on the report
  document_id     TEXT,
  raw_id          TEXT NOT NULL REFERENCES raw_objects(id),
  run_id          TEXT NOT NULL,
  confidence      REAL NOT NULL DEFAULT 1.0,
  status          TEXT NOT NULL DEFAULT 'auto'
                  CHECK (status IN ('auto', 'pending', 'confirmed', 'rejected', 'superseded', 'duplicate')),
  page            INTEGER,                  -- 1-based page of document_id
  bbox            TEXT,                     -- JSON [x0, y0, x1, y1] on that page, top-left origin
  label           TEXT,                     -- analyte name as printed (untrusted document text)
  ref_text        TEXT,                     -- reference range as printed
  value_canonical REAL,                     -- value in the analyte's canonical unit
  unit_canonical  TEXT,
  extracted       TEXT,                     -- the extraction as first read (JSON, §5 schema)
  review          TEXT                      -- review reasons and the user's decision (JSON)
);
INSERT INTO observations_v2
  (id, fp, code_system, code, value_num, value_text, unit, start_ts, end_ts, tz_offset, local_date,
   device, source_name, source_tier, ref_low, ref_high, flag, document_id, raw_id, run_id, confidence, status)
SELECT id, fp, code_system, code, value_num, value_text, unit, start_ts, end_ts, tz_offset, local_date,
       device, source_name, source_tier, ref_low, ref_high, flag, document_id, raw_id, run_id, confidence, status
FROM observations;
DROP TABLE observations;
ALTER TABLE observations_v2 RENAME TO observations;
CREATE INDEX observations_code_date ON observations(code, local_date);
CREATE INDEX observations_raw ON observations(raw_id);
CREATE INDEX observations_document ON observations(document_id, page);

ALTER TABLE documents ADD COLUMN title TEXT;
ALTER TABLE documents ADD COLUMN provider_name TEXT;   -- as printed (untrusted)
ALTER TABLE documents ADD COLUMN ocr_engine TEXT;
ALTER TABLE documents ADD COLUMN extractor TEXT;
ALTER TABLE documents ADD COLUMN run_id TEXT;
CREATE INDEX documents_raw ON documents(raw_id);

-- OCR output per page, kept so a new extractor version re-reads it without
-- running OCR again.
CREATE TABLE document_pages (
  document_id TEXT NOT NULL,
  page        INTEGER NOT NULL,
  width       REAL NOT NULL,
  height      REAL NOT NULL,
  source      TEXT NOT NULL CHECK (source IN ('text_layer', 'ocr')),
  text        TEXT NOT NULL,                -- untrusted document text
  ocr         TEXT NOT NULL,                -- JSON: lines and words with boxes and confidence
  run_id      TEXT NOT NULL,
  PRIMARY KEY (document_id, page)
);

CREATE INDEX ops_log_hlc ON ops_log(hlc);
CREATE INDEX ops_log_target ON ops_log(target);

CREATE VIEW v_labs AS
SELECT o.id, o.code, c.display, o.value_num, o.value_text, o.unit, o.ref_low, o.ref_high,
       o.flag, o.start_ts AS observed_at, o.local_date, o.document_id, o.raw_id,
       o.confidence, o.status,
       -- v2
       o.code_system, c.display_zh, o.label AS analyte_name, o.ref_text,
       CASE WHEN o.value_num IS NULL OR (o.ref_low IS NULL AND o.ref_high IS NULL) THEN NULL
            WHEN o.ref_high IS NOT NULL AND o.value_num > o.ref_high THEN 'above'
            WHEN o.ref_low IS NOT NULL AND o.value_num < o.ref_low THEN 'below'
            ELSE 'within' END AS vs_printed_range,
       o.value_canonical, o.unit_canonical, o.page, o.bbox,
       o.status = 'confirmed' AS verified,
       coalesce(json_extract(o.review, '$.decision') = 'edited', 0) AS edited,
       d.provider_name AS provider, d.issued_at AS report_date
FROM observations o
LEFT JOIN codes c ON c.code = o.code
LEFT JOIN documents d ON d.id = o.document_id
WHERE o.code_system IN ('loinc', 'local') AND o.status IN ('auto', 'confirmed');

CREATE VIEW v_documents AS
SELECT d.id, d.doc_type, d.issued_at, d.provider_id, d.language, d.pages, d.raw_id,
       -- v2
       d.provider_name, d.title, d.ocr_engine,
       (SELECT count(*) FROM observations o
         WHERE o.document_id = d.id AND o.status IN ('auto', 'confirmed')) AS results,
       (SELECT count(*) FROM observations o
         WHERE o.document_id = d.id AND o.status = 'pending') AS pending_review
FROM documents d;
