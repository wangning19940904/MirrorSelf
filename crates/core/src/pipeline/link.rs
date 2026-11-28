//! Rule-based linking (architecture §5, stage 6) and the search index.
//!
//! v1 rules: devices and apps become `device` entities, workout types become
//! `activity_type` entities, and events link to them. AI fuzzy matching of
//! providers and people arrives with documents (M2).

use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::codes::RECORD_CATEGORIES;
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
        .prepare(
            "SELECT DISTINCT o.source_name FROM observations o JOIN codes c ON c.code = o.code
             WHERE o.source_name IS NOT NULL
               AND c.category_id NOT IN (SELECT value FROM json_each(?1))
             ORDER BY o.source_name",
        )?
        .query_map([serde_json::to_string(RECORD_CATEGORIES)?], |r| r.get(0))?
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
           SELECT 'item', id, local_date, coalesce(title, type),
                  type || ' ' || coalesce(subtype, '') || ' ' || coalesce(place, '') || ' ' || coalesce(text, '')
           FROM items WHERE status != 'rejected';
         INSERT INTO search_fts (kind, ref_id, local_date, title, body)
           SELECT 'metric', code, NULL, display, code || ' ' || coalesce(display_zh, '') || ' ' || category_id
           FROM codes;",
    )?;
    let rows: i64 = conn.query_row("SELECT count(*) FROM search_fts", [], |r| r.get(0))?;
    Ok(json!({ "documents": rows }))
}
