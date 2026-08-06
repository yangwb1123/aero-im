//! Workspace invitations / invite-link HTTP API (ROADMAP 方向一 — multi-tenant).
//!
//! Additive layer over [`aero_storage::InvitationRepo`] (migration 0017): a
//! workspace admin/owner mints an invitation (email or open shareable link), an
//! admin lists / revokes them, and any logged-in user redeems one by its token to
//! join the workspace with the invite's role.
//!
//! ## Authorization mirrors `crate::workspaces`
//!
//! The admin gate is the same `caller_role` + `can_administer` pattern the member
//! management routes use, surfaced as a pure DB-free guard ([`authorize_manage_invites`])
//! so it unit-tests over every role offline (Postgres is absent in CI). Minting
//! an invite additionally requires the caller be able to *grant* the requested
//! role without escalation, reusing the storage predicates (`role_can_invite`,
//! `role_can_assign`) via the same [`authorize_invite`](crate::workspaces::authorize_invite)
//! decision the direct-add route uses.
//!
//! ## Token handling
//!
//! The plaintext token is generated once at creation and returned to the caller
//! (shown once, embedded in a ready-to-share `invite_url`); only its SHA-256 hash
//! is stored. Redemption resolves `sha256(token)` → an *active* invite and
//! re-validates redeemability ([`aero_storage::invitation_is_redeemable`]).
//!
//! ## Email handling (this iteration)
//!
//! An invite's `email`, when present, is treated as **informational**: accept does
//! not hard-block on a caller whose address differs (we don't resolve a
//! participant's verified email here, and open links have no email at all). The
//! address is recorded on the audit event so a mismatch is observable; tightening
//! this into a hard check is a later, separate change.

use std::str::FromStr;

use aero_common::{
    Error as AeroError, InvitationId, Result as AeroResult, WorkspaceId, WorkspaceRole,
};
use aero_storage::invitation::{generate_token, hash_token};
use aero_storage::{InvitationAcceptError, InvitationRepo};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::post,
    Json, Router,
};
use serde::Deserialize;

use aero_auth::AuthUser;

use crate::error::ApiResult;
use crate::state::AppState;
use crate::workspaces::authorize_invite;

// ---------- Router ----------

/// Mount the invitation routes. Folded into the main router by
/// [`crate::routes::build`]; kept separate so the route table and its handlers
/// live next to the authorization logic they enforce.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/workspaces/:id/invitations",
            post(create_invitation).get(list_invitations),
        )
        .route(
            "/api/invitations/:id",
            axum::routing::delete(revoke_invitation),
        )
        .route("/api/invitations/accept", post(accept_invitation))
}

/// Construct the invitation repo from the shared pool. `AppState` exposes the
/// pool via `participants.pool()` (repos are cheap `Arc<PgPool>` wrappers), so no
/// new `AppState` field is needed.
fn repo(s: &AppState) -> InvitationRepo {
    InvitationRepo::new(s.participants.pool().clone())
}

// ---------- Pure authorization (DB-free, unit-tested) ----------

/// May `caller` manage (list / revoke) a workspace's invitations? Restricted to
/// admins/owners — invitations are workspace administration, the same bar as
/// member management and the audit trail.
///
/// # Errors
/// [`AeroError::Forbidden`] for non-administrators (members / guests).
pub fn authorize_manage_invites(caller: WorkspaceRole) -> AeroResult<()> {
    if caller.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden(
            "managing invitations requires admin".into(),
        ))
    }
}

// ---------- Shared helpers ----------

fn parse_workspace_id(s: &str) -> AeroResult<WorkspaceId> {
    WorkspaceId::from_str(s).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn parse_invitation_id(s: &str) -> AeroResult<InvitationId> {
    InvitationId::from_str(s).map_err(|e| AeroError::Invalid(format!("invitation id: {e}")))
}

/// Resolve the caller's role in a workspace, rejecting non-members with `403`.
/// Mirrors the helper in [`crate::workspaces`] so the membership check is uniform.
async fn caller_role(
    s: &AppState,
    ws: WorkspaceId,
    caller: aero_common::ParticipantId,
) -> AeroResult<WorkspaceRole> {
    s.workspaces
        .effective_member_role(ws, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))
}

/// Largest accepted invite lifetime: one year, in seconds. A bounded ceiling
/// keeps a fat-fingered `expires_in_secs` from minting an effectively-eternal
/// link. Pure so it is validated at the edge and unit-tested offline.
const MAX_EXPIRES_IN_SECS: i64 = 365 * 24 * 60 * 60;

/// Validate a requested `expires_in_secs`, returning the absolute expiry instant
/// relative to `now` (or `None` for a never-expiring invite).
///
/// `None`/absent ⇒ never expires. A `Some(n)` must be in `1..=MAX_EXPIRES_IN_SECS`.
/// Pure: no DB, `now` is injected, so the rule unit-tests offline.
///
/// # Errors
/// [`AeroError::Invalid`] when `n <= 0` or `n > MAX_EXPIRES_IN_SECS`.
fn resolve_expiry(
    expires_in_secs: Option<i64>,
    now: time::OffsetDateTime,
) -> AeroResult<Option<time::OffsetDateTime>> {
    match expires_in_secs {
        None => Ok(None),
        Some(n) if (1..=MAX_EXPIRES_IN_SECS).contains(&n) => {
            Ok(Some(now + time::Duration::seconds(n)))
        }
        Some(n) => Err(AeroError::Invalid(format!(
            "expires_in_secs must be 1..={MAX_EXPIRES_IN_SECS}, got {n}"
        ))),
    }
}

/// Validate a requested `max_uses`: `None` (unlimited) is fine, otherwise it must
/// be `>= 1`. Pure so it unit-tests offline.
///
/// # Errors
/// [`AeroError::Invalid`] when `max_uses = Some(n)` with `n < 1`.
fn validate_max_uses(max_uses: Option<i32>) -> AeroResult<()> {
    match max_uses {
        None => Ok(()),
        Some(n) if n >= 1 => Ok(()),
        Some(n) => Err(AeroError::Invalid(format!(
            "max_uses must be >= 1, got {n}"
        ))),
    }
}

// ---------- Handlers ----------

#[derive(Deserialize)]
struct CreateInvitationReq {
    /// Optional target address. Absent / null ⇒ an open shareable link.
    #[serde(default)]
    email: Option<String>,
    /// Role granted on accept. Absent ⇒ `member`.
    #[serde(default)]
    role: Option<WorkspaceRole>,
    /// Optional cap on redemptions. Absent ⇒ unlimited.
    #[serde(default)]
    max_uses: Option<i32>,
    /// Optional lifetime in seconds. Absent ⇒ never expires.
    #[serde(default)]
    expires_in_secs: Option<i64>,
}

/// `POST /api/workspaces/:id/invitations` — **admin/owner**: mint an invitation.
///
/// The caller must be able to invite *and* to grant the requested role without
/// escalation (same gate as the direct-add route). Returns the new invite's `id`,
/// the plaintext `token` (shown **once**), and a ready-to-share `invite_url`
/// (`{public_base_url}/invite/{token}`). Emits an `"invitation.create"` audit
/// event (the token is never logged).
async fn create_invitation(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<CreateInvitationReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace_id(&id_str)?;
    let caller = caller_role(&s, ws, auth.participant_id).await?;
    let role = req.role.unwrap_or(WorkspaceRole::Member);
    // Caller must be allowed to invite at all AND to grant this specific role.
    authorize_invite(caller, role)?;
    validate_max_uses(req.max_uses)?;
    let expires_at = resolve_expiry(req.expires_in_secs, time::OffsetDateTime::now_utc())?;
    let email = req
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty());

    // Generate once, store only the hash; the plaintext is returned below.
    let token = generate_token();
    let token_hash = hash_token(&token);
    let id = repo(&s)
        .create_authorized(
            ws,
            &token_hash,
            email,
            role,
            auth.participant_id,
            req.max_uses,
            expires_at,
        )
        .await
        .map_err(map_invitation_write_error)?;

    let invite_url = format!(
        "{}/invite/{}",
        s.public_base_url.trim_end_matches('/'),
        token
    );
    // Email the invitation link when SMTP is configured and a recipient
    // address was provided. Best-effort: the admin already received the
    // invite URL in the API response and can share it manually.
    if let (Some(ref mailer), Some(invite_email)) = (s.mailer, email) {
        // Best-effort: the workspace name is displayed in the email, but
        // failing to fetch it is non-fatal — the email still delivers.
        let ws_name = match s.workspaces.get(ws).await {
            Ok(Some(w)) => w.name,
            _ => "your team's workspace".into(),
        };
        mailer
            .send_invitation(invite_email, &invite_url, &ws_name)
            .await;
    }
    Ok(Json(serde_json::json!({
        "id": id,
        // Shown exactly once — only the hash is persisted server-side.
        "token": token,
        "invite_url": invite_url,
    })))
}

/// `GET /api/workspaces/:id/invitations` — **admin/owner**: list the workspace's
/// invitations (tokens omitted — only hashes are stored).
async fn list_invitations(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace_id(&id_str)?;
    let caller = caller_role(&s, ws, auth.participant_id).await?;
    authorize_manage_invites(caller)?;
    let list = repo(&s)
        .list_for_workspace_authorized(ws, auth.participant_id)
        .await
        .map_err(map_invitation_write_error)?;
    Ok(Json(serde_json::to_value(list).map_err(AeroError::from)?))
}

/// `DELETE /api/invitations/:id` — **admin/owner of the invite's workspace**:
/// revoke an invitation. Storage resolves and locks the invite's own workspace,
/// rechecks effective admin access, and commits revoke + audit together. `404`
/// covers an unknown id or an id outside the caller's effective tenants;
/// revoking an already-revoked invite is idempotent (still `204`).
async fn revoke_invitation(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<StatusCode> {
    let id = parse_invitation_id(&id_str)?;
    repo(&s)
        .revoke_authorized(id, auth.participant_id)
        .await
        .map_err(map_invitation_write_error)?;
    Ok(StatusCode::NO_CONTENT)
}

fn map_invitation_write_error(error: AeroError) -> AeroError {
    match error {
        AeroError::NotFound(_) => AeroError::NotFound("invitation".into()),
        AeroError::Forbidden(_) => {
            AeroError::Forbidden("current workspace invitation authority required".into())
        }
        other => other,
    }
}

#[derive(Deserialize)]
struct AcceptInvitationReq {
    token: String,
}

fn map_accept_error(error: InvitationAcceptError) -> AeroError {
    match error {
        InvitationAcceptError::NotFound => AeroError::NotFound("invitation".into()),
        InvitationAcceptError::ParticipantUnavailable => {
            AeroError::Forbidden("participant is inactive".into())
        }
        InvitationAcceptError::NotRedeemable => {
            AeroError::Invalid("invite is expired, revoked, or already used".into())
        }
        InvitationAcceptError::InvalidRole => AeroError::Internal(anyhow::anyhow!(
            "invitation acceptance encountered an invalid persisted role"
        )),
        InvitationAcceptError::Storage(error) => AeroError::from(error),
    }
}

/// `POST /api/invitations/accept` — **any logged-in user**: redeem an invite by
/// its plaintext token and join the workspace with the invite's role.
///
/// The repository locks the invitation and commits membership, durable redemption,
/// and capacity use as one transaction. Unknown tokens return `404`; a known but
/// expired/revoked/exhausted token returns `400`. Repeating a successful redemption
/// returns `200` without consuming another use. If an administrator removed the
/// membership afterward, retry remains a no-op and reports
/// `membership_active=false` rather than bypassing that removal.
///
/// Existing workspace members keep their current role and consume no capacity;
/// invitation acceptance is never a role-update path.
async fn accept_invitation(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<AcceptInvitationReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let token = req.token.trim();
    if token.is_empty() {
        return Err(AeroError::Invalid("invite token is empty".into()).into());
    }
    let r = repo(&s);
    let token_hash = hash_token(token);
    let accepted = r
        .accept_by_token_hash(&token_hash, auth.participant_id)
        .await
        .map_err(map_accept_error)?;

    // Acceptance already committed membership, durable redemption, capacity,
    // and audit together. Match direct member-add onboarding: a newly-created
    // ordinary membership receives current default channels; guest invitations
    // must not expand beyond explicitly granted channel access.
    if accepted.membership_created && accepted.role != WorkspaceRole::Guest {
        crate::default_channels::auto_join_defaults(
            &s,
            accepted.invitation.workspace_id,
            auth.participant_id,
        )
        .await;
    }

    Ok(Json(serde_json::json!({
        "workspace_id": accepted.invitation.workspace_id,
        "role": accepted.role,
        "already_accepted": accepted.already_accepted,
        "membership_created": accepted.membership_created,
        "membership_active": accepted.membership_active,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [WorkspaceRole; 4] = [
        WorkspaceRole::Guest,
        WorkspaceRole::Member,
        WorkspaceRole::Admin,
        WorkspaceRole::Owner,
    ];

    fn status_of(r: &AeroResult<()>) -> u16 {
        r.as_ref().err().map_or(200, AeroError::status_code)
    }

    fn allowed(r: &AeroResult<()>) -> bool {
        r.is_ok()
    }

    fn t(secs: i64) -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp(secs).expect("valid timestamp")
    }

    #[test]
    fn invitation_write_errors_hide_cross_tenant_ids() {
        let missing = map_invitation_write_error(AeroError::NotFound("other workspace".into()));
        assert_eq!(missing.status_code(), 404);
        assert_eq!(missing.to_string(), "not found: invitation");

        let revoked =
            map_invitation_write_error(AeroError::Forbidden("demoted after preflight".into()));
        assert_eq!(revoked.status_code(), 403);
        assert_eq!(
            revoked.to_string(),
            "forbidden: current workspace invitation authority required"
        );
    }

    #[test]
    fn manage_invites_is_admin_and_owner_only() {
        for r in ALL {
            assert_eq!(
                allowed(&authorize_manage_invites(r)),
                r.can_administer(),
                "manage invites allowed only for admin/owner, role {r:?}"
            );
        }
        // Denials are 403 (authorization), not 400/404.
        assert_eq!(
            status_of(&authorize_manage_invites(WorkspaceRole::Member)),
            403
        );
        assert_eq!(
            status_of(&authorize_manage_invites(WorkspaceRole::Guest)),
            403
        );
    }

    #[test]
    fn resolve_expiry_none_is_never() {
        assert_eq!(resolve_expiry(None, t(1_000)).unwrap(), None);
    }

    #[test]
    fn resolve_expiry_adds_offset_within_bounds() {
        let now = t(1_000);
        assert_eq!(
            resolve_expiry(Some(60), now).unwrap(),
            Some(now + time::Duration::seconds(60))
        );
        // The ceiling itself is accepted.
        assert_eq!(
            resolve_expiry(Some(MAX_EXPIRES_IN_SECS), now).unwrap(),
            Some(now + time::Duration::seconds(MAX_EXPIRES_IN_SECS))
        );
    }

    #[test]
    fn resolve_expiry_rejects_non_positive_and_overlong() {
        let now = t(1_000);
        assert_eq!(status_of(&resolve_expiry(Some(0), now).map(|_| ())), 400);
        assert_eq!(status_of(&resolve_expiry(Some(-5), now).map(|_| ())), 400);
        assert_eq!(
            status_of(&resolve_expiry(Some(MAX_EXPIRES_IN_SECS + 1), now).map(|_| ())),
            400
        );
    }

    #[test]
    fn validate_max_uses_accepts_none_and_positive_rejects_below_one() {
        assert!(validate_max_uses(None).is_ok());
        assert!(validate_max_uses(Some(1)).is_ok());
        assert!(validate_max_uses(Some(100)).is_ok());
        assert_eq!(status_of(&validate_max_uses(Some(0))), 400);
        assert_eq!(status_of(&validate_max_uses(Some(-3))), 400);
    }
}
