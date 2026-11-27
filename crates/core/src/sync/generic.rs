//! The generic tier: `mcp_response` and `cli_response` objects become
//! `items` rows (type `record`), one per response or one per record when
//! the response holds an array of records.
//!
//! Extraction follows the recipe's `[sync.items]` rules, looked up by the
//! recipe name in the object's `origin_ref`. Changing those rules takes
//! effect on `mirrorself process --rebuild`.

use anyhow::Result;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, TimeZone, Utc};
use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::ids::stable_id;
use crate::pipeline::Processor;
use crate::sources::RawCtx;
use crate::sync::process::clean_text;
use crate::sync::recipe::{ItemRules, KIND_MCP_RESPONSE};
use crate::sync::runner::recipe_for_origin;

pub const PROCESSOR: Processor = Processor { name: "sync.generic", version: "1.0.0" };

const MAX_RECORDS: usize = 50_000;
const MAX_TITLE_CHARS: usize = 200;
const MAX_TEXT_CHARS: usize = 100_000;
const TITLE_FIELDS: &[&str] = &["title", "name", "subject", "summary", "message"];
const TEXT_FIELDS: &[&str] = &["body", "description", "text", "content"];

/// The useful part of a stored response.
#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
    Json(Value),
    Text(String),
}

/// For an MCP tool result: a single text block that is JSON (servers send
/// structured results that way for compatibility), else
/// `structuredContent`, else the text blocks. Anything else: JSON if it
/// parses, else text.
pub fn payload(kind: &str, content: &[u8]) -> Payload {
    let text = || Payload::Text(String::from_utf8_lossy(content).into_owned());
    let Ok(v) = serde_json::from_slice::<Value>(content) else {
        return text();
    };
    if kind != KIND_MCP_RESPONSE {
        return Payload::Json(v);
    }
    let texts: Vec<&str> = v["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["type"] == "text")
        .filter_map(|c| c["text"].as_str())
        .collect();
    if let [one] = texts.as_slice() {
        if let Ok(j) = serde_json::from_str::<Value>(one) {
            if j.is_object() || j.is_array() {
                return Payload::Json(j);
            }
        }
    }
    if let Some(s) = v.get("structuredContent").filter(|s| !s.is_null()) {
        return Payload::Json(s.clone());
    }
    if !texts.is_empty() {
        return Payload::Text(texts.join("\n"));
    }
    Payload::Json(v)
}

/// A dot-separated path into objects (`commit.author.date`); `""` is `v`.
pub fn get_path<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(v);
    }
    path.split('.').try_fold(v, |cur, key| cur.get(key)).filter(|x| !x.is_null())
}

fn scalar_string(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// When something happened, as stored in the index.
#[derive(Debug, Clone, PartialEq)]
pub struct When {
    pub utc: String,
    pub tz_offset: i32,
    pub local_date: String,
}

/// Parses RFC 3339, a naive `YYYY-MM-DDTHH:MM:SS`, a `YYYY-MM-DD` date, or
/// Unix seconds (milliseconds above 10^12). Timestamps in UTC or without an
/// offset take `local` for their local day.
pub fn parse_when(v: &Value, local: FixedOffset) -> Option<When> {
    let at = |t: DateTime<FixedOffset>| {
        let offset = if t.offset().local_minus_utc() == 0 { local } else { *t.offset() };
        let local_t = t.with_timezone(&offset);
        When {
            utc: t.with_timezone(&Utc).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            tz_offset: offset.local_minus_utc(),
            local_date: local_t.format("%Y-%m-%d").to_string(),
        }
    };
    match v {
        Value::Number(n) => {
            let n = n.as_f64()?;
            let secs = if n.abs() >= 1e12 { n / 1000.0 } else { n };
            let t = Utc.timestamp_opt(secs.floor() as i64, 0).single()?;
            Some(at(t.fixed_offset()))
        }
        Value::String(s) => {
            let s = s.trim();
            if let Ok(t) = DateTime::parse_from_rfc3339(s) {
                return Some(at(t));
            }
            for f in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
                if let Ok(n) = NaiveDateTime::parse_from_str(s, f) {
                    return local.from_local_datetime(&n).single().map(at);
                }
            }
            if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                let t = local.from_local_datetime(&d.and_hms_opt(0, 0, 0)?).single()?;
                return Some(When { local_date: s.to_string(), ..at(t) });
            }
            None
        }
        _ => None,
    }
}

fn first_line(s: &str) -> &str {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("")
}

fn title_of(rec: &Value, rules: &ItemRules) -> Option<String> {
    let raw = match (&rules.title_field, rec) {
        (Some(f), _) => scalar_string(get_path(rec, f)),
        (None, Value::String(s)) => Some(s.clone()),
        (None, Value::Object(_)) => TITLE_FIELDS.iter().find_map(|f| scalar_string(rec.get(*f))),
        _ => None,
    }?;
    let t = clean_text(first_line(&raw), MAX_TITLE_CHARS);
    (!t.is_empty()).then_some(t)
}

fn text_of(rec: &Value, rules: &ItemRules) -> Option<String> {
    let raw = match (&rules.text_field, rec) {
        (Some(f), _) => scalar_string(get_path(rec, f)),
        (None, Value::String(s)) => Some(s.clone()),
        (None, Value::Object(_)) => TEXT_FIELDS.iter().find_map(|f| scalar_string(rec.get(*f))),
        _ => None,
    }?;
    let t: String = raw.chars().take(MAX_TEXT_CHARS).collect();
    (!t.trim().is_empty()).then_some(t)
}

const INSERT_ITEM: &str = "INSERT OR REPLACE INTO items
  (id, raw_id, part, type, subtype, category_id, title, text, event_start, event_end, tz_offset, local_date,
   received_at, source_updated_at, data, run_id, confidence, method, status)
  VALUES (?1, ?2, ?3, 'record', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, 1.0, 'rule', 'auto')";

pub fn load(conn: &Connection, ctx: &RawCtx<'_>, content: &[u8]) -> Result<Value> {
    let recipe = recipe_for_origin(ctx.config, ctx.meta.origin_ref.as_deref());
    let default_rules = ItemRules::default();
    let rules = recipe.map(|r| &r.def.items).unwrap_or(&default_rules);
    let local = recipe.map(|r| r.utc_offset).unwrap_or_else(|| FixedOffset::east_opt(0).expect("zero"));
    let recipe_name = recipe.map(|r| r.name.as_str());

    let payload = payload(&ctx.meta.kind, content);
    let records: Vec<(String, Value)> = match &payload {
        Payload::Text(t) => {
            if t.trim().is_empty() {
                Vec::new()
            } else {
                vec![(String::new(), Value::String(t.clone()))]
            }
        }
        Payload::Json(v) => {
            let base = match &rules.records {
                Some(p) => get_path(v, p),
                None => Some(v),
            };
            match base {
                Some(Value::Array(a)) => a.iter().enumerate().map(|(i, x)| (i.to_string(), x.clone())).collect(),
                // `records` named a path that is absent or not an array: no records.
                _ if rules.records.is_some() => Vec::new(),
                Some(x) => vec![(String::new(), x.clone())],
                None => Vec::new(),
            }
        }
    };
    if records.len() > MAX_RECORDS {
        anyhow::bail!("the response holds {} records; at most {MAX_RECORDS} are kept", records.len());
    }

    let mut written = 0u64;
    let mut replaced = 0u64;
    let mut delete_older = conn.prepare_cached(
        "DELETE FROM items WHERE raw_id != ?1 AND json_extract(data, '$.sync_key') = ?2
           AND coalesce(subtype, '') = ?3
           AND raw_id IN (SELECT id FROM raw_objects WHERE source = ?4)",
    )?;
    let mut insert = conn.prepare_cached(INSERT_ITEM)?;
    for (index, rec) in records {
        let key = rules.key_field.as_deref().and_then(|f| scalar_string(get_path(&rec, f)));
        let part = match &key {
            Some(k) => format!("key:{k}"),
            None => index,
        };
        if let Some(k) = &key {
            replaced += delete_older.execute(params![
                ctx.raw_id,
                k,
                rules.subtype.as_deref().unwrap_or(""),
                ctx.meta.source
            ])? as u64;
        }
        let start = rules.time_field.as_deref().and_then(|f| get_path(&rec, f)).and_then(|v| parse_when(v, local));
        let end = rules.end_field.as_deref().and_then(|f| get_path(&rec, f)).and_then(|v| parse_when(v, local));
        let updated = rules.updated_field.as_deref().and_then(|f| get_path(&rec, f)).and_then(|v| parse_when(v, local));
        let data = json!({ "recipe": recipe_name, "sync_key": key, "record": rec });
        insert.execute(params![
            stable_id("item", &[ctx.raw_id, &part]),
            ctx.raw_id,
            part,
            rules.subtype,
            rules.category,
            title_of(&rec, rules),
            text_of(&rec, rules),
            start.as_ref().map(|w| &w.utc),
            end.as_ref().map(|w| &w.utc),
            start.as_ref().map(|w| w.tz_offset),
            start.as_ref().map(|w| &w.local_date),
            ctx.meta.received_at,
            updated.as_ref().map(|w| &w.utc),
            data.to_string(),
            ctx.run_id,
        ])?;
        written += 1;
    }
    Ok(json!({ "items": written, "replaced": replaced }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_prefers_a_json_text_block() {
        let r = br#"{"content":[{"type":"text","text":"[{\"a\":1}]"}],"structuredContent":{"result":[{"a":1}]}}"#;
        assert_eq!(payload(KIND_MCP_RESPONSE, r), Payload::Json(json!([{ "a": 1 }])));
        let r = br#"{"content":[{"type":"text","text":"plain words"}],"structuredContent":{"n":2}}"#;
        assert_eq!(payload(KIND_MCP_RESPONSE, r), Payload::Json(json!({ "n": 2 })));
        let r = br#"{"content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}"#;
        assert_eq!(payload(KIND_MCP_RESPONSE, r), Payload::Text("a\nb".into()));
        assert_eq!(payload("cli_response", b"not json"), Payload::Text("not json".into()));
    }

    #[test]
    fn times_take_the_recipe_offset_only_when_utc() {
        let cn = FixedOffset::east_opt(8 * 3600).unwrap();
        let w = parse_when(&json!("2026-11-02T18:30:00Z"), cn).unwrap();
        assert_eq!((w.utc.as_str(), w.tz_offset, w.local_date.as_str()), ("2026-11-02T18:30:00Z", 28_800, "2026-11-03"));
        let w = parse_when(&json!("2026-11-02T18:30:00-05:00"), cn).unwrap();
        assert_eq!((w.utc.as_str(), w.tz_offset, w.local_date.as_str()), ("2026-11-02T23:30:00Z", -18_000, "2026-11-02"));
        let w = parse_when(&json!("2026-11-02"), cn).unwrap();
        assert_eq!((w.utc.as_str(), w.local_date.as_str()), ("2026-11-01T16:00:00Z", "2026-11-02"));
        let w = parse_when(&json!(1_793_000_000_000i64), cn).unwrap();
        assert_eq!(w.utc, "2026-10-26T07:33:20Z");
        assert!(parse_when(&json!("yesterday"), cn).is_none());
    }
}
