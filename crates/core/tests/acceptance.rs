//! Milestone acceptance tests (architecture §13) against the fixture export
//! and synthetic data. Fixture values are documented in the fixture itself.

use chrono::NaiveDate;
use mirrorself_core::config::Home;
use mirrorself_core::error::user_error;
use mirrorself_core::query::{QueryService, SearchQuery, SeriesQuery, TimelineQuery};
use mirrorself_core::synth::{self, SynthOptions};
use mirrorself_core::{App, UserError};
use serde_json::{json, Value};

const FIXTURE: &[u8] = include_bytes!("../../../fixtures/apple-health/export.xml");

fn new_app() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path().join("home"));
    App::init(&home, "server", None).unwrap();
    (dir, App::open(home).unwrap())
}

fn import(app: &App, bytes: &[u8], name: &str) -> mirrorself_core::ImportReport {
    let report = app
        .import_bytes("apple-health", Some(name.into()), bytes, true)
        .unwrap();
    if let Some(p) = &report.process {
        let errors: Vec<_> = p.errors().collect();
        assert!(errors.is_empty(), "processor errors: {errors:?}");
    }
    report
}

fn daily(app: &App, code: &str, stat: &str, date: &str) -> Option<f64> {
    app.conn()
        .query_row(
            "SELECT value FROM daily_metrics WHERE code = ?1 AND stat = ?2 AND date = ?3",
            [code, stat, date],
            |r| r.get(0),
        )
        .ok()
}

fn count(app: &App, sql: &str) -> i64 {
    app.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn close(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| (a - b).abs() < 1e-3)
}

#[test]
fn m0_importing_a_file_twice_stores_one_raw_object() {
    let (_dir, app) = new_app();
    let first = import(&app, FIXTURE, "export.xml");
    let second = import(&app, FIXTURE, "export.xml");
    assert!(first.created);
    assert!(!second.created);
    assert_eq!(first.raw_id, second.raw_id);
    assert_eq!(count(&app, "SELECT count(*) FROM raw_objects"), 1);
    assert_eq!(app.vault().list_ids().unwrap(), vec![first.raw_id.clone()]);
    assert!(second.process.unwrap().runs.is_empty(), "nothing new to process");

    let (meta, original) = app.raw_original(&first.raw_id).unwrap();
    assert_eq!(original, FIXTURE);
    assert_eq!(meta.kind, "apple_health_export");
}

#[test]
fn m1_fixture_is_normalized_and_deduplicated() {
    let (_dir, app) = new_app();
    let report = import(&app, FIXTURE, "export.xml");
    let parse = &report.process.as_ref().unwrap().runs[0];
    assert_eq!(parse.processor, "apple_health.parse");
    assert_eq!(parse.outputs["skipped_types"]["HKQuantityTypeIdentifierDietaryWater"], 1);
    assert_eq!(parse.outputs["already_known"], 2, "correlation children repeat top-level records");

    // Watch beats phone where they overlap; phone-only time still counts.
    assert_eq!(daily(&app, "ms:steps", "sum", "2026-10-01"), Some(1800.0));
    assert_eq!(daily(&app, "ms:steps", "sum", "2026-10-02"), Some(2000.0));
    // Local date follows each sample's own offset (+0900 in Tokyo).
    assert_eq!(daily(&app, "ms:steps", "sum", "2026-10-03"), Some(1500.0));
    assert_eq!(
        count(&app, "SELECT count(*) FROM observations WHERE code = 'ms:steps' AND tz_offset = 32400"),
        1
    );

    assert_eq!(daily(&app, "ms:heart_rate", "mean", "2026-10-01"), Some(70.0));
    assert_eq!(daily(&app, "ms:heart_rate", "min", "2026-10-01"), Some(60.0));
    assert_eq!(daily(&app, "ms:heart_rate", "max", "2026-10-01"), Some(80.0));
    assert_eq!(daily(&app, "ms:hrv_sdnn", "mean", "2026-10-01"), Some(50.0));
    assert!(close(daily(&app, "ms:body_mass", "mean", "2026-10-01"), 70.0));
    assert!(close(daily(&app, "ms:spo2", "mean", "2026-10-01"), 97.0));
    assert!(close(daily(&app, "ms:glucose", "mean", "2026-10-02"), 99.0 / 18.0156));
    assert_eq!(count(&app, "SELECT count(*) FROM observations WHERE code = 'ms:bp_systolic'"), 1);

    // Sleep: 410 min overnight + 30 min nap, both on 2026-10-02; the phone's
    // overlapping in-bed sample is a duplicate.
    assert_eq!(daily(&app, "ms:sleep_asleep_total", "sum", "2026-10-02"), Some(440.0));
    assert_eq!(daily(&app, "ms:sleep_awake", "sum", "2026-10-02"), Some(10.0));
    assert_eq!(daily(&app, "ms:sleep_in_bed", "sum", "2026-10-02"), None);
    let sleep_titles: Vec<String> = app
        .conn()
        .prepare("SELECT title FROM events WHERE type = 'sleep' ORDER BY start_ts")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        sleep_titles,
        vec![
            "Sleep 6h 50m asleep (deep 45m, REM 45m, core 5h 20m)".to_string(),
            "Sleep 30m asleep".to_string(),
        ]
    );

    let workouts: Vec<(String, String)> = app
        .conn()
        .prepare("SELECT local_date, title FROM events WHERE type = 'workout' ORDER BY start_ts")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        workouts,
        vec![
            ("2026-10-01".to_string(), "Running · 5.20 km · 30 min · 320 kcal".to_string()),
            ("2026-10-03".to_string(), "Cycling · 20.00 km · 45 min · 450 kcal".to_string()),
        ]
    );

    // Linking and search.
    let q = app.query();
    let running = q.entity("running").unwrap();
    assert_eq!(running["entity"]["kind"], "activity_type");
    assert_eq!(running["linked"].as_array().unwrap().len(), 1);
    let watch = q.entity("Fixture Apple Watch").unwrap();
    assert!(watch["observations_from_source"].as_i64().unwrap() > 10);
    let found = q
        .search(&SearchQuery { query: "cycling".into(), kinds: vec!["event".into()], ..Default::default() })
        .unwrap();
    assert_eq!(found["results"][0]["kind"], "event");
    assert_eq!(found["count"], 1);
    assert!(found["results"][0]["citations"][0]["raw_id"].is_string());
}

#[test]
fn m1_overlapping_export_adds_no_duplicates() {
    let (_dir, app) = new_app();
    import(&app, FIXTURE, "export.xml");
    let observations = count(&app, "SELECT count(*) FROM observations");

    // A later export repeats every record (different bytes, same samples).
    let mut later = FIXTURE.to_vec();
    later.extend_from_slice(b"<!-- exported again -->\n");
    let report = import(&app, &later, "export-2.xml");
    assert!(report.created);
    assert_eq!(count(&app, "SELECT count(*) FROM raw_objects"), 2);
    assert_eq!(count(&app, "SELECT count(*) FROM observations"), observations);
    assert_eq!(daily(&app, "ms:steps", "sum", "2026-10-01"), Some(1800.0));
}

fn snapshot(app: &App, today: NaiveDate) -> Value {
    let q = QueryService::with_today(app.conn(), today);
    json!([
        q.overview().unwrap(),
        q.timeline(&TimelineQuery { limit: Some(1000), ..Default::default() }).unwrap(),
        q.metric_series(&SeriesQuery { metric: "resting_hr".into(), ..Default::default() }).unwrap(),
        q.metric_series(&SeriesQuery { metric: "steps".into(), bucket: Some("week".into()), ..Default::default() }).unwrap(),
        q.metric_series(&SeriesQuery { metric: "ms:sleep_asleep_total".into(), from: Some("30d".into()), ..Default::default() }).unwrap(),
        q.search(&SearchQuery { query: "sleep".into(), ..Default::default() }).unwrap(),
        q.entity("Demo Apple Watch").unwrap(),
        q.run_sql("SELECT * FROM v_metrics_daily ORDER BY code, stat, date").unwrap(),
    ])
}

#[test]
fn m0_deleting_the_index_and_rebuilding_gives_identical_results() {
    let (_dir, app) = new_app();
    let end = NaiveDate::from_ymd_opt(2026, 10, 31).unwrap();
    let today = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
    let (zip, _) = synth::export_zip(&SynthOptions::days(90, end)).unwrap();
    import(&app, &zip, "export.zip");
    import(&app, FIXTURE, "export.xml");
    let before = snapshot(&app, today);

    let home = app.home().clone();
    drop(app);
    for suffix in ["", "-wal", "-shm"] {
        let path = format!("{}{suffix}", home.index_path().display());
        let _ = std::fs::remove_file(path);
    }

    let app = App::open(home).unwrap();
    assert_eq!(count(&app, "SELECT count(*) FROM raw_objects"), 0);
    let report = app.process(true).unwrap();
    assert_eq!(report.registered_from_vault, 2);
    assert!(report.errors().next().is_none());
    assert_eq!(snapshot(&app, today), before);
}

#[test]
fn m1_baselines_flag_the_illness_week_and_series_cite_records() {
    let (_dir, app) = new_app();
    let end = NaiveDate::from_ymd_opt(2026, 10, 31).unwrap();
    let today = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
    let (zip, records) = synth::export_zip(&SynthOptions::days(120, end)).unwrap();
    assert!(records > 120 * 200);
    import(&app, &zip, "export.zip");

    let q = QueryService::with_today(app.conn(), today);
    let deviations = q
        .timeline(&TimelineQuery { types: vec!["metric_deviation".into()], limit: Some(1000), ..Default::default() })
        .unwrap();
    // Synthetic illness: 2026-09-22 .. 2026-09-28 (39 to 33 days before the end).
    let rhr_spikes: Vec<&Value> = deviations["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["data"]["code"] == "ms:resting_hr" && i["data"]["z"].as_f64().unwrap() >= 2.0)
        .collect();
    assert!(!rhr_spikes.is_empty(), "expected resting-HR deviations, got {deviations:#}");
    assert!(rhr_spikes
        .iter()
        .any(|i| ("2026-09-22".."2026-09-29").contains(&i["local_date"].as_str().unwrap())));

    // "How did my resting HR change in the last 90 days?"
    let series = q
        .metric_series(&SeriesQuery { metric: "resting_hr".into(), from: Some("90d".into()), ..Default::default() })
        .unwrap();
    let points = series["points"].as_array().unwrap();
    assert!(points.len() >= 85);
    assert!(points.iter().all(|p| p["record_ids"].as_array().is_some_and(|r| r.len() == 1)));
    assert!(series["summary"]["slope_per_30_days"].is_number());
    let citation = &series["citations"][0];
    assert_eq!(citation["source"], "observations");
    assert!(citation["record_id"].as_str().unwrap().starts_with("obs_"));
    assert!(citation["raw_id"].is_string());

    // Steps are de-duplicated: the phone duplicates every watch hour.
    let overview = q.overview().unwrap();
    let phone = overview["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source"] == "Demo iPhone")
        .unwrap();
    assert!(phone["duplicates_skipped"].as_i64().unwrap() >= 120 * 15);
}

#[test]
fn run_sql_reads_views_only() {
    let (_dir, app) = new_app();
    import(&app, FIXTURE, "export.xml");
    let q = app.query();

    let ok = q.run_sql("SELECT code, value FROM v_metrics_daily WHERE code = 'ms:steps' ORDER BY date").unwrap();
    assert_eq!(ok["row_count"], 3);
    assert_eq!(ok["rows"][0][1], 1800.0);
    assert!(q.run_sql("SELECT count(*) FROM v_timeline t JOIN v_codes c ON c.category = t.category;").is_ok());

    for bad in [
        "SELECT * FROM observations",
        "SELECT * FROM sqlite_master",
        "DELETE FROM v_codes",
        "PRAGMA table_info(observations)",
        "SELECT * FROM pragma_table_info('observations')",
        "ATTACH DATABASE 'x.db' AS x",
        "SELECT 1; DELETE FROM observations",
        "SELECT load_extension('evil')",
    ] {
        let err = q.run_sql(bad).unwrap_err();
        assert!(
            matches!(user_error(&err), Some(UserError::Denied(_) | UserError::Invalid(_))),
            "{bad}: {err:#}"
        );
    }
    // The authorizer is removed afterwards: ordinary queries still work.
    assert_eq!(count(&app, "SELECT count(*) FROM observations WHERE code = 'ms:steps' AND status = 'auto'"), 5);
    assert!(q.overview().is_ok());
}

#[test]
fn the_index_is_unreadable_without_the_vault_key() {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path().join("home"));
    App::init(&home, "server", None).unwrap();
    {
        let app = App::open(home.clone()).unwrap();
        import(&app, FIXTURE, "export.xml");
    }
    let index = std::fs::read(home.index_path()).unwrap();
    assert!(!index.windows(b"Fixture Apple Watch".len()).any(|w| w == b"Fixture Apple Watch"));

    std::fs::remove_file(home.key_path()).unwrap();
    std::fs::write(home.key_path(), mirrorself_core::crypto::VaultKey::generate().to_hex()).unwrap();
    assert!(App::open(home).is_err());
}

#[test]
fn opening_an_uninitialized_home_is_a_user_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = App::open(Home::new(dir.path().join("nothing-here"))).err().unwrap();
    assert!(matches!(user_error(&err), Some(UserError::NotInitialized(_))));
}
