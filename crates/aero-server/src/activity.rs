//! Activity feed — a durable, per-participant feed of non-message events.
//!
//! The first event carried is "a creator you follow went live" (fanned out by
//! [`crate::golive_bot`]), but the feed is deliberately general (kind + optional
//! actor/subject + human summary) and reusable for future non-message notices.
//! Distinct from the message+room-scoped notification inbox.
//!
//! Thin handlers over [`ActivityFeedRepo`](aero_storage::ActivityFeedRepo): every
//! route is recipient-scoped to the authenticated caller at the SQL layer (the
//! caller's `participant_id` is always in the `WHERE`), so one participant can
//! never read or mutate another's feed. Mounted via [`routes`] and `.merge`d into
//! the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{ActivityId, Error as AeroError};
use aero_storage::ActivityFeedRepo;
use axum::{
    extract::{Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All activity-feed routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/activity", get(list_activity))
        .route("/api/activity/count", get(activity_count))
        .route("/api/activity/read", post(mark_activity_read))
}

/// Default page size when the caller does not specify `limit`.
const DEFAULT_LIMIT: i64 = 50;
/// Hard cap on a single page so a caller cannot request an unbounded scan.
const MAX_LIMIT: i64 = 200;

/// Build an [`ActivityFeedRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> ActivityFeedRepo {
    ActivityFeedRepo::new(s.pg.clone())
}

fn parse_activity(s: &str) -> Result<ActivityId, AeroError> {
    ActivityId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("activity id: {e}")))
}

#[derive(Deserialize)]
struct ListQuery {
    /// Exclusive keyset cursor: the oldest id from the previous page. Omit for the
    /// first (newest) page.
    #[serde(default)]
    before: Option<String>,
    /// Page size (clamped to `[1, MAX_LIMIT]`); defaults to `DEFAULT_LIMIT`.
    #[serde(default)]
    limit: Option<i64>,
}

/// `GET /api/activity?before=&limit=` — the caller's activity feed, newest first.
/// `before` is an exclusive keyset cursor (the oldest id from the previous page).
/// Recipient-scoped at the SQL layer.
async fn list_activity(
    State(s): State<AppState>,
    auth: AuthUser,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let before = match q.before {
        Some(ref b) if !b.trim().is_empty() => Some(parse_activity(b)?),
        _ => None,
    };
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let entries = repo(&s)
        .list(auth.participant_id, before, limit)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(
        serde_json::to_value(entries).map_err(AeroError::from)?,
    ))
}

/// `GET /api/activity/count` — the caller's unread activity count (for a badge).
async fn activity_count(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let count = repo(&s)
        .unread_count(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "unread": count })))
}

#[derive(Deserialize, Default)]
struct MarkReadReq {
    /// Mark this entry and everything older read. Omit to mark the whole feed read.
    #[serde(default)]
    before: Option<String>,
}

/// `POST /api/activity/read` — mark the caller's unread entries read. With a
/// `before` id in the body, only that entry and older ones are marked ("mark read
/// up to here"); with an empty body, the whole feed is marked. Returns how many
/// rows transitioned to read.
async fn mark_activity_read(
    State(s): State<AppState>,
    auth: AuthUser,
    body: Option<Json<MarkReadReq>>,
) -> ApiResult<Json<serde_json::Value>> {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    let before = match req.before {
        Some(ref b) if !b.trim().is_empty() => Some(parse_activity(b)?),
        _ => None,
    };
    let marked = repo(&s)
        .mark_read(auth.participant_id, before)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "marked": marked })))
}
