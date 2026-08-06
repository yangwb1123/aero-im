//! Workspace-wide error type used by library crates.
//!
//! Library crates return [`Result<T>`]. The `aero-server` binary upgrades these to
//! HTTP responses via `IntoResponse` (defined in `aero-server`).

use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("forbidden: {0}")]
    Forbidden(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("invalid input: {0}")]
    Invalid(String),

    #[error("rate limited")]
    RateLimited,

    #[error("upstream unavailable: {0}")]
    Upstream(String),

    #[error("database: {0}")]
    Database(sqlx::Error),

    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("internal: {0}")]
    Internal(#[from] anyhow::Error),
}

impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        let constraint = match &error {
            sqlx::Error::Database(database) => database.constraint(),
            _ => None,
        };
        match constraint {
            Some("snaplink_binding_required" | "snaplink_entitlement_required") => {
                Self::Upstream("commercial entitlement projection is unavailable".into())
            }
            Some("snaplink_feature_disabled") => {
                Self::Forbidden("feature is not enabled for this workspace".into())
            }
            Some("snaplink_quota_exceeded") => Self::RateLimited,
            _ => Self::Database(error),
        }
    }
}

impl Error {
    /// Returns the HTTP status code this error maps to.
    /// Used by `aero-server` to render responses.
    #[must_use]
    pub fn status_code(&self) -> u16 {
        match self {
            Self::NotFound(_) => 404,
            Self::Unauthorized(_) => 401,
            Self::Forbidden(_) => 403,
            Self::Conflict(_) => 409,
            Self::Invalid(_) => 400,
            Self::RateLimited => 429,
            Self::Upstream(_) => 502,
            Self::Database(_) | Self::Serde(_) | Self::Internal(_) => 500,
        }
    }

    /// Stable error code for clients to switch on.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "not_found",
            Self::Unauthorized(_) => "unauthorized",
            Self::Forbidden(_) => "forbidden",
            Self::Conflict(_) => "conflict",
            Self::Invalid(_) => "invalid",
            Self::RateLimited => "rate_limited",
            Self::Upstream(_) => "upstream",
            Self::Database(_) | Self::Serde(_) | Self::Internal(_) => "internal",
        }
    }
}
