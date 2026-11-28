//! The multi-source demo, end to end (docs/demo-plan.md): every source in
//! one home, and the two demo questions answered through the audited tools
//! an agent uses. All data is synthetic.

use chrono::{NaiveDate, Utc};
use mirrorself_core::config::Home;
use mirrorself_core::synth::{self, SynthOptions};
use mirrorself_core::{sync, App};
use mirrorself_server::tools;
use serde_json::{json, Value};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures");

fn fixture(path: &str) -> Vec<u8> {
    std::fs::read(format!("{FIXTURES}/{path}")).unwrap()
}

fn call(app: &App, tool: &str, args: Value) -> Value {
    tools::call(app, "agent", "demo-test", tool, &args).unwrap_or_else(|e| panic!("{tool}: {e:#}"))
}

fn sql(app: &App, query: &str) -> Vec<Value> {
    call(app, "run_sql", json!({ "sql": query }))["rows"].as_array().unwrap().clone()
}

/// A home holding a month of synthetic health data, two Alipay bills, the
/// Feishu calendar (one fetch through a scheduled-sync recipe, one imported)
/// and three uploaded files.
fn demo_home() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path().join("home"));
    App::init(&home, "server", None).unwrap();

    // A sync recipe whose "program" prints a stored calendar fetch, standing
    // in for lark-cli.
    let mut cfg = std::fs::read_to_string(home.config_path()).unwrap();
    cfg.push_str(&format!(
        r#"
[[sync]]
name = "feishu-calendar"
adapter = "cli"
source = "feishu"
kind = "feishu_calendar_events"
interval = "6h"
[sync.cli]
program = "/bin/cat"
args = ["{FIXTURES}/feishu/fetch-1_2026-10-12_2026-10-17.json"]
"#
    ));
    std::fs::write(home.config_path(), cfg).unwrap();
    let app = App::open(home).unwrap();

    let end = NaiveDate::from_ymd_opt(2026, 10, 31).unwrap();
    let (health, _) = synth::export_zip(&SynthOptions::days(45, end)).unwrap();
    app.import_bytes("apple-health", Some("export.zip".into()), &health, false).unwrap();

    let synced = sync::run_recipe(&app, "feishu-calendar", Utc::now()).unwrap();
    assert!(synced.is_ok(), "sync failed: {:?}", synced.error);

    // Everything else arrives the way an upload does: detected, not named.
    for path in [
        "feishu/fetch-2_2026-10-15_2026-10-20.json",
        "alipay/alipay_record_20261020_101500.csv",
        "alipay/alipay_record_20261105_093000.csv",
        "files/photo_gps_time.jpg",
        "files/itinerary.pdf",
        "files/note.md",
    ] {
        let name = path.rsplit('/').next().map(str::to_string);
        app.import_auto(name, &fixture(path), false).unwrap();
    }
    let report = app.process(false).unwrap();
    let errors: Vec<_> = report.errors().collect();
    assert!(errors.is_empty(), "processor errors: {errors:?}");
    (dir, app)
}

#[test]
fn question_1_meeting_load_next_to_sleep_and_resting_heart_rate() {
    let (_dir, app) = demo_home();

    // "In weeks with more meeting hours, how did my sleep and resting heart
    // rate differ?" The join an agent would run: one row per day with meetings.
    let days = sql(
        &app,
        "SELECT m.date, m.value AS meeting_minutes, n.value AS meetings,
                s.value AS sleep_minutes, h.value AS resting_hr
         FROM v_metrics_daily m
         LEFT JOIN v_metrics_daily n ON n.date = m.date AND n.code = 'ms:meeting_count' AND n.stat = 'sum'
         LEFT JOIN v_metrics_daily s ON s.date = m.date AND s.code = 'ms:sleep_asleep_total' AND s.stat = 'sum'
         LEFT JOIN v_metrics_daily h ON h.date = m.date AND h.code = 'ms:resting_hr' AND h.stat = 'mean'
         WHERE m.code = 'ms:meeting_minutes' AND m.stat = 'sum'
         ORDER BY m.date",
    );
    let meeting_minutes: Vec<(String, f64)> =
        days.iter().map(|r| (r[0].as_str().unwrap().to_string(), r[1].as_f64().unwrap())).collect();
    assert_eq!(
        meeting_minutes,
        vec![
            ("2026-10-12".to_string(), 90.0),
            ("2026-10-13".to_string(), 90.0),
            ("2026-10-14".to_string(), 60.0),
            ("2026-10-15".to_string(), 150.0),
            ("2026-10-16".to_string(), 150.0),
        ]
    );
    for day in &days {
        // Both health metrics exist for every meeting day, so the two
        // sources really are joined on the same local dates.
        let sleep = day[3].as_f64().unwrap_or_else(|| panic!("no sleep on {}", day[0]));
        let resting = day[4].as_f64().unwrap_or_else(|| panic!("no resting HR on {}", day[0]));
        assert!((180.0..=720.0).contains(&sleep), "sleep {sleep} on {}", day[0]);
        assert!((35.0..=110.0).contains(&resting), "resting HR {resting} on {}", day[0]);
    }

    // The same metric through the typed tool, with record citations.
    let series = call(&app, "get_metric_series", json!({ "metric": "ms:meeting_minutes", "from": "2026-10-12", "to": "2026-10-18" }));
    assert_eq!(series["metric"], "ms:meeting_minutes");
    assert_eq!(series["points"].as_array().map(Vec::len), Some(5), "{series}");

    // Meetings are timeline events with their people linked.
    let meetings = call(&app, "query_timeline", json!({ "from": "2026-10-12", "to": "2026-10-20", "types": ["meeting"], "limit": 100 }));
    assert_eq!(meetings["count"], 13);
    let entity_kinds: Vec<String> = sql(&app, "SELECT DISTINCT kind FROM v_entities ORDER BY kind")
        .iter()
        .map(|r| r[0].as_str().unwrap().to_string())
        .collect();
    assert_eq!(entity_kinds, vec!["activity_type", "device", "merchant", "person", "place"]);
    // The calendar is a source of records, not a measuring device.
    let calendar_devices = sql(&app, "SELECT count(*) FROM v_entities WHERE kind = 'device' AND name LIKE '%Feishu%'");
    assert_eq!(calendar_devices[0][0], 0);
}

#[test]
fn question_2_spending_and_places_on_given_days() {
    let (_dir, app) = demo_home();

    // "What did I spend on the days I was travelling, and where was I?"
    let spend = call(&app, "query_transactions", json!({ "counted_only": true, "limit": 1000 }));
    assert_eq!(spend["summary"]["matched"], 30);
    assert_eq!(spend["summary"]["counted_spend"], "1929.70");
    assert_eq!(spend["summary"]["currency"], "CNY");
    let first = &spend["transactions"][0];
    assert_eq!(first["citations"][0]["source"], "transactions");
    assert!(first["citations"][0]["raw_id"].is_string());

    // The daily metric agrees with the exact total, to the fen.
    let daily = sql(&app, "SELECT round(sum(value) * 100) FROM v_metrics_daily WHERE code = 'ms:spend' AND stat = 'sum'");
    assert_eq!(daily[0][0].as_f64(), Some(192_970.0));

    // Where: items that carry coordinates.
    let located = call(&app, "query_items", json!({ "located_only": true }));
    assert_eq!(located["count"], 1);
    let photo = &located["items"][0];
    assert_eq!(photo["type"], "photo");
    assert_eq!(photo["local_date"], "2026-10-03");
    assert!(photo["lat"].is_f64() && photo["lon"].is_f64(), "{photo}");
    assert_eq!(photo["citations"][0]["source"], "items");

    // One timeline across sources for a stretch of days.
    let timeline = call(
        &app,
        "query_timeline",
        json!({ "from": "2026-10-01", "to": "2026-10-20", "types": ["photo", "transaction", "meeting", "workout"], "limit": 1000 }),
    );
    let mut kinds: Vec<String> = timeline["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| format!("{}:{}", i["kind"].as_str().unwrap(), i["citations"][0]["source"].as_str().unwrap()))
        .collect();
    kinds.sort();
    kinds.dedup();
    assert_eq!(kinds, vec!["event:events", "item:items", "transaction:transactions"]);

    // Documents are found by a word inside a run of Chinese text.
    let found = call(&app, "search_records", json!({ "query": "北京", "kinds": ["item"] }));
    assert_eq!(found["match"], "substring");
    assert_eq!(found["count"], 1, "{found}");
    assert_eq!(found["results"][0]["citations"][0]["source"], "items");
    assert!(found["results"][0]["citations"][0]["raw_id"].is_string());

    // Every one of those reads is in the audit log under its own scope.
    let audit = call(&app, "run_sql", json!({ "sql": "SELECT 1" }));
    assert!(audit["rows"].is_array());
    let scopes: Vec<(String, String)> = app
        .conn()
        .prepare("SELECT DISTINCT action, scope FROM audit_log WHERE client_id = 'demo-test' ORDER BY action")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(scopes.contains(&("query_transactions".into(), "finance:read".into())), "{scopes:?}");
    assert!(scopes.contains(&("query_items".into(), "documents:read".into())), "{scopes:?}");
    let logged_rows: i64 = app
        .conn()
        .query_row(
            "SELECT rows FROM audit_log WHERE client_id = 'demo-test' AND action = 'query_transactions'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(logged_rows, 30);
}

#[test]
fn the_whole_home_rebuilds_to_identical_results() {
    let (_dir, app) = demo_home();
    let snapshot = |app: &App| -> Vec<Value> {
        [
            "SELECT * FROM v_metrics_daily ORDER BY code, stat, date",
            "SELECT * FROM v_transactions ORDER BY id",
            "SELECT * FROM v_items ORDER BY id",
            "SELECT * FROM v_timeline ORDER BY start_ts, kind, id",
            "SELECT * FROM v_entities ORDER BY id",
            "SELECT * FROM v_links ORDER BY from_type, from_id, to_id, relation",
        ]
        .iter()
        .map(|q| app.query().run_sql(q).unwrap())
        .collect()
    };
    let before = snapshot(&app);
    let sync_runs: i64 = app.conn().query_row("SELECT count(*) FROM sync_runs", [], |r| r.get(0)).unwrap();

    let home = app.home().clone();
    drop(app);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", home.index_path().display()));
    }
    let app = App::open(home).unwrap();
    let report = app.process(true).unwrap();
    assert!(report.errors().next().is_none());
    assert_eq!(snapshot(&app), before);
    // The calendar fetch came through the scheduler and is in that snapshot.
    assert!(sync_runs >= 1);
}
