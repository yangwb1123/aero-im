//! Active session inventory + remote / global sign-out (HTTP layer).
//!
//! Surfaces a user's active login sessions/devices and lets them revoke one — or
//! all-others ("sign out everywhere else"). Built on the EXISTING refresh/logout
//! machinery: login records a session ([`SessionRepo::record`](aero_storage::SessionRepo)
//! keyed on the refresh-token hash), [`crate::session`] refresh/logout touch and
//! revoke it, and these handlers add explicit inventory + revocation on top.
//!
//! Revoking a session does two things in lock-step: it flips the row's
//! `revoked_at` AND adds the session's refresh-token hash to `revoked_tokens` (the
//! SAME [`RevokedTokenRepo`](aero_storage::RevokedTokenRepo) the refresh path
//! checks), so a subsequent `POST /api/auth/refresh` with that token `401`s. The
//! full token hash is never exposed — list responses carry only a short prefix.
//!
//! Every route is owner-scoped at the SQL layer (a non-owner's id resolves to
//! `None`/empty ⇒ `404`/no-op), so a caller can only ever see or revoke their own
//! sessions. Purely additive: thin handlers over a NEW [`SessionRepo`] and the
//! existing revoked-token repo; no existing handler or repo is touched. Mounted
//! via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, SessionId};
use aero_storage::revoked_token::{hash_token, RevokedTokenRepo};
use aero_storage::SessionRepo;
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All active-session routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/auth/sessions", get(list_sessions))
        .route("/api/auth/sessions/:sid", axum::routing::delete(revoke_session))
        .route("/api/auth/sessions/revoke-others", post(revoke_others))
}

/// Build a [`SessionRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> SessionRepo {
    SessionRepo::new(s.pg.clone())
}

fn parse_session(s: &str) -> Result<SessionId, AeroError> {
    SessionId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("session id: {e}")))
}

/// `GET /api/auth/sessions` — the caller's currently-active sessions/devices, most
/// recently seen first. Owner-scoped; the full refresh-token hash is never
/// exposed (only a short prefix).
async fn list_sessions(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let sessions = repo(&s)
        .list_active(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(sessions).map_err(AeroError::from)?))
}

/// `DELETE /api/auth/sessions/:sid` — revoke one of the caller's own sessions.
///
/// Owner-scoped: a `404` if it isn't the caller's active session (someone else's,
/// unknown, or already revoked). On success the session's refresh-token hash is
/// ALSO added to `revoked_tokens`, so a refresh with that token then `401`s.
async fn revoke_session(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_session(&id_str)?;
    let hash = repo(&s)
        .revoke(id, auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("session {id}")))?;
    // Belt-and-braces: also blacklist the refresh token so it can't be refreshed,
    // attributing the revocation to the caller. The hash already came from the
    // (just-revoked) owner-scoped row.
    RevokedTokenRepo::new(s.pg.clone())
        .revoke(&hash, Some(auth.participant_id))
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "revoked": true, "session_id": id })))
}

/// Request body for "sign out everywhere else" — the caller's current refresh
/// token, whose session is the one kept active.
#[derive(Deserialize)]
struct RevokeOthersReq {
    /// The refresh token of the session to KEEP (typically this device's).
    current_refresh_token: String,
}

/// `POST /api/auth/sessions/revoke-others` — revoke every active session EXCEPT
/// the one whose refresh token is supplied ("sign out everywhere else").
///
/// Each revoked session's refresh-token hash is added to `revoked_tokens`, so a
/// refresh with any of them then `401`s. Owner-scoped; the kept session is matched
/// by `hash_token(current_refresh_token)` so the same hashing as login/refresh is
/// used. Returns the count of sessions signed out.
async fn revoke_others(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<RevokeOthersReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let token = req.current_refresh_token.trim();
    if token.is_empty() {
        return Err(AeroError::Invalid("current_refresh_token must not be empty".into()).into());
    }
    let keep = hash_token(token);
    let revoked = repo(&s)
        .revoke_others(auth.participant_id, &keep)
        .await
        .map_err(AeroError::from)?;
    // Blacklist each revoked refresh token so none can be refreshed. Attribute to
    // the caller; `revoke` is idempotent (ON CONFLICT DO NOTHING).
    let blacklist = RevokedTokenRepo::new(s.pg.clone());
    for hash in &revoked {
        blacklist
            .revoke(hash, Some(auth.participant_id))
            .await
            .map_err(AeroError::from)?;
    }
    Ok(Json(serde_json::json!({ "revoked_count": revoked.len() })))
}
