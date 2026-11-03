//! Processing pipeline (architecture §5): raw vault → parse → normalize →
//! derive → link → index.
//!
//! Per-object processors run once per raw object and processor version, as
//! a chain per kind (a checkup report is OCR'd, then extracted). Whole-index
//! stages (dedupe, op replay, daily metrics, sleep sessions, links, search)
//! run after any per-object output changes. Every run is recorded in
//! `processor_runs`, so each output can be traced to the code that made it.

mod apple_health;
mod dedupe;
mod derive;
mod link;
pub mod report;
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
use crate::models::ModelProvider;
use crate::ops;
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

pub const REPORT_OCR: Processor = Processor {
    name: "report.ocr",
    version: "1.0.0",
};

pub const REPORT_EXTRACT: Processor = Processor {
    name: "report.extract",
    version: "1.0.0",
};

pub const GLOBAL_STAGES: &[Processor] = &[
    Processor { name: "normalize.dedupe", version: "1.0.0" },
    Processor { name: "ops.replay", version: "1.0.0" },
    Processor { name: "derive.daily_metrics", version: "1.0.0" },
    Processor { name: "derive.sleep_sessions", version: "1.0.0" },
    // 1.1.0: providers from checkup reports.
    Processor { name: "link.rules", version: "1.1.0" },
    // 1.1.0: documents and lab analytes.
    Processor { name: "index.search", version: "1.1.0" },
];

/// The per-object processors for a kind, in order.
fn chain_for(kind: &str) -> &'static [Processor] {
    match kind {
        importers::apple_health::KIND => &[APPLE_HEALTH_PARSE],
        importers::report::KIND => &[REPORT_OCR, REPORT_EXTRACT],
        _ => &[],
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
    models: &dyn ModelProvider,
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
        for p in &chain_for(&raw.kind)[raw.from..] {
            let run = run_stage(conn, vault, cfg, models, raw, p)?;
            let ok = run.status == "ok";
            report.runs.push(run);
            if !ok {
                break;
            }
        }
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
/// was deleted or the vault was synced from another host. User ops go back
/// into the op log; everything else is a raw object.
fn register_vault_objects(conn: &Connection, vault: &Vault) -> Result<usize> {
    let known: HashSet<String> = conn
        .prepare("SELECT id FROM raw_objects UNION ALL SELECT id FROM ops_log")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut added = 0;
    for id in vault.list_ids()? {
        if known.contains(&id) {
            continue;
        }
        let (meta, content) = vault.get(&id)?;
        if meta.kind == ops::KIND {
            let op: ops::Op = serde_json::from_slice(&content)?;
            ops::insert(conn, &id, &op)?;
        } else if insert_raw(conn, vault, &id, &meta)? {
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
         DELETE FROM document_pages;
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
    /// The first processor of the kind's chain that has to run.
    from: usize,
}

fn pending_raw(conn: &Connection) -> Result<Vec<PendingRaw>> {
    let kinds: Vec<String> = conn
        .prepare("SELECT DISTINCT kind FROM raw_objects ORDER BY kind")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for kind in kinds {
        let chain = chain_for(&kind);
        for p in chain {
            // A new processor version reprocesses every object of its kind
            // from scratch, in import order, so ownership of shared records
            // stays stable.
            let stale: i64 = conn.query_row(
                "SELECT count(*) FROM processor_runs WHERE processor = ?1 AND version != ?2",
                params![p.name, p.version],
                |r| r.get(0),
            )?;
            if stale > 0 {
                clear_parser_outputs(conn, p)?;
            }
        }
        let ids: Vec<String> = conn
            .prepare("SELECT id FROM raw_objects WHERE kind = ?1 ORDER BY received_at, id")?
            .query_map([&kind], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut done = conn.prepare_cached(
            "SELECT count(*) FROM processor_runs
             WHERE raw_id = ?1 AND processor = ?2 AND version = ?3 AND status = 'ok'",
        )?;
        for id in ids {
            let mut from = None;
            for (i, p) in chain.iter().enumerate() {
                let ok: i64 = done.query_row(params![id, p.name, p.version], |r| r.get(0))?;
                if ok == 0 {
                    from = Some(i);
                    break;
                }
            }
            if let Some(from) = from {
                out.push(PendingRaw { id, kind: kind.clone(), from });
            }
        }
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
    tx.execute(
        "DELETE FROM document_pages WHERE run_id IN (SELECT id FROM processor_runs WHERE processor = ?1)",
        [p.name],
    )?;
    tx.execute(
        "DELETE FROM documents WHERE run_id IN (SELECT id FROM processor_runs WHERE processor = ?1)",
        [p.name],
    )?;
    tx.execute("DELETE FROM processor_runs WHERE processor = ?1", [p.name])?;
    tx.commit()?;
    Ok(())
}

/// Removes what an earlier run of `run` (same processor, version and raw
/// object) wrote, so re-running after an upstream change starts clean.
fn clear_run_outputs(conn: &Connection, run: &str) -> Result<()> {
    conn.execute("DELETE FROM observations WHERE run_id = ?1", [run])?;
    conn.execute("DELETE FROM events WHERE derived = 0 AND run_id = ?1", [run])?;
    conn.execute("DELETE FROM document_pages WHERE run_id = ?1", [run])?;
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

fn run_stage(
    conn: &Connection,
    vault: &Vault,
    cfg: &PipelineConfig,
    models: &dyn ModelProvider,
    raw: &PendingRaw,
    p: &Processor,
) -> Result<RunReport> {
    let run = run_id(p, Some(&raw.id));
    let started_at = now_utc_string();
    let timer = Instant::now();

    let result = (|| -> Result<Value> {
        let tx = conn.unchecked_transaction()?;
        clear_run_outputs(&tx, &run)?;
        let outputs = match p.name {
            "apple_health.parse" => {
                let (_, content) = vault.get(&raw.id)?;
                apple_health::load(&tx, cfg, &raw.id, &run, &content)?
            }
            "report.ocr" => {
                let (meta, content) = vault.get(&raw.id)?;
                report::ocr(&tx, models, &raw.id, &run, &meta, &content)?
            }
            "report.extract" => {
                let received_at: String =
                    tx.query_row("SELECT received_at FROM raw_objects WHERE id = ?1", [&raw.id], |r| r.get(0))?;
                report::extract(&tx, models, cfg, &raw.id, &run, &received_at)?
            }
            other => unreachable!("no processor named {other}"),
        };
        record_run(&tx, &run, p, Some(&raw.id), "ok", &started_at, Some(&outputs), None)?;
        tx.commit()?;
        Ok(outputs)
    })();

    let (status, outputs, error) = match result {
        Ok(outputs) => ("ok", outputs, None),
        Err(err) => {
            let msg = format!("{err:#}");
            record_run(conn, &run, p, Some(&raw.id), "error", &started_at, None, Some(&msg))?;
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
            "ops.replay" => ops::replay(&tx)?,
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
