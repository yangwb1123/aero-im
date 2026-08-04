//! Creator goal / bounty-bar HTTP API.
//!
//! A "goal" is a streamer goal bar: a titled target on a metric (`gifts` / `viewers`
//! / `points`) the broadcast fills toward as progress accrues. Thin handlers over
//! [`aero_storage::GoalRepo`]; the actual progress feed for `gifts` goals lives in the
//! gift path ([`crate::live::LiveService::send_gift`] -> [`feed_gift_goals`]).
//!
//! Routes:
//!   * `POST   /api/streams/:id/goals` — the creator creates a goal.
//!   * `GET    /api/streams/:id/goals` — list a stream's goals.
//!   * `DELETE /api/goals/:id`         — the creator cancels a goal.
//!
//! Creator-gating on create/cancel resolves the stream's current canonical owner
//! and effective access in the storage transaction.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, GoalId};
use aero_storage::goals::GoalCancelOutcome;
use aero_storage::{
    is_valid_goal_metric, GoalCreateError, GoalRepo, MAX_ACTIVE_GOALS_PER_STREAM,
    MAX_GOAL_DESCRIPTION_CHARS, MAX_GOAL_TITLE_CHARS,
};
use axum::{
    extract::{Path, Query, State},
    routing::{delete, post},
    Json, Router,
};
use serde::Deserialize;
use time::OffsetDateTime;
use ulid::Ulid;

use crate::error::ApiResult;
use crate::state::AppState;

/// Mount the goal routes, folded into the main router by [`crate::routes::build`].
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/streams/:id/goals", post(create_goal).get(list_goals))
        .route("/api/goals/:id", delete(cancel_goal))
}

fn parse_stream(s: &str) -> Result<Ulid, AeroError> {
    Ulid::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("stream id: {e}")))
}

fn parse_goal(s: &str) -> Result<GoalId, AeroError> {
    GoalId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("goal id: {e}")))
}

fn repo(s: &AppState) -> GoalRepo {
    GoalRepo::new(s.pg.clone())
}

fn map_owner_scoped_cancel(goal: GoalId, outcome: GoalCancelOutcome) -> Result<bool, AeroError> {
    match outcome {
        GoalCancelOutcome::Cancelled => Ok(true),
        GoalCancelOutcome::AlreadyCancelled => Ok(false),
        GoalCancelOutcome::NotFound => Err(AeroError::NotFound(format!("goal {goal}"))),
    }
}

#[derive(Deserialize)]
struct CreateGoalReq {
    title: String,
    #[serde(default)]
    description: Option<String>,
    metric_type: String,
    target: i64,
    #[serde(default, with = "time::serde::rfc3339::option")]
    expires_at: Option<OffsetDateTime>,
}

/// `POST /api/streams/:id/goals` — the stream's owner creates a goal.
async fn create_goal(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<CreateGoalReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&id_str)?;

    let title = req.title.trim();
    if title.is_empty() {
        return Err(AeroError::Invalid("goal title is empty".into()).into());
    }
    if title.chars().count() > MAX_GOAL_TITLE_CHARS {
        return Err(AeroError::Invalid(format!(
            "goal title is too long (max {MAX_GOAL_TITLE_CHARS} chars)"
        ))
        .into());
    }
    if !is_valid_goal_metric(&req.metric_type) {
        return Err(AeroError::Invalid(
            "metric_type must be 'gifts', 'viewers', or 'points'".into(),
        )
        .into());
    }
    if req.target < 1 {
        return Err(AeroError::Invalid("target must be >= 1".into()).into());
    }
    let description = req
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty());
    if description.is_some_and(|value| value.chars().count() > MAX_GOAL_DESCRIPTION_CHARS) {
        return Err(AeroError::Invalid(format!(
            "goal description is too long (max {MAX_GOAL_DESCRIPTION_CHARS} chars)"
        ))
        .into());
    }
    if req
        .expires_at
        .is_some_and(|expiry| expiry <= OffsetDateTime::now_utc())
    {
        return Err(AeroError::Invalid("expires_at must be in the future".into()).into());
    }

    let id = repo(&s)
        .create_goal_authorized(
            stream,
            auth.participant_id,
            title,
            description,
            &req.metric_type,
            req.target,
            req.expires_at,
        )
        .await
        .map_err(|error| match error {
            GoalCreateError::StreamNotFound => AeroError::NotFound(format!("stream {stream}")),
            GoalCreateError::NotOwner => {
                AeroError::Forbidden("only the stream owner may manage goals".into())
            }
            GoalCreateError::NotAuthorized => {
                AeroError::Forbidden("stream owner lacks effective access".into())
            }
            GoalCreateError::LimitReached => AeroError::Invalid(format!(
                "active goal limit reached ({MAX_ACTIVE_GOALS_PER_STREAM})"
            )),
            GoalCreateError::InvalidInput(message) => AeroError::Invalid(message),
            GoalCreateError::Storage(error) => AeroError::from(error),
        })?;
    Ok(Json(serde_json::json!({
        "goal_id": id.to_string(),
        "stream_id": stream.to_string(),
        "metric_type": req.metric_type,
        "target": req.target,
    })))
}

#[derive(Deserialize)]
struct ListGoalsFilter {
    /// When `true`, only `active` goals; otherwise all (active/reached/cancelled).
    #[serde(default)]
    active_only: bool,
}

/// `GET /api/streams/:id/goals` — a stream's goals. Any authenticated viewer may
/// read: like `GET /api/streams/:id`, stream metadata and its broadcast goal bars
/// are public even when the stream carries an optional room association.
/// `?active_only=true` filters to active.
async fn list_goals(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
    Query(filter): Query<ListGoalsFilter>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&id_str)?;
    // Match the canonical public stream-detail contract: optional room
    // association does not make a broadcast private, but an unknown/pruned
    // stream cannot be used to expose retained goal text.
    s.streams
        .get(stream)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("stream {stream}")))?;
    let r = repo(&s);
    let goals = if filter.active_only {
        r.list_active(stream).await
    } else {
        r.list_all(stream).await
    }
    .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "goals": goals })))
}

/// `DELETE /api/goals/:id` — the stream's current canonical owner cancels it.
async fn cancel_goal(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let goal = parse_goal(&id_str)?;
    let outcome = repo(&s)
        .cancel_goal(goal, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    // Unknown, former-owner, and foreign-owned ids are deliberately
    // indistinguishable. Only the authorized current owner can observe the
    // idempotent already-cancelled result.
    let cancelled = map_owner_scoped_cancel(goal, outcome)?;
    Ok(Json(serde_json::json!({ "cancelled": cancelled })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_cancel_retry_is_idempotent_but_foreign_ids_are_opaque() {
        let goal = GoalId::new();
        assert!(map_owner_scoped_cancel(goal, GoalCancelOutcome::Cancelled).unwrap());
        assert!(!map_owner_scoped_cancel(goal, GoalCancelOutcome::AlreadyCancelled).unwrap());
        assert!(matches!(
            map_owner_scoped_cancel(goal, GoalCancelOutcome::NotFound),
            Err(AeroError::NotFound(_))
        ));
    }
}
