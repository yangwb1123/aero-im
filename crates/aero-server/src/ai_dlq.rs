//! Admin — AI dead-letter queue visibility and re-queue (ROADMAP 方向五).
//!
//! AI jobs that exhaust their retry budget transition to `status = 'dead'`.
//! Without an API they are only visible via a raw SQL query. These routes let
//! admins list dead jobs and re-queue individual ones for a fresh attempt.
//!
//! Routes (all workspace-admin-gated):
//! * `GET  /api/workspaces/:ws/admin/ai/dlq?limit=50`  — list dead jobs
//! * `POST /api/workspaces/:ws/admin/ai/dlq/:id/requeue` — reset and re-queue one job

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, WorkspaceId};
use aero_storage::AiJobRepo;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use ulid::Ulid;

use crate::error::ApiResult;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/workspaces/:ws/admin/ai/dlq", get(list_dlq))
        .route(
            "/api/workspaces/:ws/admin/ai/dlq/:job_id/requeue",
            post(requeue_job),
        )
}

const DEFAULT_DLQ_LIMIT: i64 = 50;
const MAX_DLQ_LIMIT: i64 = 200;

#[derive(Deserialize)]
struct DlqQuery {
    #[serde(default)]
    limit: Option<i64>,
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn map_requeue_error(error: AeroError) -> AeroError {
    match error {
        AeroError::Forbidden(_) => {
            AeroError::Forbidden("requeue requires a current workspace admin".into())
        }
        AeroError::NotFound(_) => AeroError::NotFound("dead AI job".into()),
        other => other,
    }
}

async fn assert_admin(
    s: &AppState,
    ws: WorkspaceId,
    caller: aero_common::ParticipantId,
) -> Result<(), AeroError> {
    let role = s
        .workspaces
        .effective_member_role(ws, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))?;
    if role.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden("admin required".into()))
    }
}

/// `GET /api/workspaces/:ws/admin/ai/dlq?limit=50` — list dead AI jobs.
async fn list_dlq(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Query(q): Query<DlqQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_admin(&s, ws, auth.participant_id).await?;

    let limit = q.limit.unwrap_or(DEFAULT_DLQ_LIMIT).clamp(1, MAX_DLQ_LIMIT);
    let repo = AiJobRepo::new(s.pg.clone());
    let jobs = repo
        .list_dead_for_workspace(ws, limit)
        .await
        .map_err(AeroError::from)?;
    let total = repo
        .count_dead_for_workspace(ws, None)
        .await
        .map_err(AeroError::from)?;

    let items: Vec<_> = jobs
        .iter()
        .map(|j| {
            serde_json::json!({
                "id": j.id.to_string(),
                "kind": format!("{:?}", j.kind).to_lowercase(),
                "attempts": j.attempts,
                "error": j.error,
                "scheduled_at": j.scheduled_at,
                "finished_at": j.finished_at,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "total_dead": total,
        "jobs": items,
    })))
}

/// `POST /api/workspaces/:ws/admin/ai/dlq/:job_id/requeue` — re-queue a dead job.
async fn requeue_job(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((ws_str, job_id_str)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let ws = parse_workspace(&ws_str)?;
    let job_id = Ulid::from_str(job_id_str.trim())
        .map_err(|_| AeroError::Invalid("invalid job id".into()))?;

    AiJobRepo::new(s.pg.clone())
        .requeue_for_workspace_authorized(job_id, ws, auth.participant_id)
        .await
        .map_err(map_requeue_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dlq_limit_clamp() {
        assert_eq!(0_i64.clamp(1, MAX_DLQ_LIMIT), 1);
        assert_eq!(50_i64.clamp(1, MAX_DLQ_LIMIT), 50);
        assert_eq!(9999_i64.clamp(1, MAX_DLQ_LIMIT), MAX_DLQ_LIMIT);
    }

    #[test]
    fn admin_required_not_member() {
        use aero_common::WorkspaceRole;
        let cases: &[(WorkspaceRole, bool)] = &[
            (WorkspaceRole::Owner, true),
            (WorkspaceRole::Admin, true),
            (WorkspaceRole::Member, false),
            (WorkspaceRole::Guest, false),
        ];
        for (role, expect_ok) in cases {
            assert_eq!(role.can_administer(), *expect_ok, "role {role:?}");
        }
    }

    #[test]
    fn requeue_authorization_and_scope_errors_have_stable_http_mappings() {
        let forbidden =
            map_requeue_error(AeroError::Forbidden("storage detail must not leak".into()));
        assert_eq!(forbidden.status_code(), 403);
        assert_eq!(forbidden.code(), "forbidden");
        assert_eq!(
            forbidden.to_string(),
            "forbidden: requeue requires a current workspace admin"
        );

        let missing = map_requeue_error(AeroError::NotFound(
            "cross-tenant/global/state detail must not leak".into(),
        ));
        assert_eq!(missing.status_code(), 404);
        assert_eq!(missing.code(), "not_found");
        assert_eq!(missing.to_string(), "not found: dead AI job");
    }
}
