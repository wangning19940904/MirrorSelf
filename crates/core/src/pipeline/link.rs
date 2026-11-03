//! Rule-based linking (architecture §5, stage 6) and the search index.
//!
//! Rules: devices and apps become `device` entities, workout types become
//! `activity_type` entities, the providers named on checkup reports become
//! `provider` entities, and events link to them. AI fuzzy matching of
//! provider and people names ("Dr. Wang" = "王医生") comes later.

use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::ids::stable_id;

pub(super) fn run(conn: &Connection) -> Result<Value> {
    conn.execute("DELETE FROM links WHERE method = 'rule'", [])?;
    conn.execute("DELETE FROM entities", [])?;

    let mut entity = conn.prepare("INSERT OR IGNORE INTO entities (id, kind, name) VALUES (?1, ?2, ?3)")?;
    let mut link = conn.prepare(
        "INSERT OR IGNORE INTO links (from_type, from_id, to_type, to_id, relation, confidence, method)
         VALUES ('event', ?1, 'entity', ?2, ?3, 1.0, 'rule')",
    )?;
    let mut add_entity = |kind: &str, name: &str| -> Result<String> {
        let id = stable_id("ent", &[kind, name]);
        entity.execute(params![id, kind, name])?;
        Ok(id)
    };

    let sources: Vec<String> = conn
        .prepare("SELECT DISTINCT source_name FROM observations
                  WHERE source_name IS NOT NULL AND code_system = 'ms' ORDER BY source_name")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for name in &sources {
        add_entity("device", name)?;
    }

    let events: Vec<(String, String, String)> = conn
        .prepare("SELECT id, type, data FROM events ORDER BY start_ts, id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut links = 0u64;
    for (event_id, kind, data) in events {
        let data: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        if kind == "workout" {
            if let Some(activity) = data["activity"].as_str() {
                let to = add_entity("activity_type", activity)?;
                links += link.execute(params![event_id, to, "involves"])? as u64;
            }
            if let Some(source) = data["source_name"].as_str() {
                let to = add_entity("device", source)?;
                links += link.execute(params![event_id, to, "measured_by"])? as u64;
            }
        } else if kind == "sleep" {
            for source in data["sources"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                let to = add_entity("device", source)?;
                links += link.execute(params![event_id, to, "measured_by"])? as u64;
            }
        } else if kind == "checkup" {
            if let Some(provider) = data["provider"].as_str() {
                let to = add_entity("provider", provider)?;
                links += link.execute(params![event_id, to, "occurred_at"])? as u64;
            }
        }
    }
    let entities: i64 = conn.query_row("SELECT count(*) FROM entities", [], |r| r.get(0))?;
    Ok(json!({ "entities": entities, "links": links }))
}

pub(super) fn index_search(conn: &Connection) -> Result<Value> {
    conn.execute_batch(
        "DELETE FROM search_fts;
         INSERT INTO search_fts (kind, ref_id, local_date, title, body)
           SELECT 'event', id, local_date, title,
                  type || ' ' || coalesce(body, '') || ' ' || coalesce(json_extract(data, '$.source_name'), '')
           FROM events;
         INSERT INTO search_fts (kind, ref_id, local_date, title, body)
           SELECT 'entity', id, NULL, name, kind || ' ' || aliases FROM entities;
         INSERT INTO search_fts (kind, ref_id, local_date, title, body)
           SELECT CASE code_system WHEN 'loinc' THEN 'analyte' ELSE 'metric' END, code, NULL, display,
                  code || ' ' || coalesce(display_zh, '') || ' ' || category_id
           FROM codes;
         INSERT INTO search_fts (kind, ref_id, local_date, title, body)
           SELECT 'document', d.id, d.issued_at, coalesce(d.title, 'Document'),
                  coalesce(d.provider_name, '') || ' ' ||
                  coalesce((SELECT group_concat(o.label, ' ') FROM observations o WHERE o.document_id = d.id), '') || ' ' ||
                  coalesce(d.text, '')
           FROM documents d;",
    )?;
    let rows: i64 = conn.query_row("SELECT count(*) FROM search_fts", [], |r| r.get(0))?;
    Ok(json!({ "documents": rows }))
}
