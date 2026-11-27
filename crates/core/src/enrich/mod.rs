//! Optional model-based enrichment (workstream W5 in `docs/demo-plan.md`).
//!
//! Placeholder: the config type is registered so `Config` does not change
//! when enrichment lands.

use serde::{Deserialize, Serialize};

/// The `[model]` table in `mirrorself.toml`. Unset means no model: files
/// that rules cannot read are stored and left for review.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Base URL of an OpenAI-compatible endpoint, local or privately deployed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl ModelConfig {
    pub fn is_unset(&self) -> bool {
        self.endpoint.is_none() && self.model.is_none()
    }
}
