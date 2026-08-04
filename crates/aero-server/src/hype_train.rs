//! Hype-train HTTP API + gift-path hook (escalating combo-gift momentum).
//!
//! A "hype train" is an escalating momentum session on a live stream: rapid
//! successive gifts accumulate units within a sliding window and advance a
//! `level` (Twitch Hype Train). The escalation arithmetic is the pure state
//! machine in [`aero_storage::hype_train`]; this module exposes the read endpoint
//! and the [`on_gift`] hook the gift handlers call after a gift is recorded.
//!
//! [`on_gift`] feeds a gift's `qty` into the train, advancing the level, then
//! broadcasts a [`StreamEvent::HypeTrain`] on the stream's subject so watchers'
//! clients render the escalating meter. It is best-effort: a storage/broadcast
//! hiccup is logged and must never fail the underlying gift send.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, StreamEvent};
use aero_storage::HypeTrainRepo;
use axum::{
    extract::{Path, Query, State},
    routing::get,
    Json, Router,
};
use time::OffsetDateTime;
use ulid::Ulid;
use uuid::Uuid;

use crate::error::ApiResult;
use crate::state::AppState;

/// Mount the hype-train read route, folded into the main router by
/// [`crate::routes::build`].
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/streams/:id/hype-train", get(get_hype_train))
        .route(
            "/api/streams/:id/hype-train/history",
            get(hype_train_history),
        )
        .route(
            "/api/streams/:id/hype-train/leaderboard",
            get(stream_leaderboard),
        )
        .route(
            "/api/hype-train-sessions/:id/leaderboard",
            get(session_leaderboard),
        )
}

fn parse_stream(s: &str) -> Result<Ulid, AeroError> {
    Ulid::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("stream id: {e}")))
}

fn repo(s: &AppState) -> HypeTrainRepo {
    HypeTrainRepo::new(s.pg.clone())
}

/// `GET /api/streams/:id/hype-train` — the stream's current (active, un-lapsed)
/// hype-train session, or `{"hype_train": null}` when none is running. Any
/// authenticated viewer may read it.
async fn get_hype_train(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&id_str)?;
    let session = repo(&s)
        .current(stream, OffsetDateTime::now_utc())
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "hype_train": session })))
}

/// Parse a session id from the URL path (a UUID string).
fn parse_session_id(s: &str) -> Result<Uuid, AeroError> {
    s.trim()
        .parse::<Uuid>()
        .map_err(|e| AeroError::Invalid(format!("session id: {e}")))
}

#[derive(serde::Deserialize)]
struct Pagination {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

fn default_limit() -> i64 {
    20
}

#[derive(serde::Deserialize)]
struct LeaderboardQuery {
    #[serde(default = "default_leaderboard_limit")]
    limit: i64,
}

fn default_leaderboard_limit() -> i64 {
    10
}

/// `GET /api/streams/:id/hype-train/history?limit=20&offset=0` — completed/expired
/// hype-train sessions for the stream, newest first. Any authenticated viewer may read.
async fn hype_train_history(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
    Query(pg): Query<Pagination>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&id_str)?;
    let sessions = repo(&s)
        .list_for_stream(stream, pg.limit, pg.offset)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "sessions": sessions })))
}

/// `GET /api/streams/:id/hype-train/leaderboard?limit=10` — top contributors across
/// ALL sessions for the stream, summed and ranked by total units. Any authenticated
/// viewer may read.
async fn stream_leaderboard(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<LeaderboardQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&id_str)?;
    let leaderboard = repo(&s)
        .stream_leaderboard(stream, q.limit)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "leaderboard": leaderboard })))
}

/// `GET /api/hype-train-sessions/:id/leaderboard?limit=10` — top contributors for a
/// single hype-train session, ranked by total units contributed.
async fn session_leaderboard(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<LeaderboardQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let session_id = parse_session_id(&id_str)?;
    let leaderboard = repo(&s)
        .session_leaderboard(session_id, q.limit)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "leaderboard": leaderboard })))
}

/// Hook the gift path: feed `qty` units into `stream`'s hype train (started fresh
/// if none is active), advancing the escalation state machine, then broadcast a
/// [`StreamEvent::HypeTrain`] so watchers see the level/contribution/expiry.
///
/// Best-effort and infallible to the caller: any storage error is logged and
/// swallowed so a hype-train hiccup never fails the gift send that triggered it.
pub async fn on_gift(state: &AppState, stream: Ulid, sender: ParticipantId, qty: u32) {
    let now = OffsetDateTime::now_utc();
    match HypeTrainRepo::new(state.pg.clone())
        .add_contribution(stream, sender, qty, now)
        .await
    {
        Ok(session) => {
            let event = StreamEvent::HypeTrain {
                stream_id: stream,
                level: u32::try_from(session.level.max(0)).unwrap_or(0),
                contribution: u32::try_from(session.contribution.max(0)).unwrap_or(0),
                expires_at: session.expires_at,
            };
            state.live.broadcast(&event).await;
        }
        Err(e) => tracing::warn!(error = ?e, %stream, "hype-train add_contribution failed"),
    }
}
