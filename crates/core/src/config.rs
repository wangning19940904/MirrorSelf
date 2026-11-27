//! The MirrorSelf home directory and its `mirrorself.toml`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::enrich::ModelConfig;
use crate::sync::Recipe;

/// Public HTTP origin of MirrorSelf's reserved port block (12500–12599 in
/// the host's service registry). Loopback only; see AGENTS.md.
pub const DEFAULT_ADDR: &str = "127.0.0.1:12500";

#[derive(Debug, Clone)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `--home`, then `MIRRORSELF_HOME`, then `~/.mirrorself`.
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self> {
        if let Some(p) = explicit {
            return Ok(Self::new(p));
        }
        if let Some(p) = std::env::var_os("MIRRORSELF_HOME") {
            return Ok(Self::new(p));
        }
        let home = std::env::var_os("HOME").context("HOME is not set; pass --home")?;
        Ok(Self::new(PathBuf::from(home).join(".mirrorself")))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("mirrorself.toml")
    }

    pub fn key_path(&self) -> PathBuf {
        self.root.join("vault.key")
    }

    pub fn token_path(&self) -> PathBuf {
        self.root.join("api.token")
    }

    pub fn index_path(&self) -> PathBuf {
        self.root.join("index.db")
    }

    pub fn default_vault_dir(&self) -> PathBuf {
        self.root.join("vault")
    }

    pub fn is_initialized(&self) -> bool {
        self.config_path().exists() && self.key_path().exists()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// `server` (personal server, mode B). Device and cloud modes come later.
    pub mode: String,
    /// Storage adapter URI, e.g. `local:/data/mirrorself`.
    pub storage: String,
    #[serde(default)]
    pub pipeline: PipelineConfig,
    #[serde(default)]
    pub server: ServerConfig,
    /// Optional model for files that rules cannot read (`[model]`).
    #[serde(default, skip_serializing_if = "ModelConfig::is_unset")]
    pub model: ModelConfig,
    /// Scheduled sync recipes (`[[sync]]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sync: Vec<Recipe>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    /// Sleep that starts at or after this local hour counts toward the next
    /// day's night (18 → a 23:30 bedtime belongs to the following date).
    #[serde(default = "default_sleep_boundary")]
    pub sleep_day_boundary_hour: u32,
    /// |z| at or above this marks a daily value as notable on the timeline.
    #[serde(default = "default_notable_z")]
    pub notable_z: f64,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            sleep_day_boundary_hour: default_sleep_boundary(),
            notable_z: default_notable_z(),
        }
    }
}

fn default_sleep_boundary() -> u32 {
    18
}

fn default_notable_z() -> f64 {
    2.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_addr")]
    pub addr: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            addr: default_addr(),
        }
    }
}

fn default_addr() -> String {
    DEFAULT_ADDR.to_string()
}

impl Config {
    pub fn new(mode: impl Into<String>, storage: impl Into<String>) -> Self {
        Self {
            mode: mode.into(),
            storage: storage.into(),
            pipeline: PipelineConfig::default(),
            server: ServerConfig::default(),
            model: ModelConfig::default(),
            sync: Vec::new(),
        }
    }

    pub fn load(home: &Home) -> Result<Self> {
        let path = home.config_path();
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, home: &Home) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        std::fs::write(home.config_path(), text)?;
        Ok(())
    }
}
