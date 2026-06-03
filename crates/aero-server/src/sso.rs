//! SSO via OIDC — ID-token login + JIT provisioning (HTTP layer).
//!
//! `POST /api/auth/oidc` accepts an external `IdP`'s `OpenID` Connect ID token,
//! validates it ([`aero_auth::oidc::validate_id_token`]), resolves the
//! `(issuer, subject)` to an internal participant — JIT-provisioning one on first
//! sight — and returns *our own* access/refresh token pair, shaped exactly like
//! the `register`/`login` responses in [`crate::routes`].
//!
//! ## Configuration & wiring
//!
//! OIDC is **off by default**. The handler reads [`OidcConfig::from_env`] per
//! request (issuer / audience / JWKS URI); when unset it returns
//! `501 Not Implemented`-style `Invalid` ("oidc not configured"). The signing
//! keys come from a live JWKS fetch ([`JwksKeyProvider`]) — the documented
//! network seam. Constructing config + key provider inside the handler keeps the
//! feature self-contained: no `AppState` field and no binary change are required.
//!
//! Purely additive — registered via `pub mod sso;` in `crate::lib` and
//! `.merge(crate::sso::routes())` in [`crate::routes::build`].

use aero_auth::{JwksKeyProvider, OidcConfig};
use aero_common::{Error as AeroError, ParticipantId, WorkspaceId, WorkspaceRole};
use aero_storage::SsoRepo;
use axum::{routing::post, Json, Router};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// The legacy / default workspace every login enrolls into — the all-zero UUID,
/// matching `crate::routes::DEFAULT_WORKSPACE_ID` (kept in sync; that const is
/// private to `routes`, so SSO declares its own copy of the same well-known id
/// established by migration `0006_workspaces.sql`).
const DEFAULT_WORKSPACE_ID: WorkspaceId = WorkspaceId(ulid::Ulid(0));

/// Mount the SSO routes. Folded into the main router by [`crate::routes::build`].
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/auth/oidc", post(oidc_login))
}

#[derive(Deserialize)]
struct OidcLoginReq {
    /// The raw OIDC ID token (a signed JWT) issued by the external `IdP`.
    id_token: String,
}

/// `POST /api/auth/oidc` — exchange a validated external OIDC ID token for our
/// own session tokens, JIT-provisioning a participant on first login.
///
/// Returns the same `{access_token, refresh_token, participant}` envelope as
/// `register`/`login`. Errors:
/// * `400` if OIDC is not configured, or the token is malformed/invalid.
/// * `401`/`400` collapse all token-validation failures (we never leak which
///   check failed to the client).
async fn oidc_login(
    axum::extract::State(s): axum::extract::State<AppState>,
    Json(req): Json<OidcLoginReq>,
) -> ApiResult<Json<serde_json::Value>> {
    // 1. Load config from env; OIDC is off by default.
    let cfg = OidcConfig::from_env()
        .ok_or_else(|| AeroError::Invalid("oidc not configured".into()))?;

    // 2. Validate the external ID token against the IdP's live JWKS. Any failure
    //    (bad signature / wrong iss/aud / expired / unknown key) maps to 401 so
    //    the client cannot distinguish *why* the token was rejected.
    let keys = JwksKeyProvider::from_config(&cfg);
    let claims = aero_auth::validate_id_token(&req.id_token, &cfg, &keys)
        .await
        .map_err(|e| AeroError::Unauthorized(format!("oidc: {e}")))?;

    // 3. Resolve the external identity to an internal participant, or provision.
    //    The shared pool is reached via the participant repo (no dedicated
    //    `AppState` pool field exists), so SSO adds no `AppState` surface.
    let sso = SsoRepo::new(s.participants.pool().clone());
    let participant_id = match sso
        .find_participant(&cfg.issuer, &claims.sub)
        .await
        .map_err(AeroError::from)?
    {
        Some(pid) => pid,
        None => jit_provision(&s, &sso, &cfg.issuer, &claims).await?,
    };

    // 4. Mint OUR tokens for the resolved participant + return the standard
    //    envelope (resolve the participant row so the client gets full profile).
    let tokens = s.auth.issue_for_participant(participant_id)?;
    let participant = s
        .participants
        .get(participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("participant".into()))?;
    Ok(Json(serde_json::json!({
        "access_token": tokens.access_token,
        "refresh_token": tokens.refresh_token,
        "participant": participant,
    })))
}

/// JIT-provision a participant for a first-seen external identity: create a human
/// participant, enroll it into the default workspace (so it can immediately
/// create rooms — same invariant `register` upholds), and link the identity.
///
/// Enrollment and linking mirror the `auth_register` flow; failures propagate so
/// the "logged-in ⇒ workspace member ⇒ identity linked" invariant holds.
async fn jit_provision(
    s: &AppState,
    sso: &SsoRepo,
    issuer: &str,
    claims: &aero_auth::OidcClaims,
) -> Result<ParticipantId, AeroError> {
    let display_name = claims.best_display_name();
    // Bot-style insert path: a SSO participant has no local password/credential
    // row, so we use `create_bot` with kind=Human (no credentials), matching how
    // `create_human` shapes a participant minus the credentials side-table.
    let participant = s
        .participants
        .create_bot(
            &display_name,
            aero_common::ParticipantKind::Human,
            None,
            None,
        )
        .await
        .map_err(AeroError::from)?;
    // Enroll into the default workspace (idempotent ON CONFLICT DO NOTHING upsert)
    // so the new participant immediately belongs to a tenant.
    s.workspaces
        .add_member(DEFAULT_WORKSPACE_ID, participant.id, WorkspaceRole::Member)
        .await
        .map_err(AeroError::from)?;
    // Link the external identity so subsequent logins resolve directly.
    sso.link(issuer, &claims.sub, participant.id, claims.email.as_deref())
        .await
        .map_err(AeroError::from)?;
    Ok(participant.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_workspace_is_the_all_zero_uuid() {
        // Must equal the well-known default workspace established by migration
        // 0006 (the same const `routes` uses for `register` enrollment).
        assert_eq!(DEFAULT_WORKSPACE_ID.to_uuid(), uuid::Uuid::nil());
    }
}
