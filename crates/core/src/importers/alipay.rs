//! Alipay bill CSV reader (支付宝 → 我的 → 账单 → 开具交易流水证明 / 导出账单).
//!
//! The file Alipay emails is GBK-encoded CSV with LF line endings. An account
//! preamble (export period, export time, record counts and notes) comes
//! before the header row, cells are padded with spaces, order numbers end in
//! a tab, and every line ends with a comma. A typical file starts:
//!
//! ```text
//! ------------------------------------------------------------------------------------
//! 导出信息：
//! 姓名：…
//! 起始时间：[2026-09-21 00:00:00]    终止时间：[2026-10-20 23:59:59]
//! 导出时间：[2026-10-20 10:15:00]
//! …
//! ------------------------支付宝（中国）网络技术有限公司  电子客户回单------------------------
//! 交易时间,交易分类,交易对方,对方账号,商品说明,收/支,金额,收/付款方式,交易状态,交易订单号,商家订单号,备注,
//! 2026-10-20 08:00:00,餐饮美食,…,支出,12.00,余额,交易成功,2026…\t,…\t,,
//! ```
//!
//! The header row is located by its column names, not its line number, and
//! columns are looked up by name, so reordered or missing optional columns,
//! UTF-8 re-saves (with or without BOM), CRLF endings and trailing summary
//! lines are all tolerated. Times are Beijing time without an offset.

use std::borrow::Cow;

use anyhow::Result;
use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone};
use serde::Serialize;

use crate::error::invalid;

pub const SOURCE: &str = "alipay";
pub const KIND: &str = "alipay_bill_csv";
pub const MIME: &str = "text/csv";
/// Beijing time; the file's timestamps carry no offset.
pub const TZ_OFFSET: i32 = 8 * 3600;
pub const CURRENCY: &str = "CNY";

/// Records searched for the header row. The preamble is about two dozen lines.
const HEADER_SEARCH_RECORDS: usize = 80;
/// Bytes decoded when detecting a format: enough for the preamble and header.
const DETECT_PREFIX: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Col {
    Time,
    Category,
    Counterparty,
    CounterpartyAccount,
    Item,
    Direction,
    Amount,
    PaymentMethod,
    Status,
    OrderNo,
    MerchantOrderNo,
    Note,
}

/// Header names per column. The first name is the current export; the rest
/// are spellings seen in older or web exports.
const COLUMNS: &[(Col, &[&str])] = &[
    (Col::Time, &["交易时间", "交易创建时间"]),
    (Col::Category, &["交易分类"]),
    (Col::Counterparty, &["交易对方"]),
    (Col::CounterpartyAccount, &["对方账号"]),
    (Col::Item, &["商品说明", "商品名称"]),
    (Col::Direction, &["收/支"]),
    (Col::Amount, &["金额", "金额（元）", "金额(元)"]),
    (Col::PaymentMethod, &["收/付款方式"]),
    (Col::Status, &["交易状态"]),
    (Col::OrderNo, &["交易订单号", "交易号"]),
    (Col::MerchantOrderNo, &["商家订单号"]),
    (Col::Note, &["备注"]),
];

/// Columns a header must have to be an Alipay bill. `交易订单号` / `交易号`
/// and `交易状态` are Alipay's own names (WeChat Pay says `交易单号` and
/// `当前状态`), so other payment exports are not claimed.
const REQUIRED: &[Col] = &[Col::Time, Col::Counterparty, Col::Direction, Col::Amount, Col::Status, Col::OrderNo];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Expense,
    Income,
    /// 不计收支 (top-ups, withdrawals, transfers between own accounts,
    /// investments, repayments, refunds) and rows with an empty or unknown 收/支.
    Neutral,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Expense => "expense",
            Direction::Income => "income",
            Direction::Neutral => "neutral",
        }
    }

    fn from_label(label: &str) -> Self {
        match label {
            "支出" => Direction::Expense,
            "收入" => Direction::Income,
            _ => Direction::Neutral,
        }
    }
}

/// One transaction row, with cells trimmed and `/` placeholders removed.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// 1-based line in the decoded file, for error messages.
    pub line: usize,
    pub time: DateTime<FixedOffset>,
    pub category: Option<String>,
    pub counterparty: Option<String>,
    pub counterparty_account: Option<String>,
    pub item: Option<String>,
    pub direction: Direction,
    /// 收/支 as written.
    pub direction_label: Option<String>,
    /// Exact amount in fen (1/100 CNY), never negative.
    pub amount_minor: i64,
    pub payment_method: Option<String>,
    pub status: Option<String>,
    pub order_no: Option<String>,
    pub merchant_order_no: Option<String>,
    pub note: Option<String>,
}

/// What the preamble says about the export.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BillInfo {
    /// 姓名: the account holder, used to recognise transfers to oneself.
    pub account_name: Option<String>,
    /// 导出时间, Beijing time.
    pub exported_at: Option<DateTime<FixedOffset>>,
    pub period_start: Option<DateTime<FixedOffset>>,
    pub period_end: Option<DateTime<FixedOffset>>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ParseStats {
    pub encoding: &'static str,
    pub header_line: usize,
    pub rows: u64,
    /// Rows with a timestamp but an unreadable amount; skipped.
    pub malformed: u64,
    /// Lines after the header that are not transactions (summaries, rules).
    pub skipped_lines: u64,
    /// The first few malformed lines, for the run report.
    pub malformed_lines: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct Bill {
    pub info: BillInfo,
    pub rows: Vec<Row>,
    pub stats: ParseStats,
}

/// The MIME type when `content` is an Alipay bill: decodable text whose
/// header row has the Alipay column names.
pub fn detect(content: &[u8]) -> Option<&'static str> {
    let head = &content[..content.len().min(DETECT_PREFIX)];
    // Binary files (images, archives) stop here.
    if head.contains(&0) {
        return None;
    }
    // Cut at the last newline so a truncated multi-byte character does not
    // fail decoding. 0x0A never occurs inside a GBK or GB18030 character.
    let head = if content.len() > DETECT_PREFIX {
        &head[..head.iter().rposition(|&b| b == b'\n')? + 1]
    } else {
        head
    };
    let (text, _) = decode(head)?;
    let records = Records::new(&text);
    records
        .take(HEADER_SEARCH_RECORDS)
        .any(|(_, cells)| header_columns(&cells).is_some())
        .then_some(MIME)
}

pub fn parse(content: &[u8]) -> Result<Bill> {
    let (text, encoding) =
        decode(content).ok_or_else(|| invalid("not an Alipay bill: the file is neither UTF-8 nor GBK text"))?;
    let mut stats = ParseStats {
        encoding,
        ..Default::default()
    };
    let mut info = BillInfo::default();
    let mut records = Records::new(&text);

    let mut columns = None;
    for (line, cells) in records.by_ref().take(HEADER_SEARCH_RECORDS) {
        if let Some(found) = header_columns(&cells) {
            stats.header_line = line;
            columns = Some(found);
            break;
        }
        read_preamble(&cells, &mut info);
    }
    let Some(columns) = columns else {
        return Err(invalid("not an Alipay bill: no header row with 交易时间, 交易对方, 收/支, 金额, 交易状态 and 交易订单号"));
    };
    let get = |cells: &[String], col: Col| -> Option<String> {
        columns
            .iter()
            .find(|(c, _)| *c == col)
            .and_then(|(_, i)| cells.get(*i))
            .map(|s| clean(s))
            .filter(|s| !s.is_empty() && s != "/")
    };

    let mut rows = Vec::new();
    for (line, cells) in records {
        if cells.iter().all(|c| clean(c).is_empty()) {
            continue;
        }
        let Some(time) = get(&cells, Col::Time).and_then(|t| parse_time(&t)) else {
            // Summary lines, dashes and notes after the table.
            stats.skipped_lines += 1;
            continue;
        };
        let Some(amount_minor) = get(&cells, Col::Amount).and_then(|a| parse_amount(&a)) else {
            stats.malformed += 1;
            if stats.malformed_lines.len() < 10 {
                stats.malformed_lines.push(line);
            }
            continue;
        };
        let direction_label = get(&cells, Col::Direction);
        rows.push(Row {
            line,
            time,
            category: get(&cells, Col::Category),
            counterparty: get(&cells, Col::Counterparty),
            counterparty_account: get(&cells, Col::CounterpartyAccount),
            item: get(&cells, Col::Item),
            direction: Direction::from_label(direction_label.as_deref().unwrap_or("")),
            direction_label,
            amount_minor,
            payment_method: get(&cells, Col::PaymentMethod),
            status: get(&cells, Col::Status),
            order_no: get(&cells, Col::OrderNo),
            merchant_order_no: get(&cells, Col::MerchantOrderNo),
            note: get(&cells, Col::Note),
        });
    }
    stats.rows = rows.len() as u64;
    Ok(Bill { info, rows, stats })
}

/// UTF-8 (a BOM is dropped) or else GBK, read as its superset GB18030.
/// `None` when the bytes are neither.
fn decode(content: &[u8]) -> Option<(Cow<'_, str>, &'static str)> {
    if let Some(rest) = content.strip_prefix(b"\xEF\xBB\xBF") {
        return std::str::from_utf8(rest).ok().map(|s| (Cow::Borrowed(s), "utf-8"));
    }
    if let Ok(s) = std::str::from_utf8(content) {
        return Some((Cow::Borrowed(s), "utf-8"));
    }
    encoding_rs::GB18030
        .decode_without_bom_handling_and_without_replacement(content)
        .map(|s| (s, "gbk"))
}

/// Column positions when `cells` is an Alipay header row.
fn header_columns(cells: &[String]) -> Option<Vec<(Col, usize)>> {
    let names: Vec<String> = cells.iter().map(|c| clean(c)).collect();
    let mut found = Vec::new();
    for (col, aliases) in COLUMNS {
        if let Some(i) = names.iter().position(|n| aliases.contains(&n.as_str())) {
            found.push((*col, i));
        }
    }
    REQUIRED
        .iter()
        .all(|r| found.iter().any(|(c, _)| c == r))
        .then_some(found)
}

/// Picks the account holder and export times out of a preamble line.
fn read_preamble(cells: &[String], info: &mut BillInfo) {
    let line = cells.join(",");
    let line = line.trim();
    if let Some(name) = line.strip_prefix("姓名：").or_else(|| line.strip_prefix("姓名:")) {
        let name = clean(name);
        if !name.is_empty() {
            info.account_name = Some(name);
        }
    }
    let bracketed = |key: &str| -> Option<DateTime<FixedOffset>> {
        let rest = &line[line.find(key)? + key.len()..];
        let rest = rest.trim_start_matches(['：', ':', ' ']).strip_prefix('[')?;
        parse_time(&rest[..rest.find(']')?])
    };
    if let Some(t) = bracketed("导出时间") {
        info.exported_at = Some(t);
    }
    if let Some(t) = bracketed("起始时间").or_else(|| bracketed("起始日期")) {
        info.period_start = Some(t);
    }
    if let Some(t) = bracketed("终止时间").or_else(|| bracketed("终止日期")) {
        info.period_end = Some(t);
    }
}

/// Trims spaces, tabs and full-width spaces, and Excel's `="…"` text guard.
fn clean(cell: &str) -> String {
    let t = cell.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    let t = t
        .strip_prefix("=\"")
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(t);
    t.trim_matches(|c: char| c.is_whitespace()).to_string()
}

/// Beijing wall-clock time in the formats Alipay and spreadsheet re-saves use.
pub fn parse_time(s: &str) -> Option<DateTime<FixedOffset>> {
    let s = s.trim();
    let naive = ["%Y-%m-%d %H:%M:%S", "%Y/%m/%d %H:%M:%S", "%Y-%m-%d %H:%M", "%Y/%m/%d %H:%M"]
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(s, f).ok())?;
    FixedOffset::east_opt(TZ_OFFSET)?.from_local_datetime(&naive).single()
}

/// `25.00`, `¥1,234.5`, `3` → fen. Negative or over-precise amounts are
/// rejected rather than rounded: the 收/支 column carries the sign.
pub fn parse_amount(s: &str) -> Option<i64> {
    let s: String = s
        .trim()
        .trim_start_matches(['¥', '￥'])
        .chars()
        .filter(|c| *c != ',')
        .collect();
    let (whole, frac) = s.split_once('.').unwrap_or((&s, ""));
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if frac.len() > 2 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let yuan: i64 = whole.parse().ok()?;
    let fen: i64 = format!("{frac:0<2}").parse().ok()?;
    yuan.checked_mul(100)?.checked_add(fen)
}

/// CSV records with their 1-based starting line: commas, `"` quoting with
/// `""` escapes, LF or CRLF. A quote inside an unquoted cell is literal.
struct Records<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    line: usize,
}

impl<'a> Records<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            chars: text.chars().peekable(),
            line: 1,
        }
    }
}

impl Iterator for Records<'_> {
    type Item = (usize, Vec<String>);

    fn next(&mut self) -> Option<Self::Item> {
        self.chars.peek()?;
        let start = self.line;
        let mut cells = Vec::new();
        let mut cell = String::new();
        let mut quoted = false;
        let mut at_cell_start = true;
        while let Some(c) = self.chars.next() {
            if quoted {
                match c {
                    '"' if self.chars.peek() == Some(&'"') => {
                        self.chars.next();
                        cell.push('"');
                    }
                    '"' => quoted = false,
                    '\n' => {
                        self.line += 1;
                        cell.push(c);
                    }
                    _ => cell.push(c),
                }
                continue;
            }
            match c {
                '"' if at_cell_start => {
                    quoted = true;
                    at_cell_start = false;
                }
                ',' => {
                    cells.push(std::mem::take(&mut cell));
                    at_cell_start = true;
                }
                '\r' if self.chars.peek() == Some(&'\n') => {}
                '\n' => {
                    self.line += 1;
                    break;
                }
                _ => {
                    // Leading padding before a quote still opens a quoted cell.
                    if !(c == ' ' || c == '\t') {
                        at_cell_start = false;
                    }
                    cell.push(c);
                }
            }
        }
        cells.push(cell);
        Some((start, cells))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gbk(s: &str) -> Vec<u8> {
        encoding_rs::GB18030.encode(s).0.into_owned()
    }

    const HEADER: &str = "交易时间,交易分类,交易对方,对方账号,商品说明,收/支,金额,收/付款方式,交易状态,交易订单号,商家订单号,备注,";

    #[test]
    fn amounts_are_exact_fen() {
        assert_eq!(parse_amount("25.00"), Some(2500));
        assert_eq!(parse_amount(" 9.9 "), Some(990));
        assert_eq!(parse_amount("3"), Some(300));
        assert_eq!(parse_amount("¥1,234.56"), Some(123456));
        assert_eq!(parse_amount("222228.50"), Some(22222850));
        assert_eq!(parse_amount("-5.00"), None);
        assert_eq!(parse_amount("1.005"), None);
        assert_eq!(parse_amount(""), None);
        assert_eq!(parse_amount("abc"), None);
    }

    #[test]
    fn records_handle_quotes_padding_and_crlf() {
        let text = "a, \"b,c\" ,\"say \"\"hi\"\"\"\r\n\"multi\nline\",x\nlast";
        let recs: Vec<_> = Records::new(text).collect();
        assert_eq!(recs[0], (1, vec!["a".into(), " b,c ".into(), "say \"hi\"".into()]));
        assert_eq!(recs[1], (2, vec!["multi\nline".into(), "x".into()]));
        assert_eq!(recs[2], (4, vec!["last".into()]));
    }

    #[test]
    fn header_is_found_by_name_in_any_order_and_trailers_are_skipped() {
        let text =
            "\u{feff}导出信息：\n姓名：测试用户\n导出时间：[2026-10-20 10:15:00]\n\n\
             金额 ,交易订单号 ,交易时间 ,收/支 ,交易对方 ,交易状态 ,\n\
             12.50 ,A001\t,2026-10-01 08:00:00 ,支出 ,晨光早餐铺 ,交易成功 ,\n\
             3.00 ,A002\t,2026-10-01 23:30:00 ,收入 ,/ ,交易成功 ,\n\
             ------------------------------------\n共2笔记录\n";
        let bill = parse(text.as_bytes()).unwrap();
        assert_eq!(bill.stats.encoding, "utf-8");
        assert_eq!(bill.stats.header_line, 5);
        assert_eq!(bill.stats.skipped_lines, 2);
        assert_eq!(bill.info.account_name.as_deref(), Some("测试用户"));
        assert_eq!(bill.info.exported_at, parse_time("2026-10-20 10:15:00"));
        assert_eq!(bill.rows.len(), 2);
        let r = &bill.rows[1];
        assert_eq!(r.order_no.as_deref(), Some("A002"));
        assert_eq!(r.counterparty, None);
        assert_eq!(r.category, None);
        assert_eq!((r.direction, r.amount_minor), (Direction::Income, 300));
        assert_eq!(r.time.to_rfc3339(), "2026-10-01T23:30:00+08:00");
    }

    #[test]
    fn gbk_bills_are_detected_and_other_csvs_are_not() {
        let bill = gbk(&format!("导出信息：\n{HEADER}\n2026-10-01 08:00:00,餐饮美食,晨光早餐铺,/,豆浆,支出,12.50,余额,交易成功,X1\t,,,\n"));
        assert_eq!(detect(&bill), Some(MIME));
        let parsed = parse(&bill).unwrap();
        assert_eq!(parsed.stats.encoding, "gbk");
        assert_eq!(parsed.rows[0].counterparty.as_deref(), Some("晨光早餐铺"));
        assert_eq!(parsed.rows[0].counterparty_account, None);

        // WeChat Pay's export shares some column names but not the order or
        // status columns.
        let wechat = "交易时间,交易类型,交易对方,商品,收/支,金额(元),支付方式,当前状态,交易单号,商户单号,备注\n";
        let plain = "date,amount,description\n2026-10-01,12.50,coffee\n";
        for other in [gbk(wechat), plain.as_bytes().to_vec(), b"\x89PNG\r\n\x1a\n\0\0".to_vec(), vec![]] {
            assert_eq!(detect(&other), None);
            assert!(parse(&other).is_err());
        }
    }

    #[test]
    fn rows_without_a_readable_amount_are_counted_as_malformed() {
        let text = format!("{HEADER}\n2026-10-01 08:00:00,,A,,,支出,abc,,交易成功,X1,,,\n");
        let bill = parse(text.as_bytes()).unwrap();
        assert_eq!((bill.stats.rows, bill.stats.malformed), (0, 1));
        assert_eq!(bill.stats.malformed_lines, vec![2]);
    }
}
