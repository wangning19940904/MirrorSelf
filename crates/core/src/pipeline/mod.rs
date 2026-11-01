//! Processing pipeline (architecture §5): raw vault → parse → normalize →
//! derive → link → index.
//!
//! Per-object parsers run once per raw object and processor version.
//! Whole-index stages (dedupe, daily metrics, sleep sessions, links, search)
//! run after any parser output changes. Every run is recorded in
//! `processor_runs`, so each output can be traced to the code that made it.

mod apple_health;
mod dedupe;
mod derive;
mod link;
mod sleep;

use std::collections::HashSet;
use std::time::Instant;

use anyhow::Result;
use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::Value;

use crate::config::PipelineConfig;
use crate::ids::stable_id;
use crate::importers;
use crate::time::now_utc_string;
use crate::vault::{RawMeta, Vault};

#[derive(Debug, Clone, Copy)]
pub struct Processor {
    pub name: &'static str,
    pub version: &'static str,
}

pub const APPLE_HEALTH_PARSE: Processor = Processor {
    name: "apple_health.parse",
    version: "1.0.0",
};

pub const GLOBAL_STAGES: &[Processor] = &[
    Processor { name: "normalize.dedupe", version: "1.0.0" },
    Processor { name: "derive.daily_metrics", version: "1.0.0" },
    Processor { name: "derive.sleep_sessions", version: "1.0.0" },
    Processor { name: "link.rules", version: "1.0.0" },
    Processor { name: "index.search", version: "1.0.0" },
];

fn parser_for(kind: &str) -> Option<Processor> {
    match kind {
        importers::apple_health::KIND => Some(APPLE_HEALTH_PARSE),
        _ => None,
    }
}

pub fn run_id(p: &Processor, raw_id: Option<&str>) -> String {
    stable_id("run", &[p.name, p.version, raw_id.unwrap_or("*")])
}

#[derive(Debug, Default, Serialize)]
pub struct ProcessReport {
    pub rebuilt: bool,
    /// Raw objects found in the vault but missing from the index.
    pub registered_from_vault: usize,
    pub runs: Vec<RunReport>,
    pub elapsed_ms: u128,
}

impl ProcessReport {
    pub fn errors(&self) -> impl Iterator<Item = &RunReport> {
        self.runs.iter().filter(|r| r.status != "ok")
    }
}

#[derive(Debug, Serialize)]
pub struct RunReport {
    pub processor: String,
    pub version: String,
    pub raw_id: Option<String>,
    pub status: String,
    pub outputs: Value,
    pub error: Option<String>,
    pub elapsed_ms: u128,
}

pub fn process(
    conn: &Connection,
    vault: &Vault,
    cfg: &PipelineConfig,
    rebuild: bool,
) -> Result<ProcessReport> {
    let started = Instant::now();
    let mut report = ProcessReport {
        rebuilt: rebuild,
        ..Default::default()
    };
    if rebuild {
        clear_derived(conn)?;
    }
    report.registered_from_vault = register_vault_objects(conn, vault)?;

    let pending = pending_raw(conn)?;
    for raw in &pending {
        report.runs.push(run_parser(conn, vault, cfg, raw)?);
    }
    if rebuild || !pending.is_empty() || global_stale(conn)? {
        report.runs.extend(run_global(conn, cfg)?);
    }
    report.elapsed_ms = started.elapsed().as_millis();
    Ok(report)
}

pub(crate) fn insert_raw(conn: &Connection, vault: &Vault, id: &str, meta: &RawMeta) -> Result<bool> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO raw_objects
           (id, source, kind, mime, name, received_at, origin_ref, storage_uri, key_id, bytes, meta)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            id,
            meta.source,
            meta.kind,
            meta.mime,
            meta.name,
            meta.received_at,
            meta.origin_ref,
            vault.storage_uri(),
            vault.key_id(),
            meta.bytes as i64,
            serde_json::to_string(meta)?
        ],
    )?;
    Ok(n > 0)
}

/// Re-registers vault objects the index doesn't know, e.g. after `index.db`
/// was deleted or the vault was synced from another host.
fn register_vault_objects(conn: &Connection, vault: &Vault) -> Result<usize> {
    let known: HashSet<String> = conn
        .prepare("SELECT id FROM raw_objects")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut added = 0;
    for id in vault.list_ids()? {
        if known.contains(&id) {
            continue;
        }
        let (meta, _) = vault.get(&id)?;
        if insert_raw(conn, vault, &id, &meta)? {
            added += 1;
        }
    }
    Ok(added)
}

/// Removes everything derived from the vault. Keeps raw_objects, ops_log,
/// grants, audit_log and user-made links.
fn clear_derived(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "BEGIN;
         DELETE FROM observations;
         DELETE FROM events;
         DELETE FROM documents;
         DELETE FROM links WHERE method != 'user';
         DELETE FROM entities;
         DELETE FROM episodes;
         DELETE FROM daily_metrics;
         DELETE FROM search_fts;
         DELETE FROM processor_runs;
         COMMIT;",
    )?;
    Ok(())
}

struct PendingRaw {
    id: String,
    kind: String,
}

fn pending_raw(conn: &Connection) -> Result<Vec<PendingRaw>> {
    let kinds: Vec<String> = conn
        .prepare("SELECT DISTINCT kind FROM raw_objects ORDER BY kind")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for kind in kinds {
        let Some(p) = parser_for(&kind) else { continue };
        // A new parser version reprocesses every object of its kind from
        // scratch, in import order, so ownership of shared records stays stable.
        let stale: i64 = conn.query_row(
            "SELECT count(*) FROM processor_runs WHERE processor = ?1 AND version != ?2",
            params![p.name, p.version],
            |r| r.get(0),
        )?;
        if stale > 0 {
            clear_parser_outputs(conn, &p)?;
        }
        let mut stmt = conn.prepare(
            "SELECT r.id FROM raw_objects r
             WHERE r.kind = ?1 AND NOT EXISTS (
               SELECT 1 FROM processor_runs pr
               WHERE pr.raw_id = r.id AND pr.processor = ?2 AND pr.version = ?3 AND pr.status = 'ok')
             ORDER BY r.received_at, r.id",
        )?;
        let ids = stmt
            .query_map(params![kind, p.name, p.version], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        out.extend(ids.into_iter().map(|id| PendingRaw { id, kind: kind.clone() }));
    }
    Ok(out)
}

fn clear_parser_outputs(conn: &Connection, p: &Processor) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM observations WHERE run_id IN (SELECT id FROM processor_runs WHERE processor = ?1)",
        [p.name],
    )?;
    tx.execute(
        "DELETE FROM events WHERE derived = 0 AND run_id IN (SELECT id FROM processor_runs WHERE processor = ?1)",
        [p.name],
    )?;
    tx.execute("DELETE FROM processor_runs WHERE processor = ?1", [p.name])?;
    tx.commit()?;
    Ok(())
}

fn global_stale(conn: &Connection) -> Result<bool> {
    for p in GLOBAL_STAGES {
        let ok: i64 = conn.query_row(
            "SELECT count(*) FROM processor_runs WHERE id = ?1 AND status = 'ok'",
            [run_id(p, None)],
            |r| r.get(0),
        )?;
        if ok == 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

#[allow(clippy::too_many_arguments)]
fn record_run(
    conn: &Connection,
    id: &str,
    p: &Processor,
    raw_id: Option<&str>,
    status: &str,
    started_at: &str,
    outputs: Option<&Value>,
    error: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO processor_runs
           (id, processor, version, raw_id, status, started_at, finished_at, outputs, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id,
            p.name,
            p.version,
            raw_id,
            status,
            started_at,
            now_utc_string(),
            outputs.map(Value::to_string),
            error
        ],
    )?;
    Ok(())
}

fn run_parser(conn: &Connection, vault: &Vault, cfg: &PipelineConfig, raw: &PendingRaw) -> Result<RunReport> {
    let p = parser_for(&raw.kind).expect("pending_raw only lists kinds with a parser");
    let run = run_id(&p, Some(&raw.id));
    let started_at = now_utc_string();
    let timer = Instant::now();

    let result = (|| -> Result<Value> {
        let (_, content) = vault.get(&raw.id)?;
        let tx = conn.unchecked_transaction()?;
        let outputs = apple_health::load(&tx, cfg, &raw.id, &run, &content)?;
        record_run(&tx, &run, &p, Some(&raw.id), "ok", &started_at, Some(&outputs), None)?;
        tx.commit()?;
        Ok(outputs)
    })();

    let (status, outputs, error) = match result {
        Ok(outputs) => ("ok", outputs, None),
        Err(err) => {
            let msg = format!("{err:#}");
            record_run(conn, &run, &p, Some(&raw.id), "error", &started_at, None, Some(&msg))?;
            ("error", Value::Null, Some(msg))
        }
    };
    Ok(RunReport {
        processor: p.name.into(),
        version: p.version.into(),
        raw_id: Some(raw.id.clone()),
        status: status.into(),
        outputs,
        error,
        elapsed_ms: timer.elapsed().as_millis(),
    })
}

fn run_global(conn: &Connection, cfg: &PipelineConfig) -> Result<Vec<RunReport>> {
    let mut reports = Vec::new();
    for p in GLOBAL_STAGES {
        let run = run_id(p, None);
        let started_at = now_utc_string();
        let timer = Instant::now();
        let tx = conn.unchecked_transaction()?;
        let outputs = match p.name {
            "normalize.dedupe" => dedupe::run(&tx)?,
            "derive.daily_metrics" => derive::run(&tx, cfg)?,
            "derive.sleep_sessions" => sleep::run(&tx, &run)?,
            "link.rules" => link::run(&tx)?,
            "index.search" => link::index_search(&tx)?,
            other => unreachable!("no stage named {other}"),
        };
        record_run(&tx, &run, p, None, "ok", &started_at, Some(&outputs), None)?;
        tx.commit()?;
        reports.push(RunReport {
            processor: p.name.into(),
            version: p.version.into(),
            raw_id: None,
            status: "ok".into(),
            outputs,
            error: None,
            elapsed_ms: timer.elapsed().as_millis(),
        });
    }
    Ok(reports)
}
