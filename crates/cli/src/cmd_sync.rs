//! `mirrorself sync`: scheduled pulls from MCP servers and command-line
//! tools (`docs/sync.md`).

use std::fmt::Write as _;
use std::io::{IsTerminal, Read};

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Subcommand;
use mirrorself_core::error::{denied, invalid};
use mirrorself_core::sync::{self, state, SecretStore};
use mirrorself_core::App;
use serde_json::{json, Value};

use crate::render::Out;

#[derive(Subcommand)]
pub enum SyncCmd {
    /// List the sync recipes in mirrorself.toml with their state
    List,
    /// Run one recipe now, or every recipe that is due
    Run {
        /// Recipe name
        #[arg(required_unless_present = "due", conflicts_with = "due")]
        name: Option<String>,
        /// Run every recipe whose interval has passed
        #[arg(long)]
        due: bool,
    },
    /// Run due recipes in a loop until interrupted (Ctrl-C)
    Daemon,
    /// Clear a recipe's pause (after a schema change or a refused tool)
    Resume { name: String },
    /// Recent runs, newest first
    History {
        name: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Credentials for recipes, stored encrypted under the vault key
    #[command(subcommand)]
    Secret(SecretCmd),
}

#[derive(Subcommand)]
pub enum SecretCmd {
    /// Store a secret read from stdin (or from an environment variable)
    Set {
        name: String,
        /// Read the value from this environment variable instead of stdin
        #[arg(long, value_name = "VAR")]
        from_env: Option<String>,
    },
    /// Names of stored secrets (never their values)
    List,
    /// Delete a stored secret
    Remove { name: String },
}

pub fn run(app: &App, cmd: SyncCmd, out: Out<'_>) -> Result<()> {
    match cmd {
        SyncCmd::List => out(sync::list(app, Utc::now())?, render_list),
        SyncCmd::Run { name, due } => {
            let outcomes = match (name, due) {
                (_, true) => sync::run_due(app, Utc::now())?,
                (Some(name), false) => vec![sync::run_recipe(app, &name, Utc::now())?],
                (None, false) => return Err(invalid("name a recipe or pass --due")),
            };
            let failed: Vec<String> = outcomes.iter().filter(|o| !o.is_ok()).map(|o| o.recipe.clone()).collect();
            let refused = outcomes.iter().any(|o| o.status == "refused" || o.status == "paused");
            out(json!({ "count": outcomes.len(), "runs": outcomes }), render_runs);
            if !failed.is_empty() {
                let msg = format!("sync failed or is paused for: {}", failed.join(", "));
                return Err(if refused { denied(msg) } else { anyhow::anyhow!(msg) });
            }
        }
        SyncCmd::Daemon => {
            eprintln!(
                "mirrorself sync daemon: running {} recipe(s) when due; Ctrl-C to stop",
                app.config().sync.len()
            );
            let mut report = |o: &sync::RunOutcome| {
                out(json!({ "count": 1, "runs": [o] }), render_runs);
            };
            sync::daemon(app, &|| false, &mut report)?;
        }
        SyncCmd::Resume { name } => {
            let st = sync::resume(app, &name)?;
            out(json!({ "resumed": name, "state": st }), |v| {
                format!("Resumed {}. It runs at its next due time, or now with `mirrorself sync run`.\n", text(&v["resumed"]))
            });
        }
        SyncCmd::History { name, limit } => {
            let runs = state::recent_runs(app.conn(), name.as_deref(), limit)?;
            out(json!({ "count": runs.len(), "runs": runs }), render_history);
        }
        SyncCmd::Secret(cmd) => {
            let store = SecretStore::open(app.home(), app.vault().subkeys());
            match cmd {
                SecretCmd::Set { name, from_env } => {
                    let value = match from_env {
                        Some(var) => std::env::var(&var).with_context(|| format!("environment variable {var} is not set"))?,
                        None => read_stdin_secret()?,
                    };
                    store.set(&name, value.trim_end_matches(['\r', '\n']))?;
                    out(json!({ "stored": name }), |v| {
                        format!("Stored secret '{}' (encrypted; the value is not shown).\n", text(&v["stored"]))
                    });
                }
                SecretCmd::List => {
                    let names = store.names()?;
                    out(json!({ "count": names.len(), "secrets": names }), |v| {
                        let names = v["secrets"].as_array().map(Vec::as_slice).unwrap_or_default();
                        if names.is_empty() {
                            return "No secrets stored.\n".into();
                        }
                        names.iter().map(|n| format!("{}\n", text(n))).collect()
                    });
                }
                SecretCmd::Remove { name } => {
                    if !store.remove(&name)? {
                        return Err(mirrorself_core::error::not_found(format!("no secret named '{name}'")));
                    }
                    out(json!({ "removed": name }), |v| format!("Removed secret '{}'.\n", text(&v["removed"])));
                }
            }
        }
    }
    Ok(())
}

fn read_stdin_secret() -> Result<String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        eprintln!("Paste the secret and press Enter (input is visible; prefer --from-env VAR or a pipe):");
        let mut line = String::new();
        stdin.read_line(&mut line)?;
        return Ok(line);
    }
    let mut buf = String::new();
    stdin.lock().take(64 * 1024).read_to_string(&mut buf).context("reading the secret from stdin")?;
    Ok(buf)
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or("–")
}

fn render_list(v: &Value) -> String {
    let recipes = v["recipes"].as_array().map(Vec::as_slice).unwrap_or_default();
    if recipes.is_empty() {
        return "No sync recipes configured. Add [[sync]] tables to mirrorself.toml (see docs/sync.md).\n".into();
    }
    let mut out = String::new();
    for r in recipes {
        let st = &r["state"];
        let status = if st["paused"].as_bool() == Some(true) {
            "PAUSED".to_string()
        } else if r["due"].as_bool() == Some(true) {
            "due".to_string()
        } else {
            format!("next {}", text(&r["next_due"]))
        };
        let target = &r["target"];
        let what = match target["tool"].as_str() {
            Some(tool) => format!("{} → {}", text(&target["server"]), tool),
            None => text(&target["program"]).to_string(),
        };
        let _ = writeln!(
            out,
            "{:<20} {:<4} every {:<5} {:<24} {}",
            text(&r["name"]),
            text(&r["adapter"]),
            text(&r["interval"]),
            text(&r["kind"]),
            status
        );
        let _ = writeln!(out, "    {what}");
        let _ = writeln!(
            out,
            "    cursor {}  last success {}",
            text(&st["cursor"]),
            text(&st["last_success_at"])
        );
        if let Some(reason) = st["paused_reason"].as_str() {
            let _ = writeln!(out, "    paused: {reason}");
        } else if let Some(err) = st["last_error"].as_str() {
            let _ = writeln!(out, "    last error: {err}");
        }
        let missing: Vec<&str> = r["missing_secrets"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        if !missing.is_empty() {
            let _ = writeln!(out, "    missing secrets: {} (mirrorself sync secret set <name>)", missing.join(", "));
        }
        if r["kind_has_parser"].as_bool() == Some(false) {
            let _ = writeln!(out, "    note: nothing in this build parses kind '{}'; responses are stored only", text(&r["kind"]));
        }
    }
    out
}

fn render_runs(v: &Value) -> String {
    let runs = v["runs"].as_array().map(Vec::as_slice).unwrap_or_default();
    if runs.is_empty() {
        return "Nothing is due.\n".into();
    }
    let mut out = String::new();
    for r in runs {
        let _ = write!(out, "{:<20} {:<8}", text(&r["recipe"]), text(&r["status"]));
        if let (Some(f), Some(t)) = (r["window_from"].as_str(), r["window_to"].as_str()) {
            let _ = write!(out, " {f} .. {t}");
        }
        out.push('\n');
        match r["raw_id"].as_str() {
            Some(id) if r["created"].as_bool() == Some(true) => {
                let _ = writeln!(out, "    stored {} ({} bytes)", &id[..12.min(id.len())], r["bytes"]);
            }
            Some(id) => {
                let _ = writeln!(out, "    unchanged: already in the vault as {}", &id[..12.min(id.len())]);
            }
            None if r["status"] == "ok" => out.push_str("    empty response; nothing stored\n"),
            None => {}
        }
        if let Some(e) = r["error"].as_str() {
            let _ = writeln!(out, "    {e}");
        }
        if let Some(p) = r["process"].as_object() {
            let errors = p["runs"].as_array().into_iter().flatten().filter(|x| x["status"] != "ok").count();
            let _ = writeln!(out, "    processed: {} run(s), {} error(s)", p["runs"].as_array().map_or(0, Vec::len), errors);
        }
    }
    out
}

fn render_history(v: &Value) -> String {
    let runs = v["runs"].as_array().map(Vec::as_slice).unwrap_or_default();
    if runs.is_empty() {
        return "No sync runs yet.\n".into();
    }
    let mut out = String::new();
    for r in runs {
        let _ = writeln!(
            out,
            "{:>5}  {}  {:<20} {:<8} {}",
            r["id"],
            text(&r["started_at"]),
            text(&r["recipe"]),
            text(&r["status"]),
            r["error"].as_str().unwrap_or_else(|| r["raw_id"].as_str().map(|s| &s[..12.min(s.len())]).unwrap_or(""))
        );
    }
    out
}
