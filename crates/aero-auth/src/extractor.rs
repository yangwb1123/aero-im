//! Axum extractor that resolves the authenticated participant from the
//! `Authorization: Bearer <jwt>` request header.
//!
//! Wiring:
//! ```ignore
//! let app = Router::new()
//!     .route("/api/me", get(me))
//!     .with_state(auth_service.clone());
//!
//! async fn me(user: AuthUser) -> Json<...> { /* user.participant_id */ }
//! ```
//!
//! The extractor expects [`AuthService`] in router state (or any `S` that
//! references one via `FromRef`). On failure it returns `401` with a JSON body
//! `{ "code": "unauthorized", "msg": "..." }` so clients can switch on `code`.

use async_trait::async_trait;
use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use aero_common::ParticipantId;

use crate::service::AuthService;

/// The authenticated participant. Inserted into the request as a typed extractor
/// — handlers ask for `user: AuthUser` and Axum will reject the request with 401
/// before the handler body runs if no valid bearer token is present.
#[derive(Debug, Clone, Copy)]
pub struct AuthUser {
    pub participant_id: ParticipantId,
}

/// Rejection produced when the bearer token is missing, malformed, or invalid.
/// Serializes to `401 { "code": "unauthorized", "msg": "..." }`.
#[derive(Debug)]
pub struct AuthRejection {
    msg: &'static str,
}

impl AuthRejection {
    fn new(msg: &'static str) -> Self {
        Self { msg }
    }
}

impl IntoResponse for AuthRejection {
    fn into_response(self) -> Response {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "code": "unauthorized", "msg": self.msg })),
        )
            .into_response()
    }
}

#[async_trait]
impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
    AuthService: FromRef<S>,
{
    type Rejection = AuthRejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let svc = AuthService::from_ref(state);
        let header_val = parts
            .headers
            .get(header::AUTHORIZATION)
            .ok_or_else(|| AuthRejection::new("missing Authorization header"))?
            .to_str()
            .map_err(|_| AuthRejection::new("Authorization header is not valid ASCII"))?;
        let token = header_val
            .strip_prefix("Bearer ")
            .or_else(|| header_val.strip_prefix("bearer "))
            .ok_or_else(|| AuthRejection::new("expected Bearer token"))?
            .trim();
        if token.is_empty() {
            return Err(AuthRejection::new("empty bearer token"));
        }

        let claims = svc
            .verify(token)
            .map_err(|_| AuthRejection::new("invalid or expired token"))?;
        if claims.kind != crate::jwt::TokenKind::Access {
            return Err(AuthRejection::new("not an access token"));
        }
        let pid = claims
            .participant_id()
            .map_err(|_| AuthRejection::new("invalid sub claim"))?;

        // Stash the participant id in request extensions so downstream layers
        // (logging, authorization checks) can read it without re-decoding.
        parts.extensions.insert(pid);

        Ok(AuthUser { participant_id: pid })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The extractor itself needs a live DB to fully exercise via `AuthService`,
    // so we just sanity-check the rejection serialization here. End-to-end
    // testing belongs in `aero-server`.
    #[test]
    fn rejection_renders_as_401_json() {
        let resp = AuthRejection::new("nope").into_response();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
