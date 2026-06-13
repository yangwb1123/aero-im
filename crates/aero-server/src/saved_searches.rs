//! Saved searches — per-user, workspace-scoped named queries.
//!
//! A user saves a named search query within a workspace, then lists, re-runs, or
//! deletes it. Running a saved search reuses the SAME membership-scoped
//! cross-room search path as [`crate::search`]
//! ([`MessageRepo::search_all_rooms_in_workspace`](aero_storage::MessageRepo)),
//! whose `JOIN room_members` is the security boundary — so a saved search can
//! never surface a room the owner isn't in, and the stored workspace scopes the
//! results to that one tenant.
//!
//! Thin handlers over [`SavedSearchRepo`](aero_storage::SavedSearchRepo): create
//! and list assert the caller is a member of the workspace (via the shared
//! [`WorkspaceRepo`](aero_storage::WorkspaceRepo), mirroring
//! [`crate::scheduled_streams`]); delete and run are owner-scoped at the SQL
//! layer (a non-owner's id resolves to `None`/`false` ⇒ `404`). Mounted via
//! [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, SavedSearchId, WorkspaceId};
use aero_storage::SavedSearchRepo;
use axum::{
    extract::{Path, Query, State},
    routing::{delete, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All saved-search routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/workspaces/:id/saved-searches",
            post(create_saved_search).get(list_saved_searches),
        )
        .route("/api/saved-searches/:sid", delete(delete_saved_search))
        .route("/api/saved-searches/:sid/run", post(run_saved_search))
}

/// Default number of hits a saved-search run returns (mirrors [`crate::search`]).
const RUN_LIMIT: i64 = 20;

/// Build a [`SavedSearchRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> SavedSearchRepo {
    SavedSearchRepo::new(s.pg.clone())
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn parse_saved_search(s: &str) -> Result<SavedSearchId, AeroError> {
    SavedSearchId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("saved search id: {e}")))
}

/// Assert the caller is a member of the workspace, rejecting non-members with a
/// `403`. Mirrors `crate::scheduled_streams::assert_member`.
async fn assert_member(
    s: &AppState,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    s.workspaces
        .member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))?;
    Ok(())
}

#[derive(Deserialize)]
struct CreateSavedSearchReq {
    /// Human-readable name for the saved search.
    name: String,
    /// The query string to persist (re-run later through the scoped search).
    query: String,
}

/// `POST /api/workspaces/:id/saved-searches` — save a named query for the caller
/// in this workspace. The caller must be a member; a blank name or query is
/// rejected `400`. Returns the created row.
async fn create_saved_search(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Json(req): Json<CreateSavedSearchReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_member(&s, ws, auth.participant_id).await?;

    let name = req.name.trim();
    if name.is_empty() {
        return Err(AeroError::Invalid("name must not be empty".into()).into());
    }
    if name.len() > 256 {
        return Err(AeroError::Invalid("name too long".into()).into());
    }
    let query = req.query.trim();
    if query.is_empty() {
        return Err(AeroError::Invalid("query must not be empty".into()).into());
    }

    let id = repo(&s)
        .create(auth.participant_id, ws, name, query)
        .await
        .map_err(AeroError::from)?;
    // Re-read so the response carries the full, canonical row (created_at).
    let row = repo(&s)
        .get(id, auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("saved search".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `GET /api/workspaces/:id/saved-searches` — the caller's saved searches in this
/// workspace, newest first. Members only; owner-scoped at the SQL layer.
async fn list_saved_searches(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_member(&s, ws, auth.participant_id).await?;
    let saved = repo(&s)
        .list_for(auth.participant_id, ws)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(saved).map_err(AeroError::from)?))
}

/// `DELETE /api/saved-searches/:sid` — delete one of the caller's own saved
/// searches. Owner-scoped: a `404` if it isn't the caller's row (someone else's
/// or unknown).
async fn delete_saved_search(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_saved_search(&id_str)?;
    let removed = repo(&s)
        .delete(id, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    if !removed {
        return Err(AeroError::NotFound(format!("saved search {id}")).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// Query params for [`run_saved_search`].
#[derive(Deserialize)]
struct RunQuery {
    /// When `true`, return only matches created since this saved search was last
    /// run — the "new since I last looked" delta. Defaults to `false` (all hits).
    #[serde(default)]
    only_new: bool,
}

/// `POST /api/saved-searches/:sid/run` — load the saved query (owner-scoped) and
/// execute it through the SAME membership-scoped search path [`crate::search`]
/// uses, restricted to the saved search's workspace. `404` if the saved search
/// isn't the caller's. The result shape mirrors `POST /api/search`.
///
/// `?only_new=true` returns only matches created since the previous run (using
/// the saved search's `last_run_at` cursor). Note: the delta is applied to the
/// relevance-ranked top [`RUN_LIMIT`] hits, so it surfaces the newest *relevant*
/// matches, not an exhaustive change-log.
async fn run_saved_search(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<RunQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_saved_search(&id_str)?;
    let saved = repo(&s)
        .get(id, auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("saved search {id}")))?;

    // Reuse the exact membership-scoped search call `crate::search` uses (the
    // workspace-scoped variant), so results stay bounded to rooms the caller
    // belongs to within the saved search's workspace. The repo's `JOIN
    // room_members` is the security boundary — there is no post-filter.
    let mut hits = s
        .messages
        .search_all_rooms_in_workspace(
            auth.participant_id,
            saved.workspace_id,
            &saved.query,
            RUN_LIMIT,
        )
        .await
        .map_err(AeroError::from)?;

    // Stamp this run and capture the previous run's timestamp — the cursor for the
    // "new since last run" delta (ROADMAP5 方向三).
    let previous_run_at = repo(&s)
        .mark_run(id, auth.participant_id, time::OffsetDateTime::now_utc())
        .await
        .map_err(AeroError::from)?;
    if q.only_new {
        // First-ever run has no cursor — everything is "new", so no filter applies.
        if let Some(cursor) = previous_run_at {
            hits.retain(|h| h.message.created_at > cursor);
        }
    }

    let previous_run_str =
        previous_run_at.and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok());
    Ok(Json(serde_json::json!({
        "saved_search_id": saved.id,
        "name": saved.name,
        "query": saved.query,
        "only_new": q.only_new,
        "previous_run_at": previous_run_str,
        "results": hits.into_iter().map(|h| {
            serde_json::json!({
                "score": h.score,
                "message": h.message,
            })
        }).collect::<Vec<_>>(),
    })))
}
