//! Per-conversation export — download one room/DM's full message history as JSON.
//!
//! Compliance / portability complement to the workspace-level export: instead of
//! a whole tenant, the caller asks for a single room and gets back every
//! (non-deleted) message in chronological order. There is no new storage: the
//! handler pages the EXISTING keyset history method
//! ([`MessageRepo::list_recent`](aero_storage::MessageRepo::list_recent)), which
//! returns a room's messages newest-first, repeatedly walking the `before`
//! cursor backward until the room is drained or a hard cap is reached.
//!
//! `list_recent` orders **newest-first**, so paging accumulates newest→oldest;
//! the response is reversed to oldest-first so the export reads chronologically.
//! Accumulation is bounded by [`EXPORT_CAP`]: if the room has more messages than
//! the cap, only the most recent [`EXPORT_CAP`] are returned and `truncated` is
//! set. Access is the standard tenant guard — `assert_room_access` first — so a
//! caller can only export a room they may read. Purely additive: a thin handler
//! over existing [`AppState`] state; no existing repo or service is touched.
//! Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, Message, MessageId, RoomId};
use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// All conversation-export routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/rooms/:id/export", get(export_conversation))
}

/// Hard cap on how many messages a single export returns. A larger backlog is
/// truncated to the most recent `EXPORT_CAP` and the response is flagged
/// `truncated` (mirrors the history/search/catch-up page caps, just larger
/// because an export is a bulk download rather than an interactive page).
const EXPORT_CAP: usize = 5000;

/// Per-page fetch size when walking history. Matches the upper bound
/// [`MessageRepo::list_recent`](aero_storage::MessageRepo::list_recent) honors,
/// so each round-trip pulls a full page and the export needs the fewest queries.
const PAGE_SIZE: usize = 200;

/// The oldest message id in a page — the `before` cursor for the next, older
/// page. [`MessageId`] is a time-sortable ULID, so "oldest" is just the minimum
/// id; taking the minimum makes the cursor correct regardless of the page's own
/// ordering (`list_recent` returns newest-first, so this is its last element).
/// `None` for an empty page (nothing left to page from).
#[must_use]
fn next_before(msgs: &[Message]) -> Option<MessageId> {
    msgs.iter().map(|m| m.id).min()
}

/// Parse a `RoomId` from a path segment, mapping a decode failure to `400`.
fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

/// `GET /api/rooms/:id/export` — download the room's message history as JSON.
///
/// Requires room access. Pages the room's (non-deleted) messages backward via
/// [`MessageRepo::list_recent`](aero_storage::MessageRepo::list_recent),
/// accumulating up to [`EXPORT_CAP`]; if the room holds more than the cap, the
/// most recent [`EXPORT_CAP`] messages are returned and `truncated` is `true`.
/// Returns `{ "room_id", "count", "truncated", "messages": [<Message>...] }`
/// with `messages` in chronological (oldest-first) order.
async fn export_conversation(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    // Tenant guard (workspace + room membership) before reading the room's
    // messages — a caller may only export a room they are allowed to read.
    s.im.assert_room_access(auth.participant_id, room).await?;

    let page_limit = i64::try_from(PAGE_SIZE).unwrap_or(200);
    let mut messages: Vec<Message> = Vec::new();
    let mut before: Option<MessageId> = None;
    let mut truncated = false;

    // Walk `before` backward (newest→oldest) one full page at a time until the
    // room is drained (a short/empty page) or the cap is reached.
    loop {
        let page = s
            .messages
            .list_recent(room, before, page_limit)
            .await
            .map_err(AeroError::from)?;
        if page.is_empty() {
            break;
        }
        let full_page = page.len() == PAGE_SIZE;
        before = next_before(&page);
        messages.extend(page);

        if messages.len() >= EXPORT_CAP {
            // We hit the cap: keep the most recent EXPORT_CAP (collected
            // newest-first, so the kept prefix is the newest). It is genuinely
            // truncated if we overshot the cap, or if the page was full and so
            // older messages likely remain.
            let overshot = messages.len() > EXPORT_CAP;
            messages.truncate(EXPORT_CAP);
            truncated = overshot || full_page;
            break;
        }
        // A short page means the room is drained — stop without an extra query.
        if !full_page {
            break;
        }
    }

    // Collected newest-first; reverse to chronological oldest-first for the export.
    messages.reverse();

    Ok(Json(serde_json::json!({
        "room_id": room,
        "count": messages.len(),
        "truncated": truncated,
        "messages": messages,
    })))
}

#[cfg(test)]
mod tests {
    use super::next_before;
    use aero_common::{Block, Message, MessageId, ParticipantId, RoomId};
    use ulid::Ulid;

    /// A minimal `Message` carrying just the id the cursor logic cares about.
    fn msg(id: MessageId) -> Message {
        Message {
            id,
            room_id: RoomId::new(),
            sender_id: ParticipantId::new(),
            blocks: vec![Block::text("hi")],
            reply_to: None,
            metadata: serde_json::json!({}),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            edited_at: None,
            deleted_at: None,
            expires_at: None,
        }
    }

    /// `next_before` returns the OLDEST (minimum) id in a page — the `before`
    /// cursor for the next, older page — independent of the page's order, and
    /// `None` for an empty page.
    #[test]
    fn next_before_picks_oldest_id() {
        // Three ids with strictly increasing timestamps (oldest → newest).
        let a = MessageId::from_ulid(Ulid::from_parts(1_000, 0));
        let b = MessageId::from_ulid(Ulid::from_parts(2_000, 0));
        let c = MessageId::from_ulid(Ulid::from_parts(3_000, 0));

        // Newest-first, as `list_recent` returns: oldest is the last element.
        let page = [msg(c), msg(b), msg(a)];
        assert_eq!(next_before(&page), Some(a));

        // Order-independent: scrambled input still yields the oldest id.
        let scrambled = [msg(b), msg(a), msg(c)];
        assert_eq!(next_before(&scrambled), Some(a));

        // Empty page has no cursor to continue from.
        assert_eq!(next_before(&[]), None);
    }
}
