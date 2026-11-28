//! Review of items and model annotations: confirm, reject, edit.
//!
//! A decision is stored twice: as a `review_decision` raw object in the
//! vault (so it survives deleting the index) and, through the pipeline, as
//! an `ops_log` row. The `files.review_apply` stage replays `ops_log` in
//! order to compute `item_effective`, which `v_items` shows. Nothing here
//! edits `items` or `item_annotations` rows in place except their review
//! status, which the replay recomputes from scratch every time.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use chrono::DateTime;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::crypto::random_bytes;
use crate::error::{conflict, invalid, not_found};
use crate::importers::files::{clean_line, truncate_chars, MAX_TEXT_CHARS, MAX_TITLE_CHARS};
use crate::sources::{GlobalCtx, RawCtx};
use crate::time::now_utc_precise;
use crate::{App, RawInput};

use super::parse_event_time;

/// `raw_objects.kind` of a stored review decision.
pub const KIND_REVIEW_DECISION: &str = "review_decision";
pub const SOURCE_USER: &str = "user";
pub const DECISION_FORMAT: &str = "mirrorself.review/1";
pub const OP_CONFIRM: &str = "item.confirm";
pub const OP_REJECT: &str = "item.reject";
pub const OP_EDIT: &str = "item.edit";

/// Types a user may set: the rule types plus the model's.
pub const EDIT_TYPES: &[&str] = &["document", "photo", "screenshot", "file", "other"];
/// Fields `edit --clear` accepts.
pub const CLEARABLE: &[&str] = &["title", "subtype", "text", "event_time", "place", "location"];

/// One review decision as stored in the vault.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub format: String,
    pub op_id: String,
    pub hlc: String,
    pub actor: String,
    pub op: String,
    /// The item id.
    pub target: String,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub enum Review {
    /// Accept a pending annotation (or, with none pending, the item as is).
    Confirm { annotation: Option<String> },
    /// Reject a pending annotation (or, with none pending, hide the item).
    Reject { annotation: Option<String> },
    Edit(Edit),
}

/// Corrected values. `None` leaves a field alone; `clear` empties one.
#[derive(Debug, Clone, Default)]
pub struct Edit {
    pub title: Option<String>,
    pub item_type: Option<String>,
    pub subtype: Option<String>,
    pub text: Option<String>,
    /// RFC 3339 with offset, a local date-time, or a date.
    pub event_time: Option<String>,
    pub place: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub clear: Vec<String>,
}

impl Edit {
    /// Normalized field values for the `item.edit` payload.
    fn fields(&self) -> Result<Map<String, Value>> {
        let mut f = Map::new();
        for c in &self.clear {
            match c.as_str() {
                "event_time" => {
                    for k in ["event_start", "tz_offset", "local_date"] {
                        f.insert(k.into(), Value::Null);
                    }
                }
                "location" => {
                    f.insert("lat".into(), Value::Null);
                    f.insert("lon".into(), Value::Null);
                }
                "title" | "subtype" | "text" | "place" => {
                    f.insert(c.clone(), Value::Null);
                }
                other => return Err(invalid(format!("cannot clear '{other}' (use one of {})", CLEARABLE.join(", ")))),
            }
        }
        if let Some(t) = &self.title {
            let t = clean_line(t, MAX_TITLE_CHARS);
            if t.is_empty() {
                return Err(invalid("title is empty (use --clear title to remove it)"));
            }
            f.insert("title".into(), json!(t));
        }
        if let Some(t) = &self.item_type {
            if !EDIT_TYPES.contains(&t.as_str()) {
                return Err(invalid(format!("type must be one of {}", EDIT_TYPES.join(", "))));
            }
            f.insert("type".into(), json!(t));
        }
        if let Some(s) = &self.subtype {
            let ok = !s.is_empty() && s.len() <= 40 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
            if !ok {
                return Err(invalid("subtype must be a short lower-case name such as receipt or itinerary"));
            }
            f.insert("subtype".into(), json!(s));
        }
        if let Some(t) = &self.text {
            let (t, truncated) = truncate_chars(t.replace("\r\n", "\n"), MAX_TEXT_CHARS);
            if truncated {
                return Err(invalid(format!("text is longer than {MAX_TEXT_CHARS} characters")));
            }
            f.insert("text".into(), json!(t));
        }
        if let Some(e) = &self.event_time {
            let ev = parse_event_time(e)
                .ok_or_else(|| invalid(format!("event time '{e}' is not RFC 3339, YYYY-MM-DDTHH:MM:SS or YYYY-MM-DD")))?;
            f.insert("event_start".into(), json!(ev.start_utc));
            f.insert("tz_offset".into(), json!(ev.tz_offset));
            f.insert("local_date".into(), json!(ev.local_date));
        }
        if let Some(p) = &self.place {
            let p = clean_line(p, MAX_TITLE_CHARS);
            if p.is_empty() {
                return Err(invalid("place is empty (use --clear place to remove it)"));
            }
            f.insert("place".into(), json!(p));
        }
        match (self.lat, self.lon) {
            (None, None) => {}
            (Some(lat), Some(lon)) => {
                if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
                    return Err(invalid("latitude must be within ±90 and longitude within ±180"));
                }
                f.insert("lat".into(), json!(lat));
                f.insert("lon".into(), json!(lon));
            }
            _ => return Err(invalid("give both --lat and --lon")),
        }
        if f.is_empty() {
            return Err(invalid("nothing to edit: pass a field such as --title, or --clear"));
        }
        Ok(f)
    }
}

/// Records a decision on an item (or an annotation id) and applies it.
pub fn review(app: &App, id: &str, r: Review) -> Result<Value> {
    let conn = app.conn();
    let (item_id, named_annotation) = resolve(conn, id)?;
    let (op, payload) = match r {
        Review::Confirm { annotation } => (OP_CONFIRM, annotation_payload(conn, &item_id, annotation.or(named_annotation), true)?),
        Review::Reject { annotation } => (OP_REJECT, annotation_payload(conn, &item_id, annotation.or(named_annotation), false)?),
        Review::Edit(e) => {
            if named_annotation.is_some() {
                return Err(invalid("edit an item, not an annotation"));
            }
            (OP_EDIT, json!({ "fields": e.fields()? }))
        }
    };
    let decision = Decision {
        format: DECISION_FORMAT.into(),
        op_id: format!("op_{}", hex::encode(random_bytes::<16>())),
        hlc: now_utc_precise(),
        actor: "user:cli".into(),
        op: op.into(),
        target: item_id.clone(),
        payload,
    };
    let report = app.ingest(
        RawInput {
            source: SOURCE_USER.into(),
            kind: KIND_REVIEW_DECISION.into(),
            mime: "application/json".into(),
            name: None,
            origin_ref: Some(format!("review:{op}:{item_id}")),
        },
        &serde_json::to_vec(&decision)?,
        true,
    )?;
    if let Some(err) = report.process.as_ref().and_then(|p| p.errors().find(|r| r.raw_id.as_deref() == Some(&report.raw_id))) {
        bail!("the decision was stored but not applied: {}", err.error.clone().unwrap_or_default());
    }
    let mut out = show(conn, &item_id)?;
    out["decision"] = json!({ "op_id": decision.op_id, "op": op, "payload": decision.payload, "raw_id": report.raw_id });
    Ok(out)
}

/// An item id, or an annotation id and its item.
fn resolve(conn: &Connection, id: &str) -> Result<(String, Option<String>)> {
    if id.starts_with("ann_") {
        let item: Option<String> =
            conn.query_row("SELECT item_id FROM item_annotations WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        let item = item.ok_or_else(|| not_found(format!("no annotation '{id}'")))?;
        return Ok((item, Some(id.to_string())));
    }
    let exists: Option<String> = conn.query_row("SELECT id FROM items WHERE id = ?1", [id], |r| r.get(0)).optional()?;
    exists.map(|i| (i, None)).ok_or_else(|| not_found(format!("no item '{id}'")))
}

fn annotation_payload(conn: &Connection, item_id: &str, annotation: Option<String>, confirm: bool) -> Result<Value> {
    if let Some(a) = annotation {
        let row: Option<(String, bool)> = conn
            .query_row("SELECT item_id, valid FROM item_annotations WHERE id = ?1", [&a], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        match row {
            None => return Err(not_found(format!("no annotation '{a}'"))),
            Some((i, _)) if i != item_id => return Err(invalid(format!("annotation '{a}' belongs to another item"))),
            Some((_, false)) => return Err(invalid(format!("annotation '{a}' failed validation and cannot be reviewed"))),
            Some(_) => return Ok(json!({ "annotation_id": a })),
        }
    }
    let pending: Vec<String> = conn
        .prepare("SELECT id FROM item_annotations WHERE item_id = ?1 AND status = 'needs_review' ORDER BY created_at, id")?
        .query_map([item_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    match pending.as_slice() {
        [] => {
            let status: String = conn.query_row(
                "SELECT coalesce(e.status, i.status) FROM items i LEFT JOIN item_effective e ON e.item_id = i.id WHERE i.id = ?1",
                [item_id],
                |r| r.get(0),
            )?;
            if confirm && status == "confirmed" {
                return Err(conflict(format!("item '{item_id}' is already confirmed")));
            }
            Ok(json!({}))
        }
        [one] => Ok(json!({ "annotation_id": one })),
        many => Err(invalid(format!(
            "item '{item_id}' has {} annotations awaiting review; pass --annotation with one of {}",
            many.len(),
            many.join(", ")
        ))),
    }
}

/// The `review_decision` processor: copies a stored decision into `ops_log`.
pub fn load_decision(conn: &Connection, _ctx: &RawCtx<'_>, content: &[u8]) -> Result<Value> {
    let d: Decision = serde_json::from_slice(content).context("review_decision is not a MirrorSelf decision")?;
    let id_ok = d.op_id.len() == 35 && d.op_id.starts_with("op_") && d.op_id[3..].bytes().all(|b| b.is_ascii_hexdigit());
    if d.format != DECISION_FORMAT
        || !id_ok
        || ![OP_CONFIRM, OP_REJECT, OP_EDIT].contains(&d.op.as_str())
        || !d.target.starts_with("item_")
        || d.target.len() > 64
        || d.actor.len() > 64
        || DateTime::parse_from_rfc3339(&d.hlc).is_err()
        || !d.payload.is_object()
    {
        bail!("review_decision is malformed");
    }
    conn.execute(
        "INSERT OR IGNORE INTO ops_log (id, hlc, actor, op, target, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![d.op_id, d.hlc, d.actor, d.op, d.target, d.payload.to_string()],
    )?;
    Ok(json!({ "ops": 1 }))
}

/// Decisions are user data: a new processor version must not drop them.
pub fn keep_decisions(_conn: &Connection, _processor: &str) -> Result<()> {
    Ok(())
}

#[derive(Default)]
struct State {
    item: Option<&'static str>,
    confirmed: Option<String>,
    edits: Map<String, Value>,
}

struct Base {
    item_type: String,
    subtype: Option<String>,
    category: Option<String>,
    title: Option<String>,
    title_weak: bool,
    text: Option<String>,
    event_start: Option<String>,
    event_end: Option<String>,
    tz_offset: Option<i64>,
    local_date: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    place: Option<String>,
    confidence: f64,
    method: String,
    status: String,
}

struct Ann {
    id: String,
    item_type: Option<String>,
    subtype: Option<String>,
    title: Option<String>,
    text: Option<String>,
    event_start: Option<String>,
    tz_offset: Option<i64>,
    local_date: Option<String>,
    place: Option<String>,
    confidence: f64,
    status: String,
}

/// The `files.review_apply` stage: recomputes annotation statuses and
/// `item_effective` from `item_annotations` and `ops_log`.
pub fn apply(conn: &Connection, _ctx: &GlobalCtx<'_>) -> Result<Value> {
    conn.execute("UPDATE item_annotations SET status = CASE WHEN valid = 1 THEN 'needs_review' ELSE 'invalid' END", [])?;
    conn.execute("DELETE FROM item_effective", [])?;
    let ops: Vec<(String, String, String)> = conn
        .prepare("SELECT op, target, payload FROM ops_log WHERE op IN (?1, ?2, ?3) ORDER BY hlc, id")?
        .query_map(params![OP_CONFIRM, OP_REJECT, OP_EDIT], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut states: BTreeMap<String, State> = BTreeMap::new();
    let mut ignored = 0;
    for (op, target, payload) in &ops {
        let Ok(Value::Object(p)) = serde_json::from_str::<Value>(payload) else {
            ignored += 1;
            continue;
        };
        let ann = p.get("annotation_id").and_then(Value::as_str);
        if let Some(a) = ann {
            let valid: Option<bool> = conn
                .query_row("SELECT valid FROM item_annotations WHERE id = ?1 AND item_id = ?2", params![a, target], |r| r.get(0))
                .optional()?;
            if valid != Some(true) {
                ignored += 1;
                continue;
            }
        }
        let st = states.entry(target.clone()).or_default();
        match (op.as_str(), ann) {
            (OP_CONFIRM, Some(a)) => {
                conn.execute("UPDATE item_annotations SET status = 'confirmed' WHERE id = ?1", [a])?;
                st.confirmed = Some(a.to_string());
            }
            (OP_REJECT, Some(a)) => {
                conn.execute("UPDATE item_annotations SET status = 'rejected' WHERE id = ?1", [a])?;
                if st.confirmed.as_deref() == Some(a) {
                    st.confirmed = None;
                }
            }
            (OP_CONFIRM, None) => st.item = Some("confirmed"),
            (OP_REJECT, None) => st.item = Some("rejected"),
            (OP_EDIT, _) => {
                if let Some(Value::Object(fields)) = p.get("fields") {
                    for (k, v) in fields {
                        st.edits.insert(k.clone(), v.clone());
                    }
                }
            }
            _ => ignored += 1,
        }
    }
    let annotated: Vec<String> = conn
        .prepare("SELECT DISTINCT item_id FROM item_annotations WHERE valid = 1")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in annotated {
        states.entry(id).or_default();
    }
    let mut written = 0;
    for (item_id, st) in &states {
        let Some(base) = load_base(conn, item_id)? else { continue };
        let anns = load_annotations(conn, item_id)?;
        write_effective(conn, item_id, &base, &anns, st)?;
        written += 1;
    }
    Ok(json!({ "ops": ops.len(), "ignored_ops": ignored, "items": written }))
}

fn load_base(conn: &Connection, id: &str) -> Result<Option<Base>> {
    Ok(conn
        .query_row(
            "SELECT type, subtype, category_id, title, coalesce(json_extract(data, '$.title_source'), '') = 'file_name',
                    text, event_start, event_end, tz_offset, local_date, lat, lon, place, confidence, method, status
             FROM items WHERE id = ?1",
            [id],
            |r| {
                Ok(Base {
                    item_type: r.get(0)?,
                    subtype: r.get(1)?,
                    category: r.get(2)?,
                    title: r.get(3)?,
                    title_weak: r.get(4)?,
                    text: r.get(5)?,
                    event_start: r.get(6)?,
                    event_end: r.get(7)?,
                    tz_offset: r.get(8)?,
                    local_date: r.get(9)?,
                    lat: r.get(10)?,
                    lon: r.get(11)?,
                    place: r.get(12)?,
                    confidence: r.get(13)?,
                    method: r.get(14)?,
                    status: r.get(15)?,
                })
            },
        )
        .optional()?)
}

fn load_annotations(conn: &Connection, item_id: &str) -> Result<Vec<Ann>> {
    Ok(conn
        .prepare(
            "SELECT id, type, subtype, title, text, event_start, tz_offset, local_date, place, confidence, status
             FROM item_annotations WHERE item_id = ?1 AND valid = 1 ORDER BY created_at, id",
        )?
        .query_map([item_id], |r| {
            Ok(Ann {
                id: r.get(0)?,
                item_type: r.get(1)?,
                subtype: r.get(2)?,
                title: r.get(3)?,
                text: r.get(4)?,
                event_start: r.get(5)?,
                tz_offset: r.get(6)?,
                local_date: r.get(7)?,
                place: r.get(8)?,
                confidence: r.get::<_, Option<f64>>(9)?.unwrap_or(0.0),
                status: r.get(10)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

fn category_for(item_type: &str) -> Option<&'static str> {
    match item_type {
        "document" => Some("documents"),
        "photo" | "screenshot" => Some("media"),
        _ => None,
    }
}

/// Field precedence: a user edit, then the rule value, then the confirmed
/// annotation. A rule title taken from the file name and the rule type
/// `file` (nothing known) count as missing.
fn write_effective(conn: &Connection, item_id: &str, b: &Base, anns: &[Ann], st: &State) -> Result<()> {
    let ai = st.confirmed.as_deref().and_then(|id| anns.iter().find(|a| a.id == id && a.status == "confirmed"));
    let edits = &st.edits;
    let mut used_ai = false;
    let edited_str = |k: &str| edits.get(k).map(|v| v.as_str().map(str::to_string));
    let mut pick = |key: &str, rule: Option<&String>, rule_weak: bool, model: Option<&String>| -> Option<String> {
        if let Some(v) = edited_str(key) {
            return v;
        }
        if let (Some(r), false) = (rule, rule_weak) {
            return Some(r.clone());
        }
        if let Some(m) = model {
            used_ai = true;
            return Some(m.clone());
        }
        rule.cloned()
    };
    let title = pick("title", b.title.as_ref(), b.title_weak, ai.and_then(|a| a.title.as_ref()));
    let item_type = pick("type", Some(&b.item_type), b.item_type == "file", ai.and_then(|a| a.item_type.as_ref()))
        .unwrap_or_else(|| b.item_type.clone());
    let subtype = pick("subtype", b.subtype.as_ref(), false, ai.and_then(|a| a.subtype.as_ref()));
    let text = pick("text", b.text.as_ref(), false, ai.and_then(|a| a.text.as_ref()));
    let place = pick("place", b.place.as_ref(), false, ai.and_then(|a| a.place.as_ref()));
    let category = if item_type == b.item_type { b.category.clone() } else { category_for(&item_type).map(str::to_string) };

    // The event time moves as a group: start, offset and local date.
    let (event_start, tz_offset, local_date) = if edits.contains_key("local_date") || edits.contains_key("event_start") {
        (
            edits.get("event_start").and_then(Value::as_str).map(str::to_string),
            edits.get("tz_offset").and_then(Value::as_i64),
            edits.get("local_date").and_then(Value::as_str).map(str::to_string),
        )
    } else if b.event_start.is_some() || b.local_date.is_some() {
        (b.event_start.clone(), b.tz_offset, b.local_date.clone())
    } else if let Some(a) = ai.filter(|a| a.event_start.is_some() || a.local_date.is_some()) {
        used_ai = true;
        (a.event_start.clone(), a.tz_offset, a.local_date.clone())
    } else {
        (None, None, None)
    };
    let (lat, lon) = if edits.contains_key("lat") {
        (edits.get("lat").and_then(Value::as_f64), edits.get("lon").and_then(Value::as_f64))
    } else {
        (b.lat, b.lon)
    };

    let edited = !edits.is_empty();
    let pending = anns.iter().filter(|a| a.status == "needs_review").count();
    let status = if st.item == Some("rejected") {
        "rejected".to_string()
    } else if pending > 0 {
        "needs_review".to_string()
    } else if edited || ai.is_some() || st.item == Some("confirmed") {
        "confirmed".to_string()
    } else {
        b.status.clone()
    };
    let (method, confidence) = if edited {
        ("user".to_string(), 1.0)
    } else if used_ai {
        ("ai".to_string(), ai.map_or(0.0, |a| a.confidence))
    } else {
        (b.method.clone(), b.confidence)
    };
    conn.execute(
        "INSERT INTO item_effective
           (item_id, type, subtype, category_id, title, text, event_start, event_end, tz_offset, local_date,
            lat, lon, place, confidence, method, status, annotation_id, pending, edited)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        params![
            item_id,
            item_type,
            subtype,
            category,
            title,
            text,
            event_start,
            b.event_end,
            tz_offset,
            local_date,
            lat,
            lon,
            place,
            confidence,
            method,
            status,
            ai.map(|a| a.id.clone()),
            pending as i64,
            edited,
        ],
    )?;
    Ok(())
}

/// The `files.index_items` stage: search rows for reviewed items show their
/// effective values, and rejected items leave the index.
pub fn index(conn: &Connection, _ctx: &GlobalCtx<'_>) -> Result<Value> {
    conn.execute_batch(
        "DELETE FROM search_fts WHERE kind = 'item' AND ref_id IN (SELECT item_id FROM item_effective);
         INSERT INTO search_fts (kind, ref_id, local_date, title, body)
           SELECT 'item', item_id, local_date, coalesce(title, type),
                  type || ' ' || coalesce(subtype, '') || ' ' || coalesce(place, '') || ' ' || coalesce(text, '')
           FROM item_effective WHERE status != 'rejected';",
    )?;
    let n: i64 = conn.query_row("SELECT count(*) FROM item_effective WHERE status != 'rejected'", [], |r| r.get(0))?;
    Ok(json!({ "items": n }))
}

const ITEM_COLUMNS: &str = "id, type, subtype, category, title, text, event_start, event_end, tz_offset, local_date,
    received_at, source_updated_at, lat, lon, place, data, confidence, method, status, raw_id, source, raw_kind,
    file_name, mime, annotation_id, pending_annotations, edited";

fn row_json(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let mut m = Map::new();
    for (i, name) in r.as_ref().column_names().iter().enumerate() {
        let v = match r.get_ref(i)? {
            rusqlite::types::ValueRef::Null => Value::Null,
            rusqlite::types::ValueRef::Integer(n) => json!(n),
            rusqlite::types::ValueRef::Real(f) => json!(f),
            rusqlite::types::ValueRef::Text(t) => {
                let s = String::from_utf8_lossy(t).into_owned();
                if *name == "data" || *name == "payload" {
                    serde_json::from_str(&s).unwrap_or(Value::String(s))
                } else {
                    Value::String(s)
                }
            }
            rusqlite::types::ValueRef::Blob(_) => Value::Null,
        };
        m.insert((*name).to_string(), v);
    }
    Ok(Value::Object(m))
}

/// An item with its rule values, annotations and review history.
pub fn show(conn: &Connection, id: &str) -> Result<Value> {
    let (item_id, _) = resolve(conn, id)?;
    let item = conn.query_row(&format!("SELECT {ITEM_COLUMNS} FROM v_items_all WHERE id = ?1"), [&item_id], row_json)?;
    let rule = conn.query_row(
        "SELECT type, subtype, category_id AS category, title, text, event_start, tz_offset, local_date, lat, lon, place,
                confidence, method, status
         FROM items WHERE id = ?1",
        [&item_id],
        row_json,
    )?;
    let annotations = conn
        .prepare("SELECT * FROM v_item_annotations WHERE item_id = ?1 ORDER BY created_at, id")?
        .query_map([&item_id], row_json)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let decisions = conn
        .prepare("SELECT id, hlc, actor, op, payload FROM ops_log WHERE target = ?1 ORDER BY hlc, id")?
        .query_map([&item_id], row_json)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({ "item": item, "rule": rule, "annotations": annotations, "decisions": decisions }))
}

/// Filters for [`list`].
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    /// `auto`, `needs_review`, `confirmed` or `rejected`. Rejected items are
    /// listed only when asked for.
    pub status: Option<String>,
    pub item_type: Option<String>,
    pub limit: Option<u32>,
}

pub fn list(conn: &Connection, f: &ListFilter) -> Result<Value> {
    if let Some(s) = &f.status {
        if !["auto", "needs_review", "confirmed", "rejected"].contains(&s.as_str()) {
            return Err(invalid("status must be auto, needs_review, confirmed or rejected"));
        }
    }
    let rows = conn
        .prepare(&format!(
            "SELECT {ITEM_COLUMNS} FROM v_items_all
             WHERE ((?1 IS NULL AND status != 'rejected') OR status = ?1)
               AND (?2 IS NULL OR type = ?2)
             ORDER BY coalesce(event_start, local_date, received_at) DESC, id
             LIMIT ?3"
        ))?
        .query_map(params![f.status, f.item_type, f.limit.unwrap_or(50).clamp(1, 10_000)], row_json)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let rows: Vec<Value> = rows
        .into_iter()
        .map(|mut r| {
            // Lists stay short: full text is in `items show`.
            if let Some(t) = r["text"].as_str() {
                r["text"] = json!(truncate_chars(t.to_string(), 200).0);
            }
            r
        })
        .collect();
    Ok(json!({ "count": rows.len(), "items": rows }))
}

/// What awaits review: items rules could not read and items with pending
/// annotations, oldest first, with the pending annotations' values.
pub fn queue(conn: &Connection, limit: Option<u32>) -> Result<Value> {
    let mut items = conn
        .prepare(&format!(
            "SELECT {ITEM_COLUMNS} FROM v_items WHERE status = 'needs_review' ORDER BY received_at, id LIMIT ?1"
        ))?
        .query_map([limit.unwrap_or(50).clamp(1, 10_000)], row_json)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for item in &mut items {
        let id = item["id"].as_str().unwrap_or_default().to_string();
        let anns = conn
            .prepare("SELECT * FROM v_item_annotations WHERE item_id = ?1 AND status = 'needs_review' ORDER BY created_at, id")?
            .query_map([&id], row_json)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        item["reason"] = item["data"]["reason"].clone();
        if let Some(t) = item["text"].as_str() {
            item["text"] = json!(truncate_chars(t.to_string(), 200).0);
        }
        item["annotations"] = json!(anns);
    }
    Ok(json!({ "count": items.len(), "items": items }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_are_validated_and_normalized() {
        let e = Edit { title: Some("  New\ttitle ".into()), event_time: Some("2026-10-01T09:00:00+08:00".into()), ..Default::default() };
        let f = e.fields().unwrap();
        assert_eq!(f["title"], "New title");
        assert_eq!(f["event_start"], "2026-10-01T01:00:00Z");
        assert_eq!(f["local_date"], "2026-10-01");
        for bad in [
            Edit::default(),
            Edit { item_type: Some("invoice".into()), ..Default::default() },
            Edit { subtype: Some("Receipt!".into()), ..Default::default() },
            Edit { lat: Some(10.0), ..Default::default() },
            Edit { clear: vec!["data".into()], ..Default::default() },
            Edit { event_time: Some("soon".into()), ..Default::default() },
        ] {
            assert!(bad.fields().is_err(), "{bad:?}");
        }
        assert!(crate::enrich::TYPES.iter().all(|t| EDIT_TYPES.contains(t)));
    }
}
