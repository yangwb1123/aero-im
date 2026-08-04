//! Message-report HTTP surface — user-initiated reports + workspace moderation
//! review queue.
//!
//! The AI moderation pipeline ([`crate::moderation_bot`]) auto-soft-deletes flagged
//! messages async, with no human in the loop. This module adds the missing
//! human-review slice: a ROOM MEMBER can REPORT a message, the report lands in a
//! per-workspace `pending` queue, and a workspace ADMIN reviews it — KEEP (dismiss)
//! or REMOVE (soft-delete the message). Three routes:
//!
//! - `POST /api/rooms/:id/messages/:mid/report` — any room member files a report.
//!   Resolves the room's workspace, asserts room access (you can only report a
//!   message in a room you can see), validates the message belongs to that room and
//!   still exists, then inserts the report.
//! - `GET  /api/workspaces/:id/admin/moderation-queue` — admin/owner lists the
//!   pending reports (oldest first).
//! - `POST /api/workspaces/:id/admin/moderation-queue/:rid/review` — admin/owner
//!   decides `keep` | `remove`. Storage commits the review transition, optional
//!   message tombstone, audit row, attachment cleanup, and deleted-event outbox
//!   atomically.
//!
//! Note: this does NOT add pre-visibility gating — messages are already broadcast
//! before async screening, so a report removes a message that was already seen,
//! exactly like the AI pipeline. The repo lives in
//! [`MessageReportRepo`](aero_storage::MessageReportRepo); it is built inline from
//! the shared pool (no dedicated `AppState` field), mirroring [`crate::interactions`].
//! Admin gating mirrors [`crate::analytics`] (`WorkspaceRole::can_administer`).
//! Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{
    Error as AeroError, MessageId, MessageReportId, ParticipantId, Result as AeroResult, RoomId,
    WorkspaceId, WorkspaceRole,
};
use aero_storage::MessageReportRepo;
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All message-report + moderation-queue routes, ready to `.merge` into the router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/rooms/:id/messages/:mid/report", post(report_message))
        .route(
            "/api/workspaces/:id/admin/moderation-queue",
            get(list_queue),
        )
        .route(
            "/api/workspaces/:id/admin/moderation-queue/:rid/review",
            post(review_report),
        )
}

/// The message-report repo over the shared pool. `AppState` carries no dedicated
/// field for it, so we build it inline (cheap — a `PgPool` clone), keeping this
/// feature self-contained and `AppState` untouched (mirrors [`crate::interactions`]).
fn repo(s: &AppState) -> MessageReportRepo {
    MessageReportRepo::new(s.participants.pool().clone())
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

fn parse_message(s: &str) -> Result<MessageId, AeroError> {
    MessageId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("message id: {e}")))
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn parse_report(s: &str) -> Result<MessageReportId, AeroError> {
    MessageReportId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("report id: {e}")))
}

/// May `caller` administer a workspace's moderation queue? **Admin or owner only** —
/// the same bar as analytics / audit / retention. Gates on
/// [`WorkspaceRole::can_administer`].
///
/// # Errors
/// [`AeroError::Forbidden`] for non-administrators (members / guests).
pub fn authorize_admin(caller: WorkspaceRole) -> AeroResult<()> {
    if caller.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden(
            "moderation queue requires admin".into(),
        ))
    }
}

/// Resolve the caller's workspace role and assert they may administer it. Mirrors
/// [`crate::analytics::assert_admin`]: rejects non-members (`403`) and non-admins
/// (`403`).
async fn assert_admin(
    s: &AppState,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    let role = s
        .workspaces
        .effective_member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))?;
    authorize_admin(role)
}

/// Body of `POST /api/rooms/:id/messages/:mid/report`: the reporter's free-text
/// reason.
#[derive(Deserialize)]
struct ReportReq {
    /// Why the reporter is flagging the message (required, non-empty).
    reason: String,
}

/// `POST /api/rooms/:id/messages/:mid/report` — a room member flags a message for
/// moderator review. Asserts the caller may access the room (you can only report a
/// message in a room you can see), resolves the room's workspace, validates the
/// message exists, is not already removed, and actually lives in this room, then
/// inserts a `pending` report. Returns the filed report. 404 if the message is
/// unknown / already deleted / not in this room; 400 on an empty reason.
async fn report_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((room_str, msg_str)): Path<(String, String)>,
    Json(req): Json<ReportReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    let message = parse_message(&msg_str)?;
    let reason = req.reason.trim();
    if reason.is_empty() {
        return Err(AeroError::Invalid("reason is empty".into()).into());
    }

    // Fast preflight for a clear response and authz-lint visibility. The storage
    // method repeats this under the canonical workspace/room/membership/message
    // locks and owns the definitive decision through commit.
    s.im.assert_room_access(auth.participant_id, room).await?;
    let filed = repo(&s)
        .report_authorized(room, message, auth.participant_id, reason)
        .await
        .map_err(map_report_error)?;
    Ok(Json(serde_json::to_value(filed).map_err(AeroError::from)?))
}

/// `GET /api/workspaces/:id/admin/moderation-queue` — the workspace's pending
/// reports, oldest first. Admin/owner only.
async fn list_queue(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_admin(&s, ws, auth.participant_id).await?;
    let reports = repo(&s)
        .list_pending_authorized(ws, auth.participant_id)
        .await
        .map_err(map_review_error)?;
    Ok(Json(serde_json::json!({ "reports": reports })))
}

/// Body of the review route: the administrator's decision plus an optional note.
#[derive(Deserialize)]
struct ReviewReq {
    /// `"keep"` (dismiss the report) or `"remove"` (also soft-delete the message).
    decision: String,
    /// Optional reviewer note recorded with the decision.
    #[serde(default)]
    note: Option<String>,
}

/// `POST /api/workspaces/:id/admin/moderation-queue/:rid/review` — decide a pending
/// report. Admin/owner only. The repository rechecks current effective admin
/// access and commits the decision plus optional message deletion, audit, blob
/// cleanup, and durable event outbox in one transaction. A double-submit sees no
/// pending report and cannot re-fire deletion.
///
/// 404 if the report is unknown or already reviewed; 400 on an unknown `decision`.
async fn review_report(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((ws_str, rid_str)): Path<(String, String)>,
    Json(req): Json<ReviewReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_admin(&s, ws, auth.participant_id).await?;
    let rid = parse_report(&rid_str)?;

    let remove = match req.decision.trim() {
        "remove" => true,
        "keep" => false,
        other => {
            return Err(AeroError::Invalid(format!(
                "decision must be \"keep\" or \"remove\", got {other:?}"
            ))
            .into())
        }
    };

    let note = req.note.as_deref().map(str::trim).filter(|n| !n.is_empty());
    let traceparent = aero_common::telemetry::current_traceparent();
    let outcome = repo(&s)
        .review_authorized(
            rid,
            ws,
            auth.participant_id,
            remove,
            note,
            traceparent.as_deref(),
        )
        .await
        .map_err(map_review_error)?;

    // Commit is already durable. This is only the low-latency dispatch attempt;
    // the outbox dispatcher retries a failed publish without rolling back or
    // asking the reviewer to submit again.
    if let Some(outbox_id) = outcome.delete_outbox_id {
        if let Err(error) = s.im.dispatch_event_outbox_id(outbox_id).await {
            tracing::warn!(
                ?error,
                %outbox_id,
                report_id = %rid,
                "fast message-report deletion outbox dispatch failed"
            );
        }
    }
    Ok(Json(serde_json::json!({
        "reviewed": true,
        "report": outcome.report,
    })))
}

fn map_report_error(error: AeroError) -> AeroError {
    match error {
        AeroError::Forbidden(_) => {
            AeroError::Forbidden("current room access required to report a message".into())
        }
        AeroError::NotFound(_) => AeroError::NotFound("live message in requested room".into()),
        other => other,
    }
}

fn map_review_error(error: AeroError) -> AeroError {
    match error {
        AeroError::Forbidden(_) => {
            AeroError::Forbidden("current workspace admin access required".into())
        }
        AeroError::NotFound(_) => AeroError::NotFound("pending message report".into()),
        AeroError::Conflict(_) => {
            AeroError::Conflict("message report tenant binding changed".into())
        }
        other => other,
    }
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

    #[test]
    fn moderation_queue_is_admin_and_owner_only_over_all_roles() {
        // Exhaustive over the 4 roles: admin + owner may work the moderation queue;
        // member and guest may not. Tracks `can_administer` exactly, like the
        // analytics / audit gates.
        for r in ALL {
            assert_eq!(
                authorize_admin(r).is_ok(),
                r.can_administer(),
                "moderation queue allowed only for admin/owner, role {r:?}"
            );
        }
        assert!(authorize_admin(WorkspaceRole::Owner).is_ok());
        assert!(authorize_admin(WorkspaceRole::Admin).is_ok());
        assert!(authorize_admin(WorkspaceRole::Member).is_err());
        assert!(authorize_admin(WorkspaceRole::Guest).is_err());
    }

    #[test]
    fn moderation_queue_non_admin_denials_are_403() {
        for r in [WorkspaceRole::Guest, WorkspaceRole::Member] {
            assert_eq!(status_of(&authorize_admin(r)), 403, "role {r:?}");
        }
    }

    #[test]
    fn scope_and_revocation_errors_do_not_disclose_cross_tenant_details() {
        let report_missing =
            map_report_error(AeroError::NotFound("message belongs elsewhere".into()));
        assert_eq!(report_missing.status_code(), 404);
        assert_eq!(
            report_missing.to_string(),
            "not found: live message in requested room"
        );

        let review_missing =
            map_review_error(AeroError::NotFound("report belongs elsewhere".into()));
        assert_eq!(review_missing.status_code(), 404);
        assert_eq!(
            review_missing.to_string(),
            "not found: pending message report"
        );

        let revoked = map_review_error(AeroError::Forbidden("demoted after preflight".into()));
        assert_eq!(revoked.status_code(), 403);
        assert_eq!(
            revoked.to_string(),
            "forbidden: current workspace admin access required"
        );
    }
}
