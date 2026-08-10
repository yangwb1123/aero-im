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
        let mut response = (status, body).into_response();
        // Fixed-window limiters (per-client middleware AND the per-workspace
        // ws-rate gate) surface 429s through this conversion. Give clients a
        // Retry-After hint: whole seconds until the current minute window
        // rolls over (ceil, at least 1). The per-client middleware overrides
        // this with its token-bucket-exact value afterwards (HeaderMap::insert
        // replaces), so the more precise hint always wins where available.
        if status == StatusCode::TOO_MANY_REQUESTS {
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let retry_after = 60 - (now_secs % 60);
            response.headers_mut().insert(
                "retry-after",
                axum::http::HeaderValue::from_str(&retry_after.to_string())
                    .unwrap_or_else(|_| axum::http::HeaderValue::from_static("60")),
            );
        }
        response
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use serde_json::json;

    /// The 409 wire contract the recall-window web discriminator depends on:
    /// `Error::Conflict` renders with the thiserror Display prefix
    /// (`"conflict: {detail}"`) — the exact string clients match against is
    /// pinned here so a reworded variant can never silently break the client.
    #[tokio::test]
    async fn conflict_renders_thiserror_prefix_and_409() {
        let response =
            ApiError(Error::Conflict("recall window expired".into())).into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            body,
            json!({
                "code": "conflict",
                "msg": "conflict: recall window expired",
            })
        );
    }
}
