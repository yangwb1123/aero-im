//! User-level report flow — HTTP surface.
//!
//! Any authenticated user can file a report against another participant. Workspace
//! admins list pending reports for their workspace and resolve them (mark
//! `"reviewed"` or `"dismissed"`). Thin handlers over
//! [`UserReportRepo`](aero_storage::UserReportRepo).

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, WorkspaceId, WorkspaceRole};
use aero_storage::UserReportRepo;
use axum::{
    extract::{Path, Query, State},
    routing::{get, patch, post},
    Json, Router,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::error::ApiResult;
use crate::state::AppState;

/// All user-report routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/users/:id/report", post(create_report))
        .route(
            "/api/workspaces/:id/user-reports",
            get(list_workspace_reports),
        )
        .route(
            "/api/workspaces/:id/user-reports/:rid",
            patch(resolve_report),
        )
}

fn repo(s: &AppState) -> UserReportRepo {
    UserReportRepo::new(s.pg.clone())
}

fn parse_participant(s: &str) -> Result<ParticipantId, AeroError> {
    ParticipantId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

async fn assert_admin(s: &AppState, auth: &AuthUser, workspace: WorkspaceId) -> ApiResult<()> {
    let role = s
        .workspaces
        .member_role(workspace, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    match role {
        Some(r) if r.at_least(WorkspaceRole::Admin) => Ok(()),
        _ => Err(AeroError::Forbidden("workspace admin required".into()).into()),
    }
}

#[derive(Deserialize)]
struct CreateReportReq {
    /// Optional workspace context for the report.
    #[serde(default)]
    workspace_id: Option<String>,
    /// Free-text reason for the report.
    #[serde(default)]
    reason: String,
}

/// `POST /api/users/:id/report` — any authenticated user files a report against
/// another user. The caller may not report themselves (`400`). The
/// `(reporter, reported, workspace)` triple is unique; a duplicate silently
/// returns `{created: false}`.
async fn create_report(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(reported_str): Path<String>,
    Json(req): Json<CreateReportReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let reported = parse_participant(&reported_str)?;
    if reported == auth.participant_id {
        return Err(AeroError::Invalid("cannot report yourself".into()).into());
    }
    let workspace = req
        .workspace_id
        .as_deref()
        .map(parse_workspace)
        .transpose()?;
    let reason = req.reason.trim();
    if reason.len() > 2_000 {
        return Err(AeroError::Invalid("reason too long".into()).into());
    }
    let id = repo(&s)
        .create(auth.participant_id, reported, workspace, reason)
        .await?;
    Ok(Json(serde_json::json!({
        "created": id.is_some(),
        "id": id,
    })))
}

#[derive(Deserialize)]
struct ReportListQuery {
    /// Optional status filter: `"pending"` | `"reviewed"` | `"dismissed"`.
    status: Option<String>,
}

/// `GET /api/workspaces/:id/user-reports` — workspace admin lists user reports.
async fn list_workspace_reports(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Query(q): Query<ReportListQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let workspace = parse_workspace(&ws_str)?;
    assert_admin(&s, &auth, workspace).await?;
    let reports = repo(&s)
        .list_for_workspace(workspace, q.status.as_deref())
        .await?;
    Ok(Json(serde_json::json!({ "reports": reports })))
}

#[derive(Deserialize)]
struct ResolveReportReq {
    /// New status: `"reviewed"` or `"dismissed"`.
    status: String,
}

/// `PATCH /api/workspaces/:id/user-reports/:rid` — workspace admin resolves a
/// pending report to `"reviewed"` or `"dismissed"`.
async fn resolve_report(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((ws_str, rid_str)): Path<(String, String)>,
    Json(req): Json<ResolveReportReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let workspace = parse_workspace(&ws_str)?;
    assert_admin(&s, &auth, workspace).await?;
    let report_id = Uuid::from_str(rid_str.trim())
        .map_err(|e| AeroError::Invalid(format!("report id: {e}")))?;
    if !matches!(req.status.as_str(), "reviewed" | "dismissed") {
        return Err(
            AeroError::Invalid("status must be 'reviewed' or 'dismissed'".into()).into(),
        );
    }
    let updated = repo(&s)
        .resolve(report_id, workspace, &req.status)
        .await?;
    if !updated {
        return Err(AeroError::NotFound(format!("pending report {report_id}")).into());
    }
    Ok(Json(serde_json::json!({ "resolved": true, "status": req.status })))
}
