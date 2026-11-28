//! `recorded:<dir>`: replays OCR saved by `mirrorself dev ocr --record`.
//!
//! Each `*.ocr.json` file holds one document's OCR output and the SHA-256
//! of the original bytes it came from, so a replay can only answer for the
//! exact file that was recognized.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{DocumentInput, ModelProvider, OcrDocument};
use crate::crypto::sha256_hex;
use crate::error::invalid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recording {
    /// SHA-256 (hex) of the original file.
    pub sha256: String,
    /// The original file's name, for people reading the fixture.
    pub file: String,
    pub ocr: OcrDocument,
}

pub struct Recorded {
    dir: PathBuf,
    by_hash: HashMap<String, OcrDocument>,
}

impl Recorded {
    /// Loads every `*.ocr.json` under `dir` (recursively).
    pub fn open(dir: PathBuf) -> Result<Self> {
        let mut by_hash = HashMap::new();
        load_dir(&dir, &mut by_hash)?;
        Ok(Self { dir, by_hash })
    }
}

fn load_dir(dir: &Path, out: &mut HashMap<String, OcrDocument>) -> Result<()> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading recorded OCR in {}", dir.display()))?;
    for entry in entries {
        let path = entry?.path();
        if path.is_dir() {
            load_dir(&path, out)?;
        } else if path.to_string_lossy().ends_with(".ocr.json") {
            let text = std::fs::read_to_string(&path)?;
            let rec: Recording = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
            out.insert(rec.sha256, rec.ocr);
        }
    }
    Ok(())
}

impl ModelProvider for Recorded {
    fn name(&self) -> String {
        "recorded".into()
    }

    fn ocr(&self, input: &DocumentInput<'_>) -> Result<OcrDocument> {
        let hash = sha256_hex(input.bytes);
        self.by_hash.get(&hash).cloned().ok_or_else(|| {
            invalid(format!(
                "no recorded OCR for this file (sha256 {}) in {}",
                &hash[..16],
                self.dir.display()
            ))
        })
    }
}
