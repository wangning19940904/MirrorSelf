//! Checks every provider's extraction the same way (§5 stage 5b): values
//! below the confidence threshold, unit mismatches, implausible values and
//! flags that contradict the printed range go to the review queue.

use serde::Serialize;

use super::LabResult;
use crate::labs::{self, Analyte};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Confidence below the threshold (default 0.9).
    LowConfidence,
    /// The analyte has no LOINC mapping.
    UnmappedAnalyte,
    /// Missing, unreadable or wrong unit for this analyte.
    UnitMismatch,
    /// The value could not be read as a number or a known qualitative result.
    ValueUnreadable,
    /// Outside the analyte's physiological bounds: probably a misread.
    ImplausibleValue,
    /// The printed flag says one thing, the printed range another.
    FlagRangeDisagree,
    /// No report date was found; the value has no date to sit on.
    ReportDateMissing,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::LowConfidence => "low_confidence",
            Reason::UnmappedAnalyte => "unmapped_analyte",
            Reason::UnitMismatch => "unit_mismatch",
            Reason::ValueUnreadable => "value_unreadable",
            Reason::ImplausibleValue => "implausible_value",
            Reason::FlagRangeDisagree => "flag_range_disagree",
            Reason::ReportDateMissing => "report_date_missing",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Validated {
    pub analyte: Option<&'static Analyte>,
    pub code_system: &'static str,
    pub code: String,
    /// UCUM when recognized, otherwise as printed.
    pub unit: Option<String>,
    pub value_canonical: Option<f64>,
    pub unit_canonical: Option<&'static str>,
    pub reasons: Vec<Reason>,
}

pub struct Context {
    pub threshold: f64,
    /// Whether this report prints flags at all; if it does, an unflagged
    /// value outside its printed range is suspicious.
    pub prints_flags: bool,
    pub has_date: bool,
}

/// A censored result such as `<0.10`: valid without a numeric value.
fn censored(text: Option<&str>) -> bool {
    text.is_some_and(|t| labs::fold(t).trim_start().starts_with(['<', '>', '≤', '≥']))
}

pub fn validate(r: &LabResult, ctx: &Context) -> Validated {
    let mut reasons = Vec::new();
    let analyte = r
        .loinc
        .as_deref()
        .filter(|c| labs::loinc_is_valid(c))
        .and_then(labs::by_loinc)
        .or_else(|| labs::match_name(&r.analyte_name, false).map(|(a, _)| a));
    let (code_system, code) = match (analyte, r.loinc.as_deref().filter(|c| labs::loinc_is_valid(c))) {
        (Some(a), _) => ("loinc", a.loinc.to_string()),
        (None, Some(code)) => {
            reasons.push(Reason::UnmappedAnalyte);
            ("loinc", code.to_string())
        }
        (None, None) => {
            reasons.push(Reason::UnmappedAnalyte);
            let key: String = labs::name_key(&r.analyte_name).chars().take(40).collect();
            ("local", format!("local:{key}"))
        }
    };

    let printed_unit = r.unit.as_deref().map(str::trim).filter(|u| !u.is_empty());
    let ucum = printed_unit.and_then(labs::normalize_unit);
    let unit = ucum.map(str::to_string).or_else(|| printed_unit.map(str::to_string));
    let mut value_canonical = None;
    let mut unit_canonical = None;
    match analyte {
        Some(a) if a.qualitative => {
            if !r.value_text.as_deref().is_some_and(labs::is_qualitative_text) {
                reasons.push(Reason::ValueUnreadable);
            }
        }
        Some(a) => {
            match ucum.and_then(|u| a.factor(u)) {
                Some(factor) => {
                    unit_canonical = Some(a.unit);
                    value_canonical = r.value.map(|v| v * factor);
                }
                None => reasons.push(Reason::UnitMismatch),
            }
            if r.value.is_none() && !censored(r.value_text.as_deref()) {
                reasons.push(Reason::ValueUnreadable);
            }
            if value_canonical.is_some_and(|v| v < a.plausible.0 || v > a.plausible.1) {
                reasons.push(Reason::ImplausibleValue);
            }
        }
        None => {
            if r.value.is_none() && r.value_text.is_none() {
                reasons.push(Reason::ValueUnreadable);
            }
        }
    }

    if let Some(v) = r.value {
        if r.ref_low.is_some() || r.ref_high.is_some() {
            let against = labs::against_range(v, r.ref_low, r.ref_high);
            let printed = r.flag_as_printed.as_deref().filter(|f| !f.trim().is_empty());
            let disagree = match printed {
                Some(f) => labs::flag_direction(f).is_some_and(|d| Some(d) != against),
                None => ctx.prints_flags && against.is_some(),
            };
            if disagree {
                reasons.push(Reason::FlagRangeDisagree);
            }
        }
    }
    if r.confidence < ctx.threshold {
        reasons.push(Reason::LowConfidence);
    }
    if !ctx.has_date {
        reasons.push(Reason::ReportDateMissing);
    }
    Validated { analyte, code_system, code, unit, value_canonical, unit_canonical, reasons }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(name: &str, value: f64, unit: &str, range: (Option<f64>, Option<f64>), flag: Option<&str>, conf: f64) -> LabResult {
        LabResult {
            analyte_name: name.into(),
            loinc: None,
            value: Some(value),
            value_text: None,
            unit: Some(unit.into()),
            ref_low: range.0,
            ref_high: range.1,
            flag_as_printed: flag.map(String::from),
            page: 1,
            bbox: [0.0; 4],
            confidence: conf,
            ref_text: None,
        }
    }

    fn reasons(r: &LabResult) -> Vec<&'static str> {
        let ctx = Context { threshold: 0.9, prints_flags: true, has_date: true };
        validate(r, &ctx).reasons.iter().map(|r| r.as_str()).collect()
    }

    #[test]
    fn routes_suspicious_values_to_review() {
        let ok = result("LDL-C", 3.6, "mmol/L", (Some(0.0), Some(3.4)), Some("↑"), 1.0);
        assert!(reasons(&ok).is_empty());
        let v = validate(&ok, &Context { threshold: 0.9, prints_flags: true, has_date: true });
        assert_eq!((v.code.as_str(), v.unit.as_deref()), ("13457-7", Some("mmol/L")));

        assert_eq!(reasons(&result("LDL-C", 3.6, "mmol/L", (Some(0.0), Some(3.4)), Some("↑"), 0.5)), ["low_confidence"]);
        assert_eq!(reasons(&result("LDL-C", 3.6, "g/L", (Some(0.0), Some(3.4)), Some("↑"), 1.0)), ["unit_mismatch"]);
        assert_eq!(reasons(&result("LDL-C", 36.0, "mmol/L", (Some(0.0), Some(3.4)), Some("↑"), 1.0)), ["implausible_value"]);
        assert_eq!(reasons(&result("LDL-C", 3.6, "mmol/L", (Some(0.0), Some(3.4)), None, 1.0)), ["flag_range_disagree"]);
        assert_eq!(reasons(&result("LDL-C", 3.0, "mmol/L", (Some(0.0), Some(3.4)), Some("↑"), 1.0)), ["flag_range_disagree"]);
        assert_eq!(reasons(&result("Mystery index", 3.0, "mmol/L", (Some(0.0), Some(3.4)), None, 1.0)), ["unmapped_analyte"]);
        // mg/dL is a valid unit for LDL; the canonical value is converted.
        let mg = result("LDL Cholesterol", 139.0, "mg/dL", (None, Some(130.0)), Some("H"), 1.0);
        let v = validate(&mg, &Context { threshold: 0.9, prints_flags: true, has_date: true });
        assert!(v.reasons.is_empty());
        assert!((v.value_canonical.unwrap() - 3.5945).abs() < 1e-3);
    }
}
