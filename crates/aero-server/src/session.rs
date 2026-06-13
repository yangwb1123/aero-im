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

use aero_auth::{AuthUser, TokenKind};
use aero_common::Error as AeroError;
use aero_storage::revoked_token::{hash_token, RevokedTokenRepo};
use aero_storage::SessionRepo;
use axum::{extract::State, http::{HeaderMap, StatusCode}, routing::post, Json, Router};
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

/// Grace window after a refresh token is rotated during which re-presenting it is
/// treated as a benign lost-response retry (plain `401`) rather than a token-theft
/// replay (which revokes the whole session family). Wide enough to cover a client
/// retrying a refresh whose response was dropped, short enough that a stolen token
/// replayed later is still caught.
const REFRESH_REUSE_GRACE: time::Duration = time::Duration::seconds(10);

/// `POST /api/auth/refresh` — rotate the refresh token and mint a new access
/// token.
///
/// Deliberately has no [`AuthUser`] extractor: the access token may be expired, so
/// the refresh token in the body is the only credential. The old refresh token is
/// validated (signature, kind, revocation list), then immediately blacklisted so it
/// can never be reused — a stolen token detected via re-use is rejected `401` on
/// any subsequent attempt. The response mirrors `auth_login`.
async fn refresh(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RefreshReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let token = req.refresh_token.trim();
    if token.is_empty() {
        return Err(AeroError::Invalid("refresh_token must not be empty".into()).into());
    }

    // Verify signature and kind FIRST, so the participant can be identified even
    // for a revoked token (a forged token fails here). Any failure → 401.
    let claims = s.auth.verify(token)?;
    if claims.kind != TokenKind::Refresh {
        return Err(AeroError::Unauthorized("not a refresh token".into()).into());
    }
    let pid = claims.participant_id()?;

    let old_hash = hash_token(token);
    let revoked_repo = RevokedTokenRepo::new(s.pg.clone());
    let session_repo = SessionRepo::new(s.pg.clone());

    // A revoked (logged-out or already-rotated) refresh token is rejected. If it was
    // rotated only moments ago this is almost certainly a benign retry of a refresh
    // whose response was lost — just 401. But replaying a token rotated longer ago
    // (past the grace window) is a token-THEFT signal: the legitimate client holds
    // the newer token, so revoke EVERY session for the participant (attacker AND
    // victim must re-authenticate) — OWASP refresh-token-rotation reuse detection.
    if let Some(revoked_at) = revoked_repo.revoked_at(&old_hash).await.map_err(AeroError::from)? {
        if time::OffsetDateTime::now_utc() - revoked_at > REFRESH_REUSE_GRACE {
            if let Ok(hashes) = session_repo.revoke_all_for_participant(pid).await {
                for h in hashes {
                    let _ = revoked_repo.revoke(&h, Some(pid)).await;
                }
            }
            tracing::warn!(%pid, "refresh-token reuse detected; revoked all sessions");
            return Err(AeroError::Unauthorized(
                "refresh token reuse detected; all sessions revoked".into(),
            )
            .into());
        }
        return Err(AeroError::Unauthorized("refresh token revoked".into()).into());
    }

    // Issue a fresh access + refresh token pair (token rotation).
    let new_tokens = s.auth.issue_for_participant(pid)?;
    let new_hash = hash_token(&new_tokens.refresh_token);

    // Blacklist the old token immediately — hard failure: if we can't blacklist
    // it the caller might try to reuse it, so we must not issue the new pair.
    revoked_repo.revoke(&old_hash, Some(pid)).await.map_err(AeroError::from)?;

    // Retire the old session row and register the new one (best-effort — the
    // old token is already blacklisted above, so row misses are harmless).
    if let Err(e) = session_repo.revoke_by_hash(&old_hash, pid).await {
        tracing::warn!(error = ?e, "failed to retire old session on token rotate");
    }
    let ua = headers.get(axum::http::header::USER_AGENT).and_then(|v| v.to_str().ok());
    if let Err(e) = session_repo.record(pid, &new_hash, ua).await {
        tracing::warn!(error = ?e, "failed to record new session on token rotate");
    }

    let participant = s
        .participants
        .get(pid)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Unauthorized("participant missing".into()))?;

    Ok(Json(serde_json::json!({
        "access_token": new_tokens.access_token,
        "refresh_token": new_tokens.refresh_token,
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
    // Best-effort privileged-operation audit (ROADMAP 方向四). Logout carries no
    // workspace context, so the event is attributed to the all-zero default
    // workspace (uuid nil). The full token hash is never recorded — only a short
    // prefix. A logging failure only warns, never fails the already-done logout.
    audit_session_revoked(&s, auth.participant_id, &hash).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Record a `session.revoked` audit event for a logout. Attributed to the
/// all-zero default workspace (uuid nil) since logout has no tenant context, and
/// carries only a short prefix of the refresh-token hash (never the full hash).
/// Best-effort: an append failure is warn-logged and swallowed.
async fn audit_session_revoked(
    s: &AppState,
    actor: aero_common::ParticipantId,
    token_hash: &str,
) {
    let workspace = aero_common::WorkspaceId::from_uuid(uuid::Uuid::nil());
    // A short, non-reversible prefix is enough to correlate without exposing the
    // hash itself.
    let prefix: String = token_hash.chars().take(12).collect();
    if let Err(e) = s
        .audit
        .append(
            workspace,
            Some(actor),
            "session.revoked",
            None,
            serde_json::json!({ "token_hash_prefix": prefix }),
        )
        .await
    {
        tracing::warn!(error = ?e, "session.revoked audit append failed");
    }
}
