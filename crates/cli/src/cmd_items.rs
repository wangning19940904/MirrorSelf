//! `mirrorself items`: ingested files and generic records with their
//! metadata envelope (`docs/demo-plan.md`). Review commands arrive with
//! workstream W5.

use std::fmt::Write;

use anyhow::Result;
use clap::Subcommand;
use mirrorself_core::App;
use serde_json::{json, Value};

use crate::render::Out;

#[derive(Subcommand)]
pub enum ItemsCmd {
    /// List items, newest first
    List {
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
}

pub fn run(app: &App, cmd: ItemsCmd, out: Out<'_>) -> Result<()> {
    match cmd {
        ItemsCmd::List { limit } => out(list(app, limit)?, render_list),
    }
    Ok(())
}

fn list(app: &App, limit: u32) -> Result<Value> {
    let rows = app
        .conn()
        .prepare(
            "SELECT id, type, subtype, title, local_date, event_start, place, status, method,
                    source, file_name, received_at
             FROM v_items
             ORDER BY coalesce(event_start, received_at) DESC, id
             LIMIT ?1",
        )?
        .query_map([limit], |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "type": r.get::<_, String>(1)?,
                "subtype": r.get::<_, Option<String>>(2)?,
                "title": r.get::<_, Option<String>>(3)?,
                "local_date": r.get::<_, Option<String>>(4)?,
                "event_start": r.get::<_, Option<String>>(5)?,
                "place": r.get::<_, Option<String>>(6)?,
                "status": r.get::<_, String>(7)?,
                "method": r.get::<_, String>(8)?,
                "source": r.get::<_, String>(9)?,
                "file_name": r.get::<_, Option<String>>(10)?,
                "received_at": r.get::<_, String>(11)?,
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({ "count": rows.len(), "items": rows }))
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or("–")
}

fn render_list(v: &Value) -> String {
    let items = v["items"].as_array().map(Vec::as_slice).unwrap_or_default();
    if items.is_empty() {
        return "No items yet.\n".into();
    }
    let mut out = String::new();
    for item in items {
        let kind = match item["subtype"].as_str() {
            Some(sub) => format!("{}/{sub}", text(&item["type"])),
            None => text(&item["type"]).to_string(),
        };
        let title = item["title"].as_str().or(item["file_name"].as_str()).unwrap_or("–");
        let _ = writeln!(
            out,
            "{}  {:<10}  {:<24}  {}  [{}]",
            text(&item["id"]),
            text(&item["local_date"]),
            kind,
            title,
            text(&item["status"]),
        );
    }
    out
}
