//! `apple-vision`: OCR through the Swift helper in `crates/core/swift/`.
//!
//! The helper's source is compiled into this binary and built with `swiftc`
//! on first use (the Xcode command-line tools that Rust already needs on
//! macOS include it), then cached by source hash. Document bytes go to the
//! helper on stdin and never touch the disk.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};

use super::{DocumentInput, ModelProvider, OcrDocument};
use crate::crypto::sha256_hex;

pub const HELPER_SOURCE: &str = include_str!("../../swift/mirrorself-ocr.swift");

pub struct AppleVision {
    helper: Option<PathBuf>,
}

impl AppleVision {
    /// `helper` overrides the compiled-on-demand binary (`[ocr] helper`).
    pub fn new(helper: Option<PathBuf>) -> Self {
        Self { helper }
    }

    fn helper_path(&self) -> Result<PathBuf> {
        if let Some(p) = &self.helper {
            return Ok(p.clone());
        }
        ensure_helper(&cache_dir()?)
    }
}

impl ModelProvider for AppleVision {
    fn name(&self) -> String {
        "apple-vision".into()
    }

    fn ocr(&self, input: &DocumentInput<'_>) -> Result<OcrDocument> {
        if !cfg!(target_os = "macos") {
            bail!("the apple-vision provider needs macOS (set [ocr] provider to recorded:<dir> elsewhere)");
        }
        let helper = self.helper_path()?;
        let mut cmd = Command::new(&helper);
        cmd.args(["recognize", "--mime", input.mime]);
        if let Some(lang) = input.lang {
            cmd.args(["--lang", lang]);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting {}", helper.display()))?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let bytes = input.bytes.to_vec();
        let writer = std::thread::spawn(move || stdin.write_all(&bytes));
        let mut out = Vec::new();
        child.stdout.take().expect("piped stdout").read_to_end(&mut out)?;
        let mut err = String::new();
        child.stderr.take().expect("piped stderr").read_to_string(&mut err)?;
        let status = child.wait()?;
        let _ = writer.join();
        if !status.success() {
            bail!("OCR helper failed: {}", err.trim());
        }
        serde_json::from_slice(&out).context("the OCR helper returned unreadable output")
    }
}

fn cache_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("MIRRORSELF_CACHE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").context("HOME is not set; set MIRRORSELF_CACHE_DIR")?;
    Ok(PathBuf::from(home).join("Library/Caches/mirrorself"))
}

/// Builds the helper into `dir` unless a binary for this source exists.
pub fn ensure_helper(dir: &Path) -> Result<PathBuf> {
    static BUILD: Mutex<()> = Mutex::new(());
    let _guard = BUILD.lock().unwrap_or_else(|p| p.into_inner());
    let hash = &sha256_hex(HELPER_SOURCE.as_bytes())[..12];
    let bin = dir.join(format!("mirrorself-ocr-{hash}"));
    if bin.exists() {
        return Ok(bin);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let pid = std::process::id();
    let src = dir.join(format!("mirrorself-ocr-{hash}.{pid}.swift"));
    let tmp = dir.join(format!("mirrorself-ocr-{hash}.{pid}.tmp"));
    std::fs::write(&src, HELPER_SOURCE)?;
    let output = Command::new("xcrun")
        .args(["swiftc", "-O", "-o"])
        .arg(&tmp)
        .arg(&src)
        .output()
        .context("running xcrun swiftc (install the Xcode command-line tools: xcode-select --install)")?;
    let _ = std::fs::remove_file(&src);
    if !output.status.success() {
        let _ = std::fs::remove_file(&tmp);
        bail!("compiling the OCR helper failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    std::fs::rename(&tmp, &bin)?;
    Ok(bin)
}

/// True when this host can run the helper (macOS with `swiftc`).
pub fn available() -> bool {
    cfg!(target_os = "macos")
        && Command::new("xcrun")
            .args(["--find", "swiftc"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}
