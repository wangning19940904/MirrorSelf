//! `mirrorself items`: ingested files and generic records with their
//! metadata envelope, the optional model step and the review queue
//! (`docs/model.md`).

use std::fmt::Write;

use anyhow::Result;
use clap::{ArgGroup, Subcommand};
use mirrorself_core::enrich::http::HttpModelClient;
use mirrorself_core::enrich::review::{self, Edit, ListFilter, Review};
use mirrorself_core::enrich::{self, EnrichOptions, ModelClient};
use mirrorself_core::App;
use serde_json::Value;

use crate::render::Out;

#[derive(Subcommand)]
pub enum ItemsCmd {
    /// List items, newest first
    List {
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// auto, needs_review, confirmed or rejected (rejected items are
        /// listed only when asked for)
        #[arg(long)]
        status: Option<String>,
        /// document, photo, screenshot, file, record, …
        #[arg(long = "type")]
        item_type: Option<String>,
    },
    /// One item: effective values, rule values, model annotations and
    /// review history
    Show { id: String },
    /// Send files rules could not read to the model configured under
    /// [model] in mirrorself.toml, and store its answers for review
    #[command(group(ArgGroup::new("which").required(true).multiple(true).args(["ids", "pending"])))]
    Enrich {
        /// An item to send (repeatable)
        #[arg(long = "id")]
        ids: Vec<String>,
        /// Every item awaiting review that this model has not answered yet
        #[arg(long)]
        pending: bool,
        /// Show what would be sent; call nothing, store nothing
        #[arg(long)]
        dry_run: bool,
        /// Ask again even if this model already answered
        #[arg(long)]
        force: bool,
        /// Most items per run with --pending
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// What awaits review, with the model's pending answers
    Review {
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Accept the pending model answer (or, with none, the item as it is)
    Confirm {
        /// Item id (or annotation id)
        id: String,
        /// Which annotation, when several await review
        #[arg(long)]
        annotation: Option<String>,
    },
    /// Reject the pending model answer (or, with none, hide the item)
    Reject {
        /// Item id (or annotation id)
        id: String,
        #[arg(long)]
        annotation: Option<String>,
    },
    /// Correct an item's values; edits take precedence over rules and models
    Edit {
        id: String,
        #[arg(long)]
        title: Option<String>,
        /// document, photo, screenshot, file or other
        #[arg(long = "type")]
        item_type: Option<String>,
        #[arg(long)]
        subtype: Option<String>,
        #[arg(long)]
        text: Option<String>,
        /// RFC 3339 with offset, YYYY-MM-DDTHH:MM:SS, or YYYY-MM-DD
        #[arg(long)]
        event_time: Option<String>,
        #[arg(long)]
        place: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        lat: Option<f64>,
        #[arg(long, allow_hyphen_values = true)]
        lon: Option<f64>,
        /// Empty a field: title, subtype, text, event_time, place, location
        #[arg(long, value_name = "FIELD")]
        clear: Vec<String>,
    },
}

pub fn run(app: &App, cmd: ItemsCmd, out: Out<'_>) -> Result<()> {
    match cmd {
        ItemsCmd::List { limit, status, item_type } => {
            out(review::list(app.conn(), &ListFilter { status, item_type, limit: Some(limit) })?, render_list)
        }
        ItemsCmd::Show { id } => out(review::show(app.conn(), &id)?, render_show),
        ItemsCmd::Enrich { ids, pending, dry_run, force, limit } => {
            let opts = EnrichOptions { ids, pending, dry_run, force, limit: Some(limit) };
            let config = &app.config().model;
            let client: Option<Box<dyn ModelClient>> = if dry_run || config.endpoint.is_none() {
                None
            } else {
                Some(Box::new(HttpModelClient::new(&config.settings()?)?))
            };
            out(enrich::enrich(app, client.as_deref(), &opts)?, render_enrich);
        }
        ItemsCmd::Review { limit } => out(review::queue(app.conn(), Some(limit))?, render_queue),
        ItemsCmd::Confirm { id, annotation } => out(review::review(app, &id, Review::Confirm { annotation })?, render_decision),
        ItemsCmd::Reject { id, annotation } => out(review::review(app, &id, Review::Reject { annotation })?, render_decision),
        ItemsCmd::Edit { id, title, item_type, subtype, text, event_time, place, lat, lon, clear } => {
            let edit = Edit { title, item_type, subtype, text, event_time, place, lat, lon, clear };
            out(review::review(app, &id, Review::Edit(edit))?, render_decision);
        }
    }
    Ok(())
}

fn text(v: &Value) -> String {
    match v {
        Value::Null => "–".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn kind(item: &Value) -> String {
    match item["subtype"].as_str() {
        Some(sub) => format!("{}/{sub}", text(&item["type"])),
        None => text(&item["type"]),
    }
}

fn title(item: &Value) -> String {
    item["title"].as_str().or(item["file_name"].as_str()).unwrap_or("–").to_string()
}

fn render_list(v: &Value) -> String {
    let items = v["items"].as_array().map(Vec::as_slice).unwrap_or_default();
    if items.is_empty() {
        return "No items.\n".into();
    }
    let mut out = String::new();
    for item in items {
        let _ = writeln!(
            out,
            "{}  {:<10}  {:<24}  {}  [{}]",
            text(&item["id"]),
            text(&item["local_date"]),
            kind(item),
            title(item),
            text(&item["status"]),
        );
    }
    out
}

fn render_show(v: &Value) -> String {
    let i = &v["item"];
    let mut out = String::new();
    let _ = writeln!(out, "{}  {}", text(&i["id"]), title(i));
    let _ = writeln!(out, "  status      {} (method {}, confidence {})", text(&i["status"]), text(&i["method"]), text(&i["confidence"]));
    for (label, key) in [
        ("type", "type"),
        ("subtype", "subtype"),
        ("event", "event_start"),
        ("local date", "local_date"),
        ("place", "place"),
        ("lat", "lat"),
        ("lon", "lon"),
        ("file", "file_name"),
        ("received", "received_at"),
        ("updated", "source_updated_at"),
    ] {
        let _ = writeln!(out, "  {label:<11} {}", text(&i[key]));
    }
    if let Some(reason) = i["data"]["reason"].as_str() {
        let _ = writeln!(out, "  rules       {reason}");
    }
    if let Some(t) = i["text"].as_str() {
        let _ = writeln!(out, "\n{t}\n");
    }
    for a in v["annotations"].as_array().into_iter().flatten() {
        let _ = writeln!(
            out,
            "annotation {}  [{}]  {} ({}), confidence {}",
            text(&a["id"]),
            text(&a["status"]),
            text(&a["model"]),
            text(&a["prompt_version"]),
            text(&a["confidence"]),
        );
        if let Some(e) = a["error"].as_str() {
            let _ = writeln!(out, "  rejected by validation: {e}");
        } else {
            let _ = writeln!(out, "  {}  {}  {}  {}", kind(a), text(&a["title"]), text(&a["event_time"]), text(&a["place"]));
        }
    }
    for d in v["decisions"].as_array().into_iter().flatten() {
        let _ = writeln!(out, "decision {}  {}  {}", text(&d["hlc"]), text(&d["op"]), d["payload"]);
    }
    out
}

fn render_enrich(v: &Value) -> String {
    let mut out = String::new();
    let model = v["model"].as_str().unwrap_or("(no model configured)");
    let _ = writeln!(
        out,
        "{} with {model}, prompt {}",
        if v["dry_run"].as_bool() == Some(true) { "Dry run" } else { "Enrichment" },
        text(&v["prompt_version"])
    );
    for r in v["results"].as_array().into_iter().flatten() {
        let _ = writeln!(out, "  {}  {:<28}  {}", text(&r["item_id"]), text(&r["file_name"]), enrich::describe(r));
    }
    if v["results"].as_array().is_none_or(Vec::is_empty) {
        out.push_str("  Nothing to send.\n");
    }
    if v["needs_review"].as_u64().unwrap_or(0) > 0 {
        out.push_str("Review the answers with `mirrorself items review`.\n");
    }
    out
}

fn render_queue(v: &Value) -> String {
    let items = v["items"].as_array().map(Vec::as_slice).unwrap_or_default();
    if items.is_empty() {
        return "Nothing awaits review.\n".into();
    }
    let mut out = String::new();
    for i in items {
        let _ = writeln!(out, "{}  {:<16}  {}  ({})", text(&i["id"]), kind(i), title(i), text(&i["reason"]));
        for a in i["annotations"].as_array().into_iter().flatten() {
            let _ = writeln!(
                out,
                "    model says: {}  \"{}\"  {}  {}  (confidence {}, {})",
                kind(a),
                text(&a["title"]),
                text(&a["event_time"]),
                text(&a["place"]),
                text(&a["confidence"]),
                text(&a["id"]),
            );
        }
    }
    out.push_str("Confirm, reject or edit with `mirrorself items confirm|reject|edit <id>`.\n");
    out
}

fn render_decision(v: &Value) -> String {
    let d = &v["decision"];
    let i = &v["item"];
    format!(
        "Recorded {} ({}). {} is now {}: {}\n",
        text(&d["op"]),
        text(&d["op_id"]),
        text(&i["id"]),
        text(&i["status"]),
        title(i)
    )
}
