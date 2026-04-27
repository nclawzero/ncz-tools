//! Typed error enum + exit-code mapping. Bash contract:
//!   0  success
//!   1  usage error
//!   2  missing dependency / precondition fail
//!   3  inconsistent state
//!
//! `NczError::exit_code()` is the single source of truth for the mapping.
//! Variants are coarse on purpose; reach for `.to_string()` context rather
//! than adding a new variant per call site.

use std::io;

#[derive(thiserror::Error, Debug)]
pub enum NczError {
    #[error("usage: {0}")]
    Usage(String),

    #[error("missing dependency: {0}")]
    MissingDep(String),

    #[error("precondition failed: {0}")]
    Precondition(String),

    #[error("inconsistent state: {0}")]
    Inconsistent(String),

    #[error("subprocess `{cmd}` failed: {msg}")]
    Exec { cmd: String, msg: String },

    #[error(transparent)]
    Io(#[from] io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl NczError {
    pub fn exit_code(&self) -> i32 {
        match self {
            NczError::Usage(_) => 1,
            NczError::Inconsistent(_) => 3,
            _ => 2,
        }
    }
}
