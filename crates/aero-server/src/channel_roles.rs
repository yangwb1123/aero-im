//! Channel role management — see member roles, change a member's room role, and
//! transfer channel ownership.
//!
//! Operates entirely within the EXISTING `room_members.role` values
//! (`CHECK (role IN ('owner', 'member', 'admin'))`, migration 0001) via
//! [`RoomRoleRepo`](aero_storage::RoomRoleRepo) — no new table, no new id, no
//! schema change. A requested role is parsed into [`RoomMemberRole`] up front
//! (unknown values ⇒ `400`), so the DB CHECK is never the input validator.
//!
//! Authorization is two-tier: everyone with room access may *view* the roster
//! (the same [`assert_room_access`](aero_im_core::ImService::assert_room_access)
//! tenant + membership guard the rest of the app uses); only the room's CURRENT
//! owner (`role_of == "owner"`) may *mutate* roles or transfer ownership. Storage
//! locks the workspace, channel, and membership aggregate and commits each
//! governance operation atomically, so concurrent demotions/transfers cannot
//! leave the channel ownerless.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, RoomId};
use aero_storage::{RoomMemberRole, RoomMembershipWriteError, RoomRoleRepo};
use axum::{
    extract::{Path, State},
    routing::{get, post, put},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All channel-role routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/rooms/:id/roles", get(list_roles))
        .route("/api/rooms/:id/roles/:pid", put(set_member_role))
        .route(
            "/api/rooms/:id/transfer-ownership",
            post(transfer_ownership),
        )
}

/// Build a [`RoomRoleRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> RoomRoleRepo {
    RoomRoleRepo::new(s.pg.clone())
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

fn parse_participant(s: &str) -> Result<ParticipantId, AeroError> {
    ParticipantId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
}

/// Assert the caller is the room's CURRENT owner (and may therefore mutate roles).
/// Runs the shared tenant + membership guard first
/// ([`assert_room_access`](aero_im_core::ImService::assert_room_access)), so a
/// missing room is `404` and a non-member / cross-tenant caller is `403`; a member
/// who is not the owner is then rejected `403`.
async fn assert_owner(s: &AppState, room: RoomId, caller: ParticipantId) -> Result<(), AeroError> {
    s.im.assert_channel_access(caller, room).await?;
    let role = repo(s)
        .role_of(room, caller)
        .await
        .map_err(AeroError::from)?;
    if role.as_deref() == Some("owner") {
        Ok(())
    } else {
        Err(AeroError::Forbidden(
            "only the channel owner may manage roles".into(),
        ))
    }
}

fn parse_role(role: &str) -> Result<RoomMemberRole, AeroError> {
    match role.trim() {
        "owner" => Ok(RoomMemberRole::Owner),
        "admin" => Ok(RoomMemberRole::Admin),
        "member" => Ok(RoomMemberRole::Member),
        _ => Err(AeroError::Invalid(
            "role must be one of [owner, admin, member]".into(),
        )),
    }
}

fn map_channel_write_error(error: RoomMembershipWriteError) -> AeroError {
    match error {
        RoomMembershipWriteError::WorkspaceNotFound => AeroError::NotFound("workspace".into()),
        RoomMembershipWriteError::RoomNotFound => AeroError::NotFound("room".into()),
        RoomMembershipWriteError::NotChannel => AeroError::Invalid("room is not a channel".into()),
        RoomMembershipWriteError::FixedMembership => {
            AeroError::Conflict("direct and group-DM membership is fixed".into())
        }
        RoomMembershipWriteError::NotJoinable => {
            AeroError::Forbidden("channel is private or archived".into())
        }
        RoomMembershipWriteError::MemberNotFound => AeroError::NotFound("room member".into()),
        RoomMembershipWriteError::NotAuthorized => {
            AeroError::Forbidden("only the current channel owner may manage roles".into())
        }
        RoomMembershipWriteError::LastOwner => {
            AeroError::Conflict("cannot demote the last owner".into())
        }
        RoomMembershipWriteError::TargetNotEligible => AeroError::Conflict(
            "target must be a retained, non-guest workspace member with an active account".into(),
        ),
        RoomMembershipWriteError::TransferToSelf => {
            AeroError::Invalid("cannot transfer ownership to yourself".into())
        }
        RoomMembershipWriteError::InvalidInput(message) => AeroError::Invalid(message),
        RoomMembershipWriteError::Storage(error) => AeroError::from(error),
    }
}

/// `GET /api/rooms/:id/roles` — every member of the room paired with their role.
/// Requires room access (the standard tenant + membership guard); any member may
/// view. Shape: `{ "members": [{ "participant_id", "role" }, ...] }`.
async fn list_roles(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_channel_access(auth.participant_id, room)
        .await?;
    let members = repo(&s)
        .members_with_roles(room)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({
        "members": members
            .into_iter()
            .map(|(pid, role)| serde_json::json!({
                "participant_id": pid,
                "role": role,
            }))
            .collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct SetRoleReq {
    /// The new room role for the target member (`owner`, `admin`, or `member`).
    role: String,
}

/// `PUT /api/rooms/:id/roles/:pid` — change member `pid`'s role in the room. Only
/// the room's owner may call (`403` otherwise). The requested role must be one of
/// those three role tokens (`400` otherwise); the target must already be a member (`404`
/// otherwise). Demoting the last owner is refused `400` so the channel is never
/// left ownerless. Returns `{ "participant_id", "role" }`.
async fn set_member_role(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((room_str, pid_str)): Path<(String, String)>,
    Json(req): Json<SetRoleReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    let target = parse_participant(&pid_str)?;
    assert_owner(&s, room, auth.participant_id).await?;

    let role = parse_role(&req.role)?;
    s.rooms
        .change_channel_member_role_authorized(room, auth.participant_id, target, role)
        .await
        .map_err(map_channel_write_error)?;
    // Best-effort privileged-operation audit (ROADMAP 方向四). The role change has
    // already committed; a logging failure must only warn, never fail the request.
    audit_role_changed(&s, room, auth.participant_id, target, role.as_str()).await;
    Ok(Json(serde_json::json!({
        "participant_id": target,
        "role": role.as_str(),
    })))
}

/// Record a `channel_role.changed` audit event, attributing it to `actor` against
/// the workspace of the room. Best-effort throughout: failing to resolve the
/// workspace or to append is warn-logged and swallowed (the role change already
/// committed); a room with no resolvable workspace is silently skipped.
async fn audit_role_changed(
    s: &AppState,
    room: RoomId,
    actor: ParticipantId,
    target: ParticipantId,
    new_role: &str,
) {
    let workspace = match s.rooms.room_workspace(room).await {
        Ok(Some(ws)) => ws,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(error = ?e, %room, "channel_role audit: resolve workspace failed");
            return;
        }
    };
    if let Err(e) = s
        .audit
        .append(
            workspace,
            Some(actor),
            "channel_role.changed",
            Some(&target.to_string()),
            serde_json::json!({
                "room_id": room.to_string(),
                "target_participant": target.to_string(),
                "new_role": new_role,
            }),
        )
        .await
    {
        tracing::warn!(error = ?e, %workspace, "channel_role.changed audit append failed");
    }
}

#[derive(Deserialize)]
struct TransferReq {
    /// The member to promote to owner.
    to: String,
}

/// `POST /api/rooms/:id/transfer-ownership` — hand the room's ownership to another
/// member. Only the current owner may call (`403` otherwise); the recipient must
/// be an existing member (`404` otherwise) and distinct from the caller (`400`
/// otherwise). The recipient is promoted to `owner` FIRST (so the room is never
/// momentarily ownerless), then the caller is demoted to `member`. Returns
/// `{ "ok": true }`.
async fn transfer_ownership(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<TransferReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    assert_owner(&s, room, auth.participant_id).await?;
    let to = parse_participant(&req.to)?;

    if to == auth.participant_id {
        return Err(AeroError::Invalid("cannot transfer ownership to yourself".into()).into());
    }
    s.rooms
        .transfer_channel_ownership_authorized(room, auth.participant_id, to)
        .await
        .map_err(map_channel_write_error)?;
    // Best-effort privileged-operation audit (ROADMAP 方向四): record both halves
    // of the transfer (recipient → owner, former owner → demoted). Already
    // committed above; logging failures only warn, never fail the request.
    audit_role_changed(&s, room, auth.participant_id, to, "owner").await;
    audit_role_changed(
        &s,
        room,
        auth.participant_id,
        auth.participant_id,
        RoomMemberRole::Member.as_str(),
    )
    .await;
    Ok(Json(serde_json::json!({ "ok": true })))
}
