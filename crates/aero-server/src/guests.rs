//! Single-channel guest accounts HTTP API (To-B external collaborators).
//!
//! A workspace admin invites an external **guest**: a workspace member flagged
//! `is_guest = true` who is added ONLY to the specific channel they were invited
//! to. Guests cannot self-join other (public) channels — that enforcement lives
//! in [`aero_im_core::ImService::add_member`], which denies pulling a guest into
//! an arbitrary channel. This module is the admin-facing management surface:
//! enroll a guest into one room, list a workspace's guests, and revoke a guest.
//!
//! Additive layer over the already-merged [`aero_storage::WorkspaceRepo`]
//! (guest methods) + [`aero_storage::RoomRepo`]. Mirrors the
//! `crate::workspaces` module: a thin async handler shell resolves ids + the
//! caller's role, calls a pure DB-free authorization guard
//! ([`authorize_manage_guests`]), then runs the repo mutation.

use std::str::FromStr;

use aero_common::{
    Error as AeroError, ParticipantId, Result as AeroResult, RoomId, WorkspaceId, WorkspaceRole,
};
use aero_storage::WorkspaceRepo;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use aero_auth::AuthUser;

use crate::error::ApiResult;
use crate::state::AppState;

// ---------- Router ----------

/// Mount the guest-management routes. Folded into the main router by
/// [`crate::routes::build`]; kept separate so the route table lives next to the
/// authorization logic it enforces, mirroring [`crate::workspaces::routes`].
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/workspaces/:id/guests",
            get(list_guests).post(add_guest),
        )
        .route(
            "/api/workspaces/:id/guests/:pid",
            axum::routing::delete(remove_guest),
        )
}

// ---------- Pure authorization (DB-free, unit-tested) ----------

/// May `caller` manage (invite / list / remove) guests in the workspace?
///
/// Restricted to administrators (admin/owner): inviting an external party into a
/// tenant — even confined to one channel — is a workspace-administration action,
/// the same bar as member management. Delegates to
/// [`WorkspaceRole::can_administer`].
///
/// # Errors
/// [`AeroError::Forbidden`] for non-administrators (members / guests).
pub fn authorize_manage_guests(caller: WorkspaceRole) -> AeroResult<()> {
    if caller.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden("managing guests requires admin".into()))
    }
}

// ---------- Shared helpers ----------

fn parse_workspace_id(s: &str) -> AeroResult<WorkspaceId> {
    WorkspaceId::from_str(s).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn parse_participant_id(s: &str) -> AeroResult<ParticipantId> {
    ParticipantId::from_str(s).map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
}

fn parse_room_id(s: &str) -> AeroResult<RoomId> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

/// Resolve the caller's role in a workspace, rejecting non-members with 403.
/// Mirrors `crate::workspaces`'s `caller_role` so the membership gate is written
/// once per module.
async fn caller_role(
    repo: &WorkspaceRepo,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> AeroResult<WorkspaceRole> {
    repo.member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))
}

/// Append an audit event without ever failing the caller's request (the trail is
/// observability, not a transactional invariant). Mirrors `crate::workspaces`.
async fn audit(
    s: &AppState,
    workspace: WorkspaceId,
    actor: ParticipantId,
    action: &str,
    target: Option<&str>,
    detail: serde_json::Value,
) {
    if let Err(e) = s.audit.append(workspace, Some(actor), action, target, detail).await {
        tracing::warn!(error = ?e, %workspace, action, "audit append failed");
    }
}

// ---------- Handlers ----------

#[derive(Deserialize)]
struct AddGuestReq {
    participant_id: String,
    room_id: String,
}

/// `POST /api/workspaces/:id/guests` — **admin/owner**: enroll `participant_id`
/// as a guest of the workspace AND add them to the single channel `room_id` they
/// were invited to.
///
/// Steps (the order matters): verify the caller is an admin, verify the target
/// `room` belongs to **this** workspace (so a guest cannot be slipped into
/// another tenant's channel), then (1) add the participant to the room and (2)
/// flag them as a guest member. Adding to the room first means the guest is in
/// their one invited channel; the [`aero_im_core::ImService`] guest guard then
/// prevents them being pulled into any *other* channel afterwards.
async fn add_guest(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<AddGuestReq>,
) -> ApiResult<StatusCode> {
    let ws = parse_workspace_id(&id_str)?;
    let target = parse_participant_id(&req.participant_id)?;
    let room = parse_room_id(&req.room_id)?;
    let caller = caller_role(&s.workspaces, ws, auth.participant_id).await?;
    authorize_manage_guests(caller)?;

    // The invited channel must belong to THIS workspace — otherwise an admin of
    // workspace A could plant a guest into workspace B's channel.
    let room_ws = s
        .rooms
        .room_workspace(room)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("room".into()))?;
    if room_ws != ws {
        return Err(AeroError::Invalid("room does not belong to this workspace".into()).into());
    }

    // 1. Put the guest in their one invited channel (direct repo add — this is the
    //    sanctioned admin path; the ImService guard that denies guests is for
    //    *other* channels, not this initial placement).
    s.rooms.add_member(room, target).await.map_err(AeroError::from)?;
    s.room_member_cache.invalidate(&room);
    // 2. Flag them as a guest workspace member (idempotent upsert).
    s.workspaces.add_guest_member(ws, target).await.map_err(AeroError::from)?;

    audit(
        &s,
        ws,
        auth.participant_id,
        "guest.add",
        Some(&target.to_string()),
        serde_json::json!({ "room_id": room.to_string() }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/workspaces/:id/guests` — **admin/owner**: list the workspace's
/// guest members.
async fn list_guests(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace_id(&id_str)?;
    let caller = caller_role(&s.workspaces, ws, auth.participant_id).await?;
    authorize_manage_guests(caller)?;
    let guests = s.workspaces.list_guests(ws).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(guests).map_err(AeroError::from)?))
}

/// `DELETE /api/workspaces/:id/guests/:pid` — **admin/owner**: remove a guest
/// from the workspace. A guest is just a flagged member, so this removes their
/// workspace membership entirely (their per-channel room memberships are left to
/// the normal room-removal lifecycle / tenant erasure). Idempotent: removing a
/// non-guest / non-member is a no-op `204`.
async fn remove_guest(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((id_str, pid_str)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let ws = parse_workspace_id(&id_str)?;
    let target = parse_participant_id(&pid_str)?;
    let caller = caller_role(&s.workspaces, ws, auth.participant_id).await?;
    authorize_manage_guests(caller)?;
    s.workspaces.remove_member(ws, target).await.map_err(AeroError::from)?;
    audit(
        &s,
        ws,
        auth.participant_id,
        "guest.remove",
        Some(&target.to_string()),
        serde_json::json!({}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
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

    /// HTTP status a guard's error maps to (or 200 on `Ok`).
    fn status_of(r: &AeroResult<()>) -> u16 {
        r.as_ref().err().map_or(200, AeroError::status_code)
    }

    fn allowed(r: &AeroResult<()>) -> bool {
        r.is_ok()
    }

    #[test]
    fn manage_guests_is_admin_and_owner_only_over_all_roles() {
        // Exhaustive over the 4 roles: admin + owner may manage guests; member and
        // guest may not. Same bar as member management / the audit trail.
        for r in ALL {
            assert_eq!(
                allowed(&authorize_manage_guests(r)),
                r.can_administer(),
                "guest management allowed only for admin/owner, role {r:?}"
            );
        }
        assert!(allowed(&authorize_manage_guests(WorkspaceRole::Owner)));
        assert!(allowed(&authorize_manage_guests(WorkspaceRole::Admin)));
        assert!(!allowed(&authorize_manage_guests(WorkspaceRole::Member)));
        assert!(!allowed(&authorize_manage_guests(WorkspaceRole::Guest)));
    }

    #[test]
    fn manage_guests_non_admin_denials_are_403() {
        // Member and guest denials surface as 403 (authorization), not 400/404.
        for r in [WorkspaceRole::Guest, WorkspaceRole::Member] {
            assert_eq!(status_of(&authorize_manage_guests(r)), 403, "role {r:?}");
        }
    }
}
