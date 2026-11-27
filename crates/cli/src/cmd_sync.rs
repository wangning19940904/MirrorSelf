//! `mirrorself sync`: scheduled pulls from MCP servers and command-line
//! tools (`docs/demo-plan.md`). Running recipes arrives with workstream W3;
//! until then this only shows what is configured.

use std::fmt::Write;

use anyhow::Result;
use clap::Subcommand;
use mirrorself_core::App;
use serde_json::{json, Value};

use crate::render::Out;

#[derive(Subcommand)]
pub enum SyncCmd {
    /// List the sync recipes in mirrorself.toml
    List,
}

pub fn run(app: &App, cmd: SyncCmd, out: Out<'_>) -> Result<()> {
    match cmd {
        SyncCmd::List => {
            let names: Vec<&str> = app.config().sync.iter().map(|r| r.name.as_str()).collect();
            out(json!({ "count": names.len(), "recipes": names }), render_list);
        }
    }
    Ok(())
}

fn render_list(v: &Value) -> String {
    let recipes = v["recipes"].as_array().map(Vec::as_slice).unwrap_or_default();
    if recipes.is_empty() {
        return "No sync recipes configured.\n".into();
    }
    let mut out = String::new();
    for name in recipes {
        let _ = writeln!(out, "{}", name.as_str().unwrap_or("–"));
    }
    out
}
