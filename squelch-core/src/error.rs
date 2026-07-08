//! Error types for squelch-core.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    /// Thread/message not found. Sealed threads MUST surface as this over MCP
    /// so they are indistinguishable from nonexistent ones.
    #[error("not found")]
    NotFound,

    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("credential error: {0}")]
    Credential(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, CoreError>;
