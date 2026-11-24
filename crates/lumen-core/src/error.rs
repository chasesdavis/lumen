//! Error type shared across all Lumen crates.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum LumenError {
    #[error("storage backend error: {0}")]
    Storage(String),

    #[error("crypto failure: {0}")]
    Crypto(String),

    #[error("integrity check failed: chunk {0} failed authentication")]
    Integrity(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("auth error: {0}")]
    Auth(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, LumenError>;
