//! Synthetic checkup reports for fixtures and the extraction eval set.
//! Deterministic for a given seed. Never real personal data: the patient,
//! providers and values are made up.
//!
//! A [`ReportSpec`] is both the renderer's input
//! (`tools/report-fixtures/render.swift` draws it as a PDF, a scanned PDF and
//! phone photos) and the ground truth the eval scores against.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Column {
    /// `name`, `abbr`, `value`, `value_flag`, `flag`, `unit` or `range`.
    pub key: String,
    pub label: String,
    pub x: f64,
}

/// What a row should extract to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Truth {
    pub loinc: String,
    pub value: Option<f64>,
    pub value_text: Option<String>,
    /// UCUM; `None` for qualitative results.
    pub unit: Option<String>,
    pub ref_low: Option<f64>,
    pub ref_high: Option<f64>,
    pub flag: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abbr: Option<String>,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
    pub truth: Truth,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SectionSpec {
    pub title: String,
    pub rows: Vec<RowSpec>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageSpec {
    pub sections: Vec<SectionSpec>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhotoSpec {
    /// Rotation in degrees.
    pub angle: f64,
    pub blur: f64,
    pub noise: f64,
    #[serde(default)]
    pub perspective: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportSpec {
    /// File stem of the rendered files.
    pub id: String,
    pub language: String,
    pub provider: String,
    pub title: String,
    pub header: Vec<String>,
    pub columns: Vec<Column>,
    pub pages: Vec<PageSpec>,
    /// `pdf`, `scan` (image-only PDF) and `photo` (one JPEG per page).
    pub outputs: Vec<String>,
    #[serde(default)]
    pub photo: Option<PhotoSpec>,
    /// Expected report date (`YYYY-MM-DD`).
    pub issued_at: String,
}

/// A rendered file of a spec, and which of its pages it shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub file: String,
    pub output: String,
    /// 1-based spec pages, in file order.
    pub pages: Vec<u32>,
}

impl ReportSpec {
    pub fn variants(&self) -> Vec<Variant> {
        let all: Vec<u32> = (1..=self.pages.len() as u32).collect();
        let mut out = Vec::new();
        for o in &self.outputs {
            match o.as_str() {
                "pdf" => out.push(Variant { file: format!("{}.pdf", self.id), output: o.clone(), pages: all.clone() }),
                "scan" => out.push(Variant { file: format!("{}.scan.pdf", self.id), output: o.clone(), pages: all.clone() }),
                "photo" => out.extend(all.iter().map(|p| Variant {
                    file: format!("{}.photo-p{p}.jpg", self.id),
                    output: o.clone(),
                    pages: vec![*p],
                })),
                _ => {}
            }
        }
        out
    }

    /// The rows a variant shows, with their page number in that file.
    pub fn expected(&self, v: &Variant) -> Vec<(u32, &RowSpec)> {
        let mut out = Vec::new();
        for (i, spec_page) in v.pages.iter().enumerate() {
            let page = &self.pages[*spec_page as usize - 1];
            for s in &page.sections {
                out.extend(s.rows.iter().map(|r| (i as u32 + 1, r)));
            }
        }
        out
    }
}

/// SplitMix64, as in `synth`.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn normal(&mut self) -> f64 {
        let (u, v) = (self.unit().max(1e-12), self.unit());
        (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[(self.next() % items.len() as u64) as usize]
    }
}

/// How one analyte is printed by one kind of lab.
struct Gen {
    loinc: &'static str,
    names: &'static [&'static str],
    abbr: &'static str,
    unit: &'static str,
    ucum: &'static str,
    /// (low, high); `None` for one-sided ranges.
    range: (Option<f64>, Option<f64>),
    mean: f64,
    sd: f64,
    /// Change per year, to make the comparison interesting.
    trend: f64,
    decimals: usize,
}

#[allow(clippy::too_many_arguments)]
const fn g(
    loinc: &'static str,
    names: &'static [&'static str],
    abbr: &'static str,
    unit: &'static str,
    ucum: &'static str,
    range: (Option<f64>, Option<f64>),
    mean: f64,
    sd: f64,
    trend: f64,
    decimals: usize,
) -> Gen {
    Gen { loinc, names, abbr, unit, ucum, range, mean, sd, trend, decimals }
}

const S: Option<f64> = None;

#[rustfmt::skip]
const ZH_CBC: &[Gen] = &[
    g("6690-2", &["白细胞计数", "白细胞"], "WBC", "×10⁹/L", "10*9/L", (Some(3.5), Some(9.5)), 6.2, 1.6, 0.0, 2),
    g("789-8", &["红细胞计数"], "RBC", "×10¹²/L", "10*12/L", (Some(4.3), Some(5.8)), 4.9, 0.4, 0.0, 2),
    g("718-7", &["血红蛋白"], "HGB", "g/L", "g/L", (Some(130.0), Some(175.0)), 150.0, 12.0, 0.0, 0),
    g("4544-3", &["红细胞压积", "红细胞比容"], "HCT", "%", "%", (Some(40.0), Some(50.0)), 44.5, 3.0, 0.0, 1),
    g("787-2", &["平均红细胞体积"], "MCV", "fL", "fL", (Some(82.0), Some(100.0)), 90.0, 4.0, 0.0, 1),
    g("777-3", &["血小板计数"], "PLT", "×10⁹/L", "10*9/L", (Some(125.0), Some(350.0)), 230.0, 60.0, 0.0, 0),
    g("770-8", &["中性粒细胞百分比"], "NEUT%", "%", "%", (Some(40.0), Some(75.0)), 58.0, 8.0, 0.0, 1),
    g("736-9", &["淋巴细胞百分比"], "LYMPH%", "%", "%", (Some(20.0), Some(50.0)), 32.0, 7.0, 0.0, 1),
];

#[rustfmt::skip]
const ZH_LIPIDS: &[Gen] = &[
    g("2093-3", &["总胆固醇"], "TC", "mmol/L", "mmol/L", (S, Some(5.2)), 4.8, 0.5, 0.12, 2),
    g("2571-8", &["甘油三酯"], "TG", "mmol/L", "mmol/L", (S, Some(1.7)), 1.4, 0.5, 0.05, 2),
    g("2085-9", &["高密度脂蛋白胆固醇"], "HDL-C", "mmol/L", "mmol/L", (Some(1.04), S), 1.25, 0.2, -0.02, 2),
    g("13457-7", &["低密度脂蛋白胆固醇"], "LDL-C", "mmol/L", "mmol/L", (S, Some(3.4)), 3.0, 0.45, 0.15, 2),
];

#[rustfmt::skip]
const ZH_CHEM: &[Gen] = &[
    g("1558-6", &["空腹血糖", "葡萄糖"], "GLU", "mmol/L", "mmol/L", (Some(3.9), Some(6.1)), 5.3, 0.5, 0.05, 2),
    g("4548-4", &["糖化血红蛋白"], "HbA1c", "%", "%", (Some(4.0), Some(6.0)), 5.5, 0.3, 0.05, 1),
    g("1742-6", &["丙氨酸氨基转移酶", "谷丙转氨酶"], "ALT", "U/L", "U/L", (Some(9.0), Some(50.0)), 30.0, 12.0, 0.0, 0),
    g("1920-8", &["天门冬氨酸氨基转移酶"], "AST", "U/L", "U/L", (Some(15.0), Some(40.0)), 25.0, 7.0, 0.0, 0),
    g("2324-2", &["γ-谷氨酰转移酶"], "GGT", "U/L", "U/L", (Some(10.0), Some(60.0)), 35.0, 15.0, 0.0, 0),
    g("1975-2", &["总胆红素"], "TBIL", "μmol/L", "umol/L", (Some(0.0), Some(21.0)), 13.0, 4.0, 0.0, 1),
    g("1751-7", &["白蛋白"], "ALB", "g/L", "g/L", (Some(40.0), Some(55.0)), 46.0, 3.0, 0.0, 1),
    g("2160-0", &["肌酐"], "CREA", "μmol/L", "umol/L", (Some(57.0), Some(111.0)), 82.0, 12.0, 0.0, 0),
    g("3084-1", &["尿酸"], "UA", "μmol/L", "umol/L", (Some(208.0), Some(428.0)), 370.0, 60.0, 5.0, 0),
    g("22664-7", &["尿素"], "UREA", "mmol/L", "mmol/L", (Some(3.1), Some(8.0)), 5.2, 1.0, 0.0, 2),
    g("3016-3", &["促甲状腺激素"], "TSH", "mIU/L", "m[IU]/L", (Some(0.27), Some(4.2)), 2.1, 0.9, 0.0, 2),
];

#[rustfmt::skip]
const EN_SI: &[Gen] = &[
    g("6690-2", &["White Blood Cells", "WBC Count"], "", "x10^9/L", "10*9/L", (Some(4.0), Some(11.0)), 6.5, 1.6, 0.0, 1),
    g("718-7", &["Hemoglobin", "Haemoglobin"], "", "g/L", "g/L", (Some(130.0), Some(170.0)), 148.0, 11.0, 0.0, 0),
    g("777-3", &["Platelets", "Platelet Count"], "", "x10^9/L", "10*9/L", (Some(150.0), Some(400.0)), 240.0, 55.0, 0.0, 0),
    g("2093-3", &["Total Cholesterol", "Cholesterol, total"], "", "mmol/L", "mmol/L", (S, Some(5.2)), 4.9, 0.6, 0.1, 1),
    g("13457-7", &["LDL Cholesterol"], "", "mmol/L", "mmol/L", (S, Some(3.4)), 3.0, 0.5, 0.12, 1),
    g("2085-9", &["HDL Cholesterol"], "", "mmol/L", "mmol/L", (Some(1.0), S), 1.3, 0.25, 0.0, 1),
    g("2571-8", &["Triglycerides"], "", "mmol/L", "mmol/L", (S, Some(1.7)), 1.3, 0.5, 0.0, 1),
    g("1558-6", &["Glucose, fasting", "Fasting Glucose"], "", "mmol/L", "mmol/L", (Some(3.9), Some(6.0)), 5.2, 0.5, 0.0, 1),
    g("4548-4", &["HbA1c", "Hemoglobin A1c"], "", "%", "%", (Some(4.0), Some(6.0)), 5.5, 0.3, 0.0, 1),
    g("1742-6", &["ALT"], "", "U/L", "U/L", (Some(7.0), Some(40.0)), 26.0, 10.0, 0.0, 0),
    g("2160-0", &["Creatinine"], "", "umol/L", "umol/L", (Some(60.0), Some(110.0)), 85.0, 12.0, 0.0, 0),
    g("62238-1", &["eGFR"], "", "mL/min/1.73m²", "mL/min/{1.73_m2}", (Some(90.0), S), 96.0, 8.0, -1.0, 0),
    g("3016-3", &["TSH"], "", "mIU/L", "m[IU]/L", (Some(0.4), Some(4.0)), 1.9, 0.8, 0.0, 2),
    g("3024-7", &["Free T4"], "", "pmol/L", "pmol/L", (Some(12.0), Some(22.0)), 16.0, 2.5, 0.0, 1),
    g("1989-3", &["Vitamin D, 25-hydroxy"], "", "nmol/L", "nmol/L", (Some(50.0), Some(150.0)), 70.0, 20.0, 0.0, 0),
];

#[rustfmt::skip]
const EN_US: &[Gen] = &[
    g("6690-2", &["WBC"], "", "10^3/uL", "10*3/uL", (Some(3.8), Some(10.8)), 6.4, 1.6, 0.0, 1),
    g("718-7", &["Hemoglobin"], "", "g/dL", "g/dL", (Some(13.2), Some(17.1)), 15.0, 1.1, 0.0, 1),
    g("777-3", &["Platelets"], "", "10^3/uL", "10*3/uL", (Some(140.0), Some(400.0)), 240.0, 55.0, 0.0, 0),
    g("2093-3", &["Total Cholesterol"], "", "mg/dL", "mg/dL", (S, Some(200.0)), 188.0, 22.0, 4.0, 0),
    g("13457-7", &["LDL Cholesterol"], "", "mg/dL", "mg/dL", (S, Some(100.0)), 112.0, 18.0, 5.0, 0),
    g("2085-9", &["HDL Cholesterol"], "", "mg/dL", "mg/dL", (Some(40.0), S), 50.0, 9.0, 0.0, 0),
    g("2571-8", &["Triglycerides"], "", "mg/dL", "mg/dL", (S, Some(150.0)), 120.0, 40.0, 0.0, 0),
    g("1558-6", &["Glucose"], "", "mg/dL", "mg/dL", (Some(70.0), Some(99.0)), 92.0, 9.0, 1.0, 0),
    g("4548-4", &["Hemoglobin A1c"], "", "%", "%", (S, Some(5.7)), 5.4, 0.3, 0.0, 1),
    g("1742-6", &["ALT"], "", "U/L", "U/L", (Some(9.0), Some(46.0)), 28.0, 10.0, 0.0, 0),
    g("2160-0", &["Creatinine"], "", "mg/dL", "mg/dL", (Some(0.7), Some(1.33)), 0.98, 0.14, 0.0, 2),
    g("3094-0", &["BUN"], "", "mg/dL", "mg/dL", (Some(7.0), Some(25.0)), 15.0, 4.0, 0.0, 0),
    g("3084-1", &["Uric Acid"], "", "mg/dL", "mg/dL", (Some(4.0), Some(8.0)), 6.2, 1.1, 0.0, 1),
    g("3016-3", &["TSH"], "", "uIU/mL", "u[IU]/mL", (Some(0.4), Some(4.5)), 1.9, 0.8, 0.0, 2),
    g("30522-7", &["hs-CRP"], "", "mg/L", "mg/L", (S, Some(1.0)), 0.8, 0.5, 0.0, 1),
    g("2276-4", &["Ferritin"], "", "ng/mL", "ng/mL", (Some(38.0), Some(380.0)), 150.0, 70.0, 0.0, 0),
];

fn fmt(v: f64, decimals: usize) -> String {
    format!("{v:.decimals$}")
}

fn range_text(range: (Option<f64>, Option<f64>), decimals: usize) -> String {
    match range {
        (Some(l), Some(h)) => format!("{}-{}", fmt(l, decimals), fmt(h, decimals)),
        (None, Some(h)) => format!("<{}", fmt(h, decimals)),
        (Some(l), None) => format!(">{}", fmt(l, decimals)),
        (None, None) => String::new(),
    }
}

/// Range bounds rounded as printed.
fn printed_bounds(range: (Option<f64>, Option<f64>), decimals: usize) -> (Option<f64>, Option<f64>) {
    let r = |v: Option<f64>| v.map(|v| fmt(v, decimals).parse::<f64>().expect("formatted number"));
    (r(range.0), r(range.1))
}

struct Style {
    zh: bool,
    /// Flag glyphs for high and low.
    flags: (&'static str, &'static str),
    /// Whether the flag is printed right after the value (`3.60↑`).
    attached: bool,
    abbr_column: bool,
}

fn row(rng: &mut Rng, gen: &Gen, years: f64, style: &Style) -> RowSpec {
    // A quarter of the values fall outside their range, to exercise flags.
    let mut v = gen.mean + gen.trend * years + gen.sd * rng.normal() * 0.6;
    if rng.unit() < 0.25 {
        let push = gen.sd * (1.2 + rng.unit());
        v = match gen.range {
            (_, Some(h)) if rng.unit() < 0.6 || gen.range.0.is_none() => h + push,
            (Some(l), _) => (l - push).max(gen.mean * 0.2),
            _ => v,
        };
    }
    let lo_floor = 10f64.powi(-(gen.decimals as i32));
    let printed = fmt(v.max(lo_floor), gen.decimals);
    let value: f64 = printed.parse().expect("formatted number");
    // Ranges print with at least the value's decimals (`<5.20`, `0.27-4.20`).
    let range_dec = [gen.range.0, gen.range.1]
        .iter()
        .flatten()
        .map(|b| match b * 100.0 {
            x if (x - (x / 100.0).round() * 100.0).abs() < 1e-6 => 0,
            x if (x - (x / 10.0).round() * 10.0).abs() < 1e-6 => 1,
            _ => 2,
        })
        .max()
        .unwrap_or(0)
        .max(gen.decimals);
    let (ref_low, ref_high) = printed_bounds(gen.range, range_dec);
    let flag = crate::labs::against_range(value, ref_low, ref_high).map(|d| match d {
        crate::labs::Direction::High => style.flags.0,
        crate::labs::Direction::Low => style.flags.1,
    });
    let name = rng.pick(gen.names).to_string();
    let (name, abbr) = if style.abbr_column && !gen.abbr.is_empty() {
        (name, Some(gen.abbr.to_string()))
    } else if style.zh && !gen.abbr.is_empty() {
        (format!("{name}({})", gen.abbr), None)
    } else {
        (name, None)
    };
    RowSpec {
        name,
        abbr,
        value: if style.attached { format!("{printed}{}", flag.unwrap_or("")) } else { printed },
        flag: if style.attached { None } else { flag.map(String::from) },
        unit: Some(gen.unit.to_string()),
        range: Some(range_text(gen.range, range_dec)),
        truth: Truth {
            loinc: gen.loinc.into(),
            value: Some(value),
            value_text: None,
            unit: Some(gen.ucum.into()),
            ref_low,
            ref_high,
            flag: flag.map(String::from),
        },
    }
}

fn urine_rows() -> Vec<RowSpec> {
    let q = |name: &str, loinc: &str| RowSpec {
        name: name.into(),
        abbr: None,
        value: "阴性".into(),
        flag: None,
        unit: None,
        range: Some("阴性".into()),
        truth: Truth {
            loinc: loinc.into(),
            value: None,
            value_text: Some("阴性".into()),
            unit: None,
            ref_low: None,
            ref_high: None,
            flag: None,
        },
    };
    vec![q("尿蛋白(PRO)", "20454-5"), q("葡萄糖(GLU)", "25428-4")]
}

fn columns(layout: &str) -> Vec<Column> {
    let c = |key: &str, label: &str, x: f64| Column { key: key.into(), label: label.into(), x };
    match layout {
        "zh-a" => vec![c("name", "项目", 50.0), c("value", "结果", 250.0), c("flag", "提示", 315.0), c("unit", "单位", 360.0), c("range", "参考范围", 460.0)],
        "zh-b" => vec![c("name", "检验项目", 50.0), c("abbr", "缩写", 200.0), c("value_flag", "结果", 262.0), c("unit", "单位", 340.0), c("range", "参考区间", 450.0)],
        "en-a" => vec![c("name", "Test", 50.0), c("value", "Result", 240.0), c("flag", "Flag", 300.0), c("unit", "Units", 340.0), c("range", "Reference Interval", 440.0)],
        _ => vec![c("name", "Test", 50.0), c("value", "Result", 210.0), c("unit", "Units", 280.0), c("range", "Reference Range", 380.0), c("flag", "Flag", 500.0)],
    }
}

const ZH_PROVIDERS: &[&str] = &["青松健康体检中心（示例）", "江畔医院体检科（示例）", "云杉社区卫生院（示例）"];
const EN_PROVIDERS: &[&str] = &["Northwind Clinic (Synthetic)", "Harbor Lab Services (Sample)", "Maple Health Centre (Test)"];
const MONTHS: &[&str] = &["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// The eval set: `count` reports alternating zh and en layouts across years.
pub fn eval_specs(count: usize, seed: u64) -> Vec<ReportSpec> {
    let mut rng = Rng(seed);
    let layouts = ["zh-a", "en-a", "zh-b", "en-b"];
    (0..count)
        .map(|i| {
            let layout = layouts[i % layouts.len()];
            let zh = layout.starts_with("zh");
            let year = 2019 + (i as i32 % 7);
            let month = 1 + (rng.next() % 12) as u32;
            let day = 1 + (rng.next() % 28) as u32;
            let date = chrono::NaiveDate::from_ymd_opt(year, month, day).expect("valid date");
            let years = (year - 2019) as f64;
            let style = Style {
                zh,
                flags: if zh { ("↑", "↓") } else { ("H", "L") },
                attached: layout == "zh-b",
                abbr_column: layout == "zh-b",
            };
            let pick = |rng: &mut Rng, gens: &[Gen]| -> Vec<RowSpec> { gens.iter().map(|g| row(rng, g, years, &style)).collect() };
            let id = format!("eval-{i:02}-{layout}-{year}");
            let (provider, title, header, pages) = if zh {
                let date_text = match i % 3 {
                    0 => format!("报告日期：{}年{:02}月{:02}日", year, month, day),
                    1 => format!("报告日期：{date}"),
                    _ => format!("体检日期：{}", date.format("%Y/%m/%d")),
                };
                let notes = vec!["注：本报告为合成测试数据，仅用于软件测试。".to_string()];
                let page1 = PageSpec {
                    sections: vec![
                        SectionSpec { title: "血常规".into(), rows: pick(&mut rng, ZH_CBC) },
                        SectionSpec { title: "血脂".into(), rows: pick(&mut rng, ZH_LIPIDS) },
                    ],
                    notes: vec![],
                };
                let page2 = PageSpec {
                    sections: vec![
                        SectionSpec { title: "生化检查".into(), rows: pick(&mut rng, ZH_CHEM) },
                        SectionSpec { title: "尿常规".into(), rows: urine_rows() },
                    ],
                    notes,
                };
                (
                    rng.pick(ZH_PROVIDERS).to_string(),
                    "健康体检报告".to_string(),
                    vec!["姓名：测试用户".into(), "性别：男".into(), format!("年龄：{}岁", year - 1983), format!("体检编号：T{}", date.format("%Y%m%d")), date_text],
                    vec![page1, page2],
                )
            } else {
                let gens = if layout == "en-a" { EN_SI } else { EN_US };
                let rows = pick(&mut rng, gens);
                let (a, b) = rows.split_at(rows.len() / 2);
                let date_text = match i % 3 {
                    0 => format!("Report date: {} {} {}", day, MONTHS[month as usize - 1], year),
                    1 => format!("Reported: {} {}, {}", MONTHS[month as usize - 1], day, year),
                    _ => format!("Collected: {date}"),
                };
                (
                    rng.pick(EN_PROVIDERS).to_string(),
                    "Laboratory Report".to_string(),
                    vec!["Patient: Test Patient (synthetic)".into(), "Sex: M".into(), "DOB: 1983-01-01".into(), date_text],
                    vec![
                        PageSpec { sections: vec![SectionSpec { title: "Hematology and Lipids".into(), rows: a.to_vec() }], notes: vec![] },
                        PageSpec {
                            sections: vec![SectionSpec { title: "Chemistry".into(), rows: b.to_vec() }],
                            notes: vec!["Synthetic test data for software testing only.".into()],
                        },
                    ],
                )
            };
            ReportSpec {
                id,
                language: if zh { "zh" } else { "en" }.into(),
                provider,
                title,
                header,
                columns: columns(layout),
                pages,
                outputs: vec!["pdf".into(), "scan".into(), "photo".into()],
                photo: Some(PhotoSpec {
                    angle: (rng.unit() * 4.0 - 2.0) * 1.2,
                    blur: 0.5 + rng.unit() * 0.5,
                    noise: 0.04 + rng.unit() * 0.04,
                    perspective: (rng.unit() < 0.5).then(|| 0.01 + rng.unit() * 0.03),
                }),
                issued_at: date.to_string(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_specs_are_deterministic_and_consistent() {
        let a = eval_specs(8, 42);
        assert_eq!(a, eval_specs(8, 42));
        for spec in &a {
            for v in spec.variants() {
                for (_, row) in spec.expected(&v) {
                    let t = &row.truth;
                    assert!(crate::labs::by_loinc(&t.loinc).is_some(), "{}", t.loinc);
                    if let Some(u) = &t.unit {
                        assert_eq!(crate::labs::normalize_unit(row.unit.as_deref().unwrap()), Some(u.as_str()), "{}", row.name);
                    }
                    if let Some(range) = &row.range {
                        let r = crate::labs::parse_range(range).unwrap();
                        assert_eq!((r.low, r.high), (t.ref_low, t.ref_high), "{} {range}", row.name);
                    }
                }
            }
        }
    }
}
