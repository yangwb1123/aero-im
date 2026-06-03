//! Workspace custom-emoji HTTP API.
//!
//! Additive layer over the new [`aero_storage::EmojiRepo`]: a workspace defines
//! named custom emoji (`:shipit:`) backed by an already-uploaded image blob
//! (clients upload via the existing `POST /api/blobs`, then register the emoji
//! by `blob_id`). Members reference emoji by name in messages/reactions; the
//! client resolves name → image URL, served by the existing `GET /api/blobs/:id`.
//!
//! ## Authorization
//!
//! Everything is tenant-scoped. Listing and creation require workspace
//! membership; **any** member may create an emoji. Deletion is allowed only for
//! the emoji's creator **or** a workspace admin/owner — a pure, DB-free decision
//! ([`authorize_delete_emoji`]) so the role/ownership matrix unit-tests offline,
//! mirroring `crate::workspaces`. The async handlers are a thin shell: resolve
//! ids + the caller's workspace role, run the pure guard, then the repo call.

use std::str::FromStr;

use aero_common::{
    BlobId, EmojiId, Error as AeroError, ParticipantId, Result as AeroResult, WorkspaceId,
    WorkspaceRole,
};
use aero_storage::{is_valid_emoji_name, EmojiRepo, WorkspaceRepo};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use aero_auth::AuthUser;

use crate::error::ApiResult;
use crate::state::AppState;

// ---------- Router ----------

/// Mount the custom-emoji routes. Folded into the main router by
/// [`crate::routes::build`]; kept separate so the route table lives next to the
/// pure authorization logic it enforces.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/workspaces/:id/emoji",
            get(list_emoji).post(create_emoji),
        )
        .route("/api/emoji/:id", axum::routing::delete(delete_emoji))
}

// ---------- Pure authorization (DB-free, unit-tested) ----------

/// May `caller` delete an emoji created by `creator`?
///
/// Allowed when the caller is the creator (self-cleanup) **or** holds an
/// administrative workspace role ([`WorkspaceRole::can_administer`] — admin or
/// owner). An emoji whose `creator` is `None` (the original creator was removed
/// from the workspace) can only be deleted by an admin. Failure maps to `403`.
///
/// # Errors
/// [`AeroError::Forbidden`] when the caller is neither the creator nor an admin.
pub fn authorize_delete_emoji(
    caller: ParticipantId,
    caller_role: WorkspaceRole,
    creator: Option<ParticipantId>,
) -> AeroResult<()> {
    if creator == Some(caller) || caller_role.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden("only the creator or a workspace admin may delete this emoji".into()))
    }
}

// ---------- Shared helpers ----------

fn parse_workspace_id(s: &str) -> AeroResult<WorkspaceId> {
    WorkspaceId::from_str(s).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn parse_emoji_id(s: &str) -> AeroResult<EmojiId> {
    EmojiId::from_str(s).map_err(|e| AeroError::Invalid(format!("emoji id: {e}")))
}

fn parse_blob_id(s: &str) -> AeroResult<BlobId> {
    BlobId::from_str(s).map_err(|e| AeroError::Invalid(format!("blob id: {e}")))
}

/// Resolve the caller's role in a workspace, rejecting non-members with `403`.
/// Mirrors `crate::workspaces::caller_role` so the "must be a member" check is
/// consistent across feature modules.
async fn caller_role(
    repo: &WorkspaceRepo,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> AeroResult<WorkspaceRole> {
    repo.member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))
}

// ---------- Handlers ----------

/// `GET /api/workspaces/:id/emoji` — list a workspace's custom emoji. Caller
/// must be a workspace member.
async fn list_emoji(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace_id(&id_str)?;
    // Membership gate: resolving the caller's role rejects non-members with 403.
    caller_role(&s.workspaces, ws, auth.participant_id).await?;
    let repo = EmojiRepo::new(s.participants.pool().clone());
    let emoji = repo.list(ws).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(emoji).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct CreateEmojiReq {
    name: String,
    /// The id of an already-uploaded image blob (via `POST /api/blobs`).
    blob_id: String,
}

/// `POST /api/workspaces/:id/emoji` — register a custom emoji. Caller must be a
/// workspace member (any member may create). Validates the name, then `409`s on
/// a duplicate `(workspace, name)`; on success returns `201` with the new emoji.
async fn create_emoji(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<CreateEmojiReq>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let ws = parse_workspace_id(&id_str)?;
    // Any workspace member may create an emoji; non-members are rejected (403).
    caller_role(&s.workspaces, ws, auth.participant_id).await?;
    let name = req.name.trim();
    if !is_valid_emoji_name(name) {
        return Err(AeroError::Invalid(
            "emoji name must be 1-64 chars of lowercase [a-z0-9_-]".into(),
        )
        .into());
    }
    let blob_id = parse_blob_id(req.blob_id.trim())?;
    let repo = EmojiRepo::new(s.participants.pool().clone());
    let created = repo
        .create(ws, name, blob_id, auth.participant_id)
        .await
        .map_err(AeroError::from)?
        // `None` means the (workspace, name) pair already exists.
        .ok_or_else(|| AeroError::Conflict(format!("emoji ':{name}:' already exists")))?;
    // Re-read the row so the response carries the full record (created_at, etc.).
    let emoji = repo
        .get(created)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("emoji".into()))?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(emoji).map_err(AeroError::from)?),
    ))
}

/// `DELETE /api/emoji/:id` — delete a custom emoji. Allowed for the emoji's
/// creator or a workspace admin/owner; the caller must also belong to the
/// emoji's workspace (resolving their role there enforces this).
async fn delete_emoji(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<StatusCode> {
    let id = parse_emoji_id(&id_str)?;
    let repo = EmojiRepo::new(s.participants.pool().clone());
    let emoji = repo
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("emoji".into()))?;
    // The caller must belong to the emoji's workspace; their role there also
    // decides whether the admin branch of the delete guard applies.
    let role = caller_role(&s.workspaces, emoji.workspace_id, auth.participant_id).await?;
    authorize_delete_emoji(auth.participant_id, role, emoji.created_by)?;
    let deleted = repo.delete(id).await.map_err(AeroError::from)?;
    if !deleted {
        // Raced with another deleter between the fetch and the delete.
        return Err(AeroError::NotFound("emoji".into()).into());
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [WorkspaceRole; 4] = [
        WorkspaceRole::Guest,
        WorkspaceRole::Member,
        WorkspaceRole::Admin,
        WorkspaceRole::Owner,
    ];

    /// HTTP status a guard's error maps to (or 200 on `Ok`).
    fn status_of(r: &AeroResult<()>) -> u16 {
        r.as_ref().err().map_or(200, AeroError::status_code)
    }

    fn allowed(r: &AeroResult<()>) -> bool {
        r.is_ok()
    }

    #[test]
    fn creator_can_always_delete_own_emoji_regardless_of_role() {
        let me = ParticipantId::new();
        for role in ALL {
            assert!(
                allowed(&authorize_delete_emoji(me, role, Some(me))),
                "creator may delete own emoji even as {role:?}"
            );
        }
    }

    #[test]
    fn admins_and_owners_can_delete_anyone_elses_emoji() {
        let me = ParticipantId::new();
        let other = ParticipantId::new();
        assert!(allowed(&authorize_delete_emoji(me, WorkspaceRole::Admin, Some(other))));
        assert!(allowed(&authorize_delete_emoji(me, WorkspaceRole::Owner, Some(other))));
    }

    #[test]
    fn non_admin_non_creator_cannot_delete() {
        let me = ParticipantId::new();
        let other = ParticipantId::new();
        // A plain member or guest who didn't create it is forbidden.
        assert!(!allowed(&authorize_delete_emoji(me, WorkspaceRole::Member, Some(other))));
        assert!(!allowed(&authorize_delete_emoji(me, WorkspaceRole::Guest, Some(other))));
        // And the denial is a 403, not a 400/404.
        assert_eq!(
            status_of(&authorize_delete_emoji(me, WorkspaceRole::Member, Some(other))),
            403
        );
    }

    #[test]
    fn orphaned_emoji_creator_none_only_admin_can_delete() {
        let me = ParticipantId::new();
        // creator == None (original creator removed): only admins/owners qualify.
        for role in ALL {
            assert_eq!(
                allowed(&authorize_delete_emoji(me, role, None)),
                role.can_administer(),
                "orphaned emoji deletable only by admin, role {role:?}"
            );
        }
    }

    #[test]
    fn delete_guard_matches_creator_or_can_administer() {
        // Exhaustive: the guard is exactly "is creator OR can_administer" for
        // every (role, creator) shape, so it never drifts from that intent.
        let me = ParticipantId::new();
        let other = ParticipantId::new();
        for role in ALL {
            for creator in [Some(me), Some(other), None] {
                let expected = creator == Some(me) || role.can_administer();
                assert_eq!(
                    allowed(&authorize_delete_emoji(me, role, creator)),
                    expected,
                    "role {role:?} creator {creator:?}"
                );
            }
        }
    }
}
