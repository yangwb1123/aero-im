//! People directory — a searchable, read-only member list for a workspace.
//!
//! Surfaces each workspace member's identity (`participants`) plus their
//! self-service profile fields (`participant_profiles`, 0035): title, pronouns,
//! timezone. Read-only — there is no migration and no mutation; the handler just
//! projects [`DirectoryRepo::search`](aero_storage::DirectoryRepo), optionally
//! narrowed by a display-name (`?q=`) and/or job-title (`?title=`) substring, and
//! paged with `?limit=`/`?offset=`.
//!
//! Membership-gated: the caller must be a member of the workspace (any role) to
//! read its directory — a non-member is rejected `403`, mirroring
//! [`crate::saved_searches::assert_member`]. The repo's `JOIN workspace_members`
//! is the scope boundary, so the result can only ever contain members of that one
//! tenant. Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, WorkspaceId};
use aero_storage::DirectoryRepo;
use axum::{
    extract::{Path, Query, State},
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All people-directory routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/workspaces/:id/directory", get(list_directory))
}

/// Default directory page size when the client omits `?limit=`.
const DEFAULT_LIMIT: i64 = 50;

/// Build a [`DirectoryRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> DirectoryRepo {
    DirectoryRepo::new(s.pg.clone())
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

/// Assert the caller is a member of the workspace, rejecting non-members with a
/// `403`. Mirrors [`crate::saved_searches::assert_member`].
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
struct DirectoryQuery {
    /// Optional display-name substring filter.
    #[serde(default)]
    q: Option<String>,
    /// Optional job-title substring filter.
    #[serde(default)]
    title: Option<String>,
    /// Page size (clamped `[1, 200]` in the repo). Absent ⇒ [`DEFAULT_LIMIT`].
    #[serde(default)]
    limit: Option<i64>,
    /// Page offset (floored at `0` in the repo). Absent ⇒ `0`.
    #[serde(default)]
    offset: Option<i64>,
}

/// `GET /api/workspaces/:id/directory?q=&title=&limit=&offset=` — the workspace's
/// member directory, ordered by display name. Members only (any role); a
/// non-member is rejected `403`. The optional `q`/`title` substrings narrow the
/// result; `limit`/`offset` page it.
async fn list_directory(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Query(q): Query<DirectoryQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_member(&s, ws, auth.participant_id).await?;

    let entries = repo(&s)
        .search(
            ws,
            q.q.as_deref(),
            q.title.as_deref(),
            q.limit.unwrap_or(DEFAULT_LIMIT),
            q.offset.unwrap_or(0),
        )
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(entries).map_err(AeroError::from)?))
}
