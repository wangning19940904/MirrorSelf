//! Extraction eval (architecture §13, M2 acceptance): scores analyte, value
//! and unit accuracy against synthetic reports with known truth.
//!
//! "After review" counts a value as correct when it was extracted correctly
//! or routed to the review queue, where the user sees and fixes it. A wrong
//! value that skipped the queue (`unflagged_errors`) is the failure that
//! matters most: it would reach agents as fact.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::importers::report;
use crate::labs;
use crate::models::recorded::Recording;
use crate::models::{rules, validate, DocumentInput, LabExtraction, LabResult, ModelProvider, OcrDocument, ValidateContext};
use crate::synth_report::{ReportSpec, RowSpec, Variant};

#[derive(Debug, Default, Clone, Serialize)]
pub struct DocScore {
    pub file: String,
    pub output: String,
    pub expected: usize,
    /// Rows whose analyte was found (by LOINC) on the right page.
    pub found: usize,
    /// Analyte, value and unit all correct.
    pub correct: usize,
    pub range_correct: usize,
    pub flag_correct: usize,
    /// Expected rows sent to the review queue.
    pub pending: usize,
    /// Correct, or wrong but waiting in the review queue.
    pub correct_after_review: usize,
    /// Wrong values that skipped review, and extra rows that skipped review.
    pub unflagged_errors: usize,
    pub date_correct: bool,
    pub missing: Vec<String>,
    pub errors: Vec<String>,
    /// Rows sent to review, with the reasons.
    pub review: Vec<String>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Summary {
    pub documents: usize,
    pub expected: usize,
    pub found: usize,
    pub correct: usize,
    pub pending: usize,
    pub correct_after_review: usize,
    pub unflagged_errors: usize,
    /// Percent of rows with analyte, value and unit correct after extraction.
    pub accuracy: f64,
    /// Percent correct once the review queue is worked through.
    pub accuracy_after_review: f64,
    /// Percent of rows the queue asked the user to look at.
    pub review_rate: f64,
    pub dates_correct: usize,
    pub by_output: BTreeMap<String, (usize, usize)>,
}

/// Whether two printed analyte names are the same name, allowing for an OCR
/// slip after the first few characters (`Hp-lgG` for `Hp-IgG`).
pub fn same_printed_name(a: &str, b: &str) -> bool {
    let (a, b) = (labs::name_key(a), labs::name_key(b));
    let common = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
    a.contains(&b) || b.contains(&a) || common >= 4
}

fn same_value(r: &LabResult, row: &RowSpec) -> bool {
    match (row.truth.value, &row.truth.value_text) {
        (Some(v), _) => r.value.is_some_and(|x| (x - v).abs() < 1e-9),
        (None, Some(t)) => r.value_text.as_deref().is_some_and(|x| labs::fold(x).trim() == t),
        (None, None) => false,
    }
}

fn same_unit(r: &LabResult, row: &RowSpec) -> bool {
    let unit = r.unit.as_deref().map(str::trim).filter(|u| !u.is_empty());
    match (&row.truth.unit, unit) {
        (None, None) => true,
        (Some(t), Some(u)) => labs::normalize_unit(u) == Some(t.as_str()),
        _ => false,
    }
}

/// Scores one rendered file. `threshold` is the review confidence threshold.
pub fn score(spec: &ReportSpec, variant: &Variant, x: &LabExtraction, threshold: f64) -> DocScore {
    let expected = spec.expected(variant);
    let ctx = ValidateContext {
        threshold,
        prints_flags: x.results.iter().any(|r| r.flag_as_printed.is_some()),
        has_date: x.report.issued_at.is_some(),
    };
    let mut review = Vec::new();
    let checked: Vec<(&LabResult, bool, String)> = x
        .results
        .iter()
        .map(|r| {
            let v = validate(r, &ctx);
            if !v.reasons.is_empty() {
                let reasons: Vec<&str> = v.reasons.iter().map(|r| r.as_str()).collect();
                review.push(format!(
                    "p{} {} = {:?} {:?} {:?} conf {} ({})",
                    r.page, r.analyte_name, r.value, r.value_text, r.unit, r.confidence, reasons.join(", ")
                ));
            }
            (r, !v.reasons.is_empty(), v.code)
        })
        .collect();
    let mut used = vec![false; checked.len()];
    let mut s = DocScore {
        file: variant.file.clone(),
        output: variant.output.clone(),
        expected: expected.len(),
        date_correct: x.report.issued_at.as_deref() == Some(spec.issued_at.as_str()),
        review,
        ..Default::default()
    };
    for (page, row) in &expected {
        let hit = checked
            .iter()
            .enumerate()
            .find(|(i, (r, _, code))| !used[*i] && *code == row.truth.loinc && r.page == *page);
        let Some((i, (r, pending, _))) = hit else {
            // An unmapped row with this name can still be fixed in review.
            let fixable = checked.iter().enumerate().find(|(i, (r, pending, _))| {
                !used[*i] && *pending && r.page == *page && same_printed_name(&r.analyte_name, &row.name)
            });
            if let Some((i, _)) = fixable {
                used[i] = true;
                s.pending += 1;
                s.correct_after_review += 1;
            } else {
                s.missing.push(format!("p{page} {} ({})", row.name, row.truth.loinc));
            }
            continue;
        };
        used[i] = true;
        s.found += 1;
        let ok = same_value(r, row) && same_unit(r, row);
        if ok {
            s.correct += 1;
        }
        if (r.ref_low, r.ref_high) == (row.truth.ref_low, row.truth.ref_high) {
            s.range_correct += 1;
        }
        if r.flag_as_printed.as_deref().map(|f| labs::fold(f).trim().to_string()) == row.truth.flag {
            s.flag_correct += 1;
        }
        if *pending {
            s.pending += 1;
        }
        if ok || *pending {
            s.correct_after_review += 1;
        } else {
            s.unflagged_errors += 1;
            s.errors.push(format!(
                "p{page} {}: got {:?} {:?} {:?}, want {:?} {:?} {:?}",
                row.name, r.value, r.value_text, r.unit, row.truth.value, row.truth.value_text, row.truth.unit
            ));
        }
    }
    for (i, (r, pending, code)) in checked.iter().enumerate() {
        if !used[i] && !*pending {
            s.unflagged_errors += 1;
            s.errors.push(format!("p{} extra row {} ({code}) = {:?} {:?}", r.page, r.analyte_name, r.value, r.unit));
        }
    }
    s
}

pub fn summarize(scores: &[DocScore]) -> Summary {
    let mut t = Summary { documents: scores.len(), ..Default::default() };
    for s in scores {
        t.expected += s.expected;
        t.found += s.found;
        t.correct += s.correct;
        t.pending += s.pending;
        t.correct_after_review += s.correct_after_review;
        t.unflagged_errors += s.unflagged_errors;
        t.dates_correct += usize::from(s.date_correct);
        let e = t.by_output.entry(s.output.clone()).or_default();
        e.0 += s.correct;
        e.1 += s.expected;
    }
    let pct = |n: usize| if t.expected == 0 { 0.0 } else { (n as f64 / t.expected as f64 * 1000.0).round() / 10.0 };
    t.accuracy = pct(t.correct);
    t.accuracy_after_review = pct(t.correct_after_review);
    t.review_rate = pct(t.pending);
    t
}

/// Loads every report spec (`*.json`, not `*.ocr.json`) in `dir`.
pub fn load_specs(dir: &Path) -> Result<Vec<ReportSpec>> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            name.ends_with(".json") && !name.ends_with(".ocr.json")
        })
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|p| serde_json::from_str(&std::fs::read_to_string(p)?).with_context(|| format!("parsing {}", p.display())))
        .collect()
}

/// Scores every rendered file of every spec in `dir`. With `live`, OCR runs
/// on the rendered files; otherwise it replays `<file>.ocr.json` recordings.
pub fn run_dir(dir: &Path, live: Option<&dyn ModelProvider>, threshold: f64) -> Result<(Vec<DocScore>, Summary)> {
    let mut scores = Vec::new();
    for spec in load_specs(dir)? {
        for v in spec.variants() {
            let ocr: OcrDocument = match live {
                Some(p) => {
                    let bytes = std::fs::read(dir.join(&v.file)).with_context(|| format!("reading {}", v.file))?;
                    let mime = report::sniff(&bytes).with_context(|| format!("{} is not a report file", v.file))?;
                    p.ocr(&DocumentInput { mime, bytes: &bytes, lang: Some(&spec.language) })?
                }
                None => {
                    let path = dir.join(format!("{}.ocr.json", v.file));
                    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
                    serde_json::from_str::<Recording>(&text)?.ocr
                }
            };
            let x = match live {
                Some(p) => p.extract(&ocr, Some(&spec.language))?,
                None => rules::extract(&ocr, Some(&spec.language)),
            };
            scores.push(score(&spec, &v, &x, threshold));
        }
    }
    let summary = summarize(&scores);
    Ok((scores, summary))
}
