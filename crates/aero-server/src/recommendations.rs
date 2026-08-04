//! AI recommendations — "suggested channels & people to follow".
//!
//! A discovery surface distinct from [`crate::find_expert`] (which finds a
//! TOPIC's expert): this recommends channels the caller isn't in and people they
//! don't yet follow, by affinity to the caller's OWN activity within a workspace.
//! Two member-gated read-only endpoints:
//!
//! - `GET /api/workspaces/:id/recommendations/channels` — non-private,
//!   non-archived workspace channels the caller is NOT a member of
//!   ([`RoomRepo::list_workspace_channels_not_member`](aero_storage::RoomRepo::list_workspace_channels_not_member)),
//!   ranked by recent channel activity (the deterministic no-embeddings degrade
//!   signal).
//! - `GET /api/workspaces/:id/recommendations/people` — workspace members the
//!   caller doesn't already follow (and isn't), ranked by shared-room overlap
//!   ([`RoomRepo::shared_room_counts_in_workspace`](aero_storage::RoomRepo::shared_room_counts_in_workspace)
//!   minus the [`StreamFollowRepo::following`](aero_storage::StreamFollowRepo::following)
//!   set).
//!
//! Both are workspace-member-gated (`403` for a non-member, mirroring
//! [`crate::find_expert`] / [`crate::workspace_ask`]). The ranking lives behind the
//! [`AiBackend`](crate::state::AiBackend) seam as a pure deterministic aggregation
//! so it never errors and is smoke-verifiable as `200`-with-output even without an
//! LLM/embeddings key (`502` only when no AI backend is wired at all). Mounted via
//! [`routes`] and `.merge`d into the main router.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, WorkspaceId};
use aero_storage::{DirectoryRepo, RoomRepo, StreamFollowRepo};
use axum::{
    extract::{Path, Query, State},
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// The recommendations routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/workspaces/:id/recommendations/channels",
            get(recommend_channels),
        )
        .route(
            "/api/workspaces/:id/recommendations/people",
            get(recommend_people),
        )
}

/// Default number of recommendations returned when the caller omits `k`.
const DEFAULT_K: usize = 5;
/// Hard ceiling on how many recommendations are returned.
const MAX_K: usize = 25;
/// Activity window (days) used to score channel recency. Recent enough that a
/// once-busy-now-dead channel doesn't outrank a currently-active one.
const ACTIVITY_WINDOW_DAYS: i64 = 30;

/// Clamp a requested rec count into `[1, MAX_K]`, defaulting to [`DEFAULT_K`] when
/// absent. Pure, so the cap/floor is unit-tested offline.
#[must_use]
fn rec_k(requested: Option<usize>) -> usize {
    requested.unwrap_or(DEFAULT_K).clamp(1, MAX_K)
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

/// Assert the caller is a member of the workspace, rejecting non-members with a
/// `403`. Mirrors [`crate::find_expert`]'s membership gate.
async fn assert_member(
    s: &AppState,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    s.workspaces
        .effective_member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))?;
    Ok(())
}

#[derive(Deserialize)]
struct RecQuery {
    /// Optional number of recommendations to return; absent ⇒ [`DEFAULT_K`],
    /// clamped into `[1, MAX_K]`.
    #[serde(default)]
    k: Option<usize>,
}

/// `GET /api/workspaces/:id/recommendations/channels` — suggest channels the
/// caller isn't in, ranked by recent activity. Members only (`403` otherwise).
/// Returns `{ "channels": [{ "room_id", "name", "score", "reason" }, ...] }`
/// strongest-first. Degrades to a (possibly empty) ranked list — never an error;
/// `502` only when no AI backend is wired.
async fn recommend_channels(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Query(q): Query<RecQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_member(&s, ws, auth.participant_id).await?;
    let k = rec_k(q.k);

    let ai =
        s.ai.as_ref()
            .ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;

    let rooms = RoomRepo::new(s.pg.clone());
    let candidates: Vec<(aero_common::RoomId, String, i64)> = rooms
        .list_workspace_channels_not_member(ws, auth.participant_id, ACTIVITY_WINDOW_DAYS)
        .await
        .map_err(AeroError::from)?
        .into_iter()
        .map(|(room, activity)| {
            // A room may be unnamed; the ranker substitutes a fallback label.
            (room.id, room.name.unwrap_or_default(), activity)
        })
        .collect();

    let recs = ai
        .recommend_channels(candidates, k)
        .await
        .map_err(|e| AeroError::Upstream(format!("ai: {e}")))?;

    let channels: Vec<serde_json::Value> = recs
        .into_iter()
        .map(|c| {
            serde_json::json!({
                "room_id": c.room,
                "name": c.name,
                "score": c.score,
                "reason": c.reason,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({ "channels": channels })))
}

/// `GET /api/workspaces/:id/recommendations/people` — suggest workspace members
/// the caller doesn't already follow, ranked by shared-room overlap. Members only
/// (`403` otherwise). Returns
/// `{ "people": [{ "participant_id", "display_name", "score", "reason" }, ...] }`
/// strongest-first. Degrades to a (possibly empty) ranked list — never an error.
async fn recommend_people(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Query(q): Query<RecQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_member(&s, ws, auth.participant_id).await?;
    let k = rec_k(q.k);

    let ai =
        s.ai.as_ref()
            .ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;

    let rooms = RoomRepo::new(s.pg.clone());
    let follows = StreamFollowRepo::new(s.pg.clone());

    // Members sharing ≥1 room with the caller, by shared-room count (the caller
    // is already excluded by the query).
    let shared = rooms
        .shared_room_counts_in_workspace(ws, auth.participant_id)
        .await
        .map_err(AeroError::from)?;

    // Drop anyone the caller already follows — recommend only NEW people.
    let already_following: HashSet<ParticipantId> = follows
        .following(auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .into_iter()
        .collect();
    let candidates: Vec<(ParticipantId, i64)> = shared
        .into_iter()
        .filter(|(p, _)| !already_following.contains(p))
        .collect();

    let recs = ai
        .recommend_people(candidates, k)
        .await
        .map_err(|e| AeroError::Upstream(format!("ai: {e}")))?;

    // Resolve display names in one workspace-directory read rather than N point
    // lookups (the directory's `JOIN workspace_members` keeps it tenant-scoped).
    let names: HashMap<ParticipantId, String> = DirectoryRepo::new(s.pg.clone())
        .search(ws, None, None, 200, 0)
        .await
        .map_err(AeroError::from)?
        .into_iter()
        .map(|e| (e.participant_id, e.display_name))
        .collect();

    let people: Vec<serde_json::Value> = recs
        .into_iter()
        .map(|p| {
            let display_name = names
                .get(&p.participant)
                .cloned()
                .unwrap_or_else(|| "(未知成员)".to_owned());
            serde_json::json!({
                "participant_id": p.participant,
                "display_name": display_name,
                "score": p.score,
                "reason": p.reason,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({ "people": people })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure rec-count logic: an absent `k` defaults to [`DEFAULT_K`]; a present
    /// value passes through until it hits the cap, then saturates at [`MAX_K`], and
    /// a zero floors to `1`.
    #[test]
    fn rec_k_clamps_into_bounds() {
        assert_eq!(rec_k(None), DEFAULT_K);
        assert_eq!(rec_k(Some(0)), 1);
        assert_eq!(rec_k(Some(1)), 1);
        assert_eq!(rec_k(Some(5)), 5);
        assert_eq!(rec_k(Some(25)), MAX_K);
        assert_eq!(rec_k(Some(10_000)), MAX_K);
    }

    #[test]
    fn parse_workspace_trims_and_rejects_garbage() {
        let ws = WorkspaceId::new();
        let round = parse_workspace(&format!("  {ws}  ")).expect("valid id with surrounding ws");
        assert_eq!(round, ws);
        assert!(parse_workspace("not-a-uuid").is_err());
    }
}
