//! The source registry's shared behaviour: import routing, the `items`
//! envelope and what a rebuild clears (docs/demo-plan.md).

use mirrorself_core::config::Home;
use mirrorself_core::error::user_error;
use mirrorself_core::query::SearchQuery;
use mirrorself_core::{App, RawInput, UserError};

const HEALTH: &[u8] = include_bytes!("../../../fixtures/apple-health/export.xml");

fn new_app() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path().join("home"));
    App::init(&home, "server", None).unwrap();
    (dir, App::open(home).unwrap())
}

fn count(app: &App, sql: &str) -> i64 {
    app.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

#[test]
fn auto_import_routes_known_formats_and_keeps_everything_else() {
    let (_dir, app) = new_app();

    let health = app.import_bytes("auto", Some("export.xml".into()), HEALTH, true).unwrap();
    assert_eq!((health.source.as_str(), health.kind.as_str()), ("apple_health", "apple_health_export"));
    assert!(count(&app, "SELECT count(*) FROM observations") > 0);

    let pdf = app.import_auto(Some("scan.pdf".into()), b"%PDF-1.7\n% synthetic", true).unwrap();
    assert_eq!((pdf.source.as_str(), pdf.kind.as_str()), ("file", "file_pdf"));
    let note = app.import_auto(Some("note.md".into()), "# 复盘\n".as_bytes(), true).unwrap();
    assert_eq!(note.kind, "file_text");
    // A zip that is not a Health export is not mistaken for one.
    let zip = app.import_auto(Some("photos.zip".into()), b"PK\x03\x04 not a real archive", true).unwrap();
    assert_eq!((zip.source.as_str(), zip.kind.as_str()), ("file", "file_other"));

    assert_eq!(count(&app, "SELECT count(*) FROM raw_objects"), 4);
    assert_eq!(
        count(&app, "SELECT count(*) FROM raw_objects WHERE kind = 'file_text' AND mime = 'text/markdown'"),
        1
    );
    // Nothing processes generic files yet, and that is not an error.
    assert!(app.process(false).unwrap().errors().next().is_none());
}

#[test]
fn a_named_source_rejects_other_files_and_unknown_names() {
    let (_dir, app) = new_app();
    for (source, bytes) in [("apple-health", &b"PK\x03\x04 not a real archive"[..]), ("nope", HEALTH)] {
        let err = app.import_bytes(source, None, bytes, false).unwrap_err();
        assert!(matches!(user_error(&err), Some(UserError::Invalid(_))), "{source}: {err:#}");
    }
    assert_eq!(count(&app, "SELECT count(*) FROM raw_objects"), 0);
}

#[test]
fn items_are_readable_through_the_view_searchable_and_cleared_by_a_rebuild() {
    let (_dir, app) = new_app();
    let raw = app
        .ingest(
            RawInput {
                source: "test".into(),
                kind: "test_blob".into(),
                mime: "text/plain".into(),
                name: Some("ticket.txt".into()),
                origin_ref: None,
            },
            b"synthetic",
            false,
        )
        .unwrap();
    assert_eq!((raw.source.as_str(), raw.kind.as_str()), ("test", "test_blob"));

    // Stand in for a processor: write one item for the raw object.
    app.conn()
        .execute(
            "INSERT INTO items (id, raw_id, type, subtype, category_id, title, text, event_start, tz_offset,
                                local_date, received_at, lat, lon, place, run_id)
             VALUES ('item_test', ?1, 'document', 'train_ticket', 'documents', 'G123 北京南 → 上海虹桥',
                     'synthetic ticket text', '2026-10-01T01:00:00Z', 28800, '2026-10-01',
                     '2026-11-27T00:00:00Z', 39.865, 116.379, '北京南站', 'run_test')",
            [&raw.raw_id],
        )
        .unwrap();
    app.process(false).unwrap();

    let q = app.query();
    let rows = q
        .run_sql("SELECT type, subtype, category, place, method, status, source, file_name FROM v_items")
        .unwrap();
    let text = rows.to_string();
    for expected in ["train_ticket", "documents", "北京南站", "rule", "auto", "ticket.txt"] {
        assert!(text.contains(expected), "missing {expected} in {text}");
    }
    let hits = q
        .search(&SearchQuery { query: "ticket".into(), kinds: vec!["item".into()], ..Default::default() })
        .unwrap();
    assert!(hits.to_string().contains("item_test"), "{hits}");

    app.conn()
        .execute("UPDATE items SET status = 'rejected' WHERE id = 'item_test'", [])
        .unwrap();
    assert_eq!(count(&app, "SELECT count(*) FROM v_items"), 0);

    app.process(true).unwrap();
    assert_eq!(count(&app, "SELECT count(*) FROM items"), 0);
    assert_eq!(count(&app, "SELECT count(*) FROM raw_objects"), 1);
}

#[test]
fn metric_codes_for_spending_and_meetings_are_seeded() {
    let (_dir, app) = new_app();
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM codes c JOIN categories g ON g.id = c.category_id
             WHERE (c.code = 'ms:spend' AND g.id = 'finance')
                OR (c.code IN ('ms:meeting_minutes', 'ms:meeting_count') AND g.id = 'work')"
        ),
        3
    );
}
