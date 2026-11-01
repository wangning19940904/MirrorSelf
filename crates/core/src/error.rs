//! Errors the caller can act on. Surfaces downcast `anyhow::Error` to
//! [`UserError`] to pick an exit code, HTTP status or MCP error flag;
//! anything else is an internal failure.

use std::fmt;
use std::path::PathBuf;

#[derive(Debug)]
pub enum UserError {
    /// No MirrorSelf home at this path; run `mirrorself init`.
    NotInitialized(PathBuf),
    /// The requested record, metric or object does not exist.
    NotFound(String),
    /// The request itself is malformed (bad date, unknown option).
    Invalid(String),
    /// The request is well formed but not permitted (e.g. SQL outside v_* views).
    Denied(String),
    /// A resource is already taken (a port, an initialized home).
    Conflict(String),
}

impl fmt::Display for UserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UserError::NotInitialized(p) => write!(
                f,
                "no MirrorSelf home at {} (run `mirrorself init` first)",
                p.display()
            ),
            UserError::NotFound(m)
            | UserError::Invalid(m)
            | UserError::Denied(m)
            | UserError::Conflict(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for UserError {}

pub fn not_found(msg: impl Into<String>) -> anyhow::Error {
    UserError::NotFound(msg.into()).into()
}

pub fn invalid(msg: impl Into<String>) -> anyhow::Error {
    UserError::Invalid(msg.into()).into()
}

pub fn denied(msg: impl Into<String>) -> anyhow::Error {
    UserError::Denied(msg.into()).into()
}

pub fn conflict(msg: impl Into<String>) -> anyhow::Error {
    UserError::Conflict(msg.into()).into()
}

/// The [`UserError`] behind an error chain, if there is one.
pub fn user_error(err: &anyhow::Error) -> Option<&UserError> {
    err.chain().find_map(|e| e.downcast_ref::<UserError>())
}
