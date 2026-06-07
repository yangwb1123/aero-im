//! User groups / @-usergroups — workspace-scoped named groups of members.
//!
//! A workspace member creates a named group (`@designers`) of participants, lists
//! and resolves groups, and (as an admin or the group's creator) deletes a group
//! or edits its membership. Handles are unique within a workspace, so an
//! `@handle` mention resolves to at most one group — the fan-out of such a mention
//! into notifications is wired separately and reuses
//! [`UserGroupRepo::resolve`](aero_storage::UserGroupRepo)/`members` as its seam;
//! this module builds only the CRUD + membership surface.
//!
//! Thin handlers over [`UserGroupRepo`](aero_storage::UserGroupRepo): create/list
//! assert the caller is a member of the workspace (via the shared
//! [`WorkspaceRepo`](aero_storage::WorkspaceRepo), mirroring
//! [`crate::channel_sections`]); the management routes (delete / add+remove
//! member) additionally require the caller to be a workspace admin/owner OR the
//! group's creator ([`can_manage`]). Mounted via [`routes`] and `.merge`d into the
//! main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{
    Error as AeroError, ParticipantId, UserGroupId, WorkspaceId, WorkspaceRole,
};
use aero_storage::{UserGroup, UserGroupRepo};
use axum::{
    extract::{Path, State},
    routing::{get, put},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All user-group routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/workspaces/:id/user-groups",
            get(list_user_groups).post(create_user_group),
        )
        .route("/api/user-groups/:gid", get(get_user_group))
        .route(
            "/api/workspaces/:id/user-groups/:gid",
            axum::routing::delete(delete_user_group),
        )
        .route(
            "/api/workspaces/:id/user-groups/:gid/members/:pid",
            put(add_member).delete(remove_member),
        )
}

/// Build a [`UserGroupRepo`] from shared state, over the shared pool. Cheap (a
/// clone of an `Arc<PgPool>`), keeping this feature self-contained.
fn repo(s: &AppState) -> UserGroupRepo {
    UserGroupRepo::new(s.pg.clone())
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn parse_group(s: &str) -> Result<UserGroupId, AeroError> {
    UserGroupId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("user group id: {e}")))
}

fn parse_participant(s: &str) -> Result<ParticipantId, AeroError> {
    ParticipantId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
}

/// True iff a `sqlx` error is a unique-constraint violation — used to map a
/// duplicate `(workspace, handle)` insert to a `409`. Mirrors
/// `crate::scim::is_unique_violation`.
fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_unique_violation())
}

/// Maximum handle length, in characters (post-normalization).
const MAX_HANDLE_LEN: usize = 32;
/// Maximum display-name length, in bytes.
const MAX_NAME_LEN: usize = 64;

/// Validate + canonicalize a raw handle: normalized (lowercased/trimmed) it must
/// be `1..=32` chars of `[a-z0-9_-]`. Pure (no I/O) so the rule is unit-tested
/// offline; returns the normalized handle to store.
///
/// # Errors
/// [`AeroError::Invalid`] when the normalized handle is empty, too long, or
/// contains a disallowed character.
fn validate_handle(raw: &str) -> Result<String, AeroError> {
    let handle = aero_storage::normalize_handle(raw);
    if handle.is_empty() || handle.chars().count() > MAX_HANDLE_LEN {
        return Err(AeroError::Invalid(format!(
            "handle must be 1..={MAX_HANDLE_LEN} characters"
        )));
    }
    if !handle
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err(AeroError::Invalid(
            "handle may contain only [a-z0-9_-]".into(),
        ));
    }
    Ok(handle)
}

/// Resolve the caller's role in a workspace, rejecting non-members with a `403`.
/// Mirrors `crate::guests::caller_role`; returns the role so management routes can
/// gate on admin/owner.
///
/// # Errors
/// [`AeroError::Forbidden`] when the caller is not a member of the workspace.
async fn require_member(
    s: &AppState,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> Result<WorkspaceRole, AeroError> {
    s.workspaces
        .member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))
}

/// May `caller` manage (delete / edit membership of) the group? Workspace admins
/// and owners may manage any group; otherwise only the group's creator may.
/// Pure (DB-free) so it is unit-tested directly.
#[must_use]
fn can_manage(role: WorkspaceRole, creator: ParticipantId, caller: ParticipantId) -> bool {
    role.can_administer() || creator == caller
}

/// Fetch a group, asserting it exists *in this workspace* (else `404`). Shared by
/// the management routes so a group id from another tenant can never be touched.
async fn group_in_workspace(
    s: &AppState,
    workspace: WorkspaceId,
    group: UserGroupId,
) -> Result<UserGroup, AeroError> {
    let g = repo(s)
        .get(group)
        .await
        .map_err(AeroError::from)?
        .filter(|g| g.workspace_id == workspace)
        .ok_or_else(|| AeroError::NotFound(format!("user group {group}")))?;
    Ok(g)
}

#[derive(Deserialize)]
struct CreateUserGroupReq {
    /// The group's mention handle (`designers`); normalized + validated.
    handle: String,
    /// The group's human-readable display name.
    name: String,
}

/// `POST /api/workspaces/:id/user-groups` — create a group in this workspace. The
/// caller must be a member; the handle must normalize to `1..=32` chars of
/// `[a-z0-9_-]` and the name to `1..=64` chars. A duplicate handle in the
/// workspace is rejected `409`. Returns the created row.
async fn create_user_group(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Json(req): Json<CreateUserGroupReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    require_member(&s, ws, auth.participant_id).await?;

    let handle = validate_handle(&req.handle)?;
    let name = req.name.trim();
    if name.is_empty() {
        return Err(AeroError::Invalid("name must not be empty".into()).into());
    }
    if name.len() > MAX_NAME_LEN {
        return Err(AeroError::Invalid("name too long".into()).into());
    }

    let id = match repo(&s).create(ws, &handle, name, auth.participant_id).await {
        Ok(id) => id,
        Err(e) if is_unique_violation(&e) => {
            return Err(
                AeroError::Conflict(format!("user group '@{handle}' already exists")).into(),
            )
        }
        Err(e) => return Err(AeroError::from(e).into()),
    };
    // Re-read so the response carries the full, canonical row (created_at).
    let row = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("user group".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `GET /api/workspaces/:id/user-groups` — every group in the workspace, by handle
/// ascending. Members only.
async fn list_user_groups(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    require_member(&s, ws, auth.participant_id).await?;
    let groups = repo(&s)
        .list_for_workspace(ws)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(groups).map_err(AeroError::from)?))
}

/// `GET /api/user-groups/:gid` — one group plus its member participant ids. The
/// caller must be a member of the group's workspace (`403` otherwise); an unknown
/// group is `404`.
async fn get_user_group(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(gid_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let gid = parse_group(&gid_str)?;
    let group = repo(&s)
        .get(gid)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("user group {gid}")))?;
    require_member(&s, group.workspace_id, auth.participant_id).await?;
    let members = repo(&s).members(gid).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({
        "group": group,
        "members": members,
    })))
}

/// `DELETE /api/workspaces/:id/user-groups/:gid` — delete a group (its memberships
/// cascade away). The caller must be a workspace admin/owner OR the group's
/// creator; otherwise `403`. A group not in this workspace is `404`.
async fn delete_user_group(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((ws_str, gid_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    let gid = parse_group(&gid_str)?;
    let role = require_member(&s, ws, auth.participant_id).await?;
    let group = group_in_workspace(&s, ws, gid).await?;
    if !can_manage(role, group.created_by, auth.participant_id) {
        return Err(AeroError::Forbidden("managing this user group requires admin or creator".into()).into());
    }
    let deleted = repo(&s).delete(gid, ws).await.map_err(AeroError::from)?;
    if !deleted {
        return Err(AeroError::NotFound(format!("user group {gid}")).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// `PUT /api/workspaces/:id/user-groups/:gid/members/:pid` — add a participant to
/// the group, idempotently. The caller must be a workspace admin/owner OR the
/// group's creator; otherwise `403`. A group not in this workspace is `404`.
async fn add_member(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((ws_str, gid_str, pid_str)): Path<(String, String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    let gid = parse_group(&gid_str)?;
    let member = parse_participant(&pid_str)?;
    let role = require_member(&s, ws, auth.participant_id).await?;
    let group = group_in_workspace(&s, ws, gid).await?;
    if !can_manage(role, group.created_by, auth.participant_id) {
        return Err(AeroError::Forbidden("managing this user group requires admin or creator".into()).into());
    }
    repo(&s)
        .add_member(gid, member)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "added": true })))
}

/// `DELETE /api/workspaces/:id/user-groups/:gid/members/:pid` — remove a
/// participant from the group. The caller must be a workspace admin/owner OR the
/// group's creator; otherwise `403`. `removed` reports whether a membership was
/// actually dropped. A group not in this workspace is `404`.
async fn remove_member(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((ws_str, gid_str, pid_str)): Path<(String, String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    let gid = parse_group(&gid_str)?;
    let member = parse_participant(&pid_str)?;
    let role = require_member(&s, ws, auth.participant_id).await?;
    let group = group_in_workspace(&s, ws, gid).await?;
    if !can_manage(role, group.created_by, auth.participant_id) {
        return Err(AeroError::Forbidden("managing this user group requires admin or creator".into()).into());
    }
    let removed = repo(&s)
        .remove_member(gid, member)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "removed": removed })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_handle_accepts_and_normalizes() {
        assert_eq!(validate_handle("  Designers ").unwrap(), "designers");
        assert_eq!(validate_handle("on-call_2").unwrap(), "on-call_2");
        // 32 chars is the inclusive maximum.
        let max = "a".repeat(MAX_HANDLE_LEN);
        assert_eq!(validate_handle(&max).unwrap(), max);
    }

    #[test]
    fn validate_handle_rejects_bad_input() {
        assert!(validate_handle("   ").is_err(), "empty after trim");
        assert!(validate_handle(&"a".repeat(MAX_HANDLE_LEN + 1)).is_err(), "too long");
        assert!(validate_handle("has space").is_err(), "space disallowed");
        assert!(validate_handle("emoji✨").is_err(), "non-ascii disallowed");
        assert!(validate_handle("dot.dot").is_err(), "dot disallowed");
    }

    #[test]
    fn can_manage_admins_or_creator_only() {
        let creator = ParticipantId::new();
        let other = ParticipantId::new();
        // Workspace admins/owners can manage any group.
        assert!(can_manage(WorkspaceRole::Owner, creator, other));
        assert!(can_manage(WorkspaceRole::Admin, creator, other));
        // A plain member can manage only a group they created.
        assert!(can_manage(WorkspaceRole::Member, creator, creator));
        assert!(!can_manage(WorkspaceRole::Member, creator, other));
        assert!(!can_manage(WorkspaceRole::Guest, creator, other));
    }
}
