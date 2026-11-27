//! Alipay bills (W1, docs/sources/alipay.md) against the synthetic fixtures
//! in `fixtures/alipay/`: two GBK exports in the real layout that overlap on
//! 2026-10-10 .. 2026-10-20.
//!
//! - `alipay_record_20261020_101500.csv`: 27 rows, 2026-09-21 .. 2026-10-20.
//! - `alipay_record_20261105_093000.csv`: 24 rows, 2026-10-10 .. 2026-11-04.
//!   Nine orders repeat; in this later export order …019 (蓝牙耳机, 120.00)
//!   was refunded in full and closed, and order …021 (洗衣液) moved from
//!   等待确认收货 to 交易成功. It also refunds 50.00 of order …016 (运动外套,
//!   299.00) from the first file's period.

use mirrorself_core::config::Home;
use mirrorself_core::error::user_error;
use mirrorself_core::query::{QueryService, SearchQuery};
use mirrorself_core::{App, ImportReport, UserError};
use serde_json::{json, Value};

const BILL_1: &[u8] = include_bytes!("../../../fixtures/alipay/alipay_record_20261020_101500.csv");
const BILL_2: &[u8] = include_bytes!("../../../fixtures/alipay/alipay_record_20261105_093000.csv");
const NAME_1: &str = "alipay_record_20261020_101500.csv";
const NAME_2: &str = "alipay_record_20261105_093000.csv";

fn order(n: u32) -> String {
    format!("202609212200140000{n:010}")
}

fn new_app() -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::new(dir.path().join("home"));
    App::init(&home, "server", None).unwrap();
    (dir, App::open(home).unwrap())
}

fn import(app: &App, bytes: &[u8], name: &str) -> ImportReport {
    let report = app.import_bytes("alipay", Some(name.into()), bytes, true).unwrap();
    let errors: Vec<_> = report.process.as_ref().unwrap().errors().collect();
    assert!(errors.is_empty(), "processor errors: {errors:?}");
    report
}

fn parse_outputs(report: &ImportReport) -> Value {
    report
        .process
        .as_ref()
        .unwrap()
        .runs
        .iter()
        .find(|r| r.processor == "alipay.parse")
        .unwrap()
        .outputs
        .clone()
}

fn count(app: &App, sql: &str) -> i64 {
    app.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn sql(app: &App, query: &str) -> Value {
    app.query().run_sql(query).unwrap()
}

/// Rows as objects keyed by column name.
fn objects(result: &Value) -> Vec<Value> {
    let columns = result["columns"].as_array().unwrap();
    result["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            let cells = row.as_array().unwrap();
            Value::Object(
                columns
                    .iter()
                    .zip(cells)
                    .map(|(c, v)| (c.as_str().unwrap().to_string(), v.clone()))
                    .collect(),
            )
        })
        .collect()
}

fn rows(app: &App, query: &str) -> Vec<Value> {
    let result = sql(app, query);
    assert_eq!(result["truncated"], false);
    objects(&result)
}

fn txn(app: &App, order_no: &str) -> Value {
    let found = rows(app, &format!("SELECT * FROM v_transactions WHERE order_no = '{order_no}'"));
    assert_eq!(found.len(), 1, "order {order_no}");
    found[0].clone()
}

/// `(value, n)` of the daily `ms:spend` sum.
fn daily_spend(app: &App, date: &str) -> Option<(f64, i64)> {
    app.conn()
        .query_row(
            "SELECT value, n FROM daily_metrics WHERE code = 'ms:spend' AND stat = 'sum' AND date = ?1",
            [date],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()
}

fn assert_close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-6, "{actual} != {expected}");
}

#[test]
fn a_bill_becomes_typed_transactions_with_exact_values() {
    let (_dir, app) = new_app();
    let report = import(&app, BILL_1, NAME_1);
    assert_eq!((report.source.as_str(), report.kind.as_str()), ("alipay", "alipay_bill_csv"));
    assert_eq!(
        count(&app, "SELECT count(*) FROM raw_objects WHERE kind = 'alipay_bill_csv' AND mime = 'text/csv'"),
        1
    );
    let out = parse_outputs(&report);
    assert_eq!(out["encoding"], "gbk");
    assert_eq!(out["header_line"], 22);
    assert_eq!(out["rows_read"], 27);
    assert_eq!(out["malformed"], 0);
    assert_eq!(out["skipped_lines"], 0);
    assert_eq!(out["transactions_added"], 27);
    assert_eq!(
        out["spend_rules"],
        json!({ "counted": 20, "not_completed": 1, "not_expense": 4, "refund": 1, "self_transfer": 1 })
    );
    assert_eq!(count(&app, "SELECT count(*) FROM transactions"), 27);

    let coat = txn(&app, &order(16));
    assert_eq!(coat["ts"], "2026-10-05T01:30:00Z");
    assert_eq!(coat["tz_offset"], 28800);
    assert_eq!(coat["local_date"], "2026-10-05");
    assert_eq!(coat["local_time"], "2026-10-05 09:30:00");
    assert_eq!(coat["direction"], "expense");
    assert_eq!(coat["direction_label"], "支出");
    assert_eq!(coat["amount_minor"], 29900);
    assert_eq!(coat["amount_text"], "299.00");
    assert_eq!(coat["currency"], "CNY");
    assert_eq!(coat["counterparty"], "优选网购商城");
    assert_eq!(coat["counterparty_account"], "shop***@example.com");
    assert_eq!(coat["item"], "运动外套");
    assert_eq!(coat["category"], "服饰装扮");
    assert_eq!(coat["payment_method"], "花呗");
    assert_eq!(coat["status"], "交易成功");
    assert_eq!(coat["merchant_order_no"], "T200P000000000016");
    assert_eq!(coat["spend_rule"], "counted");
    assert_eq!(coat["spend"], 299.0);
    assert_eq!(coat["source_exported_at"], "2026-10-20T02:15:00Z");
    assert_eq!(coat["file_name"], NAME_1);

    // A quoted cell with a comma, an empty 商家订单号 and a "/" account.
    let boxes = txn(&app, &order(8));
    assert_eq!((boxes["item"].as_str(), boxes["amount"].as_f64()), (Some("收纳盒,大号"), Some(59.9)));
    let metro = txn(&app, &order(4));
    assert_eq!((&metro["merchant_order_no"], &metro["counterparty_account"]), (&Value::Null, &Value::Null));

    // Income, 不计收支 investment and repayment, a closed duplicate order and
    // a transfer to the account holder add nothing to spend.
    for (n, rule, direction) in [
        (5, "not_expense", "income"),
        (7, "not_expense", "neutral"),
        (13, "not_expense", "neutral"),
        (11, "not_completed", "expense"),
        (15, "self_transfer", "expense"),
    ] {
        let t = txn(&app, &order(n));
        assert_eq!((t["spend_rule"].as_str(), t["direction"].as_str(), t["spend"].as_f64()), (Some(rule), Some(direction), Some(0.0)), "{t}");
    }

    // A partial refund in the same file: 45.80 − 5.80.
    let refund = txn(&app, &format!("{}_{}", order(3), order(9003)));
    assert_eq!((refund["refund_of"].as_str(), refund["spend_rule"].as_str()), (Some(order(3).as_str()), Some("refund")));
    let shop = txn(&app, &order(3));
    assert_eq!((shop["refunded_minor"].as_i64(), shop["spend"].as_f64()), (Some(580), Some(40.0)));
    assert_eq!(daily_spend(&app, "2026-09-23"), Some((40.0, 1)));
}

#[test]
fn overlapping_exports_count_each_order_once_and_the_latest_export_wins() {
    let (_dir, app) = new_app();
    import(&app, BILL_1, NAME_1);
    let second = parse_outputs(&import(&app, BILL_2, NAME_2));
    assert_eq!(second["rows_read"], 24);
    assert_eq!(second["transactions_added"], 15);
    assert_eq!(second["replaced_by_newer_export"], 9);
    assert_eq!(count(&app, "SELECT count(*) FROM transactions"), 42);
    assert_eq!(count(&app, "SELECT count(*) FROM transactions WHERE order_no IS NULL"), 0);

    // Refunded in full and closed in the later export: no spend on 10-11.
    let earbuds = txn(&app, &order(19));
    assert_eq!((earbuds["status"].as_str(), earbuds["spend_rule"].as_str()), (Some("交易关闭"), Some("not_completed")));
    assert_eq!(earbuds["file_name"], NAME_2);
    assert_eq!(daily_spend(&app, "2026-10-11"), None);
    // Delivered in the later export; counted once either way.
    assert_eq!(txn(&app, &order(21))["status"], "交易成功");
    assert_eq!(daily_spend(&app, "2026-10-12"), Some((39.9, 1)));
    // Unchanged repeated orders.
    assert_eq!(daily_spend(&app, "2026-10-10"), Some((26.0, 1)));
    assert_eq!(daily_spend(&app, "2026-10-20"), Some((12.0, 1)));
    // A refund in the later file reduces an order from the earlier one.
    assert_eq!(txn(&app, &order(16))["spend"], 249.0);
    assert_eq!(daily_spend(&app, "2026-10-05"), Some((249.0, 1)));

    // One observation per counted expense, on the purchase's Beijing date.
    assert_eq!(count(&app, "SELECT count(*) FROM observations WHERE code = 'ms:spend'"), 30);
    assert_close(
        app.conn()
            .query_row("SELECT sum(value_num) FROM observations WHERE code = 'ms:spend' AND status = 'auto'", [], |r| r.get(0))
            .unwrap(),
        1929.70,
    );
    assert_close(
        app.conn().query_row("SELECT sum(spend) FROM v_transactions", [], |r| r.get(0)).unwrap(),
        1929.70,
    );
    assert_eq!(count(&app, "SELECT count(*) FROM daily_metrics WHERE code = 'ms:spend'"), 29);
    assert_eq!(daily_spend(&app, "2026-10-26"), Some((566.0, 2)));
    assert_eq!(daily_spend(&app, "2026-10-27"), None); // 等待付款
    assert_eq!(daily_spend(&app, "2026-10-30"), Some((66.0, 1))); // transfer to a friend
    assert_eq!(daily_spend(&app, "2026-11-03"), Some((42.0, 1))); // 23:50 Beijing
    assert_eq!(daily_spend(&app, "2026-11-04"), Some((12.0, 1))); // 07:30 Beijing, 11-03 in UTC
    assert_eq!(txn(&app, &order(40))["ts"], "2026-11-03T23:30:00Z");

    // The 28-day baseline: the 10-26 hotel day stands out, 10-30 does not.
    let metric = rows(
        &app,
        "SELECT date, value, n, baseline_28d, sd_28d, z, notable, unit, category FROM v_metrics_daily
         WHERE code = 'ms:spend' AND date IN ('2026-09-29', '2026-10-26', '2026-10-30') ORDER BY date",
    );
    assert_eq!(metric[0]["baseline_28d"], Value::Null); // six earlier days of data
    let hotel = &metric[1];
    assert_eq!((hotel["unit"].as_str(), hotel["category"].as_str()), (Some("CNY"), Some("finance")));
    assert_close(hotel["baseline_28d"].as_f64().unwrap(), 59.233333);
    assert_close(hotel["sd_28d"].as_f64().unwrap(), 63.135009);
    assert!((hotel["z"].as_f64().unwrap() - 8.0267).abs() < 1e-4);
    assert_eq!(hotel["notable"], 1);
    assert_eq!(metric[2]["notable"], 0);
    assert_eq!(
        count(&app, "SELECT count(*) FROM daily_metrics WHERE code = 'ms:spend' AND notable = 1"),
        3 // 10-01, 10-05 and 10-26
    );

    // Importing a file again changes nothing.
    let again = import(&app, BILL_2, NAME_2);
    assert!(!again.created);
    assert_eq!(count(&app, "SELECT count(*) FROM transactions"), 42);
}

/// What an agent sees of the finance data. Raw ids are keyed per vault, so
/// they are left out when comparing two homes.
fn snapshot(app: &App, with_raw_ids: bool) -> Value {
    let q = QueryService::with_today(app.conn(), chrono::NaiveDate::from_ymd_opt(2026, 11, 5).unwrap());
    let mut transactions = objects(&q.run_sql("SELECT * FROM v_transactions ORDER BY id").unwrap());
    if !with_raw_ids {
        for t in &mut transactions {
            t.as_object_mut().unwrap().remove("raw_id");
        }
    }
    json!([
        transactions,
        q.run_sql("SELECT * FROM v_metrics_daily WHERE code = 'ms:spend' ORDER BY date, stat").unwrap(),
        q.run_sql("SELECT * FROM v_entities WHERE kind = 'merchant' ORDER BY id").unwrap(),
        q.run_sql("SELECT * FROM v_links WHERE from_type = 'transaction' ORDER BY from_id, to_id").unwrap(),
        q.search(&SearchQuery {
            query: "青禾简餐".into(),
            from: Some("2026-09-01".into()),
            to: Some("2026-11-30".into()),
            limit: Some(100),
            ..Default::default()
        })
        .unwrap(),
    ])
}

#[test]
fn import_order_does_not_change_the_result() {
    let (_dir_a, forward) = new_app();
    import(&forward, BILL_1, NAME_1);
    import(&forward, BILL_2, NAME_2);
    let (_dir_b, backward) = new_app();
    let older_second = parse_outputs(&import(&backward, BILL_2, NAME_2));
    assert_eq!(older_second["transactions_added"], 24);
    let older = parse_outputs(&import(&backward, BILL_1, NAME_1));
    // The earlier export loses to the rows already there.
    assert_eq!((older["transactions_added"].as_u64(), older["already_known"].as_u64()), (Some(18), Some(9)));
    assert_eq!(snapshot(&forward, false), snapshot(&backward, false));
}

#[test]
fn deleting_the_index_and_rebuilding_gives_identical_transactions_and_metrics() {
    let (_dir, app) = new_app();
    import(&app, BILL_1, NAME_1);
    import(&app, BILL_2, NAME_2);
    let before = snapshot(&app, true);
    assert_eq!(before[0].as_array().unwrap().len(), 42);

    // A rebuild in place clears and rewrites the table.
    assert!(app.process(true).unwrap().errors().next().is_none());
    assert_eq!(snapshot(&app, true), before);

    // Runs from an older processor version are cleared and redone from raw.
    app.conn()
        .execute("UPDATE processor_runs SET version = '0.0.1' WHERE processor = 'alipay.parse'", [])
        .unwrap();
    let report = app.process(false).unwrap();
    assert!(report.errors().next().is_none());
    assert_eq!(report.runs.iter().filter(|r| r.processor == "alipay.parse").count(), 2);
    assert_eq!(count(&app, "SELECT count(*) FROM observations WHERE code = 'ms:spend'"), 30);
    assert_eq!(snapshot(&app, true), before);

    let home = app.home().clone();
    drop(app);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", home.index_path().display()));
    }
    let app = App::open(home).unwrap();
    assert_eq!(count(&app, "SELECT count(*) FROM transactions"), 0);
    let report = app.process(true).unwrap();
    assert_eq!(report.registered_from_vault, 2);
    assert!(report.errors().next().is_none());
    assert_eq!(snapshot(&app, true), before);
}

#[test]
fn counterparties_become_merchants_and_transactions_are_searchable() {
    let (_dir, app) = new_app();
    import(&app, BILL_1, NAME_1);
    import(&app, BILL_2, NAME_2);

    // 17 distinct counterparties, minus the account holder's own name.
    assert_eq!(count(&app, "SELECT count(*) FROM entities WHERE kind = 'merchant'"), 16);
    assert_eq!(count(&app, "SELECT count(*) FROM entities WHERE name = '测试用户'"), 0);
    // Spend observations are not mistaken for a measuring device.
    assert_eq!(count(&app, "SELECT count(*) FROM entities WHERE kind = 'device'"), 0);
    assert_eq!(
        count(&app, "SELECT count(*) FROM links WHERE from_type = 'transaction' AND relation = 'involves'"),
        41
    );
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM links l JOIN entities e ON e.id = l.to_id
             WHERE l.from_type = 'transaction' AND e.name = '青禾简餐'"
        ),
        8
    );

    let q = QueryService::with_today(app.conn(), chrono::NaiveDate::from_ymd_opt(2026, 11, 5).unwrap());
    let hits = q
        .search(&SearchQuery {
            query: "晨光早餐铺".into(),
            from: Some("2026-09-01".into()),
            to: Some("2026-11-30".into()),
            kinds: vec!["transaction".into()],
            limit: Some(100),
        })
        .unwrap();
    assert_eq!(hits["count"], 5, "{hits:#}");
    assert!(hits.to_string().contains("晨光早餐铺 · 12.50 CNY"), "{hits:#}");
    let merchants = q
        .search(&SearchQuery { query: "炭火小馆".into(), kinds: vec!["entity".into()], ..Default::default() })
        .unwrap();
    assert_eq!(merchants["count"], 1);
}

#[test]
fn auto_detection_claims_alipay_bills_only() {
    let (_dir, app) = new_app();
    let auto = app.import_auto(Some("bill.csv".into()), BILL_1, true).unwrap();
    assert_eq!((auto.source.as_str(), auto.kind.as_str()), ("alipay", "alipay_bill_csv"));

    // A UTF-8 re-save with BOM and CRLF is the same bill: same orders, no new rows.
    let (text, _, bad) = encoding_rs::GB18030.decode(BILL_1);
    assert!(!bad);
    let utf8 = format!("\u{feff}{}", text.replace('\n', "\r\n"));
    let resaved = app.import_auto(Some("bill-utf8.csv".into()), utf8.as_bytes(), true).unwrap();
    assert_eq!(resaved.kind, "alipay_bill_csv");
    let out = parse_outputs(&resaved);
    assert_eq!((out["encoding"].as_str(), out["rows_read"].as_u64()), (Some("utf-8"), Some(27)));
    assert_eq!(out["already_known"], 27);
    assert_eq!(count(&app, "SELECT count(*) FROM transactions"), 27);

    let wechat = "微信支付账单明细\n交易时间,交易类型,交易对方,商品,收/支,金额(元),支付方式,当前状态,交易单号,商户单号,备注\n\
                  2026-10-01 08:00:00,商户消费,示例商户,早餐,支出,¥12.50,零钱,支付成功,4200000000000000000001\t,10000000000001\t,/\n";
    let generic = "date,amount,description\n2026-10-01,12.50,coffee\n";
    for (name, bytes) in [("wechat.csv", wechat.as_bytes()), ("budget.csv", generic.as_bytes())] {
        let err = app.import_bytes("alipay", Some(name.into()), bytes, false).unwrap_err();
        assert!(matches!(user_error(&err), Some(UserError::Invalid(_))), "{name}: {err:#}");
        let other = app.import_auto(Some(name.into()), bytes, true).unwrap();
        assert_eq!((other.source.as_str(), other.kind.as_str()), ("file", "file_text"), "{name}");
    }
    assert_eq!(count(&app, "SELECT count(*) FROM raw_objects WHERE kind = 'alipay_bill_csv'"), 2);
}
