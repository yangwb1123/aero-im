//! Direct-message (1:1) find-or-create HTTP surface.
//!
//! Opening a DM with another participant is idempotent find-or-create: the first
//! request between two people creates a two-member `direct`-kind room; every later
//! request resolves to that same room. [`DmRepo`](aero_storage::DmRepo) owns the
//! transaction: an exact-conversation advisory lock serializes concurrent opens,
//! then one transaction rechecks the room and, when absent, inserts the room plus
//! both memberships atomically.
//!
//! DMs are not a per-tenant concept here, so they are created in the legacy
//! all-zero default workspace (mirroring `crate::routes`'s `DEFAULT_WORKSPACE_ID`).
//! Every participant supplied to open and every listing caller must have effective
//! access to that workspace (active account + membership + no deactivation +
//! mandatory 2FA). Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, Room, RoomKind, WorkspaceId};
use aero_storage::{DmRepo, DmWriteError};
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};

use crate::error::ApiResult;
use crate::routes::helpers::{
    assert_effective_workspace_member, assert_effective_workspace_members,
};
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
    ParticipantId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
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

    // A DM is a default-workspace resource. Gate the caller before barrier
    // lookup, find-existing, or room creation so a deleted/deactivated account
    // (or one missing mandatory 2FA) cannot observe or mutate retained rows.
    assert_effective_workspace_members(&s, DEFAULT_WORKSPACE_ID, &[auth.participant_id, target])
        .await?;

    // Information barrier (ethical wall) enforcement: if the caller and target sit
    // across a barred pair of user-groups, they may not open a DM. The repo is
    // built inline from the shared pool so the check is ALWAYS available; DMs live
    // in the default workspace, so the barrier is evaluated there (which is where a
    // single-tenant admin defines them).
    if aero_storage::BarrierRepo::new(s.pg.clone())
        .barred(DEFAULT_WORKSPACE_ID, auth.participant_id, target)
        .await
        .map_err(AeroError::from)?
    {
        return Err(AeroError::Forbidden("information barrier".into()).into());
    }

    // Block guard: neither party can open a DM with the other if a block exists
    // in EITHER direction. Checked after the barrier guard, before find-or-create,
    // so blocked users cannot reach each other's DM rooms.
    let me = auth.participant_id;
    if s.blocks.is_blocked(me, target).await.unwrap_or(false)
        || s.blocks.is_blocked(target, me).await.unwrap_or(false)
    {
        return Err(AeroError::Forbidden("blocked".into()).into());
    }

    let room = DmRepo::new(s.pg.clone())
        .find_or_create_in_workspace(DEFAULT_WORKSPACE_ID, auth.participant_id, target)
        .await
        .map_err(map_dm_write_error)?;
    Ok(Json(serde_json::to_value(room).map_err(AeroError::from)?))
}

fn map_dm_write_error(error: DmWriteError) -> AeroError {
    match error {
        DmWriteError::Forbidden => {
            AeroError::Forbidden("direct-message member access was revoked".into())
        }
        DmWriteError::InformationBarrier => AeroError::Forbidden("information barrier".into()),
        DmWriteError::Blocked => AeroError::Forbidden("blocked".into()),
        DmWriteError::Storage(source) => AeroError::from(source),
    }
}

/// `GET /api/dm` — the caller's direct rooms (1:1 and group DMs), newest first.
/// Effective-access scoped: deleted/deactivated callers and members missing
/// mandatory 2FA are rejected before the listing query, which itself is limited
/// to the default workspace and repeats those effective-access predicates.
async fn list_dms(State(s): State<AppState>, auth: AuthUser) -> ApiResult<Json<serde_json::Value>> {
    assert_effective_workspace_member(&s, DEFAULT_WORKSPACE_ID, auth.participant_id).await?;
    let rooms: Vec<Room> = s
        .rooms
        .rooms_for_in_workspace(auth.participant_id, DEFAULT_WORKSPACE_ID)
        .await
        .map_err(AeroError::from)?
        .into_iter()
        .filter(|r| r.kind == RoomKind::Direct)
        .collect();
    Ok(Json(serde_json::json!({ "rooms": rooms })))
}
