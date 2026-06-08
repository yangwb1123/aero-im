//! Channel role management — see member roles, change a member's room role, and
//! transfer channel ownership.
//!
//! Operates entirely within the EXISTING `room_members.role` values
//! (`CHECK (role IN ('owner', 'member', 'admin'))`, migration 0001) via
//! [`RoomRoleRepo`](aero_storage::RoomRoleRepo) — no new table, no new id, no
//! schema change. A requested role is validated against [`ALLOWED_ROLES`] up front
//! (others ⇒ `400`), so the DB CHECK is never the thing that rejects a bad value.
//!
//! Authorization is two-tier: everyone with room access may *view* the roster
//! (the same [`assert_room_access`](aero_im_core::ImService::assert_room_access)
//! tenant + membership guard the rest of the app uses); only the room's CURRENT
//! owner (`role_of == "owner"`) may *mutate* roles or transfer ownership. The last
//! owner can never be demoted (checked via `count_owners`), so a channel is never
//! left ownerless. Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, RoomId};
use aero_storage::RoomRoleRepo;
use axum::{
    extract::{Path, State},
    routing::{get, post, put},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// The role tokens a member may hold, matching the `room_members.role` CHECK
/// constraint (migration 0001) exactly. A requested role outside this set is
/// rejected `400` before any write, so the DB CHECK never has to.
const ALLOWED_ROLES: [&str; 3] = ["owner", "member", "admin"];

/// The role a former owner is demoted to on ownership transfer — the canonical
/// non-owner value.
const DEMOTED_ROLE: &str = "member";

/// All channel-role routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/rooms/:id/roles", get(list_roles))
        .route("/api/rooms/:id/roles/:pid", put(set_member_role))
        .route("/api/rooms/:id/transfer-ownership", post(transfer_ownership))
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
async fn assert_owner(
    s: &AppState,
    room: RoomId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    s.im.assert_room_access(caller, room).await?;
    let role = repo(s).role_of(room, caller).await.map_err(AeroError::from)?;
    if role.as_deref() == Some("owner") {
        Ok(())
    } else {
        Err(AeroError::Forbidden("only the channel owner may manage roles".into()))
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
    s.im.assert_room_access(auth.participant_id, room).await?;
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
    /// The new room role for the target member (one of [`ALLOWED_ROLES`]).
    role: String,
}

/// `PUT /api/rooms/:id/roles/:pid` — change member `pid`'s role in the room. Only
/// the room's owner may call (`403` otherwise). The requested role must be one of
/// [`ALLOWED_ROLES`] (`400` otherwise); the target must already be a member (`404`
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

    let role = req.role.trim();
    if !ALLOWED_ROLES.contains(&role) {
        return Err(AeroError::Invalid(format!(
            "role must be one of {ALLOWED_ROLES:?}"
        ))
        .into());
    }

    // The target must already be a member; their current role drives the
    // last-owner guard below.
    let current = repo(&s)
        .role_of(room, target)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("room member".into()))?;

    // Refuse to demote the final owner — that would leave the channel ownerless.
    if current == "owner" && role != "owner" {
        let owners = repo(&s).count_owners(room).await.map_err(AeroError::from)?;
        if owners <= 1 {
            return Err(AeroError::Invalid("cannot demote the last owner".into()).into());
        }
    }

    let changed = repo(&s)
        .set_role(room, target, role)
        .await
        .map_err(AeroError::from)?;
    if !changed {
        // Lost a race with a concurrent removal — the member is gone.
        return Err(AeroError::NotFound("room member".into()).into());
    }
    // Best-effort privileged-operation audit (ROADMAP 方向四). The role change has
    // already committed; a logging failure must only warn, never fail the request.
    audit_role_changed(&s, room, auth.participant_id, target, role).await;
    Ok(Json(serde_json::json!({
        "participant_id": target,
        "role": role,
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
/// momentarily ownerless), then the caller is demoted to [`DEMOTED_ROLE`]. Returns
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
    // The recipient must already be a member of the room.
    repo(&s)
        .role_of(room, to)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("room member".into()))?;

    // Promote the recipient first so there is always at least one owner, then
    // step the former owner down to the canonical non-owner role.
    repo(&s).set_role(room, to, "owner").await.map_err(AeroError::from)?;
    repo(&s)
        .set_role(room, auth.participant_id, DEMOTED_ROLE)
        .await
        .map_err(AeroError::from)?;
    // Best-effort privileged-operation audit (ROADMAP 方向四): record both halves
    // of the transfer (recipient → owner, former owner → demoted). Already
    // committed above; logging failures only warn, never fail the request.
    audit_role_changed(&s, room, auth.participant_id, to, "owner").await;
    audit_role_changed(&s, room, auth.participant_id, auth.participant_id, DEMOTED_ROLE).await;
    Ok(Json(serde_json::json!({ "ok": true })))
}
