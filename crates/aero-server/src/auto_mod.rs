//! Auto-moderation rule management — workspace admin API.
//!
//! Workspace admins (Admin or Owner role) may define text-matching rules that the
//! IM service enforces at message-send time. Three match strategies are supported:
//! `"contains"` (default), `"exact"`, and `"prefix"`. The only send-time action
//! is `"block"`; `"delete"` and `"warn"` are reserved.
//!
//! Thin handlers over [`AutoModRuleRepo`](aero_storage::AutoModRuleRepo); mounted
//! via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, WorkspaceId, WorkspaceRole};
use aero_storage::AutoModRuleRepo;
use axum::{
    extract::{Path, State},
    routing::{delete, post},
    Json, Router,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::error::ApiResult;
use crate::state::AppState;

/// All auto-mod rule routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/workspaces/:id/auto-mod-rules",
            post(create_rule).get(list_rules),
        )
        .route(
            "/api/workspaces/:id/auto-mod-rules/:rid",
            delete(delete_rule),
        )
}

fn repo(s: &AppState) -> AutoModRuleRepo {
    AutoModRuleRepo::new(s.pg.clone())
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

/// Verify the caller is a workspace Admin or Owner; returns `403` otherwise.
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

const VALID_MATCH_TYPES: &[&str] = &["contains", "exact", "prefix"];
const VALID_ACTIONS: &[&str] = &["block", "delete", "warn"];

#[derive(Deserialize)]
struct CreateRuleReq {
    /// The pattern text to match against.
    pattern: String,
    /// How to match: `"contains"` | `"exact"` | `"prefix"`. Defaults to `"contains"`.
    #[serde(default = "default_match_type")]
    match_type: String,
    /// What to do on a match: `"block"` | `"delete"` | `"warn"`. Defaults to `"block"`.
    #[serde(default = "default_action")]
    action: String,
}

fn default_match_type() -> String {
    "contains".to_owned()
}
fn default_action() -> String {
    "block".to_owned()
}

/// `POST /api/workspaces/:id/auto-mod-rules` — create a rule. Admin only.
async fn create_rule(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Json(req): Json<CreateRuleReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let workspace = parse_workspace(&ws_str)?;
    assert_admin(&s, &auth, workspace).await?;

    let pattern = req.pattern.trim();
    if pattern.is_empty() {
        return Err(AeroError::Invalid("pattern must not be empty".into()).into());
    }
    if pattern.len() > 1_000 {
        return Err(AeroError::Invalid("pattern too long".into()).into());
    }
    if !VALID_MATCH_TYPES.contains(&req.match_type.as_str()) {
        return Err(AeroError::Invalid(format!(
            "match_type must be one of: {}",
            VALID_MATCH_TYPES.join(", ")
        ))
        .into());
    }
    if !VALID_ACTIONS.contains(&req.action.as_str()) {
        return Err(AeroError::Invalid(format!(
            "action must be one of: {}",
            VALID_ACTIONS.join(", ")
        ))
        .into());
    }

    let rule = repo(&s)
        .create(
            workspace,
            pattern,
            &req.match_type,
            &req.action,
            auth.participant_id,
        )
        .await?;
    Ok(Json(serde_json::to_value(rule).map_err(AeroError::from)?))
}

/// `GET /api/workspaces/:id/auto-mod-rules` — list rules. Admin only.
async fn list_rules(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let workspace = parse_workspace(&ws_str)?;
    assert_admin(&s, &auth, workspace).await?;
    let rules = repo(&s).list_for_workspace(workspace).await?;
    Ok(Json(serde_json::json!({ "rules": rules })))
}

/// `DELETE /api/workspaces/:id/auto-mod-rules/:rid` — delete a rule. Admin only.
async fn delete_rule(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((ws_str, rid_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let workspace = parse_workspace(&ws_str)?;
    assert_admin(&s, &auth, workspace).await?;
    let rule_id = Uuid::from_str(rid_str.trim())
        .map_err(|e| AeroError::Invalid(format!("rule id: {e}")))?;
    let removed = repo(&s).delete(rule_id, workspace).await?;
    if !removed {
        return Err(AeroError::NotFound(format!("rule {rule_id}")).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}
