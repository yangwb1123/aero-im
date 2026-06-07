//! Direct-message (1:1) find-or-create HTTP surface.
//!
//! Opening a DM with another participant is idempotent find-or-create: the first
//! request between two people creates a two-member `direct`-kind room; every later
//! request resolves to that same room. Lookup is owned by
//! [`DmRepo`](aero_storage::DmRepo); room *creation* reuses the existing
//! [`RoomRepo`](aero_storage::RoomRepo) (`create_in_workspace` enrolls the caller
//! as owner, then the target is added as the second member), so the create SQL is
//! not duplicated here.
//!
//! DMs are not a per-tenant concept here, so they are created in the legacy
//! all-zero default workspace (mirroring `crate::routes`'s `DEFAULT_WORKSPACE_ID`).
//! Listing is owner-scoped — it filters the caller's own room list down to the
//! `direct` rooms — so a caller can only ever see their own DMs. Mounted via
//! [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, Room, RoomId, RoomKind, WorkspaceId};
use aero_storage::DmRepo;
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// All direct-message routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/dm", get(list_dms))
        .route("/api/dm/:participant_id", post(open_dm))
}

/// The legacy / default workspace (the all-zero `WorkspaceId`) that 1:1 DMs are
/// created in, mirroring `crate::routes`'s `DEFAULT_WORKSPACE_ID`. DMs are not a
/// per-tenant concept here, so they land in the default workspace alongside other
/// untenanted rooms.
const DEFAULT_WORKSPACE_ID: WorkspaceId = WorkspaceId(ulid::Ulid(0));

fn parse_participant(s: &str) -> Result<ParticipantId, AeroError> {
    ParticipantId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
}

/// Resolve the full [`Room`] row for a room the caller belongs to, by scanning the
/// caller's own room list — no new single-room query is needed. `find_direct` only
/// ever returns a room the caller is a member of, so the lookup succeeds; a
/// defensive `NotFound` covers the impossible miss.
async fn caller_room(s: &AppState, caller: ParticipantId, room: RoomId) -> Result<Room, AeroError> {
    s.rooms
        .rooms_for(caller)
        .await
        .map_err(AeroError::from)?
        .into_iter()
        .find(|r| r.id == room)
        .ok_or_else(|| AeroError::NotFound("direct room".into()))
}

/// `POST /api/dm/:participant_id` — open the 1:1 direct room with another
/// participant, creating it on first use. Self-DM (target == caller) is rejected
/// `400`. Idempotent: returns the existing room when one already exists, otherwise
/// creates a fresh two-member `direct` room (caller + target). Returns the room.
async fn open_dm(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(target_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = parse_participant(&target_str)?;
    if target == auth.participant_id {
        return Err(AeroError::Invalid("cannot open a direct message with yourself".into()).into());
    }

    let dm = DmRepo::new(s.pg.clone());
    if let Some(room_id) = dm
        .find_direct(auth.participant_id, target)
        .await
        .map_err(AeroError::from)?
    {
        // Existing 1:1 — return its full row (the caller is a member, so it is in
        // their room list).
        let room = caller_room(&s, auth.participant_id, room_id).await?;
        return Ok(Json(serde_json::to_value(room).map_err(AeroError::from)?));
    }

    // First contact: create the direct room (the caller is enrolled as owner by
    // `create_in_workspace`) and enroll the target as the second member, so the
    // room has exactly two members and future lookups match.
    let room = s
        .rooms
        .create_in_workspace(DEFAULT_WORKSPACE_ID, RoomKind::Direct, None, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    s.rooms
        .add_member(room.id, target)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(room).map_err(AeroError::from)?))
}

/// `GET /api/dm` — the caller's direct rooms (1:1 and group DMs), newest first.
/// Owner-scoped: filters the caller's own room list down to `direct`-kind rooms,
/// so it can never surface a room the caller does not belong to.
async fn list_dms(State(s): State<AppState>, auth: AuthUser) -> ApiResult<Json<serde_json::Value>> {
    let rooms: Vec<Room> = s
        .rooms
        .rooms_for(auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .into_iter()
        .filter(|r| r.kind == RoomKind::Direct)
        .collect();
    Ok(Json(serde_json::json!({ "rooms": rooms })))
}
