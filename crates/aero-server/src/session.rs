//! Session management — access-token refresh + logout / revocation.
//!
//! Exposes the [`AuthService`](aero_auth)'s existing refresh logic over HTTP and
//! layers explicit refresh-token revocation on top:
//!
//! * `POST /api/auth/refresh` — trade a (valid, non-revoked) refresh token for a
//!   fresh access token. NO auth extractor: the caller's *access* token may have
//!   expired (that's the whole point), so the refresh token in the body is the
//!   only credential. The token is checked against the revocation list
//!   ([`RevokedTokenRepo`](aero_storage::RevokedTokenRepo)) before anything is
//!   minted; a revoked or otherwise invalid token is `401`. The response mirrors
//!   `auth_login` (`{ access_token, refresh_token, participant }`).
//! * `POST /api/auth/logout` (auth required) — record the caller's refresh token
//!   as revoked so it can no longer be refreshed, then `204`. Best-effort and
//!   idempotent (`ON CONFLICT DO NOTHING`), so a repeated logout is harmless.
//!
//! Purely additive: thin handlers over the existing [`AppState::auth`] service and
//! a NEW revoked-token repo; no existing handler or repo is touched. Mounted via
//! [`routes`] and `.merge`d into the main router.

use aero_auth::AuthUser;
use aero_common::Error as AeroError;
use aero_storage::revoked_token::{hash_token, RevokedTokenRepo};
use aero_storage::SessionRepo;
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All session-management routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/auth/refresh", post(refresh))
        .route("/api/auth/logout", post(logout))
}

/// Request body for both refresh and logout — just the refresh token.
#[derive(Deserialize)]
struct RefreshReq {
    /// The refresh token previously issued by login/register/refresh.
    refresh_token: String,
}

/// `POST /api/auth/refresh` — mint a new access token from a refresh token.
///
/// Deliberately has no [`AuthUser`] extractor: the access token may be expired, so
/// the refresh token in the body is the only credential. The token is rejected
/// `401` if it has been revoked (logged out) or is otherwise invalid; otherwise a
/// fresh access token is issued and the response mirrors `auth_login`.
async fn refresh(
    State(s): State<AppState>,
    Json(req): Json<RefreshReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let token = req.refresh_token.trim();
    if token.is_empty() {
        return Err(AeroError::Invalid("refresh_token must not be empty".into()).into());
    }

    // Reject a revoked (logged-out) refresh token before minting anything.
    let revoked = RevokedTokenRepo::new(s.pg.clone());
    if revoked.is_revoked(&hash_token(token)).await.map_err(AeroError::from)? {
        return Err(AeroError::Unauthorized("refresh token revoked".into()).into());
    }

    // Reuse the AuthService's existing refresh logic. Any auth failure (bad
    // signature, expired, wrong token kind) is already `Error::Unauthorized`, so
    // `?` surfaces it as `401`.
    let tokens = s.auth.refresh(token).await?;

    // Wave 21: bump the active session's `last_seen_at` so the session inventory
    // shows recent activity. Best-effort — a touch failure must NOT fail refresh.
    if let Err(e) = SessionRepo::new(s.pg.clone()).touch(&hash_token(token)).await {
        tracing::warn!(error = ?e, "auth session touch failed");
    }

    // Resolve the owning participant so the response carries the same shape as
    // `auth_login`. Verifying the (already-validated) token again yields its
    // claims; both steps map any failure to `401`.
    let pid = s.auth.verify(token)?.participant_id()?;
    let participant = s
        .participants
        .get(pid)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Unauthorized("participant missing".into()))?;

    Ok(Json(serde_json::json!({
        "access_token": tokens.access_token,
        "refresh_token": tokens.refresh_token,
        "participant": participant,
    })))
}

/// `POST /api/auth/logout` — revoke the caller's refresh token so it can no longer
/// be refreshed, returning `204`. Best-effort and idempotent: re-logging-out the
/// same token is a no-op. Requires a valid access token (the [`AuthUser`]
/// extractor); the revocation is attributed to that caller.
async fn logout(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<RefreshReq>,
) -> ApiResult<StatusCode> {
    let token = req.refresh_token.trim();
    if token.is_empty() {
        return Err(AeroError::Invalid("refresh_token must not be empty".into()).into());
    }
    let hash = hash_token(token);
    RevokedTokenRepo::new(s.pg.clone())
        .revoke(&hash, Some(auth.participant_id))
        .await
        .map_err(AeroError::from)?;
    // Wave 21: also retire the matching active-session row (owner-scoped) so the
    // logged-out device drops out of the session inventory. Best-effort — the
    // refresh token is already revoked above, so a session-row miss is harmless.
    if let Err(e) = SessionRepo::new(s.pg.clone())
        .revoke_by_hash(&hash, auth.participant_id)
        .await
    {
        tracing::warn!(error = ?e, "auth session logout revoke failed");
    }
    Ok(StatusCode::NO_CONTENT)
}
