//! MirrorSelf core: the schema, the encrypted raw vault, the processing
//! pipeline and the `QueryService` shared by the CLI, REST API and MCP server.
//!
//! Data flows one way: raw objects go into the vault unchanged, processors
//! derive the index from them, and every surface reads the index through
//! [`query::QueryService`]. Everything after the vault can be rebuilt.

mod app;
pub mod audit;
pub mod codes;
pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod eval;
pub mod ids;
pub mod importers;
pub mod labs;
pub mod models;
pub mod ops;
pub mod pipeline;
pub mod query;
pub mod storage;
pub mod synth;
pub mod synth_report;
pub mod time;
pub mod vault;

pub use app::{App, Decision, ImportKind, ImportReport, InitReport, Lang};
pub use error::UserError;
