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

use aero_auth::{password as auth_password, AuthUser};
use aero_common::{Error as AeroError, SessionId};
use aero_storage::revoked_token::{hash_token, RevokedTokenRepo};
use aero_storage::{
    generate_reset_token, hash_reset_token, ParticipantRepo, PasswordResetRepo, SessionRepo,
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
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
        .route("/api/auth/change-password", post(change_password))
        .route("/api/auth/change-email", post(change_email))
        .route("/api/auth/forgot-password", post(forgot_password))
        .route("/api/auth/reset-password", post(reset_password))
        .route("/api/me", axum::routing::delete(delete_me))
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

#[derive(Deserialize)]
struct ChangePasswordReq {
    current_password: String,
    new_password: String,
}

#[derive(Deserialize)]
struct ChangeEmailReq {
    current_password: String,
    new_email: String,
}

#[derive(Deserialize)]
struct ForgotPasswordReq {
    email: String,
}

#[derive(Deserialize)]
struct ResetPasswordReq {
    token: String,
    new_password: String,
}

#[derive(Deserialize)]
struct DeleteMeReq {
    password: String,
}

/// `POST /api/auth/change-password` — update the authenticated user's password.
///
/// 1. Verifies `current_password` against the stored hash.
/// 2. Validates `new_password` meets strength requirements (≥ 8 chars).
/// 3. Hashes and persists the new password.
/// 4. Revokes **all** active sessions so every device is forced to log in again.
/// 5. Blacklists every revoked refresh-token hash so refresh attempts `401`.
///
/// Returns `{ "sessions_invalidated": N }` on success. The caller's current
/// session is included in the count — they must log in again immediately.
async fn change_password(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<ChangePasswordReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let participants = ParticipantRepo::new(s.pg.clone());

    // Verify current password.
    let creds = participants
        .find_credentials_by_participant_id(auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("no credentials for this account".into()))?;
    auth_password::verify(&req.current_password, &creds.password_hash)
        .map_err(|_| AeroError::Unauthorized("current_password is incorrect".into()))?;

    // Validate the new password.
    if req.new_password.len() < 8 {
        return Err(AeroError::Invalid("new_password must be at least 8 characters".into()).into());
    }
    if req.new_password == req.current_password {
        return Err(AeroError::Invalid("new_password must differ from current_password".into()).into());
    }

    // Hash and store the new password.
    let new_hash = auth_password::hash(&req.new_password)?;
    participants
        .update_password_hash(auth.participant_id, &new_hash)
        .await
        .map_err(AeroError::from)?;

    // Revoke all sessions (including the current one) and blacklist their tokens.
    let revoked = repo(&s)
        .revoke_all_for_participant(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    let blacklist = RevokedTokenRepo::new(s.pg.clone());
    for hash in &revoked {
        blacklist
            .revoke(hash, Some(auth.participant_id))
            .await
            .map_err(AeroError::from)?;
    }

    Ok(Json(serde_json::json!({ "sessions_invalidated": revoked.len() })))
}

/// `POST /api/auth/change-email` — update the authenticated user's email address.
///
/// 1. Verifies `current_password` against the stored hash (guards against
///    hijacked sessions making silent email changes).
/// 2. Validates basic email shape (must contain `@` and a `.` after it).
/// 3. Persists the new address; a unique-constraint violation returns `409` so
///    the caller can detect "that address is already in use".
/// 4. Returns `{ "email": "new@example.com" }` on success. No session revocation
///    — an email change alone does not invalidate existing login sessions.
async fn change_email(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<ChangeEmailReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let participants = ParticipantRepo::new(s.pg.clone());

    // Verify the caller's current password before allowing a silent email swap.
    let creds = participants
        .find_credentials_by_participant_id(auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("no credentials for this account".into()))?;
    auth_password::verify(&req.current_password, &creds.password_hash)
        .map_err(|_| AeroError::Unauthorized("current_password is incorrect".into()))?;

    // Lightweight format check: must contain '@' with a '.' somewhere after it.
    let email = req.new_email.trim().to_ascii_lowercase();
    let valid = email
        .find('@')
        .and_then(|at| email[at + 1..].find('.'))
        .is_some();
    if !valid || email.len() < 5 {
        return Err(AeroError::Invalid("new_email is not a valid address".into()).into());
    }
    if email == creds.email {
        return Err(AeroError::Invalid("new_email must differ from the current address".into()).into());
    }

    participants
        .update_email(auth.participant_id, &email)
        .await
        .map_err(|e| {
            // UniqueViolation → 409 Conflict so the caller knows the address is taken.
            if let sqlx::Error::Database(ref db) = e {
                if db.code().as_deref() == Some("23505") {
                    return AeroError::Conflict("that email address is already in use".into());
                }
            }
            AeroError::from(e)
        })?;

    Ok(Json(serde_json::json!({ "email": email })))
}

/// `POST /api/auth/forgot-password` — issue a password-reset token for the
/// given email address.
///
/// Always returns the same success body regardless of whether the address is
/// registered — this prevents user-enumeration via error codes. When the address
/// IS registered, a plaintext reset token is **logged** at INFO level (a real
/// deployment would instead queue an email containing the token). The token
/// expires in one hour and is single-use.
async fn forgot_password(
    State(s): State<AppState>,
    Json(req): Json<ForgotPasswordReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let email = req.email.trim().to_ascii_lowercase();
    // Best-effort: look up the participant; silently succeed if not found.
    let participants = ParticipantRepo::new(s.pg.clone());
    if let Ok(Some(creds)) = participants.find_credentials_by_email(&email).await {
        let token = generate_reset_token();
        let hash = hash_reset_token(&token);
        let reset_repo = PasswordResetRepo::new(s.pg.clone());
        // Best-effort: if storage fails, we log but still return success to
        // avoid leaking whether the address is registered.
        if let Err(e) = reset_repo
            .create(creds.participant_id, &hash, aero_storage::RESET_TOKEN_TTL)
            .await
        {
            tracing::warn!("failed to store reset token for {email}: {e}");
        } else {
            // In production this would dispatch an email with the token.
            tracing::info!(
                email = %email,
                token = %token,
                "password reset token issued (log-only; wire an email sender for production)"
            );
        }
    }
    Ok(Json(serde_json::json!({
        "message": "If that address is registered, a reset link has been sent."
    })))
}

/// `POST /api/auth/reset-password` — complete a password reset using a token
/// issued by `POST /api/auth/forgot-password`.
///
/// Validates:
/// - The token exists, has not been used, and has not expired.
/// - The new password is at least 8 characters.
///
/// On success, updates the password hash and revokes all active sessions so
/// every device is forced to log in with the new password.
async fn reset_password(
    State(s): State<AppState>,
    Json(req): Json<ResetPasswordReq>,
) -> ApiResult<Json<serde_json::Value>> {
    if req.new_password.len() < 8 {
        return Err(AeroError::Invalid("new_password must be at least 8 characters".into()).into());
    }
    let hash = hash_reset_token(req.token.trim());
    let reset_repo = PasswordResetRepo::new(s.pg.clone());
    let participant = reset_repo
        .consume(&hash)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Unauthorized("reset token is invalid, expired, or already used".into()))?;

    let new_hash = auth_password::hash(&req.new_password)?;
    let participants = ParticipantRepo::new(s.pg.clone());
    participants
        .update_password_hash(participant, &new_hash)
        .await
        .map_err(AeroError::from)?;

    // Revoke all active sessions and blacklist their tokens.
    let revoked = repo(&s)
        .revoke_all_for_participant(participant)
        .await
        .map_err(AeroError::from)?;
    let blacklist = RevokedTokenRepo::new(s.pg.clone());
    for h in &revoked {
        blacklist
            .revoke(h, Some(participant))
            .await
            .map_err(AeroError::from)?;
    }

    Ok(Json(serde_json::json!({ "ok": true, "sessions_invalidated": revoked.len() })))
}

/// `DELETE /api/me` — permanently delete the caller's account.
///
/// Requires the caller's `password` in the request body as a second factor
/// of intent — a hijacked access token alone is not enough to destroy an account.
///
/// Steps:
/// 1. Verify password against the stored hash.
/// 2. Revoke + blacklist all active sessions so concurrent devices are signed out
///    before the row vanishes.
/// 3. Hard-delete the participant row; FK cascades handle credentials,
///    `room_members`, `auth_sessions`, etc.
/// 4. Returns `204 No Content` on success.
async fn delete_me(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<DeleteMeReq>,
) -> ApiResult<StatusCode> {
    let participants = ParticipantRepo::new(s.pg.clone());

    // Password confirmation guards against accidental or hijacked deletion.
    let creds = participants
        .find_credentials_by_participant_id(auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("no credentials for this account".into()))?;
    auth_password::verify(&req.password, &creds.password_hash)
        .map_err(|_| AeroError::Unauthorized("password is incorrect".into()))?;

    // Revoke and blacklist all active sessions so concurrent devices are kicked.
    let revoked = repo(&s)
        .revoke_all_for_participant(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    let blacklist = RevokedTokenRepo::new(s.pg.clone());
    for hash in &revoked {
        blacklist
            .revoke(hash, Some(auth.participant_id))
            .await
            .map_err(AeroError::from)?;
    }

    // Hard-delete; FK cascades remove credentials, room_members, etc.
    participants
        .delete_participant(auth.participant_id)
        .await
        .map_err(AeroError::from)?;

    Ok(StatusCode::NO_CONTENT)
}
