//! Approvals workflow — Lark 审批 / approvals-lite (single-approver MVP).
//!
//! A requester opens an approval request addressed to a single approver within a
//! workspace (title + optional details); the named approver then approves or
//! denies it with an optional note. Thin handlers over
//! [`ApprovalRepo`](aero_storage::ApprovalRepo).
//!
//! Workspace-membership gated: opening a request and listing the caller's inbox /
//! outbox all assert the caller is a member of the workspace (via the shared
//! [`WorkspaceRepo`](aero_storage::WorkspaceRepo), mirroring
//! [`crate::saved_searches`]); opening additionally requires the named approver to
//! be a member too. Deciding is approver-scoped at the SQL layer (a non-approver,
//! an unknown id, or an already-decided request all resolve to `false` ⇒ `404`),
//! so only the named approver can ever flip a pending request. Mounted via
//! [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{ApprovalId, Error as AeroError, ParticipantId, WorkspaceId};
use aero_storage::ApprovalRepo;
use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All approval routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/workspaces/:id/approvals", post(create_approval))
        .route(
            "/api/workspaces/:id/approvals/incoming",
            get(list_incoming),
        )
        .route(
            "/api/workspaces/:id/approvals/outgoing",
            get(list_outgoing),
        )
        .route("/api/approvals/:aid/approve", post(approve_approval))
        .route("/api/approvals/:aid/deny", post(deny_approval))
}

/// Build an [`ApprovalRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> ApprovalRepo {
    ApprovalRepo::new(s.pg.clone())
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn parse_approval(s: &str) -> Result<ApprovalId, AeroError> {
    ApprovalId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("approval id: {e}")))
}

fn parse_participant(s: &str) -> Result<ParticipantId, AeroError> {
    ParticipantId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
}

/// Assert that `who` is a member of `workspace`, mapping a non-member to the given
/// error. Mirrors `crate::saved_searches::assert_member`.
async fn assert_member(
    s: &AppState,
    workspace: WorkspaceId,
    who: ParticipantId,
    on_missing: AeroError,
) -> Result<(), AeroError> {
    s.workspaces
        .member_role(workspace, who)
        .await
        .map_err(AeroError::from)?
        .ok_or(on_missing)?;
    Ok(())
}

#[derive(Deserialize)]
struct CreateApprovalReq {
    /// The single participant the request is addressed to.
    approver_id: String,
    /// Short human-readable title of what is being requested.
    title: String,
    /// Optional longer description.
    #[serde(default)]
    details: Option<String>,
}

/// `POST /api/workspaces/:id/approvals` — open an approval request in this
/// workspace, addressed to `approver_id` (requester = caller). Both the caller
/// and the named approver must be members of the workspace; a blank title is
/// rejected `400`. Returns the created row.
async fn create_approval(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Json(req): Json<CreateApprovalReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_member(
        &s,
        ws,
        auth.participant_id,
        AeroError::Forbidden("not a workspace member".into()),
    )
    .await?;

    let approver = parse_participant(&req.approver_id)?;
    // The named approver must also belong to the workspace, else the request could
    // never be acted on.
    assert_member(
        &s,
        ws,
        approver,
        AeroError::Invalid("approver is not a workspace member".into()),
    )
    .await?;

    let title = req.title.trim();
    if title.is_empty() {
        return Err(AeroError::Invalid("title must not be empty".into()).into());
    }
    if title.len() > 256 {
        return Err(AeroError::Invalid("title too long".into()).into());
    }
    let details = req
        .details
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty());

    let id = repo(&s)
        .create(ws, auth.participant_id, approver, title, details)
        .await
        .map_err(AeroError::from)?;
    // Re-read so the response carries the full, canonical row (status/created_at).
    let row = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("approval".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct IncomingQuery {
    /// Optional status filter (e.g. `pending`); absent ⇒ every incoming request.
    #[serde(default)]
    status: Option<String>,
}

/// `GET /api/workspaces/:id/approvals/incoming?status=` — approval requests
/// addressed TO the caller in this workspace (their inbox), newest first.
/// Members only; optionally filtered by `status`.
async fn list_incoming(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Query(q): Query<IncomingQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_member(
        &s,
        ws,
        auth.participant_id,
        AeroError::Forbidden("not a workspace member".into()),
    )
    .await?;
    let status = q.status.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let list = repo(&s)
        .list_for_approver(auth.participant_id, status)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(list).map_err(AeroError::from)?))
}

/// `GET /api/workspaces/:id/approvals/outgoing` — approval requests opened BY the
/// caller in this workspace (their outbox), newest first. Members only.
async fn list_outgoing(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_member(
        &s,
        ws,
        auth.participant_id,
        AeroError::Forbidden("not a workspace member".into()),
    )
    .await?;
    let list = repo(&s)
        .list_for_requester(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(list).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct DecideReq {
    /// Optional note the approver leaves with their decision.
    #[serde(default)]
    note: Option<String>,
}

/// Decide an approval request, applying `status` (`approved`/`denied`). The caller
/// must be the named approver and the request must still be pending — both are
/// enforced approver-scoped at the SQL layer, so any other case resolves to a
/// `404`. Returns the updated row.
async fn decide(
    s: &AppState,
    caller: ParticipantId,
    id: ApprovalId,
    status: &str,
    note: Option<&str>,
) -> ApiResult<Json<serde_json::Value>> {
    let note = note.map(str::trim).filter(|n| !n.is_empty());
    let changed = repo(s)
        .decide(id, caller, status, note)
        .await
        .map_err(AeroError::from)?;
    if !changed {
        // Either not the approver, unknown id, or already decided — all 404 so a
        // non-approver can't probe which approvals exist.
        return Err(AeroError::NotFound(format!("approval {id}")).into());
    }
    // Fetch the full approval row for the response and for the post-decision hook.
    let appr = repo(s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("approval {id}")))?;

    // ROADMAP 集成三: approval-approved → auto-create task (best-effort, non-blocking).
    // When an approval is approved, create a task in the requester's first room in the
    // approval's own workspace, so the approved action is tracked and actionable.
    // Workspace-scoped lookup (not the bare `rooms_for`) — the requester may belong to
    // rooms in more than one workspace, and picking an unscoped "first room" could leak
    // the task into an unrelated tenant.
    if status == "approved" {
        let rooms = s
            .rooms
            .rooms_for_in_workspace(appr.requester_id, appr.workspace_id)
            .await
            .map_err(|e| {
                tracing::warn!(error = ?e, "failed to look up requester rooms for task creation");
                AeroError::Internal(anyhow::anyhow!("room lookup"))
            })?;
        if let Some(first_room) = rooms.first() {
            let task_title = format!("[Approved] {}", appr.title);
            let task_id = aero_storage::TaskRepo::new(s.pg.clone())
                .create(
                    first_room.id,
                    appr.requester_id,
                    &task_title,
                    Some(appr.requester_id),
                    None,
                    None,
                )
                .await;
            match task_id {
                Ok(_) => tracing::info!(%id, "auto-created task from approval approval"),
                Err(e) => tracing::warn!(error = ?e, %id, "auto-create task from approval failed"),
            }
        } else {
            tracing::warn!(requester = %appr.requester_id, "no room found for task creation from approval");
        }
    }

    Ok(Json(serde_json::to_value(&appr).map_err(AeroError::from)?))
}

/// `POST /api/approvals/:aid/approve` — the named approver approves the request
/// (optionally with a note). `404` if the caller isn't the approver or it is no
/// longer pending.
async fn approve_approval(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<DecideReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_approval(&id_str)?;
    decide(&s, auth.participant_id, id, "approved", req.note.as_deref()).await
}

/// `POST /api/approvals/:aid/deny` — the named approver denies the request
/// (optionally with a note). `404` if the caller isn't the approver or it is no
/// longer pending.
async fn deny_approval(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<DecideReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_approval(&id_str)?;
    decide(&s, auth.participant_id, id, "denied", req.note.as_deref()).await
}
