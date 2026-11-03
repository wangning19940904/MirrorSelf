//! The built-in extractor: a deterministic layout parser for tabular lab
//! reports that fills the §5 schema.
//!
//! Words are grouped into rows (correcting the small rotation of a phone
//! photo), each row is read as name, value, flag, unit and reference range
//! in whatever order the report prints them, and names map to LOINC through
//! [`crate::labs`]. Confidence is the weakest of the row's parts: a value
//! from a PDF text layer is exact; a value read by OCR is trusted when the
//! second, English-only recognition pass read the same number.

use chrono::NaiveDate;

use super::{LabExtraction, LabResult, OcrDocument, OcrLine, OcrPage, ReportInfo};
use crate::labs::{self, NameMatch, RefRange};

#[derive(Debug, Clone)]
struct Cell {
    text: String,
    bbox: [f64; 4],
    conf: f64,
}

impl Cell {
    fn h(&self) -> f64 {
        (self.bbox[3] - self.bbox[1]).max(1.0)
    }
    fn cx(&self) -> f64 {
        (self.bbox[0] + self.bbox[2]) / 2.0
    }
    fn cy(&self) -> f64 {
        (self.bbox[1] + self.bbox[3]) / 2.0
    }
}

/// Longest printed name of a lab row (the longest alias is 41 characters);
/// longer text is a remark or an instruction, never a result.
const MAX_NAME: usize = 48;
/// Longest printed name accepted for an analyte without a LOINC mapping.
const MAX_UNKNOWN_NAME: usize = 32;

pub fn extract(ocr: &OcrDocument, lang: Option<&str>) -> LabExtraction {
    let mut results = Vec::new();
    let mut dates: Vec<(u8, NaiveDate)> = Vec::new();
    let mut provider = None;
    let mut urine = false;
    let mut cjk = 0usize;
    let mut latin = 0usize;

    for page in &ocr.pages {
        let exact = page.source == "text_layer";
        let alt = page.alt_lines.as_deref().map(cells).unwrap_or_default();
        let rows = group_rows(cells(&page.lines));
        for (i, row) in rows.iter().enumerate() {
            let text = row.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join(" ");
            for ch in text.chars() {
                if is_cjk(ch) {
                    cjk += 1;
                } else if ch.is_ascii_alphabetic() {
                    latin += 1;
                }
            }
            if let Some(d) = labelled_date(&text) {
                dates.push(d);
            }
            if page.page == 1 && i < 6 && provider.is_none() && looks_like_provider(&text) {
                provider = Some(labs::fold(&text).trim().to_string());
            }
            match parse_row(row) {
                Row::Section(title) => urine = is_urine_section(&title),
                Row::Result(r) => {
                    if let Some(res) = build(&r, page, &alt, exact, urine) {
                        results.push(res);
                    }
                }
                Row::Other => {}
            }
        }
    }
    dates.sort_by_key(|(priority, _)| *priority);
    let language = lang
        .map(str::to_string)
        .or_else(|| Some(if cjk * 4 >= latin.max(1) { "zh" } else { "en" }.to_string()));
    LabExtraction {
        report: ReportInfo {
            issued_at: dates.first().map(|(_, d)| d.to_string()),
            provider,
            language,
        },
        results,
    }
}

fn is_cjk(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c)
}

/// OCR lines → word cells, splitting lines that came without word boxes.
fn cells(lines: &[OcrLine]) -> Vec<Cell> {
    let mut out = Vec::new();
    for line in lines {
        if !line.words.is_empty() {
            out.extend(line.words.iter().filter(|w| !w.text.trim().is_empty()).map(|w| Cell {
                text: w.text.trim().to_string(),
                bbox: w.bbox,
                conf: w.confidence,
            }));
            continue;
        }
        let parts: Vec<&str> = line.text.split_whitespace().collect();
        let total: usize = parts.iter().map(|p| p.chars().count() + 1).sum();
        let width = line.bbox[2] - line.bbox[0];
        let mut x = line.bbox[0];
        for p in parts {
            let w = width * (p.chars().count() + 1) as f64 / total.max(1) as f64;
            out.push(Cell {
                text: p.to_string(),
                bbox: [x, line.bbox[1], x + w, line.bbox[3]],
                conf: line.confidence,
            });
            x += w;
        }
    }
    out
}

/// Median slope between horizontally adjacent words: the page's rotation.
fn skew(cells: &[Cell]) -> f64 {
    let mut slopes = Vec::new();
    for a in cells {
        let next = cells
            .iter()
            .filter(|b| b.bbox[0] > a.bbox[2] && (b.cy() - a.cy()).abs() < a.h() * 0.7)
            .min_by(|x, y| x.bbox[0].total_cmp(&y.bbox[0]));
        if let Some(b) = next {
            let dx = b.cx() - a.cx();
            if dx > a.h() * 2.0 {
                slopes.push((b.cy() - a.cy()) / dx);
            }
        }
    }
    if slopes.len() < 5 {
        return 0.0;
    }
    slopes.sort_by(f64::total_cmp);
    slopes[slopes.len() / 2].clamp(-0.1, 0.1)
}

fn group_rows(mut cells: Vec<Cell>) -> Vec<Vec<Cell>> {
    let s = skew(&cells);
    let key = |c: &Cell| c.cy() - s * c.cx();
    cells.sort_by(|a, b| key(a).total_cmp(&key(b)));
    let mut rows: Vec<(f64, f64, Vec<Cell>)> = Vec::new();
    for c in cells {
        let (y, h) = (key(&c), c.h());
        if let Some((cy, ch, row)) = rows.last_mut() {
            if (y - *cy).abs() < 0.5 * ch.max(h) {
                *cy = (*cy * row.len() as f64 + y) / (row.len() + 1) as f64;
                *ch = ch.max(h);
                row.push(c);
                continue;
            }
        }
        rows.push((y, h, vec![c]));
    }
    rows.into_iter()
        .map(|(_, _, mut row)| {
            row.sort_by(|a, b| a.bbox[0].total_cmp(&b.bbox[0]));
            merge_cells(row)
        })
        .collect()
}

fn is_range_op(s: &str) -> bool {
    matches!(labs::fold(s).trim(), "<" | ">" | "≤" | "≥" | "<=" | ">=" | "≦" | "≧")
}

fn is_dash(s: &str) -> bool {
    matches!(labs::fold(s).trim(), "-" | "~")
}

fn join(a: &Cell, b: &Cell) -> Cell {
    Cell {
        text: format!("{}{}", a.text, b.text),
        bbox: [a.bbox[0].min(b.bbox[0]), a.bbox[1].min(b.bbox[1]), a.bbox[2].max(b.bbox[2]), a.bbox[3].max(b.bbox[3])],
        conf: a.conf.min(b.conf),
    }
}

/// Re-joins ranges and units that OCR split into several words
/// (`3.5 - 9.5`, `< 5.20`, `×10⁹ /L`).
fn merge_cells(row: Vec<Cell>) -> Vec<Cell> {
    let mut out: Vec<Cell> = Vec::new();
    let close = |a: &Cell, b: &Cell| b.bbox[0] - a.bbox[2] < 1.5 * a.h().max(b.h());
    let mut i = 0;
    while i < row.len() {
        let c = &row[i];
        if let (Some(d), Some(e)) = (row.get(i + 1), row.get(i + 2)) {
            if labs::parse_number(&c.text).is_some() && is_dash(&d.text) && labs::parse_number(&e.text).is_some() && close(c, d) && close(d, e) {
                out.push(join(&join(c, d), e));
                i += 3;
                continue;
            }
        }
        if let Some(d) = row.get(i + 1) {
            let both = format!("{}{}", c.text, d.text);
            let range = close(c, d)
                && labs::parse_range(&both).is_some_and(|r| r.low.is_some() || r.high.is_some())
                && (is_range_op(&c.text) || c.text.ends_with(['-', '~']) || d.text.starts_with(['-', '~']));
            let unit = close(c, d)
                && labs::normalize_unit(&both).is_some()
                && (labs::normalize_unit(&c.text).is_none() || labs::normalize_unit(&d.text).is_none());
            if range || unit {
                out.push(join(c, d));
                i += 2;
                continue;
            }
        }
        out.push(c.clone());
        i += 1;
    }
    out
}

#[derive(Debug, Clone)]
enum Tok {
    Text,
    Num(f64),
    NumFlag(f64, String),
    Flag(String),
    Unit,
    Range(RefRange),
    Qual,
    Garbled,
}

/// A number with its flag attached: `3.60↑`, `↓98`, `5.9H`. OCR often reads
/// an attached `↑` as `个` or `T`; those count as `↑` (the flag-range check in
/// validation catches a wrong guess).
fn number_with_flag(t: &str) -> Option<(f64, String)> {
    let t = labs::fold(t);
    let t = t.trim();
    for (printed, flag) in [("个", "↑"), ("T", "↑")] {
        if let Some(rest) = t.strip_suffix(printed) {
            if let Some(v) = labs::parse_number(rest.trim()) {
                return Some((v, flag.to_string()));
            }
        }
    }
    for flag in ["↑↑", "↓↓", "↑", "↓", "*", "HH", "LL", "H", "L"] {
        if let Some(rest) = t.strip_suffix(flag) {
            if let Some(v) = labs::parse_number(rest.trim()) {
                return Some((v, flag.to_string()));
            }
        }
        if flag.chars().all(|c| !c.is_ascii_alphabetic()) {
            if let Some(rest) = t.strip_prefix(flag) {
                if let Some(v) = labs::parse_number(rest.trim()) {
                    return Some((v, flag.to_string()));
                }
            }
        }
    }
    None
}

/// Mostly digits but not a number: `3.6O`, `6.2l`.
fn garbled_number(t: &str) -> bool {
    let n = t.chars().count();
    let digits = t.chars().filter(|c| c.is_ascii_digit() || *c == '.' || *c == ',').count();
    t.chars().any(|c| c.is_ascii_digit()) && !t.chars().any(is_cjk) && digits * 10 >= n * 6
}

fn classify(text: &str) -> Tok {
    let t = labs::fold(text);
    let t = t.trim();
    if labs::is_qualitative_text(t) {
        Tok::Qual
    } else if labs::is_flag(t) {
        Tok::Flag(t.to_string())
    } else if let Some(v) = labs::parse_number(t) {
        Tok::Num(v)
    } else if let Some((v, f)) = number_with_flag(t) {
        Tok::NumFlag(v, f)
    } else if labs::normalize_unit(t).is_some() {
        Tok::Unit
    } else if let Some(r) = labs::parse_range(t) {
        Tok::Range(r)
    } else if garbled_number(t) {
        Tok::Garbled
    } else {
        Tok::Text
    }
}

#[derive(Debug)]
enum Value {
    Num(f64),
    /// Qualitative (`阴性`) or censored (`<0.10`).
    Text(String),
    Unreadable(String),
}

#[derive(Debug)]
struct ResultRow<'a> {
    name: String,
    value: Value,
    value_cell: Option<&'a Cell>,
    flag: Option<String>,
    unit: Option<&'a Cell>,
    range: Option<(RefRange, &'a Cell)>,
    cells: &'a [Cell],
}

enum Row<'a> {
    Section(String),
    Result(ResultRow<'a>),
    Other,
}

const HEADER_WORDS: &[&str] = &[
    "项目", "检验项目", "检查项目", "结果", "提示", "单位", "参考范围", "参考值", "参考区间", "test", "result", "flag",
    "units", "unit", "reference", "range", "interval",
];

fn parse_row(row: &[Cell]) -> Row<'_> {
    let toks: Vec<Tok> = row.iter().map(|c| classify(&c.text)).collect();
    let mut i = 0;
    let mut name = Vec::new();
    while i < row.len() && matches!(toks[i], Tok::Text) {
        name.push(row[i].text.as_str());
        i += 1;
    }
    if name.is_empty() {
        return Row::Other;
    }
    let joined = name.join(" ");
    if i == row.len() {
        let folded = labs::fold(&joined);
        let is_header = name.iter().all(|w| HEADER_WORDS.contains(&labs::fold(w).to_lowercase().trim()));
        if !is_header && name.len() <= 2 && folded.chars().count() <= 16 && !folded.contains(':') {
            return Row::Section(folded);
        }
        return Row::Other;
    }

    let mut flag = None;
    while i < row.len() {
        if let Tok::Flag(f) = &toks[i] {
            flag = Some(f.clone());
            i += 1;
        } else {
            break;
        }
    }
    let Some(tok) = toks.get(i) else { return Row::Other };
    let (value, value_cell) = match tok {
        Tok::Num(v) => (Value::Num(*v), Some(&row[i])),
        Tok::NumFlag(v, f) => {
            flag = Some(f.clone());
            (Value::Num(*v), Some(&row[i]))
        }
        Tok::Qual => (Value::Text(labs::fold(&row[i].text).trim().to_string()), Some(&row[i])),
        Tok::Range(r) if r.low.is_none() || r.high.is_none() => (Value::Text(r.text.clone()), Some(&row[i])),
        Tok::Garbled => (Value::Unreadable(row[i].text.clone()), Some(&row[i])),
        // OCR dropped the value (a lone `6` often vanishes): keep the row
        // so a known analyte still reaches the review queue.
        Tok::Unit | Tok::Range(_) => (Value::Unreadable(String::new()), None),
        _ => return Row::Other,
    };
    if value_cell.is_some() {
        i += 1;
    }

    let mut unit = None;
    let mut range = None;
    for (tok, cell) in toks[i..].iter().zip(&row[i..]) {
        match tok {
            Tok::Flag(f) if flag.is_none() => flag = Some(f.clone()),
            Tok::Unit if unit.is_none() => unit = Some(cell),
            Tok::Range(r) => range = Some((r.clone(), cell)),
            Tok::Qual if matches!(value, Value::Text(_)) => {
                range = labs::parse_range(&cell.text).map(|r| (r, cell));
            }
            _ => {}
        }
    }
    Row::Result(ResultRow { name: joined, value, value_cell, flag, unit, range, cells: row })
}

fn is_urine_section(title: &str) -> bool {
    let t = title.to_lowercase();
    (t.contains('尿') && (t.contains("常规") || t.contains("分析") || t.contains("检查"))) || t.contains("urin")
}

/// Second-pass cells at the same spot as `cell`, left to right.
fn overlapping<'a>(cell: &Cell, alt: &'a [Cell]) -> Vec<&'a Cell> {
    let pad = cell.h() * 0.5;
    let inside = |x: f64, y: f64, b: &[f64; 4], p: f64| x >= b[0] - p && x <= b[2] + p && y >= b[1] - p && y <= b[3] + p;
    let mut near: Vec<&Cell> = alt
        .iter()
        .filter(|a| inside(a.cx(), a.cy(), &cell.bbox, pad) || inside(cell.cx(), cell.cy(), &a.bbox, 0.0))
        .collect();
    near.sort_by(|a, b| a.bbox[0].total_cmp(&b.bbox[0]));
    near
}

/// Do the two passes agree on this cell? `Some(false)` only when the second
/// pass read a different value there; `None` when it read nothing usable.
fn agrees<T: PartialEq>(cell: &Cell, alt: &[Cell], read: impl Fn(&str) -> Option<T>) -> Option<bool> {
    let ours = read(&cell.text)?;
    let near = overlapping(cell, alt);
    let merged: String = near.iter().map(|a| a.text.as_str()).collect();
    let theirs: Vec<T> = near.iter().filter_map(|a| read(&a.text)).chain(read(&merged)).collect();
    if theirs.is_empty() {
        None
    } else {
        Some(theirs.contains(&ours))
    }
}

/// Second-pass cells in the row's band between `after` and `before` (x).
fn in_band<'a>(row: &[Cell], alt: &'a [Cell], after: f64, before: f64) -> impl Iterator<Item = &'a Cell> {
    let top = row.iter().map(|c| c.bbox[1]).fold(f64::INFINITY, f64::min);
    let bottom = row.iter().map(|c| c.bbox[3]).fold(f64::NEG_INFINITY, f64::max);
    alt.iter()
        .filter(move |a| a.cy() > top && a.cy() < bottom && a.bbox[0] >= after && a.bbox[2] <= before)
}

/// Digits after the decimal point of the last number in `s`.
fn decimals(s: &str) -> usize {
    let t = labs::fold(s);
    let last = t.rsplit(|c: char| !(c.is_ascii_digit() || c == '.')).find(|p| p.chars().any(|c| c.is_ascii_digit()));
    last.and_then(|p| p.split_once('.')).map_or(0, |(_, frac)| frac.chars().take_while(char::is_ascii_digit).count())
}

fn read_number(s: &str) -> Option<u64> {
    labs::parse_number(s)
        .or_else(|| number_with_flag(s).map(|(v, _)| v))
        .map(|v| (v * 1e6).round() as u64)
}

fn read_range(s: &str) -> Option<(Option<u64>, Option<u64>)> {
    let r = labs::parse_range(s)?;
    let q = |v: Option<f64>| v.map(|v| (v * 1e6).round() as u64);
    (r.low.is_some() || r.high.is_some()).then_some((q(r.low), q(r.high)))
}

/// Confidence of an OCR-read number: exact from a text layer, confirmed by
/// the second pass, contradicted by it, or unconfirmed.
fn checked(cell: &Cell, exact: bool, agreement: Option<bool>) -> f64 {
    if exact {
        return 1.0;
    }
    match agreement {
        Some(true) => 1.0,
        Some(false) => cell.conf.min(0.5),
        None => cell.conf.min(0.85),
    }
}

fn build(r: &ResultRow<'_>, page: &OcrPage, alt: &[Cell], exact: bool, urine: bool) -> Option<LabResult> {
    // Remarks and instructions are never lab rows, whatever words they contain.
    let length = labs::fold(&r.name).chars().count();
    if length > MAX_NAME {
        return None;
    }
    // `白细胞计数 WBC` (name and abbreviation columns): the whole name, then each part.
    let matched = labs::match_name(&r.name, urine).or_else(|| {
        let parts: Vec<&str> = r.name.split(' ').collect();
        (2..=3).contains(&parts.len()).then(|| parts.iter().find_map(|p| labs::match_name(p, urine))).flatten()
    });
    if matched.is_none() && (r.unit.is_none() && r.range.is_none() || length > MAX_UNKNOWN_NAME) {
        return None;
    }
    let name_c: f64 = match matched {
        Some((_, NameMatch::Exact)) => 1.0,
        Some((_, NameMatch::Fuzzy)) => 0.85,
        None => 0.5,
    };
    let analyte = matched.map(|(a, _)| a);
    if matches!(&r.value, Value::Unreadable(t) if t.is_empty()) && analyte.is_none() {
        return None;
    }
    let name_end = r.cells.iter().take_while(|c| r.value_cell.is_none_or(|v| !std::ptr::eq(*c, v))).fold(0.0, |x: f64, c| x.max(c.bbox[2]));

    let (value, value_text, value_c) = match &r.value {
        Value::Num(v) => {
            let cell = r.value_cell.expect("numeric values have a cell");
            let mut c = checked(cell, exact, agrees(cell, alt, read_number));
            // `5.77↑` read as `5.771`: more decimals than the printed range
            // suggests an attached flag that OCR turned into a digit.
            let range_decimals = r.range.as_ref().map_or(2, |(rr, _)| decimals(&rr.text));
            if !exact && decimals(&cell.text) >= 3 && decimals(&cell.text) > range_decimals + 1 {
                c = c.min(0.6);
            }
            (Some(*v), None, c)
        }
        Value::Text(t) if labs::is_qualitative_text(t) => (None, Some(t.clone()), 1.0),
        Value::Text(t) => {
            let cell = r.value_cell.expect("text values have a cell");
            (None, Some(t.clone()), checked(cell, exact, agrees(cell, alt, read_range)))
        }
        Value::Unreadable(t) => {
            // The second pass may have read what the first could not; a
            // single reading is never trusted on its own (confidence ≤ 0.85).
            let first = r.unit.map(|c| c.bbox[0]).or(r.range.as_ref().map(|(_, c)| c.bbox[0])).unwrap_or(f64::INFINITY);
            let from_alt = if exact {
                None
            } else {
                let start = if t.is_empty() { name_end } else { r.value_cell.map_or(name_end, |c| c.bbox[0] - 1.0) };
                in_band(r.cells, alt, start, first).find_map(|c| {
                    labs::parse_number(&c.text).or_else(|| number_with_flag(&c.text).map(|(v, _)| v)).map(|v| (v, c.conf))
                })
            };
            match from_alt {
                Some((v, c)) => (Some(v), None, c.min(0.85)),
                None => (None, (!t.is_empty()).then(|| t.clone()), 0.0),
            }
        }
    };
    // A unit or flag the first pass misread (`UIL` for `U/L`) or missed: the
    // English-only pass reads Latin text better. A unit is only taken from
    // it when it is valid for this analyte.
    let after = r.value_cell.map_or(r.cells[0].bbox[2], |c| c.bbox[2]);
    let before = r.range.as_ref().map_or(f64::INFINITY, |(_, c)| c.bbox[0]);
    let unit_text = r.unit.map(|c| c.text.trim().to_string()).or_else(|| {
        let a = analyte.filter(|a| !a.qualitative)?;
        in_band(r.cells, alt, after, before)
            .find(|c| labs::normalize_unit(&c.text).is_some_and(|u| a.factor(u).is_some()))
            .map(|c| c.text.trim().to_string())
    });
    let flag = r.flag.clone().or_else(|| {
        let taken: Vec<&Cell> = r.unit.into_iter().chain(r.range.as_ref().map(|(_, c)| *c)).collect();
        in_band(r.cells, alt, after, f64::INFINITY)
            .filter(|c| !taken.iter().any(|t| c.cx() >= t.bbox[0] && c.cx() <= t.bbox[2]))
            .find(|c| matches!(labs::fold(&c.text).trim(), "↑" | "↓" | "H" | "L"))
            .map(|c| labs::fold(&c.text).trim().to_string())
    });
    let unit_c = match unit_text.as_deref() {
        None => 1.0,
        Some(text) => match labs::normalize_unit(text) {
            Some(u) if analyte.is_some_and(|a| a.factor(u).is_some()) => 1.0,
            Some(_) if exact => 1.0,
            Some(_) => r.unit.map_or(0.85, |c| c.conf),
            None => 0.3,
        },
    };
    // A range the second pass did not read keeps the first pass's own
    // confidence; one it read differently is doubtful.
    let range_c = match &r.range {
        None => 1.0,
        Some((rr, _)) if rr.low.is_none() && rr.high.is_none() => 1.0,
        Some(_) if exact => 1.0,
        Some((_, cell)) => match agrees(cell, alt, read_range) {
            Some(true) => 1.0,
            Some(false) => cell.conf.min(0.5),
            None => cell.conf,
        },
    };
    let confidence = (name_c.min(value_c).min(unit_c).min(range_c) * 100.0).round() / 100.0;

    let mut bbox = r.cells[0].bbox;
    for c in r.cells {
        bbox = [bbox[0].min(c.bbox[0]), bbox[1].min(c.bbox[1]), bbox[2].max(c.bbox[2]), bbox[3].max(c.bbox[3])];
    }
    let (ref_low, ref_high, ref_text) = match &r.range {
        Some((rr, _)) => (rr.low, rr.high, Some(rr.text.clone())),
        None => (None, None, None),
    };
    Some(LabResult {
        analyte_name: labs::fold(&r.name).trim().chars().take(80).collect(),
        loinc: analyte.map(|a| a.loinc.to_string()),
        value,
        value_text,
        unit: unit_text,
        ref_low,
        ref_high,
        flag_as_printed: flag,
        page: page.page,
        bbox: bbox.map(|v| (v * 100.0).round() / 100.0),
        confidence,
        ref_text,
    })
}

const PROVIDER_WORDS: &[&str] = &[
    "医院", "体检中心", "中心", "诊所", "门诊", "检验所", "实验室", "卫生院", "clinic", "hospital", "medical", "laborator",
    "health", "centre", "center", "diagnostic",
];

fn looks_like_provider(text: &str) -> bool {
    let t = labs::fold(text).to_lowercase();
    !t.contains(':') && find_date(&t).is_none() && PROVIDER_WORDS.iter().any(|w| t.contains(w))
}

const DATE_LABELS: &[(&str, u8)] = &[
    ("报告日期", 0), ("报告时间", 0), ("report date", 0), ("reported", 0),
    ("体检日期", 1), ("检查日期", 1), ("检验日期", 1), ("exam date", 1), ("examination date", 1), ("test date", 1),
    ("采样", 2), ("采集", 2), ("送检", 2), ("collected", 2), ("collection", 2),
    ("出生", 9), ("birth", 9), ("dob", 9),
    ("日期", 3), ("date", 3),
];

/// The best-labelled date in a row, ranked by the label just before it:
/// the report date first, then the examination date, the collection date,
/// any other date. Birth dates never count (a header row can hold both).
fn labelled_date(text: &str) -> Option<(u8, NaiveDate)> {
    let t = labs::fold(text).to_lowercase();
    let dates = find_dates(&t);
    let mut previous = 0;
    let mut best: Option<(u8, NaiveDate)> = None;
    for (pos, date) in dates {
        // The label is whatever sits between the previous date and this one.
        let segment = &t[previous..pos];
        previous = pos;
        let priority = DATE_LABELS
            .iter()
            .filter(|(w, _)| segment.contains(w))
            .map(|(_, p)| *p)
            .max_by_key(|p| (*p == 9, std::cmp::Reverse(*p)))
            .unwrap_or(4);
        if priority != 9 && best.is_none_or(|(p, _)| priority < p) {
            best = Some((priority, date));
        }
    }
    best
}

const MONTHS: &[&str] = &["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

fn find_date(t: &str) -> Option<NaiveDate> {
    find_dates(t).into_iter().next().map(|(_, d)| d)
}

/// Every date in `t` with its byte offset: `2024-06-12`, `2024/6/12`,
/// `2024.06.12`, `2024年6月12日`, `12 Jun 2025`, `Jun 12, 2025`.
fn find_dates(t: &str) -> Vec<(usize, NaiveDate)> {
    let chars: Vec<(usize, char)> = t.char_indices().collect();
    let num_at = |i: usize, max: usize| -> Option<(u32, usize)> {
        let mut j = i;
        while j < chars.len() && j - i < max && chars[j].1.is_ascii_digit() {
            j += 1;
        }
        let s: String = chars[i..j].iter().map(|(_, c)| c).collect();
        s.parse().ok().map(|v| (v, j))
    };
    let mut out = Vec::new();
    for i in 0..chars.len() {
        if i > 0 && chars[i - 1].1.is_ascii_digit() {
            continue;
        }
        let Some((y, j)) = num_at(i, 4) else { continue };
        if j - i != 4 || !(1900..=2100).contains(&y) || j >= chars.len() || !matches!(chars[j].1, '-' | '/' | '.' | '年') {
            continue;
        }
        let Some((m, k)) = num_at(j + 1, 2) else { continue };
        if k >= chars.len() || !matches!(chars[k].1, '-' | '/' | '.' | '月') {
            continue;
        }
        if let Some((d, _)) = num_at(k + 1, 2) {
            if let Some(date) = NaiveDate::from_ymd_opt(y as i32, m, d) {
                out.push((chars[i].0, date));
            }
        }
    }
    let mut words: Vec<(usize, &str)> = Vec::new();
    let mut start = None;
    for (i, c) in t.char_indices().chain(std::iter::once((t.len(), ' '))) {
        match (c.is_ascii_alphanumeric(), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                words.push((s, &t[s..i]));
                start = None;
            }
            _ => {}
        }
    }
    for (i, (pos, w)) in words.iter().enumerate() {
        let Some(m) = MONTHS.iter().position(|m| w.starts_with(m)) else { continue };
        let num = |k: usize| words.get(k).and_then(|(_, s)| s.parse::<u32>().ok());
        let (day, year, at) = match (i.checked_sub(1).and_then(num), num(i + 1), num(i + 2)) {
            (Some(d), Some(y), _) if y > 1900 => (d, y, words[i - 1].0),
            (_, Some(d), Some(y)) if y > 1900 => (d, y, *pos),
            _ => continue,
        };
        if let Some(date) = NaiveDate::from_ymd_opt(year as i32, m as u32 + 1, day) {
            out.push((at, date));
        }
    }
    out.sort_by_key(|(p, _)| *p);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::OcrWord;

    fn line(y: f64, words: &[(&str, f64)]) -> OcrLine {
        OcrLine {
            text: words.iter().map(|w| w.0).collect::<Vec<_>>().join(" "),
            bbox: [words[0].1, y, 500.0, y + 12.0],
            confidence: 1.0,
            words: words
                .iter()
                .map(|(t, x)| OcrWord { text: t.to_string(), bbox: [*x, y, x + 8.0 * t.chars().count() as f64, y + 12.0], confidence: 1.0 })
                .collect(),
        }
    }

    fn doc(lines: Vec<OcrLine>, source: &str) -> OcrDocument {
        OcrDocument {
            engine: "test".into(),
            engine_version: "1".into(),
            pages: vec![OcrPage { page: 1, width: 595.0, height: 842.0, source: source.into(), lines, alt_lines: None }],
        }
    }

    #[test]
    fn reads_rows_in_any_column_order() {
        let d = doc(
            vec![
                line(20.0, &[("青松健康体检中心（示例）", 200.0)]),
                line(40.0, &[("报告日期：2024年06月12日", 50.0)]),
                line(60.0, &[("血脂", 50.0)]),
                line(80.0, &[("项目", 50.0), ("结果", 250.0), ("提示", 315.0), ("单位", 360.0), ("参考范围", 460.0)]),
                line(100.0, &[("低密度脂蛋白胆固醇(LDL-C)", 50.0), ("3.60", 250.0), ("↑", 315.0), ("mmol/L", 360.0), ("0-3.40", 460.0)]),
                line(120.0, &[("Glucose,", 50.0), ("fasting", 110.0), ("5.3", 250.0), ("3.9", 300.0), ("-", 330.0), ("6.1", 340.0), ("mmol/L", 400.0)]),
                line(140.0, &[("尿常规", 50.0)]),
                line(160.0, &[("葡萄糖(GLU)", 50.0), ("阴性", 250.0), ("阴性", 460.0)]),
                line(180.0, &[("年龄", 50.0), ("44", 250.0), ("岁", 300.0)]),
            ],
            "text_layer",
        );
        let x = extract(&d, None);
        assert_eq!(x.report.issued_at.as_deref(), Some("2024-06-12"));
        assert_eq!(x.report.provider.as_deref(), Some("青松健康体检中心(示例)"));
        assert_eq!(x.report.language.as_deref(), Some("zh"));
        assert_eq!(x.results.len(), 3, "{:#?}", x.results);
        let ldl = &x.results[0];
        assert_eq!((ldl.loinc.as_deref(), ldl.value, ldl.unit.as_deref()), (Some("13457-7"), Some(3.6), Some("mmol/L")));
        assert_eq!((ldl.ref_low, ldl.ref_high, ldl.flag_as_printed.as_deref()), (Some(0.0), Some(3.4), Some("↑")));
        assert_eq!(ldl.confidence, 1.0);
        let glu = &x.results[1];
        assert_eq!((glu.loinc.as_deref(), glu.ref_low, glu.ref_high), (Some("1558-6"), Some(3.9), Some(6.1)));
        let urine = &x.results[2];
        assert_eq!((urine.loinc.as_deref(), urine.value_text.as_deref()), (Some("25428-4"), Some("阴性")));
    }

    #[test]
    fn dates_in_many_formats() {
        let d = |s: &str| find_date(&s.to_lowercase()).map(|d| d.to_string());
        assert_eq!(d("2024/6/12").as_deref(), Some("2024-06-12"));
        assert_eq!(d("报告日期：2024年6月12日").as_deref(), Some("2024-06-12"));
        assert_eq!(d("Report date: 12 Jun 2025").as_deref(), Some("2025-06-12"));
        assert_eq!(d("Collected Jun 3, 2025 08:10").as_deref(), Some("2025-06-03"));
        assert_eq!(d("体检编号：T20240612"), None);
        let l = |s: &str| labelled_date(s).map(|(p, d)| (p, d.to_string()));
        assert_eq!(l("DOB: 1983-01-01 Reported: Mar 14, 2025"), Some((0, "2025-03-14".into())));
        assert_eq!(l("出生日期：1983-01-01"), None);
        assert_eq!(l("采样时间：2024-06-11 报告日期：2024-06-12"), Some((0, "2024-06-12".into())));
    }
}
