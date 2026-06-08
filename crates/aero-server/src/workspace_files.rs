//! Workspace-wide file browser — list every file/media attachment shared across
//! ALL rooms the caller belongs to in a workspace.
//!
//! The workspace-wide complement to the per-channel [`crate::files`] tab. A thin,
//! read-only handler over [`WorkspaceFileRepo`](aero_storage::WorkspaceFileRepo),
//! which projects `File` blocks out of the existing `messages` table (no new
//! storage). The single route is workspace-member gated via
//! [`WorkspaceRepo::member_role`](aero_storage::WorkspaceRepo) (mirroring
//! [`crate::directory`]), but the real boundary is the repo's `JOIN room_members`:
//! a caller only ever sees files from rooms they are in within the workspace — so
//! the browser can never surface an attachment from a room they don't belong to.
//! Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, WorkspaceId};
use aero_storage::WorkspaceFileRepo;
use axum::{
    extract::{Path, Query, State},
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All workspace-file-browser routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/workspaces/:id/files", get(list_workspace_files))
}

/// Default number of files a listing returns when the client omits `?limit=`. The
/// storage repo applies the hard ceiling.
const DEFAULT_FILES_LIMIT: i64 = 50;

/// Build a [`WorkspaceFileRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> WorkspaceFileRepo {
    WorkspaceFileRepo::new(s.pg.clone())
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

/// Assert the caller is a member of the workspace, rejecting non-members with a
/// `403`. Mirrors [`crate::directory::assert_member`].
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
struct FilesQuery {
    /// Optional exact file-kind filter (`image` / `video` / `audio` / `document`
    /// / `other`).
    #[serde(default)]
    kind: Option<String>,
    /// Optional filename substring filter.
    #[serde(default)]
    q: Option<String>,
    /// Page size (clamped `[1, 200]` in the repo). Absent ⇒ [`DEFAULT_FILES_LIMIT`].
    #[serde(default)]
    limit: Option<i64>,
    /// Page offset (floored at `0` in the repo). Absent ⇒ `0`.
    #[serde(default)]
    offset: Option<i64>,
}

/// `GET /api/workspaces/:id/files?kind=&q=&limit=&offset=` — the file/media
/// attachments shared across every room the caller belongs to in this workspace,
/// newest-first. Members only (any role); a non-member is rejected `403`. The
/// repo's `JOIN room_members` is the real scope boundary, so only files from the
/// caller's rooms are returned. The optional `kind`/`q` filters narrow the result;
/// `limit`/`offset` page it. A malformed workspace id is `400`.
async fn list_workspace_files(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Query(q): Query<FilesQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_member(&s, ws, auth.participant_id).await?;

    let files = repo(&s)
        .list_for_workspace(
            ws,
            auth.participant_id,
            q.kind.as_deref(),
            q.q.as_deref(),
            q.limit.unwrap_or(DEFAULT_FILES_LIMIT),
            q.offset.unwrap_or(0),
        )
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "files": files })))
}
