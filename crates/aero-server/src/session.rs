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
//! * `POST /api/auth/logout` — authenticate with the supplied refresh token,
//!   revoke it and retire its session, then `204`. This still works after the
//!   access token expires; the mutation is atomic and idempotent.
//!
//! Purely additive: thin handlers over the existing [`AppState::auth`] service and
//! a NEW revoked-token repo; no existing handler or repo is touched. Mounted via
//! [`routes`] and `.merge`d into the main router.

use aero_auth::TokenKind;
use aero_common::Error as AeroError;
use aero_storage::revoked_token::{hash_token, RevokedTokenRepo};
use aero_storage::SessionRepo;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
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

/// Reject a refresh hash already present in the revocation ledger. An older
/// replay is treated as credential theft and invalidates every session.
async fn reject_revoked_refresh(
    s: &AppState,
    sessions: &SessionRepo,
    participant: aero_common::ParticipantId,
    token_hash: &str,
) -> aero_common::Result<()> {
    let Some(revoked_at) = RevokedTokenRepo::new(s.pg.clone())
        .revoked_at(token_hash)
        .await
        .map_err(AeroError::from)?
    else {
        return Ok(());
    };

    if time::OffsetDateTime::now_utc() - revoked_at > REFRESH_REUSE_GRACE {
        let revoked = sessions
            .revoke_all_and_blacklist(participant)
            .await
            .map_err(AeroError::from)?;
        s.hub.disconnect_participant(participant);
        crate::session_control::publish_revoke_participant(s, participant).await;
        tracing::warn!(%participant, revoked, "refresh-token reuse detected; revoked all sessions");
        return Err(AeroError::Unauthorized(
            "refresh token reuse detected; all sessions revoked".into(),
        ));
    }
    Err(AeroError::Unauthorized("refresh token revoked".into()))
}

pub(crate) fn refresh_participant(
    claims: &aero_auth::Claims,
) -> aero_common::Result<aero_common::ParticipantId> {
    if claims.kind != TokenKind::Refresh {
        return Err(AeroError::Unauthorized("not a refresh token".into()));
    }
    claims.participant_id()
}

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
    let pid = refresh_participant(&claims)?;

    let old_hash = hash_token(token);
    let session_repo = SessionRepo::new(s.pg.clone());
    // Check before resolving legacy sid-less tokens: once such a token has
    // rotated, its old hash is no longer active and cannot recover the stable id,
    // but replay detection must still invalidate the session family.
    reject_revoked_refresh(&s, &session_repo, pid, &old_hash).await?;
    let session_id = match claims.session_id()? {
        Some(session_id) => session_id,
        None => session_repo
            .active_id_by_hash(pid, &old_hash)
            .await
            .map_err(AeroError::from)?
            .ok_or_else(|| AeroError::Unauthorized("refresh session is not active".into()))?,
    };

    // Resolve the participant before consuming the old token. A deleted/missing
    // account must never burn a credential and then fail later.
    let participant = s
        .participants
        .get(pid)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Unauthorized("participant missing".into()))?;

    // Tokens are minted in memory first but are returned only if the atomic DB
    // rotation below wins. A racing loser discards its pair.
    let new_tokens = s.auth.issue_for_session(pid, session_id)?;
    let new_hash = hash_token(&new_tokens.refresh_token);
    let ua = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok());
    let rotation = session_repo
        .rotate_refresh_token_in_place(pid, session_id, &old_hash, &new_hash, ua)
        .await
        .map_err(AeroError::from)?;
    if rotation == aero_storage::RefreshRotation::UnknownSession {
        return Err(AeroError::Unauthorized("refresh session is not active".into()).into());
    }
    if rotation == aero_storage::RefreshRotation::AlreadyRevoked {
        // A concurrent winner may blacklist after the pre-check above.
        reject_revoked_refresh(&s, &session_repo, pid, &old_hash).await?;
        return Err(AeroError::Internal(anyhow::anyhow!(
            "refresh rotation reported revocation without a revocation row"
        ))
        .into());
    }

    Ok(Json(serde_json::json!({
        "access_token": new_tokens.access_token,
        "refresh_token": new_tokens.refresh_token,
        "participant": participant,
    })))
}

/// `POST /api/auth/logout` — revoke the caller's refresh token so it can no longer
/// be refreshed, returning `204`. The body token must be a signed refresh token
/// whose owner is derived from its signed claims; garbage and access tokens are
/// rejected. Re-logging-out the same valid token is idempotent and no access JWT
/// is required, so logout remains possible after access expiry.
async fn logout(State(s): State<AppState>, Json(req): Json<RefreshReq>) -> ApiResult<StatusCode> {
    let token = req.refresh_token.trim();
    if token.is_empty() {
        return Err(AeroError::Invalid("refresh_token must not be empty".into()).into());
    }
    let claims = s.auth.verify(token)?;
    let participant = refresh_participant(&claims)?;
    let hash = hash_token(token);
    let sessions = SessionRepo::new(s.pg.clone());
    let claimed_session = claims.session_id()?;
    let active_session = sessions
        .active_id_by_hash(participant, &hash)
        .await
        .map_err(AeroError::from)?;
    if matches!(
        (claimed_session, active_session),
        (Some(claimed), Some(active)) if claimed != active
    ) {
        return Err(AeroError::Unauthorized("refresh session binding mismatch".into()).into());
    }
    let session_id = claimed_session.or(active_session);
    // B5-1: blacklist + revoke + governance pair (session.revoked /
    // admin.auth.session.revoke) in ONE transaction. D-N2: the audit detail
    // SHAPE CHANGES from `{"token_hash_prefix":…}` to `{"session_id":…}`
    // (trajectory token unchanged — D6; no in-repo consumers of
    // `token_hash_prefix` exist). The pair is emitted only when a row actually
    // flipped and the session id is resolvable (no-op logout audits nothing);
    // SAVEPOINT fail-open (R7) — an audit failure never fails the logout.
    let mut tx = s.pg.begin().await.map_err(AeroError::from)?;
    let revoked = SessionRepo::revoke_by_hash_and_blacklist_in_tx(&mut tx, &hash, participant)
        .await
        .map_err(AeroError::from)?;
    if revoked {
        if let Some(sid) = session_id {
            let _ = aero_storage::audit_governance::AuditGovernanceOutboxRepo::append_pair_in_tx_fail_open(
                &mut tx,
                aero_common::WorkspaceId::from_uuid(uuid::Uuid::nil()),
                Some(participant),
                aero_storage::audit_governance::tokens::SESSION_REVOKED,
                Some(&sid.to_string()),
                serde_json::json!({ "session_id": sid.to_string() }),
                aero_storage::audit_governance::tokens::OUTBOUND_AUTH_SESSION_REVOKE,
            )
            .await
            .map_err(AeroError::from)?; // Ok(None) = fail-open skip (DLQ row in-tx)
        }
    }
    tx.commit().await.map_err(AeroError::from)?;
    if let Some(session_id) = session_id {
        s.hub.disconnect_session(participant, session_id);
        crate::session_control::publish_revoke_session(&s, participant, session_id).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_auth::Claims;

    #[test]
    fn refresh_credentials_reject_access_tokens() {
        let participant = aero_common::ParticipantId::new();
        let mut claims = Claims {
            sub: participant.to_string(),
            iss: "aero-im".into(),
            iat: 1,
            exp: u64::MAX,
            kind: TokenKind::Refresh,
            jti: "test".into(),
            sid: None,
        };
        assert_eq!(refresh_participant(&claims).unwrap(), participant);
        claims.kind = TokenKind::Access;
        assert!(refresh_participant(&claims).is_err());
    }
}
