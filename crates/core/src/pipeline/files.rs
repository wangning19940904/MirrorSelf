//! Uploaded files → `items` (workstream W5 in `docs/demo-plan.md`).
//!
//! - `files.rule` reads every `file_*` raw object (assigned by
//!   `sources::generic_file_kind`) with rules only: PDF text layer, text and
//!   Markdown, image EXIF. One item per file; files with nothing readable
//!   are kept with status `needs_review`.
//! - `files.model_output` replays stored model responses (`model_output`)
//!   into `item_annotations`. It never calls a model.
//! - `files.review_decision` copies stored review decisions
//!   (`review_decision`) into `ops_log`.
//! - `files.review_apply` (after links) recomputes what `v_items` shows from
//!   rule values, annotations and decisions; `files.index_items` (after the
//!   search index) refreshes those items' search rows.
//!
//! See `docs/model.md`.

use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

use crate::enrich::review::{self, KIND_REVIEW_DECISION};
use crate::enrich::{self, KIND_MODEL_OUTPUT};
use crate::importers::files::extract;
use crate::pipeline::Processor;
use crate::sources::{
    clear_standard, GlobalCtx, GlobalStage, RawCtx, RawProcessor, Source, KIND_FILE_IMAGE, KIND_FILE_OTHER, KIND_FILE_PDF,
    KIND_FILE_TEXT,
};

pub static SOURCE: Source = Source {
    raw: &[
        RawProcessor {
            processor: Processor { name: "files.rule", version: "1.0.0" },
            kinds: &[KIND_FILE_PDF, KIND_FILE_IMAGE, KIND_FILE_TEXT, KIND_FILE_OTHER],
            load: load_rule,
            clear: clear_standard,
        },
        RawProcessor {
            processor: Processor { name: "files.model_output", version: "1.0.0" },
            kinds: &[KIND_MODEL_OUTPUT],
            load: enrich::load_output,
            clear: enrich::clear_annotations,
        },
        RawProcessor {
            processor: Processor { name: "files.review_decision", version: "1.0.0" },
            kinds: &[KIND_REVIEW_DECISION],
            load: review::load_decision,
            clear: review::keep_decisions,
        },
    ],
    post_link: &[GlobalStage {
        processor: Processor { name: "files.review_apply", version: "1.0.0" },
        run: review::apply,
    }],
    post_index: &[GlobalStage {
        processor: Processor { name: "files.index_items", version: "1.0.0" },
        run: run_index,
    }],
    derived_tables: &["item_annotations", "item_effective"],
    ..Source::empty("files")
};

fn run_index(conn: &Connection, ctx: &GlobalCtx<'_>) -> Result<Value> {
    review::index(conn, ctx)
}

/// One item per file, from rules alone.
fn load_rule(conn: &Connection, ctx: &RawCtx<'_>, content: &[u8]) -> Result<Value> {
    let m = ctx.meta;
    let x = extract(&m.kind, &m.mime, m.name.as_deref(), content);
    let mut data = x.data;
    if let Some(name) = &m.name {
        data.insert("file_name".into(), json!(name));
    }
    let id = enrich::item_id_for(ctx.raw_id);
    conn.execute(
        "INSERT OR REPLACE INTO items
           (id, raw_id, part, type, subtype, category_id, title, text, event_start, tz_offset, local_date,
            received_at, source_updated_at, lat, lon, data, run_id, confidence, method, status)
         VALUES (?1, ?2, '', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, 1.0, 'rule', ?17)",
        params![
            id,
            ctx.raw_id,
            x.item_type,
            x.subtype,
            x.category,
            x.title,
            x.text,
            x.event.start_utc,
            x.event.tz_offset,
            x.event.local_date,
            m.received_at,
            x.source_updated_at,
            x.lat,
            x.lon,
            Value::Object(data).to_string(),
            ctx.run_id,
            if x.readable { "auto" } else { "needs_review" },
        ],
    )?;
    Ok(json!({ "items": 1, "needs_review": u8::from(!x.readable) }))
}
