//! Checkup reports (architecture §5 stage 5b): OCR, then extraction into lab
//! observations that link to their page, plus a checkup on the timeline.
//!
//! - `report.ocr` stores each page's OCR output in `document_pages`, so a
//!   new extractor version re-reads it without running OCR again.
//! - `report.extract` fills the document's metadata and its lab rows; values
//!   that fail a check (`models::validate`) wait in the review queue as
//!   `pending`.
//! - `ops.replay` (before daily metrics) re-applies the user's review
//!   decisions; `report.providers` (after links) links checkups to the
//!   provider named on the report; `report.index` (after the search index)
//!   adds documents and lab analytes to search.
//!
//! Reports are imported explicitly (`import report`): a PDF or a photo
//! cannot prove it is a checkup report, so uploads never land here.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use chrono::NaiveDate;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::config::PipelineConfig;
use crate::ids::{fingerprint, from_fp, stable_id};
use crate::importers::report as files;
use crate::labs;
use crate::models::{self, DocumentInput, ModelProvider, OcrDocument, OcrPage, ValidateContext};
use crate::ops;
use crate::pipeline::Processor;
use crate::sources::{
    add_rule_link, clear_standard, upsert_entity, GlobalCtx, GlobalStage, ImportSpec, RawCtx, RawProcessor, Source,
};
use crate::vault::RawMeta;

pub const OCR: Processor = Processor { name: "report.ocr", version: "1.0.0" };
pub const EXTRACT: Processor = Processor { name: "report.extract", version: "1.0.0" };

pub static SOURCE: Source = Source {
    imports: &[ImportSpec {
        name: files::IMPORT,
        source: files::SOURCE,
        kind: files::KIND,
        expects: "a checkup report or lab sheet: a PDF or a photo (JPEG, PNG, HEIC or TIFF)",
        detect,
        auto_detect: false,
    }],
    raw: &[
        RawProcessor { processor: OCR, kinds: &[files::KIND], load: load_ocr, clear: clear_ocr },
        RawProcessor { processor: EXTRACT, kinds: &[files::KIND], load: load_extract, clear: clear_standard },
    ],
    pre_derive: &[GlobalStage {
        processor: Processor { name: "ops.replay", version: "1.0.0" },
        run: replay_ops,
    }],
    post_link: &[GlobalStage {
        processor: Processor { name: "report.providers", version: "1.0.0" },
        run: link_providers,
    }],
    post_index: &[GlobalStage {
        processor: Processor { name: "report.index", version: "1.0.0" },
        run: index_documents,
    }],
    derived_tables: &["document_pages"],
    ..Source::empty("report")
};

fn detect(_name: Option<&str>, content: &[u8]) -> Option<&'static str> {
    files::sniff(content)
}

fn load_ocr(conn: &Connection, ctx: &RawCtx<'_>, content: &[u8]) -> Result<Value> {
    ocr(conn, ctx.models, ctx.raw_id, ctx.run_id, ctx.meta, content)
}

fn load_extract(conn: &Connection, ctx: &RawCtx<'_>, _content: &[u8]) -> Result<Value> {
    extract(conn, ctx.models, &ctx.config.pipeline, ctx.raw_id, ctx.run_id, &ctx.meta.received_at)
}

/// A new OCR version also re-runs extraction, which reads OCR's output.
fn clear_ocr(conn: &Connection, processor: &str) -> Result<()> {
    clear_standard(conn, processor)?;
    for sql in [
        "DELETE FROM document_pages WHERE run_id IN (SELECT id FROM processor_runs WHERE processor = ?1)",
        "DELETE FROM documents WHERE run_id IN (SELECT id FROM processor_runs WHERE processor = ?1)",
    ] {
        conn.execute(sql, [processor])?;
    }
    clear_standard(conn, EXTRACT.name)?;
    conn.execute("DELETE FROM processor_runs WHERE processor = ?1", [EXTRACT.name])?;
    Ok(())
}

fn replay_ops(conn: &Connection, _ctx: &GlobalCtx<'_>) -> Result<Value> {
    ops::replay(conn)
}

/// Checkups link to the provider named on the report.
fn link_providers(conn: &Connection, _ctx: &GlobalCtx<'_>) -> Result<Value> {
    let checkups: Vec<(String, String)> = conn
        .prepare(
            "SELECT id, json_extract(data, '$.provider') FROM events
             WHERE type = 'checkup' AND json_extract(data, '$.provider') IS NOT NULL ORDER BY start_ts, id",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut links = 0u64;
    for (event, provider) in &checkups {
        let entity = upsert_entity(conn, "provider", provider)?;
        links += u64::from(add_rule_link(conn, "event", event, &entity, "occurred_at")?);
    }
    Ok(json!({ "checkups": checkups.len(), "links": links }))
}

/// Documents (their untrusted text included) and lab analytes in search.
fn index_documents(conn: &Connection, _ctx: &GlobalCtx<'_>) -> Result<Value> {
    conn.execute_batch(
        "DELETE FROM search_fts WHERE kind = 'metric' AND ref_id IN (SELECT code FROM codes WHERE code_system = 'loinc');
         INSERT INTO search_fts (kind, ref_id, local_date, title, body)
           SELECT 'analyte', code, NULL, display, code || ' ' || coalesce(display_zh, '') || ' ' || category_id
           FROM codes WHERE code_system = 'loinc';
         INSERT INTO search_fts (kind, ref_id, local_date, title, body)
           SELECT 'document', d.id, d.issued_at, coalesce(d.title, 'Document'),
                  coalesce(d.provider_name, '') || ' ' ||
                  coalesce((SELECT group_concat(o.label, ' ') FROM observations o WHERE o.document_id = d.id), '') || ' ' ||
                  coalesce(d.text, '')
           FROM documents d;",
    )?;
    let documents: i64 = conn.query_row("SELECT count(*) FROM search_fts WHERE kind = 'document'", [], |r| r.get(0))?;
    Ok(json!({ "documents": documents }))
}

pub const DOC_TYPE: &str = "checkup_report";

pub fn document_id(raw_id: &str) -> String {
    stable_id("doc", &[raw_id])
}

pub fn provider_entity_id(name: &str) -> String {
    stable_id("ent", &["provider", name])
}

/// Longest provider name kept from a report (it is untrusted document text).
const MAX_PROVIDER: usize = 60;

fn ocr(
    conn: &Connection,
    models: &dyn ModelProvider,
    raw_id: &str,
    run_id: &str,
    meta: &RawMeta,
    content: &[u8],
) -> Result<Value> {
    let input = DocumentInput { mime: &meta.mime, bytes: content, lang: meta.lang.as_deref() };
    let doc = models.ocr(&input).context("OCR")?;
    let id = document_id(raw_id);
    let text = doc.pages.iter().map(OcrDocument::page_text).collect::<Vec<_>>().join("\n\n");
    conn.execute("DELETE FROM document_pages WHERE document_id = ?1", [&id])?;
    conn.execute(
        "INSERT INTO documents (id, raw_id, doc_type, language, pages, text, ocr_engine, run_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET language = excluded.language, pages = excluded.pages, text = excluded.text,
           ocr_engine = excluded.ocr_engine, run_id = excluded.run_id",
        params![
            id,
            raw_id,
            DOC_TYPE,
            meta.lang,
            doc.pages.len() as i64,
            text,
            format!("{} ({})", doc.engine, doc.engine_version),
            run_id
        ],
    )?;
    let mut stmt = conn.prepare(
        "INSERT INTO document_pages (document_id, page, width, height, source, text, ocr, run_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    let mut sources: BTreeMap<&str, u64> = BTreeMap::new();
    for page in &doc.pages {
        *sources.entry(page.source.as_str()).or_default() += 1;
        stmt.execute(params![
            id,
            page.page,
            page.width,
            page.height,
            page.source,
            OcrDocument::page_text(page),
            serde_json::to_string(page)?,
            run_id
        ])?;
    }
    Ok(json!({
        "document_id": id,
        "provider": models.name(),
        "engine": doc.engine,
        "engine_version": doc.engine_version,
        "pages": doc.pages.len(),
        "page_sources": sources,
    }))
}

/// The OCR output `report.ocr` stored for a document.
fn stored_ocr(conn: &Connection, document_id: &str) -> Result<(OcrDocument, Option<String>)> {
    let (engine, lang): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT ocr_engine, language FROM documents WHERE id = ?1",
            [document_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .context("the report has not been read yet: report.ocr failed or has not run")?;
    let pages: Vec<OcrPage> = conn
        .prepare("SELECT ocr FROM document_pages WHERE document_id = ?1 ORDER BY page")?
        .query_map([document_id], |r| r.get::<_, String>(0))?
        .map(|t| Ok(serde_json::from_str(&t?)?))
        .collect::<Result<_>>()?;
    let engine = engine.unwrap_or_default();
    Ok((OcrDocument { engine: engine.clone(), engine_version: engine, pages }, lang))
}

const INSERT_LAB: &str = "INSERT OR IGNORE INTO observations
  (id, fp, code_system, code, value_num, value_text, unit, start_ts, end_ts, tz_offset, local_date,
   source_name, source_tier, ref_low, ref_high, flag, document_id, raw_id, run_id, confidence, status,
   page, bbox, label, ref_text, value_canonical, unit_canonical, extracted, review)
  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, 0, ?9, ?10, 2, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
          ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)";

fn extract(
    conn: &Connection,
    models: &dyn ModelProvider,
    cfg: &PipelineConfig,
    raw_id: &str,
    run_id: &str,
    received_at: &str,
) -> Result<Value> {
    let doc_id = document_id(raw_id);
    let (ocr, lang) = stored_ocr(conn, &doc_id)?;
    let x = models.extract(&ocr, lang.as_deref()).context("extraction")?;

    let issued = x
        .report
        .issued_at
        .as_deref()
        .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
    let date = issued
        .or_else(|| received_at.get(..10).and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()))
        .context("no usable date")?
        .to_string();
    let provider: Option<String> = x
        .report
        .provider
        .as_deref()
        .map(|p| labs::fold(p).trim().chars().take(MAX_PROVIDER).collect());
    let title = match &provider {
        Some(p) => format!("Checkup report · {p} · {date}"),
        None => format!("Checkup report · {date}"),
    };
    conn.execute(
        "UPDATE documents SET issued_at = ?2, provider_name = ?3, provider_id = ?4, language = ?5, title = ?6, extractor = ?7
         WHERE id = ?1",
        params![
            doc_id,
            issued.map(|d| d.to_string()),
            provider,
            provider.as_deref().map(provider_entity_id),
            x.report.language.clone().or(lang),
            title,
            models.extractor()
        ],
    )?;

    let ctx = ValidateContext {
        threshold: cfg.review_confidence,
        prints_flags: x.results.iter().any(|r| r.flag_as_printed.is_some()),
        has_date: issued.is_some(),
    };
    let start_ts = format!("{date}T00:00:00Z");
    let mut stmt = conn.prepare_cached(INSERT_LAB)?;
    let mut seen: HashMap<(u32, String), u32> = HashMap::new();
    let mut reasons: BTreeMap<&str, u64> = BTreeMap::new();
    let (mut added, mut pending) = (0u64, 0u64);
    for r in &x.results {
        let v = models::validate(r, &ctx);
        let n = seen.entry((r.page, v.code.clone())).or_default();
        *n += 1;
        let fp = fingerprint(&["lab", raw_id, &r.page.to_string(), &v.code, &n.to_string()]);
        let status = if v.reasons.is_empty() { "auto" } else { "pending" };
        for reason in &v.reasons {
            *reasons.entry(reason.as_str()).or_default() += 1;
        }
        let review = json!({ "reasons": v.reasons.iter().map(|r| r.as_str()).collect::<Vec<_>>() });
        added += stmt.execute(params![
            from_fp("lab", &fp),
            fp,
            v.code_system,
            v.code,
            r.value,
            r.value_text,
            v.unit,
            start_ts,
            date,
            Option::<String>::None,
            r.ref_low,
            r.ref_high,
            r.flag_as_printed,
            doc_id,
            raw_id,
            run_id,
            r.confidence,
            status,
            r.page,
            serde_json::to_string(&r.bbox)?,
            r.analyte_name,
            r.ref_text,
            v.value_canonical,
            v.unit_canonical,
            serde_json::to_string(r)?,
            review.to_string()
        ])? as u64;
        if status == "pending" {
            pending += 1;
        }
    }

    let event_fp = fingerprint(&["checkup", raw_id]);
    conn.execute(
        "INSERT OR REPLACE INTO events
           (id, fp, type, category_id, start_ts, end_ts, tz_offset, local_date, title, data, raw_id, run_id, derived)
         VALUES (?1, ?2, 'checkup', 'labs', ?3, ?3, 0, ?4, ?5, ?6, ?7, ?8, 0)",
        params![
            from_fp("evt", &event_fp),
            event_fp,
            start_ts,
            date,
            format!("{title} ({} results)", x.results.len()),
            json!({ "document_id": doc_id, "provider": provider, "results": x.results.len(), "pending_review": pending })
                .to_string(),
            raw_id,
            run_id
        ],
    )?;
    Ok(json!({
        "document_id": doc_id,
        "extractor": models.extractor(),
        "issued_at": issued.map(|d| d.to_string()),
        "provider": provider,
        "results": added,
        "pending_review": pending,
        "review_reasons": reasons,
    }))
}
