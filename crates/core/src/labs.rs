//! Lab analytes for checkup reports (architecture §6–7): LOINC codes, the
//! names reports print them under (zh and en), UCUM units with conversion
//! factors, and plausibility bounds that catch OCR slips such as a lost
//! decimal point.
//!
//! One LOINC code per analyte keeps an analyte comparable across years; the
//! property-specific variants (mass vs. substance concentration) are listed
//! in `also` and resolve to the same analyte. Bounds are wide physiological
//! limits for catching extraction errors, not reference ranges: flags always
//! come from the report's own printed range (§7 health guardrails).

use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::Result;
use rusqlite::{params, Connection};

#[derive(Debug)]
pub struct Analyte {
    pub loinc: &'static str,
    /// Other LOINC codes that mean the same analyte (e.g. the mmol/L variant).
    pub also: &'static [&'static str],
    pub display: &'static str,
    pub display_zh: &'static str,
    /// Printed names and abbreviations, zh and en.
    pub aliases: &'static [&'static str],
    /// Canonical UCUM unit; empty for qualitative results.
    pub unit: &'static str,
    /// Other accepted units and the factor that converts them to `unit`.
    pub alt_units: &'static [(&'static str, f64)],
    /// Wide plausibility bounds in the canonical unit.
    pub plausible: (f64, f64),
    pub qualitative: bool,
    /// Matched only in a urinalysis section (or with an explicit 尿/urine prefix).
    pub urine: bool,
}

#[allow(clippy::too_many_arguments)]
const fn a(
    loinc: &'static str,
    also: &'static [&'static str],
    display: &'static str,
    display_zh: &'static str,
    aliases: &'static [&'static str],
    unit: &'static str,
    alt_units: &'static [(&'static str, f64)],
    plausible: (f64, f64),
) -> Analyte {
    Analyte { loinc, also, display, display_zh, aliases, unit, alt_units, plausible, qualitative: false, urine: false }
}

const fn urine_qualitative(loinc: &'static str, display: &'static str, display_zh: &'static str, aliases: &'static [&'static str]) -> Analyte {
    Analyte {
        loinc,
        also: &[],
        display,
        display_zh,
        aliases,
        unit: "",
        alt_units: &[],
        plausible: (0.0, 0.0),
        qualitative: true,
        urine: true,
    }
}

const MG_DL_CHOL: f64 = 1.0 / 38.67;

pub const ANALYTES: &[Analyte] = &[
    // Complete blood count.
    a("6690-2", &[], "Leukocytes (WBC)", "白细胞计数", &["白细胞计数", "白细胞", "WBC", "White blood cell count", "White blood cells", "Leukocytes", "WBC count"],
      "10*9/L", &[("10*3/uL", 1.0)], (0.1, 100.0)),
    a("789-8", &[], "Erythrocytes (RBC)", "红细胞计数", &["红细胞计数", "红细胞", "RBC", "Red blood cell count", "Red blood cells", "Erythrocytes", "RBC count"],
      "10*12/L", &[("10*6/uL", 1.0)], (0.5, 9.0)),
    a("718-7", &[], "Hemoglobin", "血红蛋白", &["血红蛋白", "血红蛋白浓度", "HGB", "Hb", "Hemoglobin", "Haemoglobin"],
      "g/L", &[("g/dL", 10.0)], (20.0, 250.0)),
    a("4544-3", &[], "Hematocrit", "红细胞压积", &["红细胞压积", "红细胞比容", "HCT", "Hematocrit", "Haematocrit"],
      "%", &[("L/L", 100.0)], (5.0, 75.0)),
    a("787-2", &[], "MCV", "平均红细胞体积", &["平均红细胞体积", "MCV", "Mean corpuscular volume"],
      "fL", &[], (40.0, 150.0)),
    a("785-6", &[], "MCH", "平均红细胞血红蛋白量", &["平均红细胞血红蛋白量", "平均红细胞血红蛋白含量", "MCH", "Mean corpuscular hemoglobin"],
      "pg", &[], (10.0, 50.0)),
    a("786-4", &[], "MCHC", "平均红细胞血红蛋白浓度", &["平均红细胞血红蛋白浓度", "MCHC", "Mean corpuscular hemoglobin concentration"],
      "g/L", &[("g/dL", 10.0)], (200.0, 450.0)),
    a("777-3", &[], "Platelets", "血小板计数", &["血小板计数", "血小板", "PLT", "Platelets", "Platelet count"],
      "10*9/L", &[("10*3/uL", 1.0)], (1.0, 2000.0)),
    a("770-8", &[], "Neutrophils %", "中性粒细胞百分比", &["中性粒细胞百分比", "中性粒细胞百分数", "中性粒细胞%", "NEUT%", "NE%", "Neutrophils %", "Neutrophils"],
      "%", &[], (0.0, 100.0)),
    a("736-9", &[], "Lymphocytes %", "淋巴细胞百分比", &["淋巴细胞百分比", "淋巴细胞百分数", "淋巴细胞%", "LYMPH%", "LY%", "Lymphocytes %", "Lymphocytes"],
      "%", &[], (0.0, 100.0)),
    // Lipids.
    a("2093-3", &["14647-2"], "Total cholesterol", "总胆固醇", &["总胆固醇", "胆固醇", "TC", "CHOL", "Total cholesterol", "Cholesterol", "Cholesterol, total"],
      "mmol/L", &[("mg/dL", MG_DL_CHOL)], (0.5, 25.0)),
    a("2571-8", &["14927-8"], "Triglycerides", "甘油三酯", &["甘油三酯", "三酰甘油", "TG", "TRIG", "Triglycerides", "Triglyceride"],
      "mmol/L", &[("mg/dL", 1.0 / 88.57)], (0.05, 60.0)),
    a("2085-9", &["14646-4"], "HDL cholesterol", "高密度脂蛋白胆固醇", &["高密度脂蛋白胆固醇", "高密度脂蛋白", "HDL-C", "HDL", "HDL cholesterol"],
      "mmol/L", &[("mg/dL", MG_DL_CHOL)], (0.1, 6.0)),
    a("13457-7", &["2089-1", "22748-8", "39469-2"], "LDL cholesterol", "低密度脂蛋白胆固醇",
      &["低密度脂蛋白胆固醇", "低密度脂蛋白", "LDL-C", "LDL", "LDL cholesterol", "LDL cholesterol (calc)", "LDL cholesterol, calculated"],
      "mmol/L", &[("mg/dL", MG_DL_CHOL)], (0.1, 15.0)),
    // Glucose.
    a("1558-6", &["14771-0", "2345-7", "14749-6"], "Fasting glucose", "空腹血糖",
      &["空腹血糖", "空腹血葡萄糖", "血糖", "葡萄糖", "GLU", "FPG", "Glucose", "Fasting glucose", "Glucose, fasting"],
      "mmol/L", &[("mg/dL", 1.0 / 18.016)], (0.5, 40.0)),
    a("4548-4", &[], "Hemoglobin A1c", "糖化血红蛋白", &["糖化血红蛋白", "糖化血红蛋白A1c", "HbA1c", "Hemoglobin A1c", "A1C", "Glycated hemoglobin"],
      "%", &[], (2.0, 20.0)),
    // Liver.
    a("1742-6", &[], "ALT", "丙氨酸氨基转移酶", &["丙氨酸氨基转移酶", "谷丙转氨酶", "ALT", "GPT", "SGPT", "Alanine aminotransferase"],
      "U/L", &[], (0.0, 5000.0)),
    a("1920-8", &[], "AST", "天门冬氨酸氨基转移酶", &["天门冬氨酸氨基转移酶", "天冬氨酸氨基转移酶", "谷草转氨酶", "AST", "GOT", "SGOT", "Aspartate aminotransferase"],
      "U/L", &[], (0.0, 5000.0)),
    a("2324-2", &[], "GGT", "γ-谷氨酰转移酶", &["γ-谷氨酰转移酶", "γ-谷氨酰基转移酶", "谷氨酰转肽酶", "GGT", "γ-GT", "Gamma-glutamyl transferase"],
      "U/L", &[], (0.0, 5000.0)),
    a("6768-6", &[], "Alkaline phosphatase", "碱性磷酸酶", &["碱性磷酸酶", "ALP", "AKP", "Alkaline phosphatase"],
      "U/L", &[], (5.0, 3000.0)),
    a("1975-2", &["14631-6"], "Total bilirubin", "总胆红素", &["总胆红素", "TBIL", "T-BIL", "Total bilirubin", "Bilirubin, total"],
      "umol/L", &[("mg/dL", 17.1)], (0.5, 800.0)),
    a("1751-7", &[], "Albumin", "白蛋白", &["白蛋白", "清蛋白", "ALB", "Albumin"],
      "g/L", &[("g/dL", 10.0)], (5.0, 70.0)),
    a("2885-2", &[], "Total protein", "总蛋白", &["总蛋白", "TP", "Total protein", "Protein, total"],
      "g/L", &[("g/dL", 10.0)], (20.0, 130.0)),
    // Kidney.
    a("2160-0", &["14682-9"], "Creatinine", "肌酐", &["肌酐", "血肌酐", "CREA", "Cr", "SCr", "Creatinine"],
      "umol/L", &[("mg/dL", 88.42)], (5.0, 3000.0)),
    a("22664-7", &["3091-6"], "Urea", "尿素", &["尿素", "UREA", "Urea"],
      "mmol/L", &[], (0.3, 80.0)),
    a("3094-0", &[], "Urea nitrogen (BUN)", "尿素氮", &["尿素氮", "BUN", "Urea nitrogen", "Blood urea nitrogen"],
      "mg/dL", &[("mmol/L", 2.8)], (1.0, 250.0)),
    a("3084-1", &["14933-6"], "Uric acid", "尿酸", &["尿酸", "血尿酸", "UA", "URIC", "Uric acid"],
      "umol/L", &[("mg/dL", 59.48)], (20.0, 1500.0)),
    a("62238-1", &["33914-3"], "eGFR", "估算肾小球滤过率", &["估算肾小球滤过率", "肾小球滤过率", "eGFR", "Estimated GFR", "eGFR (CKD-EPI)"],
      "mL/min/{1.73_m2}", &[], (1.0, 200.0)),
    // Thyroid and other common checkup analytes.
    a("3016-3", &[], "TSH", "促甲状腺激素", &["促甲状腺激素", "促甲状腺素", "TSH", "Thyroid stimulating hormone", "Thyrotropin"],
      "m[IU]/L", &[("u[IU]/mL", 1.0)], (0.001, 200.0)),
    a("3024-7", &["14920-3"], "Free T4", "游离甲状腺素", &["游离甲状腺素", "游离T4", "FT4", "Free T4", "Free thyroxine", "Thyroxine, free"],
      "pmol/L", &[("ng/dL", 12.87)], (0.5, 150.0)),
    a("30522-7", &[], "hs-CRP", "超敏C反应蛋白", &["超敏C反应蛋白", "超敏C-反应蛋白", "hs-CRP", "hsCRP", "High-sensitivity CRP", "C-reactive protein, high sensitivity"],
      "mg/L", &[("mg/dL", 10.0)], (0.0, 500.0)),
    a("1989-3", &[], "25-hydroxyvitamin D", "25-羟基维生素D", &["25-羟基维生素D", "25羟基维生素D", "维生素D", "25(OH)D", "25-OH Vitamin D", "Vitamin D, 25-hydroxy"],
      "ng/mL", &[("nmol/L", 1.0 / 2.496)], (1.0, 250.0)),
    a("2276-4", &[], "Ferritin", "铁蛋白", &["铁蛋白", "FER", "Ferritin"],
      "ng/mL", &[("ug/L", 1.0)], (0.5, 10000.0)),
    // Urinalysis (dipstick, qualitative).
    urine_qualitative("20454-5", "Urine protein", "尿蛋白", &["尿蛋白", "尿蛋白质", "尿PRO", "PRO", "Urine protein", "Protein, urine", "Urine Protein"]),
    urine_qualitative("25428-4", "Urine glucose", "尿糖", &["尿糖", "尿葡萄糖", "尿GLU", "Urine glucose", "Glucose, urine"]),
];

/// Qualitative results and ranges as reports print them.
pub const QUALITATIVE: &[&str] = &[
    "阴性", "阳性", "弱阳性", "-", "+", "++", "+++", "±", "(-)", "(+)", "negative", "positive", "neg", "pos", "trace",
];

pub fn is_qualitative_text(s: &str) -> bool {
    let k = fold(s).trim().to_lowercase();
    QUALITATIVE.contains(&k.as_str())
}

impl Analyte {
    /// All units this analyte may be printed in, canonical first.
    pub fn units(&self) -> impl Iterator<Item = &'static str> + '_ {
        std::iter::once(self.unit).chain(self.alt_units.iter().map(|(u, _)| *u))
    }

    /// Factor converting a value in `unit` to the canonical unit.
    pub fn factor(&self, unit: &str) -> Option<f64> {
        if unit == self.unit {
            return Some(1.0);
        }
        self.alt_units.iter().find(|(u, _)| *u == unit).map(|(_, f)| *f)
    }

    pub fn codes(&self) -> impl Iterator<Item = &'static str> + '_ {
        std::iter::once(self.loinc).chain(self.also.iter().copied())
    }
}

pub fn by_loinc(code: &str) -> Option<&'static Analyte> {
    let code = code.trim();
    ANALYTES.iter().find(|a| a.codes().any(|c| c == code))
}

/// LOINC codes carry a mod-10 (Luhn) check digit: `13457-7`.
pub fn loinc_is_valid(code: &str) -> bool {
    let Some((body, check)) = code.trim().split_once('-') else { return false };
    if body.is_empty() || body.len() > 7 || check.len() != 1 || !body.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Some(check) = check.chars().next().and_then(|c| c.to_digit(10)) else { return false };
    let sum: u32 = body
        .bytes()
        .rev()
        .enumerate()
        .map(|(i, b)| {
            let d = u32::from(b - b'0');
            if i % 2 == 0 {
                let x = d * 2;
                x / 10 + x % 10
            } else {
                d
            }
        })
        .sum();
    (10 - sum % 10) % 10 == check
}

/// Full-width ASCII to ASCII, typographic dashes to `-`, and the ideographic
/// space to a space; everything else unchanged.
pub fn fold(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
            '\u{3000}' => ' ',
            '–' | '—' | '−' | '‒' | '―' => '-',
            '〜' => '~',
            _ => c,
        })
        .collect()
}

/// Matching key for printed names: folded, lowercase, without spaces and
/// punctuation; γ and its OCR look-alikes collapse to `y`.
pub fn name_key(s: &str) -> String {
    fold(s)
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            'γ' => Some('y'),
            c if c.is_whitespace() => None,
            '-' | '_' | '·' | '・' | '.' | ',' | ':' | ';' | '(' | ')' | '[' | ']' | '【' | '】' | '/' | '%' | '#' | '*'
            | '\'' | '"' | '、' | '，' | '。' | '：' => None,
            c => Some(c),
        })
        .collect()
}

fn alias_index() -> &'static HashMap<String, Vec<&'static Analyte>> {
    static INDEX: OnceLock<HashMap<String, Vec<&'static Analyte>>> = OnceLock::new();
    INDEX.get_or_init(|| {
        let mut m: HashMap<String, Vec<&'static Analyte>> = HashMap::new();
        for an in ANALYTES {
            for alias in an.aliases.iter().chain([&an.display, &an.display_zh]) {
                let entry = m.entry(name_key(alias)).or_default();
                if !entry.iter().any(|x| std::ptr::eq(*x, an)) {
                    entry.push(an);
                }
            }
        }
        m
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameMatch {
    /// The printed name (or its abbreviation) is a known alias.
    Exact,
    /// One character off a known alias, typically an OCR slip.
    Fuzzy,
}

fn lookup(key: &str, urine: bool) -> Option<&'static Analyte> {
    let found = alias_index().get(key)?;
    found.iter().find(|a| a.urine == urine).or(found.first()).copied()
}

/// Maps a printed analyte name to an analyte. `urine` is true inside a
/// urinalysis section, where `葡萄糖 GLU` means urine glucose.
pub fn match_name(printed: &str, urine: bool) -> Option<(&'static Analyte, NameMatch)> {
    let folded = fold(printed);
    let (outside, inside) = split_parens(&folded);
    let mut keys = vec![name_key(&folded), name_key(&outside)];
    keys.extend(inside.iter().map(|s| name_key(s)));
    keys.retain(|k| !k.is_empty());

    if urine {
        for k in &keys {
            for prefix in ["尿", "urine"] {
                if let Some(a) = lookup(&format!("{prefix}{k}"), true) {
                    return Some((a, NameMatch::Exact));
                }
            }
        }
    }
    for k in &keys {
        if let Some(a) = lookup(k, urine) {
            if a.urine && !urine && !k.starts_with('尿') && !k.starts_with("urine") {
                continue;
            }
            return Some((a, NameMatch::Exact));
        }
    }
    // One edit away from exactly one alias (long names only, so 血糖 never becomes 尿糖).
    for k in keys.iter().take(2) {
        if k.chars().count() < 4 {
            continue;
        }
        let mut best: Vec<&'static Analyte> = Vec::new();
        for (alias, analytes) in alias_index() {
            if alias.chars().count() >= 4 && edit_distance_at_most_one(k, alias) {
                for a in analytes {
                    if !best.iter().any(|b| std::ptr::eq(*b, *a)) {
                        best.push(a);
                    }
                }
            }
        }
        if best.len() == 1 && (urine || !best[0].urine) {
            return Some((best[0], NameMatch::Fuzzy));
        }
    }
    None
}

/// `低密度脂蛋白胆固醇(LDL-C)` → (`低密度脂蛋白胆固醇`, [`LDL-C`]).
fn split_parens(s: &str) -> (String, Vec<String>) {
    let mut outside = String::new();
    let mut inside = Vec::new();
    let mut depth = 0;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '(' | '（' | '[' => {
                depth += 1;
                if depth == 1 {
                    cur.clear();
                    continue;
                }
            }
            ')' | '）' | ']' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    inside.push(cur.trim().to_string());
                    continue;
                }
            }
            _ => {}
        }
        if depth > 0 {
            cur.push(c);
        } else {
            outside.push(c);
        }
    }
    (outside.trim().to_string(), inside)
}

fn edit_distance_at_most_one(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let (mut i, mut j, mut edits) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            i += 1;
            j += 1;
            continue;
        }
        edits += 1;
        if edits > 1 {
            return false;
        }
        match a.len().cmp(&b.len()) {
            std::cmp::Ordering::Greater => i += 1,
            std::cmp::Ordering::Less => j += 1,
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    edits + (a.len() - i) + (b.len() - j) <= 1
}

/// A printed unit in UCUM, tolerating the usual print and OCR variants
/// (`×10⁹/L`, `x109/L`, `μmol/L`, `mmoI/L`). `None` when unrecognized.
pub fn normalize_unit(printed: &str) -> Option<&'static str> {
    // `UL` is how OCR reads `U/L`; checked before lowercasing, since `uL` is a volume.
    if fold(printed).trim() == "UL" {
        return Some("U/L");
    }
    let mut k: String = fold(printed)
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| match c {
            'μ' | 'µ' => 'u',
            '⁰' => '0',
            '¹' => '1',
            '²' => '2',
            '³' => '3',
            '⁶' => '6',
            '⁹' => '9',
            '㎡' => '2',
            c => c,
        })
        .collect::<String>()
        .to_lowercase();
    for prefix in ["×", "x", "*"] {
        if let Some(rest) = k.strip_prefix(prefix) {
            if rest.starts_with("10") {
                k = rest.to_string();
            }
        }
    }
    // Print variants and unambiguous OCR confusions (`wmol/L` for `μmol/L`).
    for (from, to) in [
        ("10^", "10*"), ("10e", "10*"), ("10°", "10*9"), ("moi/", "mol/"), ("mo1/", "mol/"), ("m0l/", "mol/"),
        ("wmol/", "umol/"), ("mlu/", "miu/"), ("ulu/", "uiu/"), ("1.73mz", "1.73m2"),
    ] {
        k = k.replace(from, to);
    }
    if matches!(k.as_str(), "uil" | "u1l" | "u/i" | "ul/l") {
        k = "u/l".into();
    }
    if k.ends_with("/1") {
        k.replace_range(k.len() - 1.., "l");
    }
    // Superscripts lost in OCR: 10⁹/L read as 109/L.
    for (from, to) in [
        ("109/l", "10*9/l"), ("1019/l", "10*9/l"), ("1012/l", "10*12/l"), ("103/ul", "10*3/ul"), ("106/ul", "10*6/ul"),
    ] {
        if k == from {
            k = to.to_string();
        }
    }
    Some(match k.as_str() {
        "mmol/l" => "mmol/L",
        "umol/l" => "umol/L",
        "nmol/l" => "nmol/L",
        "pmol/l" => "pmol/L",
        "mmol/mol" => "mmol/mol",
        "g/l" => "g/L",
        "g/dl" => "g/dL",
        "mg/dl" => "mg/dL",
        "mg/l" => "mg/L",
        "ug/l" => "ug/L",
        "ng/ml" => "ng/mL",
        "u/ml" => "U/mL",
        "ng/dl" => "ng/dL",
        "u/l" | "iu/l" => "U/L",
        "10*9/l" => "10*9/L",
        "10*12/l" => "10*12/L",
        "10*3/ul" => "10*3/uL",
        "10*6/ul" => "10*6/uL",
        "%" => "%",
        "fl" => "fL",
        "pg" => "pg",
        "l/l" => "L/L",
        "miu/l" | "m[iu]/l" => "m[IU]/L",
        "uiu/ml" | "u[iu]/ml" => "u[IU]/mL",
        "ml/min/1.73m2" | "ml/min/1.73m^2" | "ml/min/{1.73_m2}" | "ml/min/1.73" => "mL/min/{1.73_m2}",
        _ => return None,
    })
}

/// A number as printed: decimal commas (`5,20`) and thousands separators
/// (`1,200`) are both handled.
pub fn parse_number(s: &str) -> Option<f64> {
    let t: String = fold(s).trim().chars().filter(|c| !c.is_whitespace()).collect();
    if t.is_empty() || !t.chars().next().is_some_and(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let t = match t.split_once(',') {
        Some((int, frac)) if !frac.contains(',') && frac.len() <= 2 && !int.contains('.') => format!("{int}.{frac}"),
        Some(_) if t.split(',').skip(1).all(|g| g.len() == 3) => t.replace(',', ""),
        Some(_) => return None,
        None => t,
    };
    if !t.chars().all(|c| c.is_ascii_digit() || c == '.') || t.matches('.').count() > 1 {
        return None;
    }
    t.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// A reference range as printed.
#[derive(Debug, Clone, PartialEq)]
pub struct RefRange {
    pub low: Option<f64>,
    pub high: Option<f64>,
    pub text: String,
}

pub fn parse_range(s: &str) -> Option<RefRange> {
    let text = fold(s).trim().to_string();
    let t: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if t.is_empty() {
        return None;
    }
    if is_qualitative_text(&t) {
        return Some(RefRange { low: None, high: None, text });
    }
    for (prefix, upper) in [("<=", true), ("≤", true), ("≦", true), ("<", true), (">=", false), ("≥", false), ("≧", false), (">", false)] {
        if let Some(rest) = t.strip_prefix(prefix) {
            let v = parse_number(rest)?;
            return Some(if upper {
                RefRange { low: None, high: Some(v), text }
            } else {
                RefRange { low: Some(v), high: None, text }
            });
        }
    }
    let split = t.char_indices().skip(1).find(|(_, c)| matches!(c, '-' | '~'))?;
    let (lo, hi) = (&t[..split.0], &t[split.0 + split.1.len_utf8()..]);
    let (lo, hi) = (parse_number(lo)?, parse_number(hi)?);
    (lo <= hi).then_some(RefRange { low: Some(lo), high: Some(hi), text })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    High,
    Low,
}

/// What a printed flag says, if it says a direction (`↑`, `H`, `偏高`, …).
pub fn flag_direction(flag: &str) -> Option<Direction> {
    match fold(flag).trim() {
        "↑" | "↑↑" | "H" | "HH" | "h" | "高" | "偏高" => Some(Direction::High),
        "↓" | "↓↓" | "L" | "LL" | "l" | "低" | "偏低" => Some(Direction::Low),
        _ => None,
    }
}

pub fn is_flag(s: &str) -> bool {
    flag_direction(s).is_some() || matches!(fold(s).trim(), "*" | "A" | "!")
}

/// Where a value falls against the printed range (bounds inclusive).
pub fn against_range(value: f64, low: Option<f64>, high: Option<f64>) -> Option<Direction> {
    if high.is_some_and(|h| value > h) {
        Some(Direction::High)
    } else if low.is_some_and(|l| value < l) {
        Some(Direction::Low)
    } else {
        None
    }
}

/// Resolves `LOINC:13457-7`, `13457-7`, `LDL-C` or `低密度脂蛋白胆固醇`.
pub fn resolve(input: &str) -> Option<&'static Analyte> {
    let t = input.trim();
    let code = t
        .strip_prefix("LOINC:")
        .or_else(|| t.strip_prefix("loinc:"))
        .unwrap_or(t);
    by_loinc(code).or_else(|| match_name(t, false).map(|(a, _)| a)).or_else(|| match_name(t, true).map(|(a, _)| a))
}

/// Keeps the `codes` table in step with [`ANALYTES`].
pub fn seed(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO codes (code, code_system, display, display_zh, category_id, unit, aggregation, minmax, notable)
             VALUES (?1, 'loinc', ?2, ?3, 'labs', ?4, 'mean', 0, 0)
             ON CONFLICT(code) DO UPDATE SET code_system = 'loinc', display = excluded.display,
               display_zh = excluded.display_zh, category_id = 'labs', unit = excluded.unit",
        )?;
        for an in ANALYTES {
            stmt.execute(params![an.loinc, an.display, an.display_zh, an.unit])?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_a_valid_check_digit() {
        for an in ANALYTES {
            for code in an.codes() {
                assert!(loinc_is_valid(code), "{code} ({})", an.display);
            }
            assert!(an.qualitative || normalize_unit(an.unit) == Some(an.unit), "{}", an.unit);
        }
        assert!(!loinc_is_valid("13457-8"));
        assert!(!loinc_is_valid("abc"));
    }

    #[test]
    fn printed_names_map_to_analytes() {
        let code = |s: &str, urine| match_name(s, urine).map(|(a, m)| (a.loinc, m));
        assert_eq!(code("低密度脂蛋白胆固醇(LDL-C)", false), Some(("13457-7", NameMatch::Exact)));
        assert_eq!(code("LDL Cholesterol", false), Some(("13457-7", NameMatch::Exact)));
        assert_eq!(code("Y-谷氮酰转移酶（GGT）", false), Some(("2324-2", NameMatch::Exact)));
        assert_eq!(code("y-谷氨酰转移酶", false), Some(("2324-2", NameMatch::Exact)));
        assert_eq!(code("低密度脂蛋白胆固酵", false), Some(("13457-7", NameMatch::Fuzzy)));
        assert_eq!(code("葡萄糖(GLU)", false), Some(("1558-6", NameMatch::Exact)));
        assert_eq!(code("葡萄糖(GLU)", true), Some(("25428-4", NameMatch::Exact)));
        assert_eq!(code("尿蛋白(PRO)", false), Some(("20454-5", NameMatch::Exact)));
        assert_eq!(code("血糖", false), Some(("1558-6", NameMatch::Exact)));
        assert_eq!(code("年龄", false), None);
        assert_eq!(resolve("LOINC:22748-8").unwrap().loinc, "13457-7");
    }

    #[test]
    fn units_numbers_and_ranges() {
        assert_eq!(normalize_unit("×10⁹/L"), Some("10*9/L"));
        assert_eq!(normalize_unit("x109/L"), Some("10*9/L"));
        assert_eq!(normalize_unit("x10°/L"), Some("10*9/L"));
        assert_eq!(normalize_unit("×10¹²/L"), Some("10*12/L"));
        assert_eq!(normalize_unit("μmol/L"), Some("umol/L"));
        assert_eq!(normalize_unit("mmoI/L"), Some("mmol/L"));
        assert_eq!(normalize_unit("wmol/L"), Some("umol/L"));
        assert_eq!(normalize_unit("mlU/L"), Some("m[IU]/L"));
        assert_eq!(normalize_unit("UIL"), Some("U/L"));
        assert_eq!(normalize_unit("UL"), Some("U/L"));
        assert_eq!(normalize_unit("uL"), None);
        assert_eq!(normalize_unit("x1019/L"), Some("10*9/L"));
        assert_eq!(normalize_unit("mL/min/1.73mz"), Some("mL/min/{1.73_m2}"));
        assert_eq!(normalize_unit("mL/min/1.73m²"), Some("mL/min/{1.73_m2}"));
        assert_eq!(normalize_unit("岁"), None);
        assert_eq!(parse_number("5,20"), Some(5.2));
        assert_eq!(parse_number("1,200"), Some(1200.0));
        assert_eq!(parse_number("3.6O"), None);
        assert_eq!(parse_range("＜5,20").unwrap().high, Some(5.2));
        assert_eq!(parse_range("3.5-9.5").map(|r| (r.low, r.high)), Some((Some(3.5), Some(9.5))));
        assert_eq!(parse_range("0~3.40").map(|r| (r.low, r.high)), Some((Some(0.0), Some(3.4))));
        assert_eq!(parse_range("≥ 90").map(|r| r.low), Some(Some(90.0)));
        assert!(parse_range("阴性").is_some_and(|r| r.low.is_none() && r.high.is_none()));
        assert!(parse_range("2024-06-12").is_none());
        assert_eq!(flag_direction("↑"), Some(Direction::High));
        assert_eq!(against_range(3.6, Some(0.0), Some(3.4)), Some(Direction::High));
    }
}
