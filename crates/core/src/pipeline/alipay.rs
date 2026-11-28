//! Alipay bills → `transactions` (workstream W1 in `docs/demo-plan.md`).
//!
//! - `alipay.parse` (per raw object) writes one `transactions` row per order.
//!   The id depends only on the order number, so the same order in an
//!   overlapping export lands on the same row; the row from the export with
//!   the latest 导出时间 wins, whatever order the files were imported in.
//! - `finance.spend` (pre-derive) nets successful refunds against their
//!   original orders and writes one `ms:spend` observation per counted
//!   expense, so `derive.daily_metrics` gives daily spend with a baseline.
//! - `finance.merchants` (post-link) makes counterparties `merchant`
//!   entities linked from their transactions.
//! - `finance.search` (post-index) adds transactions to `search_fts`.
//!
//! The whole-index stages read only `transactions`, so a later bank or
//! WeChat Pay importer that writes the same table gets them unchanged. The
//! treatment rules are documented in `docs/sources/alipay.md`.

use std::collections::BTreeMap;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::ids::{fingerprint, from_fp, stable_id};
use crate::importers::alipay::{self as reader, Direction, Row};
use crate::pipeline::{run_id, Processor};
use crate::sources::{
    add_rule_link, clear_standard, upsert_entity, GlobalCtx, GlobalStage, ImportSpec, RawCtx, RawProcessor, Source,
};
use crate::time::utc_string;

pub const PARSE: Processor = Processor {
    name: "alipay.parse",
    version: "1.0.0",
};

const SPEND: GlobalStage = GlobalStage {
    processor: Processor { name: "finance.spend", version: "1.0.0" },
    run: run_spend,
};
const MERCHANTS: GlobalStage = GlobalStage {
    processor: Processor { name: "finance.merchants", version: "1.0.0" },
    run: run_merchants,
};
const SEARCH: GlobalStage = GlobalStage {
    processor: Processor { name: "finance.search", version: "1.0.0" },
    run: run_search,
};

pub static SOURCE: Source = Source {
    imports: &[ImportSpec {
        name: "alipay",
        source: reader::SOURCE,
        kind: reader::KIND,
        expects: "an Alipay bill: the CSV from 支付宝 → 我的 → 账单 → 开具交易流水证明 / 导出账单",
        detect: detect_bill,
    }],
    raw: &[RawProcessor {
        processor: PARSE,
        kinds: &[reader::KIND],
        load: load_raw,
        clear,
    }],
    pre_derive: &[SPEND],
    post_link: &[MERCHANTS],
    post_index: &[SEARCH],
    derived_tables: &["transactions"],
    ..Source::empty("alipay")
};

/// `spend_rule` values. Only `counted` rows produce `ms:spend`.
pub mod rule {
    /// An expense the user paid: 支出 with a completed or paid status.
    pub const COUNTED: &str = "counted";
    /// 收入, 不计收支 or an empty 收/支.
    pub const NOT_EXPENSE: &str = "not_expense";
    /// A refund row. Successful refunds reduce their original order instead.
    pub const REFUND: &str = "refund";
    /// 支出 that never completed: 交易关闭 (unpaid, or fully refunded and
    /// closed), 等待付款, failed, or a status this importer does not know.
    pub const NOT_COMPLETED: &str = "not_completed";
    /// 支出 to the account holder's own name.
    pub const SELF_TRANSFER: &str = "self_transfer";
    /// 支出 that moves money rather than spends it: credit repayments
    /// (信用借还, counted when the purchase was made) and investments (投资理财).
    pub const INTERNAL_TRANSFER: &str = "internal_transfer";
}

/// 交易状态 values under which a 支出 has left the account.
const PAID_STATUSES: &[&str] = &["交易成功", "支付成功", "等待确认收货", "等待对方发货", "等待发货", "已发货"];
/// 交易分类 values whose 支出 is not consumption.
const INTERNAL_CATEGORIES: &[&str] = &["信用借还", "投资理财"];
/// The 交易状态 of a refund that went through.
const REFUND_DONE: &str = "退款成功";

const UPSERT_TRANSACTION: &str = "INSERT OR REPLACE INTO transactions
  (id, source, order_no, merchant_order_no, refund_of, ts, tz_offset, local_date, counterparty,
   counterparty_account, item, category, direction, direction_label, amount_minor, currency,
   payment_method, status, note, spend_rule, source_exported_at, raw_id, run_id)
  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)";

fn detect_bill(_name: Option<&str>, content: &[u8]) -> Option<&'static str> {
    reader::detect(content)
}

fn clear(conn: &Connection, processor: &str) -> Result<()> {
    clear_standard(conn, processor)?;
    conn.execute(
        "DELETE FROM transactions WHERE run_id IN (SELECT id FROM processor_runs WHERE processor = ?1)",
        [processor],
    )?;
    // Spend observations follow the transactions; the stage rewrites them.
    conn.execute("DELETE FROM observations WHERE run_id = ?1", [run_id(&SPEND.processor, None)])?;
    Ok(())
}

fn load_raw(conn: &Connection, ctx: &RawCtx<'_>, content: &[u8]) -> Result<Value> {
    let bill = reader::parse(content)?;
    let exported_at = bill.info.exported_at.as_ref().map(utc_string);
    let mut existing =
        conn.prepare_cached("SELECT coalesce(source_exported_at, '') FROM transactions WHERE id = ?1")?;
    let mut upsert = conn.prepare_cached(UPSERT_TRANSACTION)?;

    let (mut added, mut replaced, mut already_known) = (0u64, 0u64, 0u64);
    let mut rules: BTreeMap<&str, u64> = BTreeMap::new();
    for row in &bill.rows {
        let id = transaction_id(row);
        let newer = match existing.query_row([&id], |r| r.get::<_, String>(0)).optional()? {
            None => {
                added += 1;
                true
            }
            // Ties keep the row already stored (the earlier import).
            Some(prev) if exported_at.as_deref().unwrap_or("") > prev.as_str() => {
                replaced += 1;
                true
            }
            Some(_) => false,
        };
        if !newer {
            already_known += 1;
            continue;
        }
        let rule = spend_rule(row, bill.info.account_name.as_deref());
        *rules.entry(rule).or_default() += 1;
        upsert.execute(params![
            id,
            reader::SOURCE,
            row.order_no,
            row.merchant_order_no,
            refund_of(row),
            utc_string(&row.time),
            row.time.offset().local_minus_utc(),
            row.time.date_naive().to_string(),
            row.counterparty,
            row.counterparty_account,
            row.item,
            row.category,
            row.direction.as_str(),
            row.direction_label,
            row.amount_minor,
            reader::CURRENCY,
            row.payment_method,
            row.status,
            row.note,
            rule,
            exported_at,
            ctx.raw_id,
            ctx.run_id
        ])?;
    }
    Ok(json!({
        "encoding": bill.stats.encoding,
        "header_line": bill.stats.header_line,
        "rows_read": bill.stats.rows,
        "malformed": bill.stats.malformed,
        "malformed_lines": bill.stats.malformed_lines,
        "skipped_lines": bill.stats.skipped_lines,
        "transactions_added": added,
        "replaced_by_newer_export": replaced,
        "already_known": already_known,
        "spend_rules": rules,
    }))
}

/// Keyed by order number, so overlapping exports share rows. A row without
/// one (not seen in real exports) is keyed by its content.
fn transaction_id(row: &Row) -> String {
    match &row.order_no {
        Some(order) => stable_id("txn", &[reader::SOURCE, order]),
        None => stable_id(
            "txn",
            &[
                reader::SOURCE,
                "",
                &utc_string(&row.time),
                row.counterparty.as_deref().unwrap_or(""),
                row.item.as_deref().unwrap_or(""),
                row.direction_label.as_deref().unwrap_or(""),
                &row.amount_minor.to_string(),
            ],
        ),
    }
}

fn is_refund(row: &Row) -> bool {
    row.category.as_deref() == Some("退款") || row.status.as_deref().is_some_and(|s| s.starts_with("退款"))
}

/// A refund's order number is the original's followed by `_` and a suffix.
fn refund_of(row: &Row) -> Option<&str> {
    if !is_refund(row) {
        return None;
    }
    row.order_no
        .as_deref()
        .and_then(|o| o.split_once('_'))
        .map(|(original, _)| original)
        .filter(|o| !o.is_empty())
}

fn spend_rule(row: &Row, account_name: Option<&str>) -> &'static str {
    if is_refund(row) {
        return rule::REFUND;
    }
    if row.direction != Direction::Expense {
        return rule::NOT_EXPENSE;
    }
    if account_name.is_some() && row.counterparty.as_deref() == account_name {
        return rule::SELF_TRANSFER;
    }
    if row.category.as_deref().is_some_and(|c| INTERNAL_CATEGORIES.contains(&c)) {
        return rule::INTERNAL_TRANSFER;
    }
    if row.status.as_deref().is_some_and(|s| PAID_STATUSES.contains(&s)) {
        return rule::COUNTED;
    }
    rule::NOT_COMPLETED
}

fn run_spend(conn: &Connection, ctx: &GlobalCtx<'_>) -> Result<Value> {
    conn.execute(
        "UPDATE transactions SET refunded_minor = coalesce((
           SELECT sum(r.amount_minor) FROM transactions r
           WHERE r.source = transactions.source AND r.refund_of = transactions.order_no AND r.status = ?1), 0)",
        [REFUND_DONE],
    )?;
    conn.execute("DELETE FROM observations WHERE run_id = ?1", [ctx.run_id])?;

    let rows: Vec<(String, String, String, i64, String, i64, String)> = conn
        .prepare(
            "SELECT id, source, ts, tz_offset, local_date, amount_minor - refunded_minor, raw_id
             FROM transactions
             WHERE spend_rule = ?1 AND amount_minor > refunded_minor
             ORDER BY ts, id",
        )?
        .query_map([rule::COUNTED], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    // No source_name: link.rules would make it a `device` entity, and
    // normalize.dedupe would let one payment source cancel another's spend
    // in the same second. The raw_id says where each value came from.
    let mut insert = conn.prepare_cached(
        "INSERT INTO observations
           (id, fp, code_system, code, value_num, unit, start_ts, end_ts, tz_offset, local_date,
            source_tier, raw_id, run_id)
         VALUES (?1, ?2, 'ms', 'ms:spend', ?3, 'CNY', ?4, ?4, ?5, ?6, 2, ?7, ?8)",
    )?;
    let mut total_minor = 0i64;
    for (id, source, ts, tz_offset, local_date, net_minor, raw_id) in &rows {
        let fp = fingerprint(&["obs", "ms:spend", source, id]);
        insert.execute(params![
            from_fp("obs", &fp),
            fp,
            *net_minor as f64 / 100.0,
            ts,
            tz_offset,
            local_date,
            raw_id,
            ctx.run_id
        ])?;
        total_minor += net_minor;
    }
    let refunded: i64 = conn.query_row("SELECT coalesce(sum(refunded_minor), 0) FROM transactions", [], |r| r.get(0))?;
    Ok(json!({
        "spend_observations": rows.len(),
        "spend_total": format!("{}.{:02}", total_minor / 100, total_minor % 100),
        "refunds_netted": format!("{}.{:02}", refunded / 100, refunded % 100),
    }))
}

fn run_merchants(conn: &Connection, _ctx: &GlobalCtx<'_>) -> Result<Value> {
    let rows: Vec<(String, String)> = conn
        // A transfer to oneself names the account holder, who is not a merchant.
        .prepare(
            "SELECT id, counterparty FROM transactions
             WHERE counterparty IS NOT NULL AND spend_rule != ?1 ORDER BY id",
        )?
        .query_map([rule::SELF_TRANSFER], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut links = 0u64;
    for (id, counterparty) in &rows {
        let entity = upsert_entity(conn, "merchant", counterparty)?;
        if add_rule_link(conn, "transaction", id, &entity, "involves")? {
            links += 1;
        }
    }
    let merchants: i64 = conn.query_row("SELECT count(*) FROM entities WHERE kind = 'merchant'", [], |r| r.get(0))?;
    Ok(json!({ "merchants": merchants, "links": links }))
}

fn run_search(conn: &Connection, _ctx: &GlobalCtx<'_>) -> Result<Value> {
    // index.search empties search_fts before this runs.
    let n = conn.execute(
        "INSERT INTO search_fts (kind, ref_id, local_date, title, body)
         SELECT 'transaction', id, local_date,
                coalesce(counterparty, item, category, 'transaction') || ' · '
                  || printf('%d.%02d', amount_minor / 100, amount_minor % 100) || ' ' || currency,
                'transaction ' || source || ' ' || direction || ' ' || coalesce(direction_label, '') || ' '
                  || coalesce(category, '') || ' ' || coalesce(item, '') || ' ' || coalesce(counterparty, '') || ' '
                  || coalesce(payment_method, '') || ' ' || coalesce(status, '') || ' '
                  || coalesce(order_no, '') || ' ' || coalesce(note, '')
         FROM transactions",
        [],
    )?;
    Ok(json!({ "documents": n }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::importers::alipay::parse_time;

    fn row(direction: Direction, category: &str, counterparty: &str, status: &str, order: &str) -> Row {
        Row {
            line: 1,
            time: parse_time("2026-10-01 12:00:00").unwrap(),
            category: Some(category.into()),
            counterparty: Some(counterparty.into()),
            counterparty_account: None,
            item: None,
            direction,
            direction_label: None,
            amount_minor: 100,
            payment_method: None,
            status: Some(status.into()),
            order_no: Some(order.into()),
            merchant_order_no: None,
            note: None,
        }
    }

    #[test]
    fn spend_rules() {
        use Direction::*;
        let me = Some("测试用户");
        let cases = [
            (row(Expense, "餐饮美食", "店", "交易成功", "1"), rule::COUNTED),
            (row(Expense, "日用百货", "店", "等待确认收货", "1"), rule::COUNTED),
            (row(Expense, "餐饮美食", "店", "交易关闭", "1"), rule::NOT_COMPLETED),
            (row(Expense, "交通出行", "店", "等待付款", "1"), rule::NOT_COMPLETED),
            (row(Expense, "转账红包", "测试用户", "交易成功", "1"), rule::SELF_TRANSFER),
            (row(Expense, "信用借还", "花呗", "还款成功", "1"), rule::INTERNAL_TRANSFER),
            (row(Neutral, "投资理财", "基金", "交易成功", "1"), rule::NOT_EXPENSE),
            (row(Income, "转账红包", "朋友", "交易成功", "1"), rule::NOT_EXPENSE),
            (row(Neutral, "退款", "店", "退款成功", "1_2"), rule::REFUND),
        ];
        for (r, expected) in cases {
            assert_eq!(spend_rule(&r, me), expected, "{r:?}");
        }
        let refund = row(Direction::Neutral, "退款", "店", "退款成功", "2026A_2026B");
        assert_eq!(refund_of(&refund), Some("2026A"));
        assert_eq!(refund_of(&row(Direction::Expense, "x", "店", "交易成功", "2026A_2026B")), None);
    }
}
