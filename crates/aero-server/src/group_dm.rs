//! Group direct-message (multi-person, nameless) find-or-create HTTP surface.
//!
//! A group DM is a small, nameless `group`-kind room shared by a fixed set of
//! people (Slack/Lark "multi-person DM"). Opening one is idempotent find-or-create
//! over that *exact* set: the first request among a given group of people creates a
//! nameless group room; every later request for the same set resolves to it.
//! Lookup is owned by [`GroupDmRepo`](aero_storage::GroupDmRepo); room *creation*
//! reuses the existing [`RoomRepo`](aero_storage::RoomRepo) (`create_in_workspace`
//! enrolls the caller as owner, then each other member is added), so the create SQL
//! is not duplicated here.
//!
//! The member set is the caller ∪ the requested ids, de-duplicated, and must hold
//! 3–8 people: a 2-person set is a 1:1 DM (steered to `POST /api/dm`), and the
//! upper bound keeps a "group DM" small (larger groups are channels). Group DMs are
//! not a per-tenant concept here, so they are created in the legacy all-zero
//! default workspace (mirroring [`crate::dm`]). Listing is owner-scoped — it filters
//! the caller's own room list down to nameless `group` rooms — so a caller can only
//! ever see their own group DMs. Mounted via [`routes`] and `.merge`d into the main
//! router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, Room, RoomEvent, RoomId, RoomKind, WorkspaceId};
use aero_storage::GroupDmRepo;
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All group-DM routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/group-dm", post(open_group_dm).get(list_group_dms))
        // Group DM naming (ROADMAP6 Lane A): PATCH name/description of a group room.
        .route(
            "/api/rooms/:id/name",
            axum::routing::patch(set_group_dm_name),
        )
}

/// The legacy / default workspace (the all-zero `WorkspaceId`) that group DMs are
/// created in, mirroring [`crate::dm`]'s `DEFAULT_WORKSPACE_ID`. Group DMs are not a
/// per-tenant concept here, so they land in the default workspace alongside other
/// untenanted rooms.
const DEFAULT_WORKSPACE_ID: WorkspaceId = WorkspaceId(ulid::Ulid(0));

/// Smallest group-DM size — fewer than this is a 1:1 DM (or nonsensical), so the
/// caller is steered to `POST /api/dm` instead.
const MIN_MEMBERS: usize = 3;
/// Largest group-DM size — beyond this a conversation should be a channel.
const MAX_MEMBERS: usize = 8;

fn parse_participant(s: &str) -> Result<ParticipantId, AeroError> {
    ParticipantId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
}

/// Resolve the full [`Room`] row for a room the caller belongs to, by scanning the
/// caller's own room list — no new single-room query is needed. `find_exact` only
/// ever returns a room the caller is a member of, so the lookup succeeds; a
/// defensive `NotFound` covers the impossible miss. Mirrors `crate::dm::caller_room`.
async fn caller_room(s: &AppState, caller: ParticipantId, room: RoomId) -> Result<Room, AeroError> {
    s.rooms
        .rooms_for(caller)
        .await
        .map_err(AeroError::from)?
        .into_iter()
        .find(|r| r.id == room)
        .ok_or_else(|| AeroError::NotFound("group DM room".into()))
}

#[derive(Deserialize)]
struct CreateGroupDmReq {
    /// The other participants to include (the caller is added automatically). Parsed
    /// then unioned with the caller and de-duplicated to form the member set.
    participant_ids: Vec<String>,
}

/// `POST /api/group-dm` — open the nameless group DM among the caller and the
/// requested participants, creating it on first use. The member set is the caller ∪
/// the requested ids, de-duplicated, and must hold 3–8 people: a 2-person set is
/// rejected `400` ("use /api/dm"), as is a set smaller than 3 or larger than 8.
/// Idempotent: returns the existing group DM when one already exists for the exact
/// set, otherwise creates a fresh nameless `group` room (caller enrolled as owner,
/// every other member added). Returns the room.
async fn open_group_dm(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateGroupDmReq>,
) -> ApiResult<Json<serde_json::Value>> {
    // Build the member set: caller ∪ requested ids, de-duplicated (the caller may
    // appear in the request, and ids may repeat).
    let mut members: Vec<ParticipantId> = vec![auth.participant_id];
    for raw in &req.participant_ids {
        let pid = parse_participant(raw)?;
        if !members.contains(&pid) {
            members.push(pid);
        }
    }

    let n = members.len();
    if n == 2 {
        return Err(AeroError::Invalid(
            "a 2-person conversation is a 1:1 — use POST /api/dm".into(),
        )
        .into());
    }
    if n < MIN_MEMBERS {
        return Err(AeroError::Invalid(format!(
            "a group DM needs at least {MIN_MEMBERS} people"
        ))
        .into());
    }
    if n > MAX_MEMBERS {
        return Err(AeroError::Invalid(format!(
            "a group DM may have at most {MAX_MEMBERS} people"
        ))
        .into());
    }

    // Information barrier (ethical wall) enforcement: no two members may sit across
    // a barred pair of user-groups. Checked over every unordered pair in the member
    // set; built inline from the shared pool so the check is ALWAYS available. Group
    // DMs live in the default workspace, so the barrier is evaluated there.
    let barriers = aero_storage::BarrierRepo::new(s.pg.clone());
    for (i, x) in members.iter().enumerate() {
        for y in &members[i + 1..] {
            if barriers
                .barred(DEFAULT_WORKSPACE_ID, *x, *y)
                .await
                .map_err(AeroError::from)?
            {
                return Err(AeroError::Forbidden("information barrier".into()).into());
            }
        }
    }

    let repo = GroupDmRepo::new(s.pg.clone());
    if let Some(room_id) = repo.find_exact(&members).await.map_err(AeroError::from)? {
        // Existing group DM for this exact set — return its full row (the caller is
        // a member, so it is in their room list).
        let room = caller_room(&s, auth.participant_id, room_id).await?;
        return Ok(Json(serde_json::to_value(room).map_err(AeroError::from)?));
    }

    // First contact: create the nameless group room (the caller is enrolled as
    // owner by `create_in_workspace`) and enroll every other member, so the room's
    // full membership equals the set and future lookups match.
    let room = s
        .rooms
        .create_in_workspace(DEFAULT_WORKSPACE_ID, RoomKind::Group, None, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    for member in &members {
        if *member != auth.participant_id {
            s.rooms
                .add_member(room.id, *member)
                .await
                .map_err(AeroError::from)?;
        }
    }
    Ok(Json(serde_json::to_value(room).map_err(AeroError::from)?))
}

/// `GET /api/group-dm` — the caller's group DMs, newest first. Owner-scoped:
/// filters the caller's own room list down to nameless `group`-kind rooms (the
/// shape a group DM always has), so it can never surface a room the caller does not
/// belong to.
async fn list_group_dms(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let rooms: Vec<Room> = s
        .rooms
        .rooms_for(auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .into_iter()
        .filter(|r| r.kind == RoomKind::Group && r.name.is_none())
        .collect();
    Ok(Json(serde_json::json!({ "rooms": rooms })))
}

// ---------- Group DM naming (ROADMAP6 Lane A) ----------

#[derive(Deserialize)]
struct SetGroupDmNameReq {
    /// New display name for the group DM. Pass `null` to clear it (revert to
    /// nameless). Optional — absent means "don't change".
    #[serde(default)]
    name: Option<Option<String>>,
    /// New description for the group DM. Pass `null` to clear. Optional.
    #[serde(default)]
    description: Option<Option<String>>,
}

fn parse_room_for_name(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

/// `PATCH /api/rooms/:id/name` — update the display name and/or description of a
/// group-kind room. The room must exist, have `kind = 'group'`, and the caller must
/// be a member. Both fields are optional; absent fields are left unchanged. Passing
/// `null` for `name` or `description` clears it (reverts the group DM to nameless).
///
/// After a successful update, a `Membership { Join }` event is broadcast so
/// connected clients refresh the room metadata (no dedicated `RoomUpdated` event
/// exists yet; membership events already trigger room-list refreshes in clients).
async fn set_group_dm_name(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<SetGroupDmNameReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_for_name(&id_str)?;

    // The room must exist and be of kind 'group'.
    let kind = s
        .rooms
        .room_kind(room)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("room {room}")))?;

    if kind != RoomKind::Group {
        return Err(AeroError::Invalid(
            "only group-kind rooms support PATCH /name".into(),
        )
        .into());
    }

    // Caller must be a member.
    if !s.rooms.is_member(room, auth.participant_id).await.map_err(AeroError::from)? {
        return Err(AeroError::Forbidden(format!(
            "{} is not a member of room {}",
            auth.participant_id, room
        ))
        .into());
    }

    // Apply the requested changes — each field is optional.
    if let Some(name_val) = req.name {
        // Direct SQL update: no dedicated set_name on RoomRepo (rooms.name is
        // already nullable TEXT), so we use sqlx::query here per conventions.
        sqlx::query(r"UPDATE rooms SET name = $2 WHERE id = $1")
            .bind(room.to_uuid())
            .bind(name_val.as_deref())
            .execute(&s.pg)
            .await
            .map_err(AeroError::from)?;
    }
    if let Some(desc_val) = req.description {
        s.rooms
            .set_description(room, desc_val.as_deref())
            .await
            .map_err(AeroError::from)?;
    }

    // Broadcast a metadata-change hint (Membership Join reuses the existing event path;
    // no dedicated RoomUpdated event exists yet). Best-effort.
    s.im
        .broadcast_room_event(
            room,
            RoomEvent::Membership {
                room_id: room,
                participant: auth.participant_id,
                op: aero_common::MembershipOp::Join,
            },
        )
        .await;

    Ok(Json(serde_json::json!({ "room_id": room, "updated": true })))
}
