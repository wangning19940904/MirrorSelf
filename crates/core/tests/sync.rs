//! Scheduled sync (docs/sync.md): recipes, adapters, safety, storage and the
//! two processing tiers. Every server and program here is synthetic: a
//! shell script answering from fixtures, or an in-process HTTP server.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use mirrorself_core::config::{Config, Home};
use mirrorself_core::sync::{self, runner, state, Recipe, SecretStore};
use mirrorself_core::App;
use serde_json::{json, Value};

const FX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/sync");

fn fx(name: &str) -> String {
    format!("{FX}/{name}")
}

fn t(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

struct Env {
    dir: tempfile::TempDir,
    home: Home,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().join("home"));
        App::init(&home, "server", None).unwrap();
        Self { dir, home }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Appends recipes to mirrorself.toml and opens the home.
    fn open(&self, recipes: &str) -> App {
        let mut cfg = std::fs::read_to_string(self.home.config_path()).unwrap();
        cfg.push('\n');
        cfg.push_str(recipes);
        std::fs::write(self.home.config_path(), cfg).unwrap();
        App::open(self.home.clone()).unwrap()
    }

    fn put(&self, name: &str, fixture: &str) {
        std::fs::copy(fx(fixture), self.path(name)).unwrap();
    }

    fn log(&self, name: &str) -> Vec<String> {
        std::fs::read_to_string(self.path(name))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn count(app: &App, sql: &str) -> i64 {
    app.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn rows(app: &App, sql: &str) -> Vec<Vec<Value>> {
    let mut stmt = app.conn().prepare(sql).unwrap();
    let n = stmt.column_count();
    stmt.query_map([], |r| {
        (0..n)
            .map(|i| {
                Ok(match r.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(x) => json!(x),
                    rusqlite::types::ValueRef::Real(x) => json!(x),
                    rusqlite::types::ValueRef::Text(x) => json!(String::from_utf8_lossy(x)),
                    rusqlite::types::ValueRef::Blob(x) => json!(x.len()),
                })
            })
            .collect()
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

/// A stdio MCP recipe served by the fake server script.
fn mcp_recipe(env: &Env, name: &str, era: &str, tool: &str, extra: &str) -> String {
    format!(
        r#"
[[sync]]
name = "{name}"
adapter = "mcp"
source = "github"
kind = "mcp_response"
interval = "1h"
overlap = "1d"
lookback = "7d"
cursor_format = "%Y-%m-%d"
utc_offset = "+08:00"
{extra}
[sync.mcp]
command = "sh"
args = ["{script}"]
tool = "{tool}"
[sync.mcp.env]
FAKE_MCP_ERA = "{era}"
FAKE_MCP_TOOLS = "{tools}"
FAKE_MCP_RESULT = "{result}"
FAKE_MCP_LOG = "{log}"
"#,
        script = fx("fake_mcp_server.sh"),
        tools = env.path("tools.json").display(),
        result = env.path("result.json").display(),
        log = env.path(&format!("{name}.log")).display(),
    )
}

fn calls(log: &[String]) -> Vec<Value> {
    log.iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|m| m["method"] == "tools/call")
        .collect()
}

fn methods(log: &[String]) -> Vec<String> {
    log.iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|m| m["method"].as_str().unwrap_or("").to_string())
        .collect()
}

#[derive(serde::Deserialize)]
struct Recipes {
    sync: Vec<Recipe>,
}

fn parse_err(toml_text: &str) -> String {
    match toml::from_str::<Recipes>(toml_text) {
        Ok(_) => panic!("expected an error for:\n{toml_text}"),
        Err(e) => e.to_string(),
    }
}

const VALID_CLI: &str = r#"
[[sync]]
name = "feishu-calendar"
adapter = "cli"
source = "feishu"
kind = "feishu_calendar_events"
interval = "30m"
overlap = "2h"
[sync.cli]
program = "lark-cli"
args = ["calendar", "+agenda", "--start", "{{since}}", "--end", "{{until}}"]
"#;

#[test]
fn recipes_are_validated_when_the_config_is_parsed() {
    let ok: Recipes = toml::from_str(VALID_CLI).unwrap();
    let r = &ok.sync[0];
    assert_eq!((r.name.as_str(), r.interval_secs, r.overlap_secs, r.lookback_secs), ("feishu-calendar", 1800, 7200, 604_800));
    assert_eq!((r.timeout_secs, r.max_bytes), (120, 16 * 1024 * 1024));

    let cases: &[(&str, &str, &str)] = &[
        ("adapter = \"cli\"", "adapter = \"http\"", "sync recipe 'feishu-calendar': adapter must be \"mcp\" or \"cli\", not \"http\""),
        ("interval = \"30m\"", "interval = \"30x\"", "interval \"30x\" is not a duration such as 30s, 15m, 1h, 1d or 1w"),
        ("interval = \"30m\"", "interval = \"30s\"", "interval must be at least 1m"),
        ("{{until}}", "{{end}}", "cli.args[5]: unknown placeholder {{end}}"),
        ("\"--start\"", "\"${secret:gh}\"", "cli.args[2]: secrets can only be used in headers and environment variables"),
        ("name = \"feishu-calendar\"", "name = \"Feishu Calendar\"", "name \"Feishu Calendar\" must be 1–64 characters"),
        ("kind = \"feishu_calendar_events\"", "kind = \"feishu-calendar\"", "kind \"feishu-calendar\" must be 1–64 characters of a-z, 0-9 and '_'"),
        ("overlap = \"2h\"", "overlap = \"2h\"\nfrequency = \"daily\"", "unknown field `frequency`"),
        ("[sync.cli]", "[sync.mcp]\ntool = \"x\"\n[sync.cli]", "adapter \"cli\" needs a [sync.cli] table and no [sync.mcp] table"),
        ("overlap = \"2h\"", "overlap = \"2h\"\ncursor_format = \"%Q\"", "cursor_format \"%Q\" is not a valid strftime format"),
    ];
    for (from, to, expected) in cases {
        let err = parse_err(&VALID_CLI.replacen(from, to, 1));
        assert!(err.contains(expected), "expected «{expected}» in:\n{err}");
    }

    let mcp = |table: &str| format!("[[sync]]\nname = \"gh\"\nadapter = \"mcp\"\nsource = \"github\"\nkind = \"mcp_response\"\ninterval = \"1h\"\n[sync.mcp]\ntool = \"search_commits\"\n{table}");
    for (table, expected) in [
        ("url = \"https://example.test/mcp\"\ncommand = \"srv\"", "set exactly one of mcp.url (streamable HTTP) and mcp.command (stdio)"),
        ("", "set exactly one of mcp.url"),
        ("url = \"http://mcp.example.test/\"", "plain http:// for a non-loopback host"),
        ("url = \"https://example.test/mcp\"\n[sync.mcp.headers]\nAuthorization = \"Bearer ghp_literal\"", "mcp.headers.Authorization looks like a credential"),
        ("command = \"srv\"\n[sync.mcp.env]\nGITHUB_PERSONAL_ACCESS_TOKEN = \"ghp_literal\"", "mcp.env.GITHUB_PERSONAL_ACCESS_TOKEN looks like a credential"),
        ("command = \"srv\"\nbearer_secret = \"gh\"", "only apply to an HTTP server"),
        ("url = \"https://example.test/mcp\"\n[sync.mcp.arguments]\nquery = \"{{since\"", "a placeholder is missing its closing }}"),
        ("url = \"https://example.test/mcp\"\n[sync.mcp.headers]\nMcp-Session-Id = \"x\"", "set by the MCP client"),
    ] {
        let err = parse_err(&mcp(table));
        assert!(err.contains(expected), "expected «{expected}» in:\n{err}");
    }
    // Secrets referenced properly are fine, and the recipe names them.
    let ok: Recipes = toml::from_str(&mcp(
        "url = \"https://example.test/mcp\"\nbearer_secret = \"github\"\n[sync.mcp.headers]\nX-MCP-Toolsets = \"repos\"",
    ))
    .unwrap();
    assert_eq!(ok.sync[0].secret_names(), vec!["github"]);

    // Names are unique across recipes.
    let mut config = Config::new("server", "local:/tmp/x");
    config.sync = vec![r.clone(), r.clone()];
    let err = runner::check_config(&config).unwrap_err().to_string();
    assert_eq!(err, "two [[sync]] recipes are named 'feishu-calendar'; names must be unique");

    // A recipe survives saving and loading the config unchanged.
    let env = Env::new();
    let mut config = Config::load(&env.home).unwrap();
    config.sync = vec![r.clone()];
    config.save(&env.home).unwrap();
    assert_eq!(Config::load(&env.home).unwrap().sync, vec![r.clone()]);
}

#[test]
fn cursor_advances_with_overlap_and_identical_responses_are_stored_once() {
    let env = Env::new();
    env.put("calendar.json", "calendar.json");
    let app = env.open(&format!(
        r#"
[[sync]]
name = "calendar"
adapter = "cli"
source = "feishu"
kind = "cli_response"
interval = "1h"
overlap = "1h"
lookback = "1d"
cursor_format = "%Y-%m-%dT%H:%M:%S%:z"
utc_offset = "+08:00"
[sync.cli]
program = "sh"
args = ["{script}", "--start", "{{{{since}}}}", "--end", "{{{{until}}}}", "--day", "{{{{since_date}}}}", "{{{{since_unix}}}}"]
[sync.cli.env]
FAKE_CLI_OUTPUT = "{out}"
FAKE_CLI_LOG = "{log}"
[sync.items]
subtype = "calendar_event"
category = "work"
key_field = "id"
title_field = "summary"
text_field = "description"
time_field = "start"
end_field = "end"
updated_field = "updated"
"#,
        script = fx("fake_cli.sh"),
        out = env.path("calendar.json").display(),
        log = env.path("cli.log").display(),
    ));
    let recipe = &app.config().sync[0];
    let st = state::load(app.conn(), "calendar").unwrap();
    assert!(sync::is_due(recipe, &st, t("2026-11-02T10:00:00Z")));

    let first = sync::run_recipe(&app, "calendar", t("2026-11-02T10:00:00Z")).unwrap();
    assert_eq!(first.status, "ok", "{:?}", first.error);
    assert_eq!(
        (first.window_from.as_deref(), first.window_to.as_deref()),
        (Some("2026-11-01T10:00:00Z"), Some("2026-11-02T10:00:00Z"))
    );
    assert_eq!(first.created, Some(true));
    assert_eq!(
        env.log("cli.log"),
        vec!["--start 2026-11-01T18:00:00+08:00 --end 2026-11-02T18:00:00+08:00 --day 2026-11-01 1793527200"]
    );
    let st = state::load(app.conn(), "calendar").unwrap();
    assert_eq!(st.cursor.as_deref(), Some("2026-11-02T10:00:00Z"));
    assert_eq!(st.last_success_at.as_deref(), Some("2026-11-02T10:00:00Z"));
    assert!(!sync::is_due(recipe, &st, t("2026-11-02T10:59:59Z")));
    assert!(sync::is_due(recipe, &st, t("2026-11-02T11:00:00Z")));
    // Nothing is due half an hour later.
    assert!(sync::run_due(&app, t("2026-11-02T10:30:00Z")).unwrap().is_empty());

    // Two hours later the window starts one hour (the overlap) before the cursor.
    let second = sync::run_due(&app, t("2026-11-02T12:00:00Z")).unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(
        (second[0].window_from.as_deref(), second[0].window_to.as_deref()),
        (Some("2026-11-02T09:00:00Z"), Some("2026-11-02T12:00:00Z"))
    );
    assert_eq!(env.log("cli.log")[1], "--start 2026-11-02T17:00:00+08:00 --end 2026-11-02T20:00:00+08:00 --day 2026-11-02 1793610000");
    // Same output: the vault keeps one object; the run history has both.
    assert_eq!(second[0].created, Some(false));
    assert_eq!(second[0].raw_id, first.raw_id);
    assert_eq!(count(&app, "SELECT count(*) FROM raw_objects"), 1);
    assert_eq!(count(&app, "SELECT count(*) FROM sync_runs WHERE recipe = 'calendar' AND status = 'ok'"), 2);
    let origin: String = app.conn().query_row("SELECT origin_ref FROM raw_objects", [], |r| r.get(0)).unwrap();
    assert_eq!(origin, "sync:calendar:2026-11-01T10:00:00Z..2026-11-02T10:00:00Z");

    // The generic tier: one item per record, fields picked by the recipe.
    let items = rows(
        &app,
        "SELECT i.part, v.type, v.subtype, v.category, v.title, v.text, v.event_start, v.event_end, v.tz_offset,
                v.local_date, v.source_updated_at, v.source, v.raw_kind, json_extract(v.data, '$.recipe')
         FROM v_items v JOIN items i ON i.id = v.id ORDER BY i.part",
    );
    assert_eq!(
        items,
        vec![
            vec![json!("key:evt-001"), json!("record"), json!("calendar_event"), json!("work"), json!("Weekly planning"),
                 json!("Synthetic meeting for tests."), json!("2026-11-02T01:00:00Z"), json!("2026-11-02T02:00:00Z"),
                 json!(28_800), json!("2026-11-02"), json!("2026-10-30T12:00:00Z"), json!("feishu"), json!("cli_response"), json!("calendar")],
            vec![json!("key:evt-002"), json!("record"), json!("calendar_event"), json!("work"), json!("Design review"),
                 Value::Null, json!("2026-11-02T23:30:00Z"), json!("2026-11-03T00:30:00Z"),
                 json!(28_800), json!("2026-11-03"), Value::Null, json!("feishu"), json!("cli_response"), json!("calendar")],
            vec![json!("key:evt-003"), json!("record"), json!("calendar_event"), json!("work"), Value::Null,
                 Value::Null, json!("2026-11-03T16:00:00Z"), Value::Null,
                 json!(28_800), json!("2026-11-04"), Value::Null, json!("feishu"), json!("cli_response"), json!("calendar")],
        ]
    );
    let received: String = app.conn().query_row("SELECT received_at FROM raw_objects", [], |r| r.get(0)).unwrap();
    assert_eq!(count(&app, &format!("SELECT count(*) FROM items WHERE received_at = '{received}'")), 3);
}

#[test]
fn cli_failures_are_recorded_and_leave_the_cursor_alone() {
    let env = Env::new();
    env.put("calendar.json", "calendar.json");
    let recipe = |name: &str, program: &str, args: &str, extra: &str| {
        format!(
            "[[sync]]\nname = \"{name}\"\nadapter = \"cli\"\nsource = \"test\"\nkind = \"cli_response\"\ninterval = \"1h\"\n{extra}\n\
             [sync.cli]\nprogram = \"{program}\"\nargs = [{args}]\n[sync.cli.env]\nFAKE_CLI_OUTPUT = \"{out}\"\nFAKE_CLI_FAIL = \"{fail}\"\n",
            out = env.path("calendar.json").display(),
            fail = if name == "failing" { "1" } else { "0" },
        )
    };
    let script = format!("\"{}\"", fx("fake_cli.sh"));
    let app = env.open(&[
        recipe("failing", "sh", &script, ""),
        recipe("slow", "sleep", "\"5\"", "timeout = \"1s\""),
        recipe("large", "sh", &script, "max_bytes = 64"),
        recipe("missing", "/nonexistent/mirrorself-test-program", "", ""),
    ]
    .join("\n"));
    let now = t("2026-11-02T10:00:00Z");
    let outcomes = sync::run_due(&app, now).unwrap();
    let got: Vec<(&str, &str)> = outcomes.iter().map(|o| (o.recipe.as_str(), o.status.as_str())).collect();
    assert_eq!(got, vec![("failing", "error"), ("slow", "error"), ("large", "error"), ("missing", "error")]);
    assert_eq!(
        outcomes[0].error.as_deref(),
        Some("sh exited with exit status: 3: calendar backend unavailable (synthetic failure)")
    );
    assert_eq!(outcomes[1].error.as_deref(), Some("sleep did not finish within 1 s"));
    assert_eq!(
        outcomes[2].error.as_deref(),
        Some("sh wrote more than 64 bytes; raise max_bytes in the recipe if that is expected")
    );
    assert!(outcomes[3].error.as_deref().unwrap().starts_with("starting /nonexistent/mirrorself-test-program"));
    for o in &outcomes {
        let st = state::load(app.conn(), &o.recipe).unwrap();
        assert_eq!(st.cursor, None);
        assert_eq!(st.last_error, o.error);
        assert!(!st.paused);
        // A failed run waits for the next interval before it is retried.
        assert!(!sync::is_due(&app.config().sync[0], &st, t("2026-11-02T10:30:00Z")));
    }
    assert_eq!(count(&app, "SELECT count(*) FROM raw_objects"), 0);
    assert_eq!(count(&app, "SELECT count(*) FROM sync_runs WHERE status = 'error'"), 4);
}

#[test]
fn stdio_mcp_round_trip_in_both_protocol_eras() {
    let env = Env::new();
    env.put("tools.json", "github/tools_list.json");
    env.put("result.json", "github/search_commits.json");
    let args = "[sync.mcp.arguments]\nquery = \"author:octo-dev author-date:{{since}}..{{until}}\"\nsort = \"author-date\"\nperPage = 100\n";
    let mut cfg = mcp_recipe(&env, "modern", "modern", "search_commits", "");
    cfg.push_str(args);
    cfg.push_str(&mcp_recipe(&env, "legacy", "legacy", "search_commits", ""));
    cfg.push_str(args);
    let app = env.open(&cfg);
    let now = t("2026-11-02T10:00:00Z");

    let modern = sync::run_recipe(&app, "modern", now).unwrap();
    assert_eq!((modern.status.as_str(), modern.protocol.as_deref()), ("ok", Some("2026-07-28")), "{:?}", modern.error);
    let log = env.log("modern.log");
    assert_eq!(methods(&log), vec!["server/discover", "tools/list", "tools/call"]);
    let call = &calls(&log)[0];
    assert_eq!(call["params"]["name"], "search_commits");
    assert_eq!(
        call["params"]["arguments"],
        json!({ "query": "author:octo-dev author-date:2026-10-26..2026-11-02", "sort": "author-date", "perPage": 100 })
    );
    assert_eq!(call["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"], "2026-07-28");
    assert_eq!(call["params"]["_meta"]["io.modelcontextprotocol/clientInfo"]["name"], "mirrorself-sync");

    let legacy = sync::run_recipe(&app, "legacy", now).unwrap();
    assert_eq!((legacy.status.as_str(), legacy.protocol.as_deref()), ("ok", Some("2025-06-18")), "{:?}", legacy.error);
    let log = env.log("legacy.log");
    assert_eq!(
        methods(&log),
        vec!["server/discover", "initialize", "notifications/initialized", "tools/list", "tools/call"]
    );
    let init: Value = serde_json::from_str(&log[1]).unwrap();
    assert_eq!(init["params"]["protocolVersion"], "2025-11-25");
    assert!(calls(&log)[0]["params"].get("_meta").is_none());

    // Both recipes got the same bytes: stored once, labelled by the first.
    assert_eq!(modern.created, Some(true));
    assert_eq!(legacy.created, Some(false));
    assert_eq!(legacy.raw_id, modern.raw_id);
    let stored = app.raw_original(modern.raw_id.as_deref().unwrap()).unwrap();
    assert_eq!(stored.0.kind, "mcp_response");
    assert_eq!(stored.0.mime, "application/json");
    let expected: Value = serde_json::from_str(&std::fs::read_to_string(fx("github/search_commits.json")).unwrap()).unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&stored.1).unwrap(), expected);

    // A tool error is a failed run; nothing is stored.
    env.put("result.json", "github/tool_error.json");
    let failed = sync::run_recipe(&app, "modern", t("2026-11-02T12:00:00Z")).unwrap();
    assert_eq!(failed.status, "error");
    assert_eq!(
        failed.error.as_deref(),
        Some("tool 'search_commits' reported an error: failed to search commits: synthetic rate limit")
    );
    assert_eq!(state::load(app.conn(), "modern").unwrap().cursor.as_deref(), Some("2026-11-02T10:00:00Z"));
}

#[test]
fn tools_not_marked_read_only_are_refused_and_never_called() {
    let env = Env::new();
    env.put("tools.json", "github/tools_list.json");
    env.put("result.json", "github/search_commits.json");
    let app = env.open(
        &[
            mcp_recipe(&env, "writes", "modern", "create_pull_request", ""),
            mcp_recipe(&env, "unannotated", "legacy", "merge_pull_request", ""),
            mcp_recipe(&env, "absent", "modern", "delete_everything", ""),
        ]
        .join("\n"),
    );
    let now = t("2026-11-02T10:00:00Z");
    let first: Vec<_> = ["writes", "unannotated"].iter().map(|n| sync::run_recipe(&app, n, now).unwrap()).collect();
    for (name, o) in ["writes", "unannotated"].into_iter().zip(first) {
        assert_eq!(o.status, "refused", "{name}");
        assert!(o.error.as_deref().unwrap().contains("is not annotated read-only"), "{:?}", o.error);
        assert!(calls(&env.log(&format!("{name}.log"))).is_empty(), "{name} was called");
        let st = state::load(app.conn(), name).unwrap();
        assert!(st.paused);
        assert_eq!(st.cursor, None);
        // While paused the server is not even started.
        let before = env.log(&format!("{name}.log")).len();
        let again = sync::run_recipe(&app, name, t("2026-11-02T12:00:00Z")).unwrap();
        assert_eq!(again.status, "paused");
        assert_eq!(env.log(&format!("{name}.log")).len(), before);
        assert!(sync::run_due(&app, t("2026-11-03T12:00:00Z")).unwrap().iter().all(|o| o.recipe != name));
    }
    let absent = sync::run_recipe(&app, "absent", now).unwrap();
    assert_eq!(absent.status, "error");
    assert_eq!(absent.error.as_deref(), Some("the MCP server does not list a tool named 'delete_everything'"));
    assert!(calls(&env.log("absent.log")).is_empty());
    assert_eq!(count(&app, "SELECT count(*) FROM raw_objects"), 0);
}

#[test]
fn a_changed_input_schema_pauses_the_recipe_until_resumed() {
    let env = Env::new();
    env.put("tools.json", "github/tools_list.json");
    env.put("result.json", "github/search_commits.json");
    let app = env.open(&mcp_recipe(&env, "commits", "modern", "search_commits", ""));

    let first = sync::run_recipe(&app, "commits", t("2026-11-02T10:00:00Z")).unwrap();
    assert_eq!(first.status, "ok");
    let pinned = state::load(app.conn(), "commits").unwrap();
    assert_eq!(pinned.tool.as_deref(), Some("search_commits"));
    assert_eq!(pinned.pinned_at.as_deref(), Some("2026-11-02T10:00:00Z"));
    let hash = pinned.schema_sha256.clone().unwrap();
    assert_eq!(hash.len(), 64);

    // Same schema, keys in another order: still the same pin.
    let tools: Value = serde_json::from_str(&std::fs::read_to_string(env.path("tools.json")).unwrap()).unwrap();
    let mut reordered = tools.clone();
    let schema = reordered["tools"][2]["inputSchema"].as_object_mut().unwrap();
    let props = schema.remove("properties").unwrap();
    schema.insert("properties".into(), props);
    std::fs::write(env.path("tools.json"), serde_json::to_string_pretty(&reordered).unwrap()).unwrap();
    assert_eq!(sync::run_recipe(&app, "commits", t("2026-11-02T11:00:00Z")).unwrap().status, "ok");

    env.put("tools.json", "github/tools_list_changed.json");
    let calls_before = calls(&env.log("commits.log")).len();
    let changed = sync::run_recipe(&app, "commits", t("2026-11-02T12:00:00Z")).unwrap();
    assert_eq!(changed.status, "paused");
    let msg = changed.error.unwrap();
    assert!(msg.starts_with("schema changed: the input schema of tool 'search_commits' is no longer the one pinned on 2026-11-02T10:00:00Z"), "{msg}");
    assert!(msg.ends_with("run `mirrorself sync resume commits`"), "{msg}");
    assert_eq!(calls(&env.log("commits.log")).len(), calls_before, "the tool was called after its schema changed");
    let st = state::load(app.conn(), "commits").unwrap();
    assert!(st.paused);
    assert_eq!(st.schema_sha256.as_deref(), Some(hash.as_str()));
    assert_eq!(st.cursor.as_deref(), Some("2026-11-02T11:00:00Z"));
    assert_eq!(count(&app, "SELECT count(*) FROM sync_runs WHERE status = 'paused'"), 1);

    // Resuming accepts the schema the server shows now.
    let resumed = sync::resume(&app, "commits").unwrap();
    assert!(!resumed.paused && resumed.schema_sha256.is_none());
    assert!(sync::resume(&app, "commits").is_err(), "resuming twice");
    let after = sync::run_recipe(&app, "commits", t("2026-11-02T13:00:00Z")).unwrap();
    assert_eq!(after.status, "ok", "{:?}", after.error);
    let st = state::load(app.conn(), "commits").unwrap();
    assert_ne!(st.schema_sha256.as_deref(), Some(hash.as_str()));
    assert_eq!(st.pinned_at.as_deref(), Some("2026-11-02T13:00:00Z"));
}

#[test]
fn secrets_are_sealed_outside_the_config_and_reach_only_the_server() {
    let env = Env::new();
    env.put("tools.json", "github/tools_list.json");
    env.put("result.json", "github/search_commits.json");
    let token = "synthetic-token-not-real";
    let app = env.open(&format!(
        "{}FAKE_MCP_TOKEN = \"${{secret:fake-gh}}\"\n",
        mcp_recipe(&env, "commits", "modern", "search_commits", "")
    ));

    // Not set yet: the run fails and says how to set it.
    let o = sync::run_recipe(&app, "commits", t("2026-11-02T10:00:00Z")).unwrap();
    assert_eq!(o.status, "error");
    assert_eq!(
        o.error.as_deref(),
        Some("secret 'fake-gh' is not set; run `mirrorself sync secret set fake-gh` and paste it on stdin")
    );
    assert!(env.log("commits.log").is_empty(), "the server started without its secret");
    let listed = sync::list(&app, t("2026-11-02T10:00:00Z")).unwrap();
    assert_eq!(listed["recipes"][0]["missing_secrets"], json!(["fake-gh"]));

    let store = SecretStore::open(app.home(), app.vault().subkeys());
    store.set("fake-gh", token).unwrap();
    assert_eq!(store.get("fake-gh").unwrap().expose(), token);
    assert_eq!(store.names().unwrap(), vec!["fake-gh"]);
    assert_eq!(format!("{:?}", store.get("fake-gh").unwrap()), "Secret(..)");

    let o = sync::run_recipe(&app, "commits", t("2026-11-02T11:00:00Z")).unwrap();
    assert_eq!(o.status, "ok", "{:?}", o.error);
    assert_eq!(env.log("commits.log")[0], "token:ok");

    // The value is nowhere in clear text: not in the config, the secrets
    // file, the run history or the state.
    for path in [env.home.config_path(), SecretStore::path_for(&env.home)] {
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains(token), "{} holds the secret", path.display());
    }
    assert!(std::fs::read_to_string(env.home.config_path()).unwrap().contains("${secret:fake-gh}"));
    assert_eq!(count(&app, &format!("SELECT count(*) FROM sync_runs WHERE error LIKE '%{token}%'")), 0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(SecretStore::path_for(&env.home)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // Another vault key cannot open them.
    let other = Env::new();
    let other_app = App::open(other.home.clone()).unwrap();
    std::fs::copy(SecretStore::path_for(&env.home), SecretStore::path_for(&other.home)).unwrap();
    let err = SecretStore::open(other_app.home(), other_app.vault().subkeys()).get("fake-gh").unwrap_err();
    assert!(err.to_string().contains("cannot be decrypted"), "{err}");

    assert!(store.remove("fake-gh").unwrap());
    assert!(store.names().unwrap().is_empty());
    assert!(store.set("Bad Name", "x").is_err());
    assert!(store.set("ok", "two\nlines").is_err());
}

fn github_recipes(env: &Env) -> String {
    let login = "[sync.github]\nlogin = \"octo-dev\"\n";
    let commits = mcp_recipe(
        env,
        "github-commits",
        "modern",
        "search_commits",
        "",
    ) + "[sync.mcp.arguments]\nquery = \"author:octo-dev author-date:{{since}}..{{until}}\"\n\
         [sync.items]\nsubtype = \"github_commit\"\ncategory = \"work\"\nrecords = \"items\"\nkey_field = \"sha\"\n\
         title_field = \"commit.message\"\ntime_field = \"commit.author.date\"\n" + login;
    let prs = mcp_recipe(env, "github-prs", "modern", "search_pull_requests", "")
        + "[sync.mcp.arguments]\nquery = \"author:octo-dev updated:{{since}}..{{until}}\"\n\
           [sync.items]\nsubtype = \"github_pull_request\"\ncategory = \"work\"\nrecords = \"items\"\nkey_field = \"html_url\"\n\
           time_field = \"created_at\"\nupdated_field = \"updated_at\"\n"
        + login;
    commits + &prs
}

#[test]
fn github_responses_map_to_commit_and_pull_request_events() {
    let env = Env::new();
    env.put("tools.json", "github/tools_list.json");
    env.put("result.json", "github/search_commits.json");
    let app = env.open(&github_recipes(&env));

    let c = sync::run_recipe(&app, "github-commits", t("2026-11-03T00:00:00Z")).unwrap();
    assert_eq!(c.status, "ok", "{:?}", c.error);
    let p = c.process.as_ref().unwrap();
    assert!(p.errors().next().is_none(), "{p:?}");
    env.put("result.json", "github/search_pull_requests.json");
    let p = sync::run_recipe(&app, "github-prs", t("2026-11-03T00:00:00Z")).unwrap();
    assert_eq!(p.status, "ok", "{:?}", p.error);

    let events = rows(
        &app,
        "SELECT type, category_id, start_ts, local_date, tz_offset, title, json_extract(data, '$.repo'),
                json_extract(data, '$.state'), json_extract(data, '$.author_login')
         FROM events WHERE type IN ('commit', 'pull_request') ORDER BY start_ts",
    );
    assert_eq!(
        events,
        vec![
            vec![json!("pull_request"), json!("work"), json!("2026-11-01T08:00:00Z"), json!("2026-11-01"), json!(28_800),
                 json!("octo-dev/mirror-demo#12 Scheduled sync"), json!("octo-dev/mirror-demo"), json!("merged"), json!("octo-dev")],
            vec![json!("commit"), json!("work"), json!("2026-11-02T01:15:00Z"), json!("2026-11-02"), json!(28_800),
                 json!("octo-dev/mirror-demo: Add sync recipe parser"), json!("octo-dev/mirror-demo"), Value::Null, json!("octo-dev")],
            vec![json!("pull_request"), json!("work"), json!("2026-11-02T06:00:00Z"), json!("2026-11-02"), json!(28_800),
                 json!("octo-dev/mirror-demo#14 Generic items tier"), json!("octo-dev/mirror-demo"), json!("open"), json!("octo-dev")],
            vec![json!("commit"), json!("work"), json!("2026-11-02T17:40:00Z"), json!("2026-11-03"), json!(28_800),
                 json!("octo-dev/notes: Fix typo in weekly notes"), json!("octo-dev/notes"), Value::Null, json!("octo-dev")],
        ]
    );
    let body: String = app
        .conn()
        .query_row("SELECT body FROM events WHERE title LIKE '%Add sync recipe parser'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(body, "Add sync recipe parser\n\nValidates [[sync]] tables on load.");

    // Repositories become entities linked from their events.
    assert_eq!(
        rows(&app, "SELECT kind, name FROM v_entities WHERE kind = 'repository' ORDER BY name"),
        vec![vec![json!("repository"), json!("octo-dev/mirror-demo")], vec![json!("repository"), json!("octo-dev/notes")]]
    );
    assert_eq!(count(&app, "SELECT count(*) FROM links WHERE relation = 'in_repository'"), 4);

    // The generic tier keeps every record of each response as an item.
    let items = rows(
        &app,
        "SELECT subtype, category, title, event_start, local_date, source_updated_at FROM v_items ORDER BY subtype, event_start",
    );
    assert_eq!(
        items,
        vec![
            vec![json!("github_commit"), json!("work"), json!("Add sync recipe parser"), json!("2026-11-02T01:15:00Z"), json!("2026-11-02"), Value::Null],
            vec![json!("github_commit"), json!("work"), json!("Bump synthetic dependency"), json!("2026-11-02T03:00:00Z"), json!("2026-11-02"), Value::Null],
            vec![json!("github_commit"), json!("work"), json!("Fix typo in weekly notes"), json!("2026-11-02T17:40:00Z"), json!("2026-11-03"), Value::Null],
            vec![json!("github_pull_request"), json!("work"), json!("Scheduled sync"), json!("2026-11-01T08:00:00Z"), json!("2026-11-01"), json!("2026-11-02T09:30:00Z")],
            vec![json!("github_pull_request"), json!("work"), json!("Generic items tier"), json!("2026-11-02T06:00:00Z"), json!("2026-11-02"), json!("2026-11-02T06:05:00Z")],
            vec![json!("github_pull_request"), json!("work"), json!("Unrelated change"), json!("2026-11-02T07:00:00Z"), json!("2026-11-02"), json!("2026-11-02T07:00:00Z")],
        ]
    );

    // A later, overlapping window reports #14 merged: the event and the item update in place.
    env.put("result.json", "github/search_pull_requests_later.json");
    let later = sync::run_recipe(&app, "github-prs", t("2026-11-03T03:00:00Z")).unwrap();
    assert_eq!((later.status.as_str(), later.created), ("ok", Some(true)));
    assert_eq!(later.window_from.as_deref(), Some("2026-11-02T00:00:00Z"));
    assert_eq!(count(&app, "SELECT count(*) FROM events WHERE type = 'pull_request'"), 2);
    let state14: String = app
        .conn()
        .query_row("SELECT json_extract(data, '$.state') FROM events WHERE title LIKE '%#14 %'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(state14, "merged");
    assert_eq!(count(&app, "SELECT count(*) FROM items WHERE subtype = 'github_pull_request'"), 3);
    let item14: (String, String) = app
        .conn()
        .query_row(
            "SELECT raw_id, source_updated_at FROM items WHERE title = 'Generic items tier'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(item14, (later.raw_id.clone().unwrap(), "2026-11-03T02:00:00Z".to_string()));

    // Searchable like any other event.
    let hits = app
        .query()
        .search(&mirrorself_core::query::SearchQuery {
            query: "typo".into(),
            from: None,
            to: None,
            kinds: vec![],
            limit: Some(5),
        })
        .unwrap();
    assert!(hits.to_string().contains("Fix typo in weekly notes"), "{hits}");
}

#[test]
fn rebuild_reproduces_derived_rows_and_keeps_sync_state() {
    let env = Env::new();
    env.put("tools.json", "github/tools_list.json");
    env.put("result.json", "github/search_commits.json");
    let app = env.open(&github_recipes(&env));
    sync::run_recipe(&app, "github-commits", t("2026-11-03T00:00:00Z")).unwrap();
    for (fixture, now) in [
        ("github/search_pull_requests.json", "2026-11-03T00:00:00Z"),
        ("github/search_pull_requests_later.json", "2026-11-03T03:00:00Z"),
    ] {
        env.put("result.json", fixture);
        assert_eq!(sync::run_recipe(&app, "github-prs", t(now)).unwrap().status, "ok");
    }

    let snapshot = |app: &App| {
        json!({
            "events": rows(app, "SELECT id, fp, type, category_id, start_ts, end_ts, tz_offset, local_date, title, body, data, raw_id, run_id FROM events ORDER BY id"),
            "items": rows(app, "SELECT * FROM items ORDER BY id"),
            "entities": rows(app, "SELECT id, kind, name FROM entities ORDER BY id"),
            "links": rows(app, "SELECT from_type, from_id, to_id, relation FROM links ORDER BY from_id, to_id"),
        })
    };
    let sync_tables = |app: &App| {
        json!({
            "state": rows(app, "SELECT * FROM sync_state ORDER BY recipe"),
            "runs": rows(app, "SELECT * FROM sync_runs ORDER BY id"),
        })
    };
    let before = snapshot(&app);
    let before_sync = sync_tables(&app);
    assert_eq!(before["events"].as_array().unwrap().len(), 4);
    assert_eq!(before["items"].as_array().unwrap().len(), 6);
    assert_eq!(before_sync["runs"].as_array().unwrap().len(), 3);

    let report = app.process(true).unwrap();
    assert!(report.errors().next().is_none());
    assert_eq!(snapshot(&app), before);
    assert_eq!(sync_tables(&app), before_sync);

    // Deleting the index: the vault alone rebuilds the same derived rows.
    drop(app);
    std::fs::remove_file(env.home.index_path()).unwrap();
    for ext in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", env.home.index_path().display()));
    }
    let app = App::open(env.home.clone()).unwrap();
    app.process(false).unwrap();
    assert_eq!(snapshot(&app), before);
}

// --------------------------------------------------------------- HTTP

struct Seen {
    headers: Vec<(String, String)>,
    body: Value,
}

/// A synthetic Streamable HTTP MCP server on an ephemeral loopback port.
/// `legacy`: answers `server/discover` with 400, hands out a session on
/// `initialize`, and replies over SSE. Otherwise: the stateless revision,
/// JSON replies.
fn serve_http(legacy: bool, tools: Value, result: Value) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}/mcp", listener.local_addr().unwrap().port());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut headers = Vec::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some((k, v)) = line.trim_end().split_once(':') {
                    headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                }
            }
            let len: usize = header(&headers, "content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let method = body["method"].as_str().unwrap_or("").to_string();
            let id = body["id"].clone();
            log.lock().unwrap().push(Seen { headers: headers.clone(), body: body.clone() });

            let ok = |r: &Value| json!({ "jsonrpc": "2.0", "id": id, "result": r });
            let (status, ctype, extra, payload): (&str, &str, &str, String) = if legacy {
                let session_ok = header(&headers, "mcp-session-id") == Some("sess-synthetic-1");
                match method.as_str() {
                    "server/discover" => ("400 Bad Request", "text/plain", "", "Bad Request: No valid session ID provided".into()),
                    "initialize" => (
                        "200 OK",
                        "text/event-stream",
                        "Mcp-Session-Id: sess-synthetic-1\r\n",
                        sse(&ok(&json!({ "protocolVersion": "2025-03-26", "capabilities": { "tools": {} }, "serverInfo": { "name": "fake-http", "version": "0" } }))),
                    ),
                    "notifications/initialized" if session_ok => ("202 Accepted", "text/plain", "", String::new()),
                    _ if !session_ok => ("404 Not Found", "text/plain", "", "Session not found".into()),
                    "tools/list" => ("200 OK", "text/event-stream", "", sse(&ok(&tools))),
                    "tools/call" => ("200 OK", "text/event-stream", "", sse(&ok(&result))),
                    _ => ("404 Not Found", "text/plain", "", String::new()),
                }
            } else if header(&headers, "mcp-protocol-version") != Some("2026-07-28")
                || body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"] != "2026-07-28"
            {
                let e = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32022, "message": "Unsupported protocol version", "data": { "supported": ["2026-07-28"] } } });
                ("400 Bad Request", "application/json", "", e.to_string())
            } else {
                match method.as_str() {
                    "server/discover" => ("200 OK", "application/json", "", ok(&json!({ "supportedVersions": ["2026-07-28"] })).to_string()),
                    "tools/list" => ("200 OK", "application/json", "", ok(&tools).to_string()),
                    "tools/call" => ("200 OK", "application/json", "", ok(&result).to_string()),
                    _ => ("404 Not Found", "application/json", "", String::new()),
                }
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (url, seen)
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

fn sse(msg: &Value) -> String {
    format!("event: message\ndata: {}\n\n", msg)
}

fn read_fixture(name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(Path::new(&fx(name))).unwrap()).unwrap()
}

#[test]
fn streamable_http_round_trip_with_a_bearer_token_in_both_eras() {
    let env = Env::new();
    let tools = read_fixture("github/tools_list.json");
    let result = read_fixture("github/search_commits.json");
    let (modern_url, modern_seen) = serve_http(false, tools.clone(), result.clone());
    let (legacy_url, legacy_seen) = serve_http(true, tools, result);
    let recipe = |name: &str, url: &str| {
        format!(
            "[[sync]]\nname = \"{name}\"\nadapter = \"mcp\"\nsource = \"github\"\nkind = \"mcp_response\"\ninterval = \"1h\"\n\
             timeout = \"20s\"\n[sync.mcp]\nurl = \"{url}\"\nbearer_secret = \"gh\"\ntool = \"search_commits\"\n\
             [sync.mcp.headers]\nX-MCP-Toolsets = \"repos\"\n[sync.mcp.arguments]\nquery = \"author:octo-dev\"\n"
        )
    };
    let app = env.open(&(recipe("http-modern", &modern_url) + &recipe("http-legacy", &legacy_url)));
    SecretStore::open(app.home(), app.vault().subkeys()).set("gh", "synthetic-token-not-real").unwrap();

    let m = sync::run_recipe(&app, "http-modern", t("2026-11-02T10:00:00Z")).unwrap();
    assert_eq!((m.status.as_str(), m.protocol.as_deref()), ("ok", Some("2026-07-28")), "{:?}", m.error);
    let seen = modern_seen.lock().unwrap();
    let methods: Vec<&str> = seen.iter().map(|s| s.body["method"].as_str().unwrap()).collect();
    assert_eq!(methods, vec!["server/discover", "tools/list", "tools/call"]);
    for s in seen.iter() {
        assert_eq!(header(&s.headers, "authorization"), Some("Bearer synthetic-token-not-real"));
        assert_eq!(header(&s.headers, "x-mcp-toolsets"), Some("repos"));
        assert_eq!(header(&s.headers, "accept"), Some("application/json, text/event-stream"));
        assert_eq!(header(&s.headers, "mcp-method"), s.body["method"].as_str());
        assert_eq!(header(&s.headers, "mcp-session-id"), None);
    }
    assert_eq!(header(&seen[2].headers, "mcp-name"), Some("search_commits"));
    drop(seen);

    let l = sync::run_recipe(&app, "http-legacy", t("2026-11-02T10:00:00Z")).unwrap();
    assert_eq!((l.status.as_str(), l.protocol.as_deref()), ("ok", Some("2025-03-26")), "{:?}", l.error);
    assert_eq!(l.raw_id, m.raw_id, "the same response is stored once");
    let seen = legacy_seen.lock().unwrap();
    let methods: Vec<&str> = seen.iter().map(|s| s.body["method"].as_str().unwrap()).collect();
    assert_eq!(methods, vec!["server/discover", "initialize", "notifications/initialized", "tools/list", "tools/call"]);
    assert_eq!(header(&seen[3].headers, "mcp-session-id"), Some("sess-synthetic-1"));
    assert_eq!(header(&seen[3].headers, "mcp-protocol-version"), Some("2025-03-26"));
    assert_eq!(header(&seen[1].headers, "mcp-session-id"), None);
}

#[test]
fn the_recipes_in_the_docs_parse() {
    let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/sync.md")).unwrap();
    let blocks: Vec<&str> = doc.split("```toml\n").skip(1).filter_map(|b| b.split("```").next()).collect();
    let github: Recipes = toml::from_str(blocks[0]).unwrap();
    let names: Vec<&str> = github.sync.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["github-commits", "github-prs"]);
    assert_eq!(github.sync[0].secret_names(), vec!["github"]);
    let feishu: Recipes = toml::from_str(blocks[2]).unwrap();
    assert_eq!(feishu.sync[0].def.kind, "feishu_calendar_events");
}
