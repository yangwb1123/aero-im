//! Stream / creator-follow HTTP surface.
//!
//! A participant follows a creator (another participant), then unfollows or lists
//! who they follow / who follows a given creator. Following is PRIVATE to the
//! caller for the write side (every mutate is scoped to `auth.participant_id`), so
//! one user can never follow on another's behalf. When a followed creator goes
//! live, the orchestrator calls
//! [`StreamFollowRepo::followers`](aero_storage::StreamFollowRepo::followers) to
//! notify each follower — this module owns only the follow-graph CRUD.
//!
//! Thin handlers over [`StreamFollowRepo`](aero_storage::StreamFollowRepo): a
//! self-follow (`follower == streamer`) is rejected `400`; follow is idempotent;
//! unfollow and the listing reads are participant-scoped at the SQL layer.
//! Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId};
use aero_storage::StreamFollowRepo;
use axum::{
    extract::{Path, State},
    routing::{get, put},
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// All stream-follow routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/participants/:id/follow",
            put(follow).delete(unfollow),
        )
        .route("/api/participants/:id/followers", get(list_followers))
        .route("/api/me/following", get(list_following))
}

/// Build a [`StreamFollowRepo`] from shared state, over the shared pool. Cheap (a
/// clone of an `Arc<PgPool>`), keeping this feature self-contained.
fn repo(s: &AppState) -> StreamFollowRepo {
    StreamFollowRepo::new(s.pg.clone())
}

fn parse_participant(s: &str) -> Result<ParticipantId, AeroError> {
    ParticipantId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
}

/// Reject a self-follow (`follower == streamer`) with `400`. Pure, so the rule is
/// unit-tested offline (no Postgres needed).
///
/// # Errors
/// [`AeroError::Invalid`] when `follower` and `streamer` are the same participant.
fn reject_self_follow(
    follower: ParticipantId,
    streamer: ParticipantId,
) -> Result<(), AeroError> {
    if follower == streamer {
        return Err(AeroError::Invalid("cannot follow yourself".into()));
    }
    Ok(())
}

/// `PUT /api/participants/:id/follow` — follow the creator `:id`. A self-follow is
/// rejected `400`. Idempotent: re-following is a no-op. Always reports
/// `following: true`.
async fn follow(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let streamer = parse_participant(&id_str)?;
    reject_self_follow(auth.participant_id, streamer)?;
    repo(&s)
        .follow(auth.participant_id, streamer)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "following": true })))
}

/// `DELETE /api/participants/:id/follow` — stop following the creator `:id`.
/// Participant-scoped at the SQL layer (only the caller's own follow is touched),
/// so unfollowing a creator that was never followed is a no-op. Always reports
/// `following: false`.
async fn unfollow(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let streamer = parse_participant(&id_str)?;
    repo(&s)
        .unfollow(auth.participant_id, streamer)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "following": false })))
}

/// `GET /api/me/following` — the creators the caller follows, newest first.
async fn list_following(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let participants = repo(&s)
        .following(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "participants": participants })))
}

/// `GET /api/participants/:id/followers` — the participants who follow the creator
/// `:id`, newest first (the same set the orchestrator notifies on go-live).
async fn list_followers(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let streamer = parse_participant(&id_str)?;
    let participants = repo(&s)
        .followers(streamer)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "participants": participants })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_follow_is_rejected() {
        let me = ParticipantId::new();
        assert!(
            reject_self_follow(me, me).is_err(),
            "following yourself is a 400"
        );
    }

    #[test]
    fn following_another_is_allowed() {
        let me = ParticipantId::new();
        let other = ParticipantId::new();
        assert!(
            reject_self_follow(me, other).is_ok(),
            "following a different creator is allowed"
        );
    }
}
