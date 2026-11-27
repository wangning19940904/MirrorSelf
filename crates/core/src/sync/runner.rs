//! Running recipes: fetch, store, advance the cursor, process.

use std::collections::BTreeMap;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use crate::app::{App, RawInput};
use crate::config::Config;
use crate::error::{denied, invalid, not_found};
use crate::pipeline::ProcessReport;
use crate::sources::SOURCES;
use crate::sync::mcp::{McpClient, ServerSpec};
use crate::sync::process::{clean_text, run_captured};
use crate::sync::recipe::{render, render_arguments, rfc3339, AdapterKind, Recipe, Window};
use crate::sync::secrets::{substitute, Redactor, Secret, SecretStore};
use crate::sync::state::{self, RecipeState, RunRecord};

/// What one attempt did.
#[derive(Debug, Serialize)]
pub struct RunOutcome {
    pub recipe: String,
    /// `ok`, `error`, `refused` (tool not read-only), `paused` (paused now
    /// or already).
    pub status: String,
    pub run_id: Option<i64>,
    pub window_from: Option<String>,
    pub window_to: Option<String>,
    pub raw_id: Option<String>,
    /// False when the vault already held an identical response.
    pub created: Option<bool>,
    pub bytes: Option<u64>,
    /// MCP protocol revision the server spoke.
    pub protocol: Option<String>,
    pub error: Option<String>,
    pub process: Option<ProcessReport>,
}

impl RunOutcome {
    pub fn is_ok(&self) -> bool {
        self.status == "ok"
    }
}

enum Fail {
    Error(String),
    /// The tool is not marked read-only: pause and refuse.
    Refused(String),
    /// The tool's input schema no longer matches the pin: pause.
    SchemaChanged(String),
}

struct Fetched {
    bytes: Vec<u8>,
    mime: &'static str,
    protocol: Option<String>,
}

/// Recipe names must be unique.
pub fn check_config(config: &Config) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for r in &config.sync {
        if !seen.insert(r.name.as_str()) {
            return Err(invalid(format!("two [[sync]] recipes are named '{}'; names must be unique", r.name)));
        }
    }
    Ok(())
}

pub fn find_recipe<'a>(config: &'a Config, name: &str) -> Result<&'a Recipe> {
    check_config(config)?;
    config.sync.iter().find(|r| r.name == name).ok_or_else(|| {
        let known: Vec<&str> = config.sync.iter().map(|r| r.name.as_str()).collect();
        not_found(format!(
            "no sync recipe named '{name}' in mirrorself.toml{}",
            if known.is_empty() { String::new() } else { format!(" (recipes: {})", known.join(", ")) }
        ))
    })
}

/// The recipe a stored response came from, from its `origin_ref`
/// (`sync:<name>:<window>`).
pub fn recipe_for_origin<'a>(config: &'a Config, origin_ref: Option<&str>) -> Option<&'a Recipe> {
    let name = origin_ref?.strip_prefix("sync:")?.split(':').next()?;
    config.sync.iter().find(|r| r.name == name)
}

pub fn origin_ref(recipe: &Recipe, w: &Window) -> String {
    format!("sync:{}:{}", recipe.name, w.label())
}

pub fn is_due(recipe: &Recipe, st: &RecipeState, now: DateTime<Utc>) -> bool {
    !st.paused && next_due(recipe, st).is_none_or(|t| now >= t)
}

/// When the recipe is next due; `None` when it never ran.
pub fn next_due(recipe: &Recipe, st: &RecipeState) -> Option<DateTime<Utc>> {
    st.last_run_time().map(|t| t + chrono::Duration::seconds(recipe.interval_secs))
}

/// Runs every recipe that is due, in config order.
pub fn run_due(app: &App, now: DateTime<Utc>) -> Result<Vec<RunOutcome>> {
    check_config(app.config())?;
    let mut out = Vec::new();
    for recipe in &app.config().sync {
        let st = state::load(app.conn(), &recipe.name)?;
        if is_due(recipe, &st, now) {
            out.push(run_recipe(app, &recipe.name, now)?);
        }
    }
    Ok(out)
}

/// Runs one recipe now, whether or not it is due. A paused recipe is not
/// run. Errors from the server or program are recorded and returned in the
/// outcome; `Err` means the hub itself failed (database, vault).
pub fn run_recipe(app: &App, name: &str, now: DateTime<Utc>) -> Result<RunOutcome> {
    let recipe = find_recipe(app.config(), name)?;
    let conn = app.conn();
    let mut st = state::load(conn, name)?;
    let mut outcome = RunOutcome {
        recipe: name.to_string(),
        status: "paused".into(),
        run_id: None,
        window_from: None,
        window_to: None,
        raw_id: None,
        created: None,
        bytes: None,
        protocol: None,
        error: None,
        process: None,
    };
    if st.paused {
        outcome.error = st.paused_reason.clone();
        return Ok(outcome);
    }
    let window = recipe.window(st.cursor_time(), now);
    outcome.window_from = Some(rfc3339(&window.since));
    outcome.window_to = Some(rfc3339(&window.until));
    let timer = Instant::now();

    let store = SecretStore::open(app.home(), app.vault().subkeys());
    let mut secrets = BTreeMap::new();
    let mut secret_error = None;
    for n in recipe.secret_names() {
        match store.get(&n) {
            Ok(s) => {
                secrets.insert(n, s);
            }
            Err(e) => {
                secret_error = Some(format!("{e:#}"));
                break;
            }
        }
    }
    let redactor = Redactor::new(&secrets);
    let fetched = match secret_error {
        Some(e) => Err(Fail::Error(e)),
        None => match recipe.adapter {
            AdapterKind::Mcp => fetch_mcp(recipe, &mut st, &secrets, &window, now),
            AdapterKind::Cli => fetch_cli(recipe, &secrets, &window),
        },
    };

    st.last_run_at = Some(rfc3339(&now));
    let finished = now + chrono::Duration::milliseconds(timer.elapsed().as_millis() as i64);
    let mut run = RunRecord {
        id: 0,
        recipe: name.to_string(),
        started_at: rfc3339(&now),
        finished_at: rfc3339(&finished),
        window_from: outcome.window_from.clone(),
        window_to: outcome.window_to.clone(),
        status: String::new(),
        raw_id: None,
        created: None,
        bytes: None,
        error: None,
    };
    match fetched {
        Ok(f) => {
            outcome.protocol = f.protocol;
            outcome.bytes = Some(f.bytes.len() as u64);
            if !f.bytes.is_empty() {
                let input = RawInput {
                    source: recipe.def.source.clone(),
                    kind: recipe.def.kind.clone(),
                    mime: f.mime.into(),
                    name: None,
                    origin_ref: Some(origin_ref(recipe, &window)),
                };
                let report = app.ingest(input, &f.bytes, false)?;
                outcome.raw_id = Some(report.raw_id.clone());
                outcome.created = Some(report.created);
            }
            st.cursor = Some(rfc3339(&window.until));
            st.last_success_at = Some(rfc3339(&now));
            st.last_error = None;
            st.last_error_at = None;
            outcome.status = "ok".into();
        }
        Err(fail) => {
            let (status, msg) = match fail {
                Fail::Error(m) => ("error", m),
                Fail::Refused(m) => ("refused", m),
                Fail::SchemaChanged(m) => ("paused", m),
            };
            let msg = redactor.scrub(&msg);
            if status != "error" {
                st.paused = true;
                st.paused_reason = Some(msg.clone());
                st.paused_at = Some(rfc3339(&now));
            }
            st.last_error = Some(msg.clone());
            st.last_error_at = Some(rfc3339(&now));
            outcome.status = status.into();
            outcome.error = Some(msg);
        }
    }
    run.status = outcome.status.clone();
    run.raw_id = outcome.raw_id.clone();
    run.created = outcome.created;
    run.bytes = outcome.bytes.map(|b| b as i64);
    run.error = outcome.error.clone();
    let tx = conn.unchecked_transaction()?;
    outcome.run_id = Some(state::insert_run(&tx, &run)?);
    state::save(&tx, &st)?;
    tx.commit()?;

    if outcome.created == Some(true) {
        outcome.process = Some(app.process(false)?);
    }
    Ok(outcome)
}

fn fetch_mcp(
    recipe: &Recipe,
    st: &mut RecipeState,
    secrets: &BTreeMap<String, Secret>,
    window: &Window,
    now: DateTime<Utc>,
) -> Result<Fetched, Fail> {
    let m = recipe.def.mcp.as_ref().expect("checked: mcp recipes have [sync.mcp]");
    let err = |e: anyhow::Error| Fail::Error(format!("{e:#}"));
    let spec = match (&m.url, &m.command) {
        (Some(url), _) => {
            let mut headers = Vec::new();
            if let Some(b) = &m.bearer_secret {
                let token = secrets.get(b).ok_or_else(|| Fail::Error(format!("secret '{b}' was not loaded")))?;
                headers.push(("Authorization".to_string(), format!("Bearer {}", token.expose())));
            }
            for (k, v) in &m.headers {
                headers.push((k.clone(), substitute(v, secrets).map_err(err)?));
            }
            ServerSpec::Http { url: url.clone(), headers }
        }
        (None, Some(cmd)) => {
            let mut env = BTreeMap::new();
            for (k, v) in &m.env {
                env.insert(k.clone(), substitute(v, secrets).map_err(err)?);
            }
            ServerSpec::Stdio { program: cmd.clone(), args: m.args.clone(), env }
        }
        (None, None) => unreachable!("checked: url or command"),
    };
    let timeout = Duration::from_secs(recipe.timeout_secs);
    let mut client = McpClient::connect(&spec, timeout, recipe.max_bytes).map_err(err)?;
    let protocol = client.era().version().to_string();
    let tools = client.list_tools().map_err(err)?;
    let Some(tool) = tools.iter().find(|t| t.name == m.tool) else {
        client.close();
        return Err(Fail::Error(format!("the MCP server does not list a tool named '{}'", m.tool)));
    };
    if !tool.read_only() {
        client.close();
        return Err(Fail::Refused(format!(
            "tool '{}' is not annotated read-only (readOnlyHint: true); MirrorSelf only calls read-only tools. \
             The recipe is paused; it stays refused until the server marks the tool read-only",
            m.tool
        )));
    }
    let hash = tool.schema_hash();
    match (&st.tool, &st.schema_sha256) {
        (Some(t), Some(pinned)) if *t == m.tool && *pinned != hash => {
            client.close();
            return Err(Fail::SchemaChanged(format!(
                "schema changed: the input schema of tool '{}' is no longer the one pinned on {} (pinned sha256 {}, now {}). \
                 Nothing was called. Check the recipe's arguments against the new schema, then run `mirrorself sync resume {}`",
                m.tool,
                st.pinned_at.as_deref().unwrap_or("first run"),
                &pinned[..12],
                &hash[..12],
                recipe.name
            )));
        }
        (Some(t), Some(_)) if *t == m.tool => {}
        _ => {
            st.tool = Some(m.tool.clone());
            st.schema_sha256 = Some(hash);
            st.pinned_at = Some(rfc3339(&now));
        }
    }
    let args = render_arguments(&m.arguments, &recipe.placeholders(window));
    let result = client.call_tool(&m.tool, args);
    client.close();
    let result = result.map_err(err)?;
    if result["isError"].as_bool() == Some(true) {
        return Err(Fail::Error(format!("tool '{}' reported an error: {}", m.tool, clean_text(&content_text(&result), 300))));
    }
    let bytes = serde_json::to_vec(&result).map_err(|e| Fail::Error(e.to_string()))?;
    if bytes.len() as u64 > recipe.max_bytes {
        return Err(Fail::Error(format!("the response is larger than max_bytes ({})", recipe.max_bytes)));
    }
    Ok(Fetched { bytes, mime: "application/json", protocol: Some(protocol) })
}

fn content_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn fetch_cli(recipe: &Recipe, secrets: &BTreeMap<String, Secret>, window: &Window) -> Result<Fetched, Fail> {
    let c = recipe.def.cli.as_ref().expect("checked: cli recipes have [sync.cli]");
    let values = recipe.placeholders(window);
    let mut cmd = Command::new(&c.program);
    cmd.args(c.args.iter().map(|a| render(a, &values)));
    for (k, v) in &c.env {
        cmd.env(k, substitute(v, secrets).map_err(|e| Fail::Error(format!("{e:#}")))?);
    }
    let out = run_captured(cmd, None, Duration::from_secs(recipe.timeout_secs), recipe.max_bytes)
        .map_err(|e| Fail::Error(format!("{e:#}")))?;
    if !out.status.success() {
        let stderr = clean_text(&out.stderr_tail, 500);
        return Err(Fail::Error(format!(
            "{} exited with {}{}",
            c.program,
            out.status,
            if stderr.is_empty() { String::new() } else { format!(": {stderr}") }
        )));
    }
    let mime = if serde_json::from_slice::<Value>(&out.stdout).is_ok() { "application/json" } else { "text/plain" };
    Ok(Fetched { bytes: out.stdout, mime, protocol: None })
}

/// Clears a pause. After a schema change the old pin is dropped, so the
/// next run pins the schema the server shows now.
pub fn resume(app: &App, name: &str) -> Result<RecipeState> {
    find_recipe(app.config(), name)?;
    let mut st = state::load(app.conn(), name)?;
    if !st.paused {
        return Err(invalid(format!("sync recipe '{name}' is not paused")));
    }
    if st.paused_reason.as_deref().is_some_and(|r| r.starts_with("schema changed")) {
        st.schema_sha256 = None;
        st.pinned_at = None;
    }
    st.paused = false;
    st.paused_reason = None;
    st.paused_at = None;
    state::save(app.conn(), &st)?;
    Ok(st)
}

/// Every recipe with its state, for `mirrorself sync list`.
pub fn list(app: &App, now: DateTime<Utc>) -> Result<Value> {
    check_config(app.config())?;
    let store = SecretStore::open(app.home(), app.vault().subkeys());
    let stored = store.names()?;
    let mut rows = Vec::new();
    for r in &app.config().sync {
        let st = state::load(app.conn(), &r.name)?;
        let missing: Vec<String> = r.secret_names().into_iter().filter(|n| !stored.contains(n)).collect();
        let handled = SOURCES.iter().flat_map(|s| s.raw.iter()).any(|p| p.kinds.contains(&r.def.kind.as_str()));
        let target = match (&r.def.mcp, &r.def.cli) {
            (Some(m), _) => json!({ "server": m.url.clone().or(m.command.clone()), "tool": m.tool }),
            (_, Some(c)) => json!({ "program": c.program }),
            _ => Value::Null,
        };
        rows.push(json!({
            "name": r.name,
            "adapter": r.def.adapter,
            "source": r.def.source,
            "kind": r.def.kind,
            "kind_has_parser": handled,
            "interval": r.def.interval,
            "target": target,
            "due": is_due(r, &st, now),
            "next_due": next_due(r, &st).map(|t| rfc3339(&t)),
            "missing_secrets": missing,
            "state": st,
        }));
    }
    Ok(json!({ "count": rows.len(), "recipes": rows }))
}

/// Runs due recipes until `stop` returns true, sleeping between rounds.
/// `report` sees every outcome.
pub fn daemon(app: &App, stop: &dyn Fn() -> bool, report: &mut dyn FnMut(&RunOutcome)) -> Result<()> {
    check_config(app.config())?;
    if app.config().sync.is_empty() {
        return Err(denied("no [[sync]] recipes in mirrorself.toml; nothing to run"));
    }
    while !stop() {
        let now = Utc::now();
        for outcome in run_due(app, now)? {
            report(&outcome);
        }
        // Sleep until the next recipe is due, waking at least once a minute.
        let now = Utc::now();
        let mut wait = Duration::from_secs(60);
        for r in &app.config().sync {
            let st = state::load(app.conn(), &r.name)?;
            if st.paused {
                continue;
            }
            if let Some(t) = next_due(r, &st) {
                let secs = (t - now).num_seconds().max(1) as u64;
                wait = wait.min(Duration::from_secs(secs));
            }
        }
        let until = Instant::now() + wait;
        while Instant::now() < until && !stop() {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    Ok(())
}
