//! Thread follow / subscribe HTTP surface.
//!
//! A user follows a thread — identified by its root message — to be notified of
//! new replies, even when they are not @-mentioned. Subscriptions are PRIVATE to
//! the caller: every route is scoped to `auth.participant_id`, so one user can
//! never read or touch another's subscriptions.
//!
//! Thin handlers over [`ThreadSubscriptionRepo`](aero_storage::ThreadSubscriptionRepo):
//! following first resolves the root message's room
//! ([`ThreadSubscriptionRepo::message_room`](aero_storage::ThreadSubscriptionRepo::message_room))
//! then asserts the caller may access it
//! ([`ImService::assert_room_access`](aero_im_core::ImService::assert_room_access),
//! mirroring [`crate::routes`]), so a user can only follow a thread in a room they
//! belong to; unfollowing and listing are owner-scoped at the SQL layer and need
//! no room-access check (they only ever touch the caller's own subscriptions). The
//! reply fan-out itself ([`ThreadSubscriptionRepo::subscribers`]) is consumed by
//! `ImService::dispatch_notifications`, not here. Mounted via [`routes`] and
//! `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, MessageId};
use aero_storage::{ThreadMuteRepo, ThreadSubscriptionRepo};
use axum::{
    extract::{Path, State},
    routing::{post, put},
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// All thread-subscription routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/messages/:id/follow",
            put(follow_thread).delete(unfollow_thread),
        )
        .route(
            "/api/me/followed-threads",
            axum::routing::get(list_followed_threads),
        )
        // Thread MUTING (the inverse of follow): stop reply notifications for a
        // thread, identified by its root message id.
        .route(
            "/api/threads/:id/mute",
            post(mute_thread).delete(unmute_thread),
        )
}

/// Build a [`ThreadSubscriptionRepo`] from shared state, over the shared pool.
/// Cheap (a clone of an `Arc<PgPool>`), keeping this feature self-contained.
fn repo(s: &AppState) -> ThreadSubscriptionRepo {
    ThreadSubscriptionRepo::new(s.pg.clone())
}

/// Build a [`ThreadMuteRepo`] from shared state, over the shared pool.
fn mute_repo(s: &AppState) -> ThreadMuteRepo {
    ThreadMuteRepo::new(s.pg.clone())
}

fn parse_message(s: &str) -> Result<MessageId, AeroError> {
    MessageId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("message id: {e}")))
}

/// `PUT /api/messages/:id/follow` — follow the thread rooted at this message so
/// the caller is notified of new replies. The message's room is resolved and the
/// caller must be able to access it (workspace + room membership), else
/// `404`/`403`. An unknown message id is `404`. Idempotent: re-following is a
/// no-op. Always reports `following: true`.
async fn follow_thread(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let message = parse_message(&id_str)?;
    // Resolve the root message's room; an unknown message is a 404.
    let room = repo(&s)
        .message_room(message)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("message {message}")))?;
    // Room access guard (workspace + room membership) before recording a follow,
    // so a caller can only ever follow a thread in a room they belong to.
    s.im.assert_room_access(auth.participant_id, room).await?;
    repo(&s)
        .subscribe(auth.participant_id, message)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "following": true })))
}

/// `DELETE /api/messages/:id/follow` — unfollow the thread rooted at this message.
/// Owner-scoped at the SQL layer (only the caller's own subscription is ever
/// touched), so no room-access check is needed; unfollowing a thread that was
/// never followed is a no-op. Always reports `following: false`.
async fn unfollow_thread(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let message = parse_message(&id_str)?;
    repo(&s)
        .unsubscribe(auth.participant_id, message)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "following": false })))
}

/// `GET /api/me/followed-threads` — the root message ids of the threads the caller
/// follows, newest first. Always scoped to the caller; no room-access check is
/// needed since they are the caller's own subscriptions.
async fn list_followed_threads(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let threads = repo(&s)
        .followed_by(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "threads": threads })))
}

/// `POST /api/threads/:id/mute` — mute the thread rooted at this message so the
/// caller STOPS receiving reply notifications for it (the inverse of follow). The
/// root message's room is resolved and the caller must be able to access it
/// (workspace + room membership), else `404`/`403`; an unknown message id is
/// `404`. Idempotent: re-muting is a no-op. The mute suppresses only the
/// notification — the room broadcast of replies is unaffected. Always reports
/// `muted: true`.
async fn mute_thread(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let message = parse_message(&id_str)?;
    // Resolve the root message's room (reusing the subscription repo's resolver);
    // an unknown message is a 404, gated like follow_thread.
    let room = repo(&s)
        .message_room(message)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("message {message}")))?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    mute_repo(&s)
        .mute(auth.participant_id, message)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "muted": true })))
}

/// `DELETE /api/threads/:id/mute` — unmute the thread rooted at this message,
/// re-enabling reply notifications. Owner-scoped at the SQL layer (only the
/// caller's own mute is ever touched), so no room-access check is needed;
/// unmuting a thread that was never muted is a no-op. Always reports
/// `muted: false`.
async fn unmute_thread(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let message = parse_message(&id_str)?;
    mute_repo(&s)
        .unmute(auth.participant_id, message)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "muted": false })))
}
