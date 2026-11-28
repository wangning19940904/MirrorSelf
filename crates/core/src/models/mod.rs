//! Model providers (architecture §5 stage 5b, §12): OCR and structured lab
//! extraction behind one [`ModelProvider`] trait, so each host can use what
//! it has.
//!
//! - `apple-vision` (macOS): PDF text layers via PDFKit, scans and photos via
//!   Apple Vision, through a small Swift helper compiled on first use.
//! - `recorded:<dir>` (any host): replays OCR saved with
//!   `mirrorself dev ocr --record`. CI and the eval set use it, since Linux
//!   runners have no Vision.
//!
//! Extraction defaults to [`rules::extract`], a deterministic layout parser
//! that fills the §5 lab schema. An LLM provider (Ollama, Apple Foundation
//! Models, an opt-in cloud API) would override [`ModelProvider::extract`] and
//! return the same schema; the pipeline validates every provider's output
//! the same way ([`validate`]).

pub mod apple;
pub mod recorded;
pub mod rules;
mod validate;

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::OcrConfig;
use crate::error::invalid;

pub use validate::{validate, Context as ValidateContext, Reason, Validated};

/// Version of the built-in extractor; part of `report.extract`'s outputs.
pub const RULES_VERSION: &str = "1.0.0";

#[derive(Debug, Clone, Copy)]
pub struct DocumentInput<'a> {
    pub mime: &'a str,
    pub bytes: &'a [u8],
    /// `zh` or `en`, from `import report --lang`.
    pub lang: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OcrWord {
    pub text: String,
    /// `[x0, y0, x1, y1]`, top-left origin: PDF points or image pixels.
    pub bbox: [f64; 4],
    pub confidence: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OcrLine {
    pub text: String,
    pub bbox: [f64; 4],
    pub confidence: f64,
    #[serde(default)]
    pub words: Vec<OcrWord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OcrPage {
    pub page: u32,
    pub width: f64,
    pub height: f64,
    /// `text_layer` (exact text from a PDF) or `ocr`.
    pub source: String,
    pub lines: Vec<OcrLine>,
    /// A second recognition pass over the same image, used to confirm
    /// numbers, units and ranges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt_lines: Option<Vec<OcrLine>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OcrDocument {
    pub engine: String,
    pub engine_version: String,
    pub pages: Vec<OcrPage>,
}

impl OcrDocument {
    /// Page text in reading order, one line per OCR line.
    pub fn page_text(page: &OcrPage) -> String {
        let mut lines: Vec<&OcrLine> = page.lines.iter().collect();
        lines.sort_by(|a, b| {
            let (ya, yb) = ((a.bbox[1] + a.bbox[3]) / 2.0, (b.bbox[1] + b.bbox[3]) / 2.0);
            let h = (a.bbox[3] - a.bbox[1]).max(b.bbox[3] - b.bbox[1]).max(1.0);
            if (ya - yb).abs() < h * 0.5 {
                a.bbox[0].total_cmp(&b.bbox[0])
            } else {
                ya.total_cmp(&yb)
            }
        });
        lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n")
    }
}

/// The lab extraction output schema (§5), shared by every provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LabExtraction {
    pub report: ReportInfo,
    pub results: Vec<LabResult>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReportInfo {
    /// `YYYY-MM-DD` as printed on the report.
    pub issued_at: Option<String>,
    pub provider: Option<String>,
    /// `zh` or `en`.
    pub language: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LabResult {
    /// The analyte exactly as printed (untrusted document text).
    pub analyte_name: String,
    pub loinc: Option<String>,
    pub value: Option<f64>,
    /// Qualitative results (`阴性`, `negative`) or values that did not parse.
    pub value_text: Option<String>,
    /// Unit as printed.
    pub unit: Option<String>,
    pub ref_low: Option<f64>,
    pub ref_high: Option<f64>,
    pub flag_as_printed: Option<String>,
    /// 1-based page of the source document.
    pub page: u32,
    pub bbox: [f64; 4],
    pub confidence: f64,
    /// The printed reference range text (`0-3.40`, `<5.20`, `阴性`).
    #[serde(default)]
    pub ref_text: Option<String>,
}

pub trait ModelProvider: Send + Sync {
    /// Stable name recorded with every document it read, e.g. `apple-vision`.
    fn name(&self) -> String;

    fn ocr(&self, input: &DocumentInput<'_>) -> Result<OcrDocument>;

    /// Fills the §5 schema from OCR output. The default is the deterministic
    /// layout parser; LLM providers override this.
    fn extract(&self, ocr: &OcrDocument, lang: Option<&str>) -> Result<LabExtraction> {
        Ok(rules::extract(ocr, lang))
    }

    /// Name and version of the extractor, recorded with each extraction.
    fn extractor(&self) -> String {
        format!("layout-rules@{RULES_VERSION}")
    }
}

/// Used where no OCR is available; importing still stores the file.
pub struct Unavailable(pub String);

impl ModelProvider for Unavailable {
    fn name(&self) -> String {
        "none".into()
    }

    fn ocr(&self, _input: &DocumentInput<'_>) -> Result<OcrDocument> {
        Err(invalid(self.0.clone()))
    }
}

/// The provider named in `[ocr] provider` (default: `apple-vision` on
/// macOS, none elsewhere).
pub fn from_config(cfg: &OcrConfig) -> Result<Box<dyn ModelProvider>> {
    let name = cfg.provider.clone().unwrap_or_else(|| {
        if cfg!(target_os = "macos") { "apple-vision" } else { "none" }.to_string()
    });
    match name.split_once(':') {
        Some(("recorded", dir)) => Ok(Box::new(recorded::Recorded::open(PathBuf::from(dir))?)),
        _ if name == "apple-vision" => Ok(Box::new(apple::AppleVision::new(cfg.helper.clone()))),
        _ if name == "none" => Ok(Box::new(Unavailable(
            "no OCR provider on this host: set [ocr] provider in mirrorself.toml \
             (apple-vision on macOS, or recorded:<dir> to replay saved OCR)"
                .into(),
        ))),
        _ => Err(invalid(format!(
            "unknown model provider '{name}' (expected apple-vision, recorded:<dir> or none)"
        ))),
    }
}
