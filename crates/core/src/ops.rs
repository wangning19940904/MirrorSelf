//! User ops (architecture §6, §10): review decisions recorded in the
//! append-only op log and replayed over the derived index.
//!
//! Each op is also stored as an immutable, encrypted vault object, so the
//! index can be deleted without losing a decision: `process --rebuild`
//! re-registers ops from the vault and replays them, in hybrid-logical-clock
//! order, after the processors have re-derived everything. Later ops win
//! (last writer wins per target).

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::labs;
use crate::time::{now_utc_precise, today_local};
use crate::vault::{RawMeta, Vault};

pub const SOURCE: &str = "mirrorself";
pub const KIND: &str = "user_op";

pub const CONFIRM: &str = "review.confirm";
pub const EDIT: &str = "review.edit";
pub const REJECT: &str = "review.reject";

/// Lab review ops in `ops_log`. Other ops share the table (uploaded files'
/// `item.*` decisions, with their own clock format) and are left alone.
const LAB_OPS: &str = "op IN ('review.confirm', 'review.edit', 'review.reject')";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Op {
    /// `<unix ms, 13 digits>-<counter, 4 digits>-<node id>`: sorts in causal order.
    pub hlc: String,
    pub actor: String,
    pub op: String,
    /// The observation the op applies to.
    pub target: String,
    pub payload: Value,
}

/// The fields `review.edit` may change. Absent fields keep their value.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Edit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loinc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_low: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_high: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,
    /// `YYYY-MM-DD`, for reports without a readable date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
}

impl Edit {
    pub fn is_empty(&self) -> bool {
        *self == Edit::default()
    }

    pub fn fields(&self) -> Vec<&'static str> {
        let mut f = Vec::new();
        for (name, set) in [
            ("value", self.value.is_some()),
            ("value_text", self.value_text.is_some()),
            ("unit", self.unit.is_some()),
            ("loinc", self.loinc.is_some()),
            ("ref_low", self.ref_low.is_some()),
            ("ref_high", self.ref_high.is_some()),
            ("flag", self.flag.is_some()),
            ("date", self.date.is_some()),
        ] {
            if set {
                f.push(name);
            }
        }
        f
    }
}

fn next_hlc(conn: &Connection, node: &str) -> Result<String> {
    let last: Option<String> = conn.query_row(&format!("SELECT max(hlc) FROM ops_log WHERE {LAB_OPS}"), [], |r| r.get(0))?;
    let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let (ms, counter) = match last.as_deref().and_then(parse_hlc) {
        Some((last_ms, last_counter)) if last_ms >= now => (last_ms, last_counter + 1),
        _ => (now, 0),
    };
    Ok(format!("{ms:013}-{counter:04}-{node}"))
}

fn parse_hlc(hlc: &str) -> Option<(u64, u32)> {
    let mut parts = hlc.splitn(3, '-');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// Records an op in the vault and the op log, then applies it.
pub fn append(conn: &Connection, vault: &Vault, node: &str, actor: &str, op: &str, target: &str, payload: Value) -> Result<(String, Op)> {
    let op = Op {
        hlc: next_hlc(conn, node)?,
        actor: actor.to_string(),
        op: op.to_string(),
        target: target.to_string(),
        payload,
    };
    if locate(conn, &op)?.is_none() {
        bail!("no lab result '{}' to apply {} to", op.target, op.op);
    }
    let bytes = serde_json::to_vec(&op)?;
    let meta = RawMeta {
        source: SOURCE.into(),
        kind: KIND.into(),
        mime: "application/json".into(),
        name: None,
        origin_ref: Some(format!("{} {}", op.op, op.target)),
        received_at: now_utc_precise(),
        bytes: bytes.len() as u64,
        lang: None,
    };
    let put = vault.put(&meta, &bytes)?;
    let tx = conn.unchecked_transaction()?;
    insert(&tx, &put.id, &op)?;
    if !apply(&tx, &put.id, &op)? {
        bail!("no lab result '{}' to apply {} to", op.target, op.op);
    }
    tx.commit()?;
    Ok((put.id, op))
}

/// Adds an op read from the vault to the op log (idempotent).
pub fn insert(conn: &Connection, id: &str, op: &Op) -> Result<bool> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO ops_log (id, hlc, actor, op, target, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, op.hlc, op.actor, op.op, op.target, op.payload.to_string()],
    )?;
    Ok(n > 0)
}

/// Finds the op's observation: by id, or (if a newer extractor renumbered
/// the rows) by the document, page and printed name the op remembers.
fn locate(conn: &Connection, op: &Op) -> Result<Option<String>> {
    let found: Option<String> = conn
        .query_row("SELECT id FROM observations WHERE id = ?1", [&op.target], |r| r.get(0))
        .optional()?;
    if found.is_some() {
        return Ok(found);
    }
    let m = &op.payload["match"];
    let (Some(raw_id), Some(page), Some(label)) = (m["raw_id"].as_str(), m["page"].as_i64(), m["label"].as_str()) else {
        return Ok(None);
    };
    let ids: Vec<String> = conn
        .prepare("SELECT id FROM observations WHERE raw_id = ?1 AND page = ?2 AND label = ?3")?
        .query_map(params![raw_id, page, label], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok((ids.len() == 1).then(|| ids[0].clone()))
}

/// Applies one op to the index. False when its target no longer exists.
pub fn apply(conn: &Connection, op_id: &str, op: &Op) -> Result<bool> {
    let Some(id) = locate(conn, op)? else { return Ok(false) };
    let decision = |d: &str, fields: Value| {
        conn.execute(
            "UPDATE observations SET review = json_set(coalesce(review, '{}'),
               '$.decision', ?2, '$.op', ?3, '$.hlc', ?4, '$.actor', ?5, '$.fields', json(?6))
             WHERE id = ?1",
            params![id, d, op_id, op.hlc, op.actor, fields.to_string()],
        )
    };
    match op.op.as_str() {
        CONFIRM => {
            conn.execute("UPDATE observations SET status = 'confirmed' WHERE id = ?1", [&id])?;
            decision("confirmed", json!([]))?;
        }
        REJECT => {
            conn.execute("UPDATE observations SET status = 'rejected' WHERE id = ?1", [&id])?;
            decision("rejected", json!([]))?;
        }
        EDIT => {
            let edit: Edit = serde_json::from_value(op.payload["edit"].clone()).unwrap_or_default();
            apply_edit(conn, &id, &edit)?;
            decision("edited", json!(edit.fields()))?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn apply_edit(conn: &Connection, id: &str, e: &Edit) -> Result<()> {
    type Row = (String, String, Option<f64>, Option<String>, Option<String>, Option<f64>, Option<f64>, Option<String>, String);
    let row: Row = conn.query_row(
        "SELECT code_system, code, value_num, value_text, unit, ref_low, ref_high, flag, local_date
         FROM observations WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)),
    )?;
    let (mut system, mut code, mut value, mut text, mut unit, mut low, mut high, mut flag, mut date) = row;
    if let Some(l) = &e.loinc {
        let a = labs::by_loinc(l);
        system = "loinc".into();
        code = a.map_or_else(|| l.clone(), |a| a.loinc.to_string());
    }
    if e.value.is_some() {
        value = e.value;
        text = None;
    }
    if let Some(t) = &e.value_text {
        text = (!t.is_empty()).then(|| t.clone());
        if e.value.is_none() {
            value = None;
        }
    }
    if let Some(u) = &e.unit {
        unit = (!u.is_empty()).then(|| labs::normalize_unit(u).map_or_else(|| u.clone(), str::to_string));
    }
    low = e.ref_low.or(low);
    high = e.ref_high.or(high);
    if let Some(f) = &e.flag {
        flag = (!f.is_empty()).then(|| f.clone());
    }
    if let Some(d) = &e.date {
        date = d.clone();
    }
    let analyte = (system == "loinc").then(|| labs::by_loinc(&code)).flatten();
    let factor = analyte.zip(unit.as_deref()).and_then(|(a, u)| a.factor(u));
    let canonical = value.zip(factor).map(|(v, f)| v * f);
    conn.execute(
        "UPDATE observations SET code_system = ?2, code = ?3, value_num = ?4, value_text = ?5, unit = ?6,
           ref_low = ?7, ref_high = ?8, flag = ?9, local_date = ?10, start_ts = ?10 || 'T00:00:00Z',
           end_ts = ?10 || 'T00:00:00Z', value_canonical = ?11, unit_canonical = ?12, status = 'confirmed'
         WHERE id = ?1",
        params![
            id,
            system,
            code,
            value,
            text,
            unit,
            low,
            high,
            flag,
            date,
            canonical,
            factor.and(analyte.map(|a| a.unit))
        ],
    )?;
    Ok(())
}

/// Re-applies every op in clock order: the `ops.replay` stage.
pub fn replay(conn: &Connection) -> Result<Value> {
    let ops: Vec<(String, Op)> = conn
        .prepare(&format!("SELECT id, hlc, actor, op, target, payload FROM ops_log WHERE {LAB_OPS} ORDER BY hlc, id"))?
        .query_map([], |r| {
            let payload: String = r.get(5)?;
            Ok((
                r.get(0)?,
                Op {
                    hlc: r.get(1)?,
                    actor: r.get(2)?,
                    op: r.get(3)?,
                    target: r.get(4)?,
                    payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut applied = 0u64;
    let mut orphaned = 0u64;
    for (id, op) in &ops {
        if apply(conn, id, op)? {
            applied += 1;
        } else {
            orphaned += 1;
        }
    }
    Ok(json!({ "ops": ops.len(), "applied": applied, "orphaned": orphaned }))
}

/// Validates an edit before it becomes an op.
pub fn check_edit(e: &Edit) -> Result<(), String> {
    if e.is_empty() {
        return Err("nothing to change: pass a value, unit, LOINC code, range, flag or date".into());
    }
    if let Some(l) = &e.loinc {
        if !labs::loinc_is_valid(l) {
            return Err(format!("'{l}' is not a valid LOINC code (expected NNNNN-N with a correct check digit)"));
        }
    }
    if let Some(d) = &e.date {
        let date = chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").map_err(|_| format!("date '{d}' must be YYYY-MM-DD"))?;
        if date > today_local() + chrono::Duration::days(1) {
            return Err(format!("date '{d}' is in the future"));
        }
    }
    if let (Some(l), Some(h)) = (e.ref_low, e.ref_high) {
        if l > h {
            return Err("ref_low is above ref_high".into());
        }
    }
    Ok(())
}
