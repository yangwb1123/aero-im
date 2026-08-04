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

use aero_common::{ParticipantId, SessionId};

use crate::jwt::{Claims, TokenKind};
use crate::service::AuthService;

/// The authenticated participant. Inserted into the request as a typed extractor
/// — handlers ask for `user: AuthUser` and Axum will reject the request with 401
/// before the handler body runs if no valid bearer token is present.
#[derive(Debug, Clone, Copy)]
pub struct AuthUser {
    pub participant_id: ParticipantId,
    /// Stable browser/device session for JWT authentication. Authenticated JWTs
    /// always carry one; `None` is retained for opaque PAT/bot credentials.
    pub session_id: Option<SessionId>,
    /// JWT expiry (unix seconds). Opaque PAT/bot credentials have no JWT expiry.
    pub exp: Option<u64>,
}

/// Build extractor output from claims that have already passed signature and
/// active-session validation. Kept pure so malformed/wrong-kind claim handling
/// is unit-testable without a database.
fn auth_user_from_access_claims(claims: &Claims) -> aero_common::Result<AuthUser> {
    if claims.kind != TokenKind::Access {
        return Err(aero_common::Error::Unauthorized(
            "not an access token".into(),
        ));
    }
    Ok(AuthUser {
        participant_id: claims.participant_id()?,
        session_id: claims.session_id()?,
        exp: Some(claims.exp),
    })
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
        // A trusted edge middleware may already have performed signature,
        // token-kind, and active-session validation while applying tenant
        // network policy. Reuse that typed result so the session store is not
        // queried twice. HTTP clients cannot manufacture request extensions.
        if let Some(user) = parts.extensions.get::<AuthUser>().copied() {
            return Ok(user);
        }
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

        // A syntactically/signature-valid JWT owns this authentication attempt:
        // wrong-kind, malformed claims, and especially a revoked session are hard
        // rejects and MUST NOT fall through to PAT/bot verification. Only a value
        // that is not a valid JWT at all may be considered an opaque credential.
        let user = match svc.verify(token) {
            Ok(claims) => {
                svc.assert_access_claims_active(&claims)
                    .await
                    .map_err(|_| AuthRejection::new("invalid, expired, or revoked token"))?;
                auth_user_from_access_claims(&claims)
                    .map_err(|_| AuthRejection::new("invalid access-token claims"))?
            }
            Err(_) => {
                // PAT and bot tokens have disjoint, self-gating prefixes, so at
                // most one storage lookup runs for a given opaque bearer.
                let pid = match svc.verify_pat(token).await {
                    Some(owner) => owner,
                    None => svc
                        .verify_bot_token(token)
                        .await
                        .ok_or_else(|| AuthRejection::new("invalid or expired token"))?,
                };
                AuthUser {
                    participant_id: pid,
                    session_id: None,
                    exp: None,
                }
            }
        };

        // Stash the participant id in request extensions so downstream layers
        // (logging, authorization checks) can read it without re-decoding.
        parts.extensions.insert(user.participant_id);

        Ok(user)
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

    #[test]
    fn jwt_helper_exposes_session_and_expiry() {
        let participant = ParticipantId::new();
        let session = SessionId::new();
        let claims = Claims {
            sub: participant.to_string(),
            iss: "aero-im".into(),
            iat: 10,
            exp: 20,
            kind: TokenKind::Access,
            sid: Some(session.to_string()),
            jti: "jti".into(),
        };

        let user = auth_user_from_access_claims(&claims).unwrap();
        assert_eq!(user.participant_id, participant);
        assert_eq!(user.session_id, Some(session));
        assert_eq!(user.exp, Some(20));
    }

    #[test]
    fn jwt_helper_rejects_wrong_kind_or_malformed_sid() {
        let mut claims = Claims {
            sub: ParticipantId::new().to_string(),
            iss: "aero-im".into(),
            iat: 10,
            exp: 20,
            kind: TokenKind::Refresh,
            sid: None,
            jti: "jti".into(),
        };
        assert!(auth_user_from_access_claims(&claims).is_err());

        claims.kind = TokenKind::Access;
        claims.sid = Some("not-a-session-id".into());
        assert!(auth_user_from_access_claims(&claims).is_err());
    }
}
