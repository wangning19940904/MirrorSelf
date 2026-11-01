//! Access log: every read by an agent or API client (architecture §9),
//! including attempts that failed or were refused.

use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::Value;

use crate::crypto::sha256_hex;
use crate::error::{user_error, UserError};
use crate::time::now_utc_string;

pub struct Entry<'a> {
    pub actor: &'a str,
    pub client_id: &'a str,
    pub action: &'a str,
    pub scope: &'a str,
    pub request: &'a Value,
}

pub fn record(conn: &Connection, entry: &Entry<'_>, result: &Result<Value>) -> Result<()> {
    let digest = &sha256_hex(entry.request.to_string().as_bytes())[..16];
    let (outcome, rows) = match result {
        Ok(v) => ("ok", result_rows(v)),
        Err(e) if matches!(user_error(e), Some(UserError::Denied(_))) => ("denied", 0),
        Err(_) => ("error", 0),
    };
    conn.execute(
        "INSERT INTO audit_log (ts, actor, client_id, action, scope, query_digest, rows, outcome)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            now_utc_string(),
            entry.actor,
            entry.client_id,
            entry.action,
            entry.scope,
            digest,
            rows,
            outcome
        ],
    )?;
    Ok(())
}

/// Rough row count of a QueryService result, for the audit log.
pub fn result_rows(result: &Value) -> i64 {
    for key in ["items", "points", "results", "rows", "linked", "coverage", "entities"] {
        if let Some(a) = result.get(key).and_then(Value::as_array) {
            return a.len() as i64;
        }
    }
    1
}
