//! Auto-moderation rule management — workspace admin API.
//!
//! Workspace admins (Admin or Owner role) may define text-matching rules that the
//! IM service enforces before message sends and edits. Three match strategies are
//! supported: `"contains"` (default), `"exact"`, and `"prefix"`. The only
//! accepted action is `"block"`; unsupported actions are rejected instead of
//! becoming silent no-ops.
//!
//! Thin handlers over [`AutoModRuleRepo`](aero_storage::AutoModRuleRepo); mounted
//! via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, WorkspaceId, WorkspaceRole};
use aero_storage::{
    auto_mod::{AutoModRuleWriteError, MAX_AUTO_MOD_RULES_PER_WORKSPACE},
    AutoModRuleRepo,
};
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
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn map_write_error(error: AutoModRuleWriteError) -> AeroError {
    match error {
        AutoModRuleWriteError::QuotaExceeded => AeroError::Conflict(format!(
            "auto-mod rule quota exceeded (maximum {MAX_AUTO_MOD_RULES_PER_WORKSPACE} per workspace)"
        )),
        AutoModRuleWriteError::Domain(error) => error,
    }
}

/// Verify the caller is a workspace Admin or Owner; returns `403` otherwise.
async fn assert_admin(s: &AppState, auth: &AuthUser, workspace: WorkspaceId) -> ApiResult<()> {
    let role = s
        .workspaces
        .effective_member_role(workspace, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    match role {
        Some(r) if r.at_least(WorkspaceRole::Admin) => Ok(()),
        _ => Err(AeroError::Forbidden("workspace admin required".into()).into()),
    }
}

const VALID_MATCH_TYPES: &[&str] = &["contains", "exact", "prefix"];
const VALID_ACTIONS: &[&str] = &["block"];

#[derive(Deserialize)]
struct CreateRuleReq {
    /// The pattern text to match against.
    pattern: String,
    /// How to match: `"contains"` | `"exact"` | `"prefix"`. Defaults to `"contains"`.
    #[serde(default = "default_match_type")]
    match_type: String,
    /// What to do on a match. Only `"block"` is supported.
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
        .create_authorized(
            workspace,
            pattern,
            &req.match_type,
            &req.action,
            auth.participant_id,
        )
        .await
        .map_err(map_write_error)?;
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
    let rule_id =
        Uuid::from_str(rid_str.trim()).map_err(|e| AeroError::Invalid(format!("rule id: {e}")))?;
    let removed = repo(&s)
        .delete_authorized(rule_id, workspace, auth.participant_id)
        .await?;
    if !removed {
        return Err(AeroError::NotFound(format!("rule {rule_id}")).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[cfg(test)]
mod tests {
    use aero_common::Error as AeroError;
    use aero_storage::auto_mod::{AutoModRuleWriteError, MAX_AUTO_MOD_RULES_PER_WORKSPACE};

    use super::{map_write_error, VALID_ACTIONS, VALID_MATCH_TYPES};

    #[test]
    fn only_actions_with_runtime_semantics_are_advertised() {
        assert_eq!(VALID_ACTIONS, ["block"]);
        assert_eq!(VALID_MATCH_TYPES, ["contains", "exact", "prefix"]);
        assert!(!VALID_ACTIONS.contains(&"delete"));
        assert!(!VALID_ACTIONS.contains(&"warn"));
    }

    #[test]
    fn persistent_rule_quota_maps_to_explicit_conflict() {
        let error = map_write_error(AutoModRuleWriteError::QuotaExceeded);
        assert_eq!(error.status_code(), 409);
        assert_eq!(error.code(), "conflict");
        assert!(matches!(
            error,
            AeroError::Conflict(message)
                if message.contains(&MAX_AUTO_MOD_RULES_PER_WORKSPACE.to_string())
        ));
    }

    #[test]
    fn production_boot_wires_the_mounted_management_surface() {
        let boot = include_str!("bin/boot/services.rs");
        assert!(boot.contains("with_auto_mod_rules"));
        assert!(
            !boot.contains("AERO_AUTO_MOD_RULES"),
            "managed rules must not depend on a hidden runtime gate"
        );
    }
}
