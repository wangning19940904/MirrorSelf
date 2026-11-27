//! Scheduled sync (workstream W3 in `docs/demo-plan.md`).
//!
//! Placeholder: the source and the config type are registered so the
//! registry and `Config` do not change when the scheduler lands. Raw kinds:
//! `mcp_response`, `cli_response`.

use serde::{Deserialize, Serialize};

use crate::sources::Source;

pub static SOURCE: Source = Source::empty("sync");

/// One `[[sync]]` table in `mirrorself.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Recipe {
    pub name: String,
}
