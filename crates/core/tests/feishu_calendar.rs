//! Feishu calendar (workstream W2): two overlapping synthetic fetches in
//! `fixtures/feishu/`. Expected values are worked out by hand from
//! `fixtures/feishu/generate.py`; all times below are UTC unless marked +08.

use mirrorself_core::config::Home;
use mirrorself_core::error::user_error;
use mirrorself_core::{App, UserError};
use serde_json::{json, Value};

const FETCH_1: &[u8] = include_bytes!("../../../fixtures/feishu/fetch-1_2026-10-12_2026-10-17.json");
const FETCH_2: &[u8] = include_bytes!("../../../fixtures/feishu/fetch-2_2026-10-15_2026-10-20.json");
const HEALTH: &[u8] = include_bytes!("../../../fixtures/apple-health/export.xml");

fn new_app() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path().join("home"));
    App::init(&home, "server", None).unwrap();
    (dir, App::open(home).unwrap())
}

fn import(app: &App, bytes: &[u8], name: &str) -> mirrorself_core::ImportReport {
    let report = app.import_bytes("feishu-calendar", Some(name.into()), bytes, true).unwrap();
    let errors: Vec<_> = report.process.as_ref().unwrap().errors().collect();
    assert!(errors.is_empty(), "processor errors: {errors:?}");
    report
}

fn both(app: &App) -> (String, String) {
    let a = import(app, FETCH_1, "fetch-1.json").raw_id;
    let b = import(app, FETCH_2, "fetch-2.json").raw_id;
    (a, b)
}

fn count(app: &App, sql: &str) -> i64 {
    app.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

/// Rows of any table as JSON arrays (`run_sql` only reads the `v_*` views).
fn rows(app: &App, sql: &str) -> Value {
    use rusqlite::types::ValueRef;
    let mut stmt = app.conn().prepare(sql).unwrap();
    let n = stmt.column_count();
    let out = stmt
        .query_map([], |r| {
            Ok(Value::Array(
                (0..n)
                    .map(|i| match r.get_ref(i).unwrap() {
                        ValueRef::Null => Value::Null,
                        ValueRef::Integer(v) => json!(v),
                        ValueRef::Real(v) => json!(v),
                        ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                        ValueRef::Blob(b) => json!(b.len()),
                    })
                    .collect(),
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    Value::Array(out)
}

/// (start_ts, end_ts, tz_offset, local_date, data) of the meeting with this
/// title on this local date.
fn event(app: &App, title: &str, local_date: &str) -> (String, String, i64, String, Value, String) {
    app.conn()
        .query_row(
            "SELECT start_ts, end_ts, tz_offset, local_date, data, raw_id FROM events
             WHERE type = 'meeting' AND title = ?1 AND local_date = ?2",
            [title, local_date],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    serde_json::from_str(&r.get::<_, String>(4)?).unwrap(),
                    r.get(5)?,
                ))
            },
        )
        .unwrap_or_else(|e| panic!("{title} on {local_date}: {e}"))
}

fn daily(app: &App, code: &str, date: &str) -> Option<(f64, i64)> {
    app.conn()
        .query_row(
            "SELECT value, n FROM daily_metrics WHERE code = ?1 AND stat = 'sum' AND date = ?2",
            [code, date],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()
}

/// Everything this source derives, in a stable order.
fn snapshot(app: &App) -> Value {
    json!([
        rows(app, "SELECT id, type, category_id, start_ts, end_ts, tz_offset, local_date, title, data, raw_id, run_id
                   FROM events ORDER BY id"),
        rows(app, "SELECT id, code, value_num, unit, start_ts, end_ts, tz_offset, local_date, source_name, raw_id,
                          run_id, status
                   FROM observations ORDER BY id"),
        rows(app, "SELECT * FROM daily_metrics WHERE code LIKE 'ms:meeting%' ORDER BY code, stat, date"),
        rows(app, "SELECT id, kind, name FROM entities ORDER BY id"),
        rows(app, "SELECT * FROM links ORDER BY from_id, to_id, relation"),
    ])
}

#[test]
fn every_instance_becomes_one_meeting_with_utc_times_and_details() {
    let (_dir, app) = new_app();
    let first = import(&app, FETCH_1, "fetch-1.json");
    assert_eq!((first.source.as_str(), first.kind.as_str()), ("feishu", "feishu_calendar_events"));
    let run = &first.process.as_ref().unwrap().runs[0];
    assert_eq!(run.processor, "feishu_calendar.parse");
    assert_eq!(run.outputs["pages"], 4);
    assert_eq!(run.outputs["records_read"], 14);
    assert_eq!(run.outputs["instances"], 11);
    assert_eq!(run.outputs["events_added"], 11);
    assert_eq!(count(&app, "SELECT count(*) FROM events WHERE type = 'meeting' AND category_id = 'work'"), 11);

    // A meeting crossing midnight keeps its real span: 23:00–01:00 +08.
    let (start, end, offset, date, data, _) = event(&app, "Release window v2.14", "2026-10-13");
    assert_eq!((start.as_str(), end.as_str(), offset, date.as_str()), ("2026-10-13T15:00:00Z", "2026-10-13T17:00:00Z", 28800, "2026-10-13"));
    assert_eq!(data["minutes"], 120.0);
    assert_eq!(data["counted_in_metrics"], true);

    // All-day: Feishu's inclusive end date becomes an exclusive UTC end.
    let (start, end, offset, _, data, _) = event(&app, "Team offsite", "2026-10-14");
    assert_eq!((start.as_str(), end.as_str(), offset), ("2026-10-14T00:00:00Z", "2026-10-15T00:00:00Z", 0));
    assert_eq!((data["all_day"].clone(), data["excluded_because"].clone()), (json!(true), json!("all_day")));

    // A fixed-offset zone other than the user's: 10:00 Tokyo time.
    let (start, _, offset, _, data, _) = event(&app, "Tokyo office hours", "2026-10-16");
    assert_eq!((start.as_str(), offset), ("2026-10-16T01:00:00Z", 32400));
    assert_eq!(data["location"]["name"], "Tokyo office, 12F");
    assert_eq!(data["video_meeting"], false);

    // A zone with daylight saving time falls back to the fetch's home offset.
    let (start, _, offset, _, data, _) = event(&app, "Partner sync (US)", "2026-10-15");
    assert_eq!((start.as_str(), offset), ("2026-10-15T00:00:00Z", 28800));
    assert_eq!(data["timezone"], "America/Los_Angeles");
    assert_eq!((data["vc_type"].clone(), data["video_meeting"].clone()), (json!("third_party"), json!(true)));

    // Recurring instances are separate rows of one series.
    let (_, _, _, _, monday, _) = event(&app, "Weekly platform sync", "2026-10-12");
    let (_, _, _, _, thursday, _) = event(&app, "Weekly platform sync", "2026-10-15");
    assert_eq!(monday["recurring_event_id"], thursday["recurring_event_id"]);
    assert_ne!(monday["event_id"], thursday["event_id"]);
    assert_eq!(monday["organizer"]["name"], "Lin Qiao");
    assert_eq!(monday["rooms"], json!([{ "name": "F3-Orion (8)", "id": "omm_000000000000000000000000000r0001", "status": "accept" }]));
    // People come from `events get`; an instance list alone does not list them.
    assert_eq!(monday["attendees"].as_array().unwrap().len(), 3);
    assert_eq!(thursday["attendees"], Value::Null);

    // Stored but excluded from metrics.
    let (_, _, _, _, declined, _) = event(&app, "Vendor demo", "2026-10-14");
    assert_eq!((declined["self_rsvp"].clone(), declined["excluded_because"].clone()), (json!("decline"), json!("declined")));
    let (_, _, _, _, cancelled, _) = event(&app, "Budget planning", "2026-10-14");
    assert_eq!((cancelled["status"].clone(), cancelled["counted_in_metrics"].clone()), (json!("cancelled"), json!(false)));
    // Not yet answered still counts.
    let (_, _, _, _, pending, _) = event(&app, "1:1 Avery / Demo", "2026-10-13");
    assert_eq!((pending["self_rsvp"].clone(), pending["counted_in_metrics"].clone()), (json!("needs_action"), json!(true)));

    // Agents see meetings through the timeline view.
    assert_eq!(count(&app, "SELECT count(*) FROM v_timeline WHERE type = 'meeting'"), 11);
}

#[test]
fn an_overlapping_fetch_updates_instances_instead_of_duplicating_them() {
    let (_dir, app) = new_app();
    let (first, second) = both(&app);
    // 11 from the first fetch, 6 in the second of which 4 overlap.
    assert_eq!(count(&app, "SELECT count(*) FROM events WHERE type = 'meeting'"), 13);
    assert_eq!(count(&app, "SELECT count(DISTINCT fp) FROM events"), 13);

    // Moved and renamed since the first fetch: the latest version wins.
    assert_eq!(count(&app, "SELECT count(*) FROM events WHERE title = 'Architecture review'"), 0);
    let (start, end, _, _, data, raw_id) = event(&app, "Architecture review (moved)", "2026-10-15");
    assert_eq!((start.as_str(), end.as_str()), ("2026-10-15T08:00:00Z", "2026-10-15T09:30:00Z"));
    assert_eq!(raw_id, second);
    // The second fetch had no people list for it, so the first one's is kept.
    let names: Vec<&str> = data["attendees"].as_array().unwrap().iter().map(|a| a["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Demo User", "Lin Qiao", "Mara Okafor", "Platform team", "Partner Contact"]);

    // Declined after the first fetch.
    let (_, _, _, _, data, _) = event(&app, "Weekly platform sync", "2026-10-15");
    assert_eq!((data["self_rsvp"].clone(), data["counted_in_metrics"].clone()), (json!("decline"), json!(false)));

    // Only in the first fetch: still there, still from it.
    let (_, _, _, _, _, raw_id) = event(&app, "Design review: onboarding flow", "2026-10-12");
    assert_eq!(raw_id, first);

    // Importing either file again changes nothing.
    let before = snapshot(&app);
    for (bytes, name) in [(FETCH_1, "fetch-1.json"), (FETCH_2, "fetch-2.json")] {
        assert!(!app.import_bytes("feishu-calendar", Some(name.into()), bytes, true).unwrap().created);
    }
    assert_eq!(snapshot(&app), before);
}

#[test]
fn daily_meeting_time_merges_overlaps_and_splits_at_midnight() {
    let (_dir, app) = new_app();
    both(&app);
    let minutes = |d| daily(&app, "ms:meeting_minutes", d);
    let meetings = |d| daily(&app, "ms:meeting_count", d);
    // Mon: 10:00–11:00 and 10:30–11:30 overlap → 90 min of 2 meetings.
    assert_eq!(minutes("2026-10-12"), Some((90.0, 1)));
    assert_eq!(meetings("2026-10-12"), Some((2.0, 2)));
    // Tue: 30-min 1:1, plus the release window's first hour before midnight.
    assert_eq!(minutes("2026-10-13"), Some((90.0, 2)));
    assert_eq!(meetings("2026-10-13"), Some((2.0, 2)));
    // Wed: the release window's second hour; all-day, declined and cancelled
    // events add nothing, and no meeting starts that day.
    assert_eq!(minutes("2026-10-14"), Some((60.0, 1)));
    assert_eq!(meetings("2026-10-14"), None);
    // Thu: partner sync 60 + moved review 90; the declined weekly sync is out.
    assert_eq!(minutes("2026-10-15"), Some((150.0, 2)));
    assert_eq!(meetings("2026-10-15"), Some((2.0, 2)));
    // Fri: Tokyo hour 60 + planning 13:00–14:30 containing the 13:30 panel.
    assert_eq!(minutes("2026-10-16"), Some((150.0, 2)));
    assert_eq!(meetings("2026-10-16"), Some((3.0, 3)));
    assert_eq!(count(&app, "SELECT count(*) FROM daily_metrics WHERE code LIKE 'ms:meeting%'"), 9);
    assert_eq!(
        count(&app, "SELECT count(*) FROM observations WHERE source_name = 'Feishu Calendar' AND status = 'auto'"),
        9 + 8
    );
    // Through the agent-facing view, in the `work` category.
    let view = app
        .query()
        .run_sql("SELECT value, unit, category FROM v_metrics_daily WHERE code = 'ms:meeting_minutes' AND date = '2026-10-16'")
        .unwrap();
    assert_eq!(view["rows"], json!([[150.0, "min", "work"]]));
}

#[test]
fn people_and_rooms_become_linked_entities() {
    let (_dir, app) = new_app();
    both(&app);
    let people = rows(&app, "SELECT name FROM entities WHERE kind = 'person' ORDER BY name");
    assert_eq!(
        people,
        json!([["Avery Chen"], ["Demo User"], ["Haru Sato"], ["Jordan Li"], ["Lin Qiao"], ["Mara Okafor"],
               ["Partner Contact"], ["Quinn Zhao"], ["Riley Novak"], ["Sam Ito"]])
    );
    // Removed invitees, group chats and declined room bookings are not linked.
    for name in ["Former Member", "Platform team", "F2-Lyra (4)"] {
        assert_eq!(count(&app, &format!("SELECT count(*) FROM entities WHERE name = '{name}'")), 0, "{name}");
    }
    assert_eq!(rows(&app, "SELECT name FROM entities WHERE kind = 'place' ORDER BY name"), json!([["F3-Orion (8)"], ["F5-Vega (12)"]]));
    assert_eq!(count(&app, "SELECT count(*) FROM links WHERE relation = 'involves' AND method = 'rule'"), 23);
    assert_eq!(count(&app, "SELECT count(*) FROM links WHERE relation = 'occurred_at'"), 5);
    assert_eq!(
        rows(
            &app,
            "SELECT n.name FROM links l JOIN events e ON e.id = l.from_id JOIN entities n ON n.id = l.to_id
             WHERE e.title = 'Sprint planning' AND l.relation = 'involves' ORDER BY n.name"
        ),
        json!([["Avery Chen"], ["Demo User"], ["Jordan Li"], ["Sam Ito"]])
    );
    let lin = app.query().entity("Lin Qiao").unwrap().to_string();
    assert!(lin.contains("Weekly platform sync"), "{lin}");
}

#[test]
fn only_calendar_output_is_accepted_or_detected() {
    let (_dir, app) = new_app();
    for bytes in [
        &br#"{"ok":true,"identity":"user","data":{"items":[{"guid":"t1","summary":"a task"}]}}"#[..],
        br#"{"ok":false,"identity":"user","error":{"type":"authorization","code":99991679}}"#,
        br#"{"ok":true,"identity":"user","data":{"items":[]}}"#,
        br#"{"dependencies":{"serde":"1"}}"#,
        HEALTH,
    ] {
        let err = app.import_bytes("feishu-calendar", Some("x.json".into()), bytes, false).unwrap_err();
        assert!(matches!(user_error(&err), Some(UserError::Invalid(_))), "{err:#}");
    }
    assert_eq!(count(&app, "SELECT count(*) FROM raw_objects"), 0);

    let auto = app.import_auto(Some("calendar.json".into()), FETCH_2, false).unwrap();
    assert_eq!((auto.source.as_str(), auto.kind.as_str()), ("feishu", "feishu_calendar_events"));
    let other = app.import_auto(Some("other.json".into()), br#"{"ok":true,"data":{"items":[]}}"#, false).unwrap();
    assert_eq!(other.kind, "file_text");
    let health = app.import_auto(Some("export.xml".into()), HEALTH, false).unwrap();
    assert_eq!(health.kind, "apple_health_export");
}

#[test]
fn processing_order_does_not_change_which_version_wins() {
    let (_dir, in_order) = new_app();
    both(&in_order);

    // Same two fetches, but the index processes the newer one first.
    let (_dir2, reversed) = new_app();
    for (bytes, name) in [(FETCH_1, "fetch-1.json"), (FETCH_2, "fetch-2.json")] {
        reversed.import_bytes("feishu-calendar", Some(name.into()), bytes, false).unwrap();
    }
    reversed
        .conn()
        .execute_batch(
            "UPDATE raw_objects SET received_at = CASE name WHEN 'fetch-1.json' THEN '2100-01-02T00:00:00Z'
                                                        ELSE '2100-01-01T00:00:00Z' END",
        )
        .unwrap();
    let report = reversed.process(false).unwrap();
    assert!(report.errors().next().is_none());
    let older = report.runs.iter().filter(|r| r.processor == "feishu_calendar.parse").nth(1).unwrap();
    assert_eq!(older.outputs["older_than_stored"], 4);
    assert_eq!(older.outputs["attendee_lists_carried"], 1);
    // Raw ids and fetch times differ between the two homes; the rest must not.
    let events = "SELECT id, title, start_ts, end_ts, tz_offset, local_date, json_remove(data, '$.fetched_at')
                  FROM events ORDER BY id";
    assert_eq!(rows(&reversed, events), rows(&in_order, events));
    assert_eq!(
        rows(&reversed, "SELECT code, date, value, n FROM daily_metrics WHERE code LIKE 'ms:meeting%' ORDER BY code, date"),
        rows(&in_order, "SELECT code, date, value, n FROM daily_metrics WHERE code LIKE 'ms:meeting%' ORDER BY code, date")
    );
}

#[test]
fn deleting_the_index_and_rebuilding_gives_identical_results() {
    let (_dir, app) = new_app();
    both(&app);
    let before = snapshot(&app);
    assert_eq!(app.process(true).unwrap().errors().count(), 0);
    assert_eq!(snapshot(&app), before, "process --rebuild");

    // A new processor version clears its rows and reprocesses from raw.
    app.conn()
        .execute("UPDATE processor_runs SET version = '0.0.1' WHERE processor = 'feishu_calendar.parse'", [])
        .unwrap();
    assert_eq!(app.process(false).unwrap().errors().count(), 0);
    assert_eq!(snapshot(&app), before, "version bump");

    let home = app.home().clone();
    drop(app);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", home.index_path().display()));
    }
    let app = App::open(home).unwrap();
    let report = app.process(true).unwrap();
    assert_eq!(report.registered_from_vault, 2);
    assert!(report.errors().next().is_none());
    assert_eq!(snapshot(&app), before, "deleted index");
}
