//! Keyword / highlight alerts — per-user keyword subscriptions.
//!
//! A user subscribes to a keyword within a workspace; when a message whose text
//! contains that keyword is sent, the subscriber is notified. These handlers own
//! the subscription CRUD only — the dispatch hook that turns a match into a
//! notification (via [`KeywordAlertRepo::matching_subscribers`](aero_storage::KeywordAlertRepo::matching_subscribers))
//! is wired separately into `ImService::dispatch_notifications`.
//!
//! Thin handlers over [`KeywordAlertRepo`](aero_storage::KeywordAlertRepo): add
//! and list assert the caller is a member of the workspace (via the shared
//! [`WorkspaceRepo`](aero_storage::WorkspaceRepo), mirroring
//! [`crate::saved_searches`]); delete is owner-scoped at the SQL layer (a
//! non-owner's id resolves to `false` ⇒ `404`). Keywords are normalized
//! (trimmed + lowercased) and must be 1..=64 chars after normalization. Mounted
//! via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, KeywordAlertId, ParticipantId, WorkspaceId};
use aero_storage::{normalize_keyword, KeywordAlertRepo};
use axum::{
    extract::{Path, Query, State},
    routing::{delete, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// Maximum length of a normalized keyword, in chars.
const MAX_KEYWORD_CHARS: usize = 64;

/// All keyword-alert routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/keyword-alerts",
            post(create_keyword_alert).get(list_keyword_alerts),
        )
        .route("/api/keyword-alerts/:id", delete(delete_keyword_alert))
}

/// Build a [`KeywordAlertRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> KeywordAlertRepo {
    KeywordAlertRepo::new(s.pg.clone())
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn parse_keyword_alert(s: &str) -> Result<KeywordAlertId, AeroError> {
    KeywordAlertId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("keyword alert id: {e}")))
}

/// Assert the caller is a member of the workspace, rejecting non-members with a
/// `403`. Mirrors `crate::saved_searches::assert_member`.
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

/// Validate and normalize a keyword: trim + lowercase, then require it to be
/// 1..=64 chars. A blank or over-long keyword is rejected `400`.
fn validate_keyword(raw: &str) -> Result<String, AeroError> {
    let normalized = normalize_keyword(raw);
    let len = normalized.chars().count();
    if len == 0 {
        return Err(AeroError::Invalid("keyword must not be empty".into()));
    }
    if len > MAX_KEYWORD_CHARS {
        return Err(AeroError::Invalid(format!(
            "keyword too long (max {MAX_KEYWORD_CHARS} chars)"
        )));
    }
    Ok(normalized)
}

#[derive(Deserialize)]
struct CreateKeywordAlertReq {
    /// The tenant the alert is scoped to (only messages here can trigger it).
    workspace_id: String,
    /// The keyword to subscribe to (normalized to trimmed-lowercase before save).
    keyword: String,
}

#[derive(Deserialize)]
struct ListKeywordAlertsQuery {
    /// The tenant whose alerts (the caller's own) to list.
    workspace_id: String,
}

/// `POST /api/keyword-alerts` — subscribe the caller to a keyword in a workspace.
/// The caller must be a member; a blank or over-long keyword is rejected `400`.
/// Idempotent: re-subscribing returns the existing row. Returns the created (or
/// existing) row.
async fn create_keyword_alert(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateKeywordAlertReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&req.workspace_id)?;
    assert_member(&s, ws, auth.participant_id).await?;
    let keyword = validate_keyword(&req.keyword)?;

    let id = repo(&s)
        .add(auth.participant_id, ws, &keyword)
        .await
        .map_err(AeroError::from)?;
    // Re-read so the response carries the full, canonical row (created_at). The
    // alert is one of the caller's own, so an owner-scoped list lookup finds it.
    let row = repo(&s)
        .list_for(auth.participant_id, ws)
        .await
        .map_err(AeroError::from)?
        .into_iter()
        .find(|a| a.id == id)
        .ok_or_else(|| AeroError::NotFound("keyword alert".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `GET /api/keyword-alerts?workspace_id=...` — the caller's keyword alerts in
/// this workspace, newest first. Members only; owner-scoped at the SQL layer.
async fn list_keyword_alerts(
    State(s): State<AppState>,
    auth: AuthUser,
    Query(q): Query<ListKeywordAlertsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&q.workspace_id)?;
    assert_member(&s, ws, auth.participant_id).await?;
    let alerts = repo(&s)
        .list_for(auth.participant_id, ws)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(alerts).map_err(AeroError::from)?))
}

/// `DELETE /api/keyword-alerts/:id` — unsubscribe one of the caller's own keyword
/// alerts. Owner-scoped: a `404` if it isn't the caller's row (someone else's or
/// unknown).
async fn delete_keyword_alert(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_keyword_alert(&id_str)?;
    let removed = repo(&s)
        .delete(id, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    if !removed {
        return Err(AeroError::NotFound(format!("keyword alert {id}")).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_keyword_normalizes_and_bounds() {
        // Trimmed + lowercased.
        assert_eq!(validate_keyword("  Deploy ").unwrap(), "deploy");
        // Blank (or whitespace-only) is rejected.
        assert!(validate_keyword("   ").is_err());
        assert!(validate_keyword("").is_err());
        // Exactly 64 chars is allowed; 65 is rejected.
        let max = "a".repeat(MAX_KEYWORD_CHARS);
        assert_eq!(validate_keyword(&max).unwrap().chars().count(), MAX_KEYWORD_CHARS);
        let over = "a".repeat(MAX_KEYWORD_CHARS + 1);
        assert!(validate_keyword(&over).is_err());
    }
}
