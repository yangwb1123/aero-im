//! Channel bookmarks / header links — per-channel pinned resources.
//!
//! Pinned links/resources shown in a channel's header (like Slack channel
//! bookmarks): a titled URL with an optional leading emoji. DISTINCT from message
//! pins ([`crate::`]`PinRepo` path) and from personal saved items
//! ([`crate::bookmarks`], per-user save-for-later) — a channel bookmark belongs
//! to the channel itself.
//!
//! Thin handlers over [`ChannelBookmarkRepo`](aero_storage::ChannelBookmarkRepo).
//! Every route is room-access gated through the shared tenant + membership guard
//! ([`assert_room_access`](aero_im_core::ImService::assert_room_access)): the
//! room-addressed routes assert directly on the path room, while the
//! bookmark-addressed routes (`PATCH`/`DELETE`) first load the bookmark to
//! resolve its room, then assert access on THAT room (a `404` for an unknown
//! bookmark, a `403`/`404` for a room the caller can't reach). Mounted via
//! [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{ChannelBookmarkId, Error as AeroError, RoomId};
use aero_storage::ChannelBookmarkRepo;
use axum::{
    extract::{Path, State},
    routing::{get, patch},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// Max length of a bookmark title (defensive; the column is unbounded `text`).
const MAX_TITLE_LEN: usize = 256;
/// Max length of a bookmark URL (defensive; the column is unbounded `text`).
const MAX_URL_LEN: usize = 2048;

/// All channel-bookmark routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/rooms/:id/bookmarks",
            get(list_bookmarks).post(add_bookmark),
        )
        .route(
            "/api/channel-bookmarks/:bid",
            patch(update_bookmark).delete(delete_bookmark),
        )
}

/// Build a [`ChannelBookmarkRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> ChannelBookmarkRepo {
    ChannelBookmarkRepo::new(s.pg.clone())
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

fn parse_bookmark(s: &str) -> Result<ChannelBookmarkId, AeroError> {
    ChannelBookmarkId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("channel bookmark id: {e}")))
}

/// Normalize an optional emoji: trim, and collapse an empty string to `None`
/// (so a client can clear the emoji by sending `""`).
fn normalize_emoji(raw: Option<String>) -> Option<String> {
    raw.map(|e| e.trim().to_owned()).filter(|e| !e.is_empty())
}

/// Validate a title: trimmed, non-empty, within [`MAX_TITLE_LEN`].
fn clean_title(raw: &str) -> Result<String, AeroError> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(AeroError::Invalid("title must not be empty".into()));
    }
    if t.len() > MAX_TITLE_LEN {
        return Err(AeroError::Invalid("title too long".into()));
    }
    Ok(t.to_owned())
}

/// Validate a url: trimmed, non-empty, within [`MAX_URL_LEN`].
fn clean_url(raw: &str) -> Result<String, AeroError> {
    let u = raw.trim();
    if u.is_empty() {
        return Err(AeroError::Invalid("url must not be empty".into()));
    }
    if u.len() > MAX_URL_LEN {
        return Err(AeroError::Invalid("url too long".into()));
    }
    Ok(u.to_owned())
}

#[derive(Deserialize)]
struct AddBookmarkReq {
    /// Header label for the bookmark.
    title: String,
    /// The link the bookmark points at.
    url: String,
    /// Optional leading emoji.
    #[serde(default)]
    emoji: Option<String>,
    /// Optional display position (defaults to `0`).
    #[serde(default)]
    position: Option<i32>,
}

/// `POST /api/rooms/:id/bookmarks` — add a header bookmark to the channel. Any
/// member with room access may add one. A blank title or url is rejected `400`.
/// Returns the created row.
async fn add_bookmark(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<AddBookmarkReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;

    let title = clean_title(&req.title)?;
    let url = clean_url(&req.url)?;
    let emoji = normalize_emoji(req.emoji);
    let position = req.position.unwrap_or(0);

    let id = repo(&s)
        .add(
            room,
            auth.participant_id,
            &title,
            &url,
            emoji.as_deref(),
            position,
        )
        .await
        .map_err(AeroError::from)?;
    // Re-read so the response carries the full, canonical row (created_at).
    let row = repo(&s)
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("channel bookmark".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `GET /api/rooms/:id/bookmarks` — the channel's header bookmarks in display
/// order (by position, then creation time). Any member with room access may list.
async fn list_bookmarks(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let bookmarks = repo(&s)
        .list_for_room(room)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(bookmarks).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct UpdateBookmarkReq {
    /// New title (kept unchanged when absent).
    #[serde(default)]
    title: Option<String>,
    /// New url (kept unchanged when absent).
    #[serde(default)]
    url: Option<String>,
    /// New emoji: absent ⇒ unchanged; present-and-empty ⇒ cleared.
    #[serde(default)]
    emoji: Option<String>,
    /// New display position (kept unchanged when absent).
    #[serde(default)]
    position: Option<i32>,
}

/// `PATCH /api/channel-bookmarks/:bid` — edit a bookmark's title/url/emoji/
/// position. The bookmark is loaded first to resolve its room, then room access
/// is asserted on THAT room (`404` if the bookmark is unknown). Only the fields
/// present in the body are changed. Returns the updated row.
async fn update_bookmark(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(bid_str): Path<String>,
    Json(req): Json<UpdateBookmarkReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let bid = parse_bookmark(&bid_str)?;
    let existing = repo(&s)
        .get(bid)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("channel bookmark {bid}")))?;
    // Gate on the bookmark's OWN room — membership in that channel is required.
    s.im.assert_room_access(auth.participant_id, existing.room_id).await?;

    let title = match req.title {
        Some(t) => clean_title(&t)?,
        None => existing.title,
    };
    let url = match req.url {
        Some(u) => clean_url(&u)?,
        None => existing.url,
    };
    // Absent emoji ⇒ keep existing; present (incl. empty ⇒ cleared) ⇒ replace.
    let emoji = match req.emoji {
        Some(e) => normalize_emoji(Some(e)),
        None => existing.emoji,
    };
    let position = req.position.unwrap_or(existing.position);

    let changed = repo(&s)
        .update(bid, &title, &url, emoji.as_deref(), position)
        .await
        .map_err(AeroError::from)?;
    if !changed {
        // Lost a race with a concurrent delete.
        return Err(AeroError::NotFound(format!("channel bookmark {bid}")).into());
    }
    let row = repo(&s)
        .get(bid)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("channel bookmark {bid}")))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `DELETE /api/channel-bookmarks/:bid` — remove a header bookmark. The bookmark
/// is loaded first to resolve its room, then room access is asserted on THAT room
/// (`404` if the bookmark is unknown or already gone).
async fn delete_bookmark(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(bid_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let bid = parse_bookmark(&bid_str)?;
    let existing = repo(&s)
        .get(bid)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("channel bookmark {bid}")))?;
    s.im.assert_room_access(auth.participant_id, existing.room_id).await?;

    let removed = repo(&s).delete(bid).await.map_err(AeroError::from)?;
    if !removed {
        return Err(AeroError::NotFound(format!("channel bookmark {bid}")).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}
