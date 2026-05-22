//! Converts library errors into HTTP responses.

use aero_common::Error;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

#[derive(Debug)]
pub struct ApiError(pub Error);

impl<E: Into<Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = Json(json!({
            "code": self.0.code(),
            "msg": self.0.to_string(),
        }));
        if status.is_server_error() {
            tracing::error!(error = %self.0, "server error");
        } else {
            tracing::debug!(error = %self.0, "client error");
        }
        (status, body).into_response()
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;
