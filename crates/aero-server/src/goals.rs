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
//! Creator-gating on create/cancel resolves the stream (or the goal's creator) and
//! checks ownership against the caller.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, GoalId, ParticipantId};
use aero_storage::{is_valid_goal_metric, GoalRepo, StreamRepo};
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

/// Resolve `stream` and assert `caller` owns it. `NotFound` if unknown, `Forbidden`
/// if the caller is not the owner.
async fn require_owner(s: &AppState, stream: Ulid, caller: ParticipantId) -> Result<(), AeroError> {
    let row = StreamRepo::new(s.pg.clone())
        .get(stream)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("stream {stream}")))?;
    if row.owner_id != caller {
        return Err(AeroError::Forbidden("only the stream owner may manage goals".into()));
    }
    Ok(())
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
    require_owner(&s, stream, auth.participant_id).await?;

    let title = req.title.trim();
    if title.is_empty() {
        return Err(AeroError::Invalid("goal title is empty".into()).into());
    }
    if !is_valid_goal_metric(&req.metric_type) {
        return Err(
            AeroError::Invalid("metric_type must be 'gifts', 'viewers', or 'points'".into()).into(),
        );
    }
    if req.target < 1 {
        return Err(AeroError::Invalid("target must be >= 1".into()).into());
    }
    let description = req.description.as_deref().map(str::trim).filter(|d| !d.is_empty());

    let id = repo(&s)
        .create_goal(
            stream,
            auth.participant_id,
            title,
            description,
            &req.metric_type,
            req.target,
            req.expires_at,
        )
        .await
        .map_err(AeroError::from)?;
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

/// `GET /api/streams/:id/goals` — a stream's goals. Any authenticated viewer may read
/// (the goal bars are public broadcast UI). `?active_only=true` filters to active.
async fn list_goals(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
    Query(filter): Query<ListGoalsFilter>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&id_str)?;
    let r = repo(&s);
    let goals = if filter.active_only {
        r.list_active(stream).await
    } else {
        r.list_all(stream).await
    }
    .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "goals": goals })))
}

/// `DELETE /api/goals/:id` — the goal's creator cancels it.
async fn cancel_goal(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let goal = parse_goal(&id_str)?;
    let cancelled = repo(&s)
        .cancel_goal(goal, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    if !cancelled {
        // Either the goal is unknown, not owned by the caller, or already cancelled.
        // Distinguish: a non-existent goal is 404; otherwise it was a no-op (already
        // cancelled / not owned -> forbidden vs. conflict). Keep it simple: report
        // NotFound when the goal does not exist for this caller.
        let exists = repo(&s).get_goal(goal).await.map_err(AeroError::from)?;
        return match exists {
            None => Err(AeroError::NotFound(format!("goal {goal}")).into()),
            Some(g) if g.creator_id != auth.participant_id => {
                Err(AeroError::Forbidden("only the goal's creator may cancel it".into()).into())
            }
            Some(_) => Ok(Json(serde_json::json!({ "cancelled": false }))),
        };
    }
    Ok(Json(serde_json::json!({ "cancelled": true })))
}
