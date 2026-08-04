//! Channel bookmarks / header links — per-channel pinned resources.
//!
//! Pinned links/resources shown in a channel's header (like Slack channel
//! bookmarks): a titled URL with an optional leading emoji. DISTINCT from message
//! pins ([`crate::`]`PinRepo` path) and from personal saved items
//! ([`crate::bookmarks`], per-user save-for-later) — a channel bookmark belongs
//! to the channel itself.
//!
//! Thin handlers over transaction-authorized storage APIs. Detail routes carry
//! both room and bookmark ids, allowing storage to bind the opaque id to the
//! path room while holding the effective-access locks.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{ChannelBookmarkId, Error as AeroError, RoomId};
use aero_storage::{channel_bookmark::ChannelBookmarkPatch, ChannelBookmarkRepo};
use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// Max length of a bookmark title (defensive; the column is unbounded `text`).
const MAX_TITLE_LEN: usize = 256;
/// Max length of a bookmark URL (defensive; the column is unbounded `text`).
const MAX_URL_LEN: usize = 2048;
/// Max length of a normalized emoji/custom token.
const MAX_EMOJI_LEN: usize = 64;

/// All channel-bookmark routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/rooms/:id/bookmarks",
            get(list_bookmarks).post(add_bookmark),
        )
        .route(
            "/api/rooms/:id/bookmarks/:bid",
            get(get_bookmark)
                .patch(update_bookmark)
                .delete(delete_bookmark),
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
fn normalize_emoji(raw: Option<String>) -> Result<Option<String>, AeroError> {
    let value = raw
        .map(|emoji| emoji.trim().to_owned())
        .filter(|emoji| !emoji.is_empty());
    if value
        .as_ref()
        .is_some_and(|emoji| emoji.chars().count() > MAX_EMOJI_LEN)
    {
        return Err(AeroError::Invalid("emoji too long".into()));
    }
    Ok(value)
}

/// Validate a title: trimmed, non-empty, within [`MAX_TITLE_LEN`].
fn clean_title(raw: &str) -> Result<String, AeroError> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(AeroError::Invalid("title must not be empty".into()));
    }
    if t.chars().count() > MAX_TITLE_LEN {
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
    if u.chars().count() > MAX_URL_LEN {
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

    let title = clean_title(&req.title)?;
    let url = clean_url(&req.url)?;
    let emoji = normalize_emoji(req.emoji)?;
    let position = req.position.unwrap_or(0);

    let row = repo(&s)
        .add_channel_bookmark_authorized(
            room,
            auth.participant_id,
            &title,
            &url,
            emoji.as_deref(),
            position,
        )
        .await?;
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
    let bookmarks = repo(&s)
        .list_channel_bookmarks_authorized(room, auth.participant_id)
        .await?;
    Ok(Json(
        serde_json::to_value(bookmarks).map_err(AeroError::from)?,
    ))
}

/// `GET /api/rooms/:id/bookmarks/:bid` — one path-bound channel bookmark.
async fn get_bookmark(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((room_str, bid_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    let bid = parse_bookmark(&bid_str)?;
    let bookmark = repo(&s)
        .get_channel_bookmark_authorized(room, bid, auth.participant_id)
        .await?;
    Ok(Json(
        serde_json::to_value(bookmark).map_err(AeroError::from)?,
    ))
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

/// `PATCH /api/rooms/:id/bookmarks/:bid` — edit a path-bound bookmark.
async fn update_bookmark(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((room_str, bid_str)): Path<(String, String)>,
    Json(req): Json<UpdateBookmarkReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    let bid = parse_bookmark(&bid_str)?;
    let title = req.title.as_deref().map(clean_title).transpose()?;
    let url = req.url.as_deref().map(clean_url).transpose()?;
    let emoji = req
        .emoji
        .map(|value| normalize_emoji(Some(value)))
        .transpose()?;
    let row = repo(&s)
        .update_channel_bookmark_authorized(
            room,
            bid,
            auth.participant_id,
            ChannelBookmarkPatch {
                title: title.as_deref(),
                url: url.as_deref(),
                emoji: emoji.as_ref().map(Option::as_deref),
                position: req.position,
            },
        )
        .await?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `DELETE /api/rooms/:id/bookmarks/:bid` — remove a path-bound bookmark.
async fn delete_bookmark(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((room_str, bid_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    let bid = parse_bookmark(&bid_str)?;
    repo(&s)
        .delete_channel_bookmark_authorized(room, bid, auth.participant_id)
        .await?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}
