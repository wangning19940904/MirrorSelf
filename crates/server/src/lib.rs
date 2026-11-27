//! MirrorSelf access layer (architecture §8–9): the REST API and the MCP
//! server. Both call the same tools over `QueryService`, so scopes, results
//! and audit entries match whichever way a client connects. `upload` adds
//! the phone upload page and its pairing flow.

pub mod mcp;
pub mod rest;
pub mod tools;
pub mod upload;
