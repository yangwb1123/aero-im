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
//! mirroring [`crate::routes`]), then every mutation repeats the current-access
//! and live-root check inside its storage transaction. Listings filter revoked
//! rooms. The reply fan-out itself
//! ([`ThreadSubscriptionRepo::subscribers`]) is consumed by
//! `ImService::dispatch_notifications`, not here.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, MessageId};
use aero_storage::{ThreadNotificationPrefsRepo, ThreadReadStateRepo, ThreadSubscriptionRepo};
use axum::{
    extract::{Path, State},
    routing::{get, post, put},
    Json, Router,
};
use serde::Deserialize;

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
        // Thread notification level (ROADMAP6 Lane A)
        .route(
            "/api/messages/:id/thread-notification-level",
            put(set_thread_notif_level).get(get_thread_notif_level),
        )
        // Thread participant roster (ROADMAP6 Lane A)
        .route(
            "/api/messages/:id/thread-participants",
            get(list_thread_participants),
        )
        // Thread read state + unread count (ROADMAP7 Lane A)
        .route("/api/messages/:id/read", post(mark_thread_read))
        .route("/api/messages/:id/unread-count", get(get_unread_count))
}

/// Build a [`ThreadSubscriptionRepo`] from shared state, over the shared pool.
/// Cheap (a clone of an `Arc<PgPool>`), keeping this feature self-contained.
fn repo(s: &AppState) -> ThreadSubscriptionRepo {
    ThreadSubscriptionRepo::new(s.pg.clone())
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
        .subscribe_authorized(auth.participant_id, message)
        .await?;
    Ok(Json(serde_json::json!({ "following": true })))
}

/// `DELETE /api/messages/:id/follow` — unfollow the thread rooted at this message.
/// Current room access and canonical-root status are checked as a preflight and
/// again inside the delete transaction. Unfollowing an absent row is a no-op.
async fn unfollow_thread(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let message = parse_message(&id_str)?;
    let room = repo(&s)
        .message_room(message)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("thread".into()))?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    repo(&s)
        .unsubscribe_authorized(auth.participant_id, message)
        .await?;
    Ok(Json(serde_json::json!({ "following": false })))
}

/// `GET /api/me/followed-threads` — the root message ids of the threads the caller
/// follows, newest first, filtered to live roots in currently accessible rooms.
async fn list_followed_threads(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let threads = repo(&s).followed_by_accessible(auth.participant_id).await?;
    Ok(Json(serde_json::json!({ "threads": threads })))
}

// ----------------------------------- Thread notification level (ROADMAP6 Lane A)

fn notif_prefs_repo(s: &AppState) -> ThreadNotificationPrefsRepo {
    ThreadNotificationPrefsRepo::new(s.pg.clone())
}

#[derive(Deserialize)]
struct SetThreadNotifLevelReq {
    level: String,
}

/// `PUT /api/messages/:id/thread-notification-level` — set the caller's per-thread
/// notification level for the thread rooted at `:id`. The message must exist and
/// the caller must have access to its room. Valid levels: `all`, `mentions`, `none`.
async fn set_thread_notif_level(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<SetThreadNotifLevelReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let message = parse_message(&id_str)?;

    // Validate the level before touching the DB.
    match req.level.as_str() {
        "all" | "mentions" | "none" => {}
        other => {
            return Err(AeroError::Invalid(format!(
                "invalid level {other:?}; must be 'all', 'mentions', or 'none'"
            ))
            .into())
        }
    }

    // Resolve the root message's room; an unknown message is a 404.
    let room = repo(&s)
        .message_room(message)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("message {message}")))?;

    // Room access guard before writing.
    s.im.assert_room_access(auth.participant_id, room).await?;

    notif_prefs_repo(&s)
        .set_level_authorized(auth.participant_id, message, &req.level)
        .await?;

    Ok(Json(serde_json::json!({
        "root_message_id": message,
        "level": req.level,
    })))
}

/// `GET /api/messages/:id/thread-notification-level` — return the caller's
/// per-thread notification level for the thread rooted at `:id`. Defaults to
/// `"all"` when no preference has been set.
async fn get_thread_notif_level(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let message = parse_message(&id_str)?;

    // Resolve the room (and assert access) so an unknown message id is a 404.
    let room = repo(&s)
        .message_room(message)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("message {message}")))?;

    s.im.assert_room_access(auth.participant_id, room).await?;

    let level = notif_prefs_repo(&s)
        .get_level_authorized(auth.participant_id, message)
        .await?;

    Ok(Json(serde_json::json!({ "level": level })))
}

// ----------------------------------- Thread participant roster (ROADMAP6 Lane A)

/// `GET /api/messages/:id/thread-participants` — the distinct participants who have
/// posted at least one (non-deleted) reply in the thread rooted at `:id`. The
/// caller must have access to the root message's room. Returns each participant's
/// `id`, `display_name`, and `avatar_url`.
async fn list_thread_participants(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let root = parse_message(&id_str)?;

    // Resolve the root message's room; an unknown message id is a 404.
    let thread_room = repo(&s)
        .message_room(root)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("message {root}")))?;

    s.im.assert_room_access(auth.participant_id, thread_room)
        .await?;

    // Fetch distinct participant ids from the thread.
    let participant_ids = s
        .messages
        .thread_participants(root)
        .await
        .map_err(AeroError::from)?;

    // Join with participant metadata: display_name + avatar_url.
    let mut participants = Vec::with_capacity(participant_ids.len());
    for pid in &participant_ids {
        let p = s.participants.get(*pid).await.map_err(AeroError::from)?;
        if let Some(p) = p {
            participants.push(serde_json::json!({
                "id": p.id,
                "display_name": p.display_name,
                "avatar_url": p.avatar_url,
            }));
        }
    }

    Ok(Json(serde_json::json!({ "participants": participants })))
}

// ----------------------------------- Thread read state (ROADMAP7 Lane A)

fn read_state_repo(s: &AppState) -> ThreadReadStateRepo {
    s.thread_read_state.clone()
}

/// `POST /api/messages/:id/read` — mark the thread rooted at `:id` as read
/// for the caller. Idempotent: re-marking updates the cursor to `now()`.
async fn mark_thread_read(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let root = parse_message(&id_str)?;
    let thread_room = repo(&s)
        .message_room(root)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("thread".into()))?;
    s.im.assert_room_access(auth.participant_id, thread_room)
        .await?;
    read_state_repo(&s)
        .mark_read_authorized(auth.participant_id, root)
        .await?;
    Ok(Json(serde_json::json!({ "read": true })))
}

/// `GET /api/messages/:id/unread-count` — return the number of replies in the
/// thread rooted at `:id` that the caller has not yet read.
async fn get_unread_count(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let root = parse_message(&id_str)?;
    let thread_room = repo(&s)
        .message_room(root)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("thread".into()))?;
    s.im.assert_room_access(auth.participant_id, thread_room)
        .await?;
    let count = read_state_repo(&s)
        .unread_count_authorized(auth.participant_id, root)
        .await?;
    Ok(Json(serde_json::json!({ "unread": count })))
}
