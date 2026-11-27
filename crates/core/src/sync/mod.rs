//! Scheduled sync (workstream W3 in `docs/demo-plan.md`; reference in
//! `docs/sync.md`).
//!
//! A recipe (`[[sync]]` in `mirrorself.toml`) names one MCP tool or one
//! program, its arguments with window placeholders, and how often to run.
//! `mirrorself sync run` fetches, stores each response as an immutable raw
//! object (`mcp_response`, `cli_response` or a typed kind such as
//! `feishu_calendar_events`), advances the recipe's cursor and runs the
//! pipeline. Fetching never happens inside `process`; the processors here
//! only parse responses already in the vault.
//!
//! Safety: only the tool a recipe names is called, only when the server
//! annotates it read-only, and only while its input schema matches the one
//! pinned on the first run. Responses are data and never choose a call.

pub mod generic;
pub mod github;
pub mod mcp;
pub mod process;
pub mod recipe;
pub mod runner;
pub mod secrets;
pub mod state;

use crate::sources::{clear_standard, RawProcessor, Source};

pub use recipe::{Recipe, RecipeDef, Window, KIND_CLI_RESPONSE, KIND_MCP_RESPONSE};
pub use runner::{daemon, is_due, list, resume, run_due, run_recipe, RunOutcome};
pub use secrets::SecretStore;

pub static SOURCE: Source = Source {
    raw: &[
        RawProcessor {
            processor: generic::PROCESSOR,
            kinds: &[KIND_MCP_RESPONSE, KIND_CLI_RESPONSE],
            load: generic::load,
            clear: clear_standard,
        },
        RawProcessor {
            processor: github::PROCESSOR,
            kinds: &[KIND_MCP_RESPONSE],
            load: github::load,
            clear: clear_standard,
        },
    ],
    post_link: &[github::LINKS],
    ..Source::empty("sync")
};
