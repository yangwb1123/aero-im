//! Error type for the AI control plane.
//!
//! Library crates return `Result<T, AiError>`. The server upgrades these to HTTP
//! responses or job-failure rows; the worker logs them and reschedules the job.

use thiserror::Error;

pub type Result<T, E = AiError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum AiError {
    /// Transport-level failure talking to an upstream API (network, TLS, timeout).
    #[error("http: {0}")]
    Http(String),

    /// Anthropic Messages API returned a non-2xx response.
    #[error("anthropic: {status}: {message}")]
    Anthropic { status: u16, message: String },

    /// Embedding provider failure (Voyage, etc.) or local embedder error.
    #[error("embedding: {0}")]
    Embedding(String),

    /// Persistence error surfaced from `aero-storage` (sqlx).
    #[error("storage: {0}")]
    Storage(String),

    /// JSON encode/decode error on a request or response payload.
    #[error("json: {0}")]
    Json(String),

    /// Required configuration is missing (e.g. API key for a requested provider).
    #[error("config: {0}")]
    Config(String),

    /// The referenced entity (message, room) was not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// Caller passed semantically invalid input.
    #[error("invalid input: {0}")]
    Invalid(String),

    /// Catch-all for unexpected internal errors.
    #[error("internal: {0}")]
    Internal(String),
}

impl From<reqwest::Error> for AiError {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e.to_string())
    }
}

impl From<sqlx::Error> for AiError {
    fn from(e: sqlx::Error) -> Self {
        match e {
            sqlx::Error::RowNotFound => Self::NotFound("row not found".into()),
            other => Self::Storage(other.to_string()),
        }
    }
}

impl From<serde_json::Error> for AiError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e.to_string())
    }
}

impl From<anyhow::Error> for AiError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e.to_string())
    }
}
