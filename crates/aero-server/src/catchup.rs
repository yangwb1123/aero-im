//! AI "catch me up" — summarize the caller's UNREAD backlog in a room.
//!
//! An AI-native convenience over the existing summarizer: instead of asking for
//! the last *N* messages (`POST /api/ai/summarize`), the caller asks "what did I
//! miss?" and the server sizes the window itself from the caller's own unread
//! count in that room. The count comes from the SAME sidebar-badge aggregate the
//! rest of the app uses
//! ([`MessageRepo::unread_counts_by_room`](aero_storage::MessageRepo::unread_counts_by_room)),
//! so it already excludes the caller's own messages and deleted ones and is
//! membership-scoped.
//!
//! Degrades exactly like `POST /api/ai/summarize`: `502` when no AI backend is
//! wired ([`AppState::ai`] is `None`), and when a backend *is* wired but no LLM
//! key is configured it echoes / heuristically summarizes. When the caller has
//! nothing unread the handler short-circuits with a fixed "all caught up" reply
//! and never spends an LLM call. Purely additive: a thin handler over existing
//! [`AppState`] state; no existing repo or service is touched. Mounted via
//! [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, RoomId};
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// All catch-me-up routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/rooms/:id/catchup", post(catchup))
}

/// Upper bound on how many unread messages a single catch-up summarizes. A large
/// backlog is clamped to this so the backend is always asked for a bounded,
/// sensible window (mirrors the history/search page caps).
const MAX_CATCHUP: usize = 200;

/// Clamp a raw unread count into the window the summarizer will read. A zero
/// unread count is handled by the caller (it returns the "all caught up" reply
/// without an LLM call), so this only ever sees `unread >= 1`; the result is
/// pinned into `[1, MAX_CATCHUP]`.
#[must_use]
fn catchup_window(unread: u32) -> usize {
    usize::try_from(unread).unwrap_or(MAX_CATCHUP).clamp(1, MAX_CATCHUP)
}

/// Parse a `RoomId` from a path segment, mapping a decode failure to `400`.
fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

/// `POST /api/rooms/:id/catchup` — summarize the caller's unread messages in the
/// room. Requires room access; when the caller is all caught up returns
/// `{ "summary": "You're all caught up.", "unread": 0 }` without invoking the AI
/// backend, otherwise `{ "summary": <text>, "unread": <n> }` where `n` is the
/// (clamped) number of unread messages summarized. `502` when no AI backend is
/// configured.
async fn catchup(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    // Tenant guard (workspace + room membership) before reading the caller's
    // unread backlog or invoking the AI backend over the room's messages.
    s.im.assert_room_access(auth.participant_id, room).await?;

    // The caller's unread count in THIS room. `unread_counts_by_room` only lists
    // rooms with at least one unread, so an absent room means zero unread.
    let unread = s
        .messages
        .unread_counts_by_room(auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .into_iter()
        .find_map(|(r, c)| (r == room).then_some(c))
        .unwrap_or(0);

    // Nothing to summarize — short-circuit before spending an LLM call.
    if unread == 0 {
        return Ok(Json(serde_json::json!({
            "summary": "You're all caught up.",
            "unread": 0,
        })));
    }

    let n = catchup_window(unread);
    // Degrade exactly like `POST /api/ai/summarize`: `502` when no AI backend is
    // wired; a wired backend echoes / heuristically summarizes when no LLM key
    // is configured.
    let ai = s
        .ai
        .as_ref()
        .ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let summary = ai
        .summarize_room(room, n)
        .await
        .map_err(|e| AeroError::Upstream(format!("ai: {e}")))?;
    Ok(Json(serde_json::json!({
        "summary": summary,
        "unread": n,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure window-sizing logic: a positive unread count passes through until it
    /// hits the cap, then saturates at `MAX_CATCHUP`. (Zero is special-cased by
    /// the handler and never reaches `catchup_window`.)
    #[test]
    fn catchup_window_clamps_into_bounds() {
        assert_eq!(catchup_window(1), 1);
        assert_eq!(catchup_window(50), 50);
        assert_eq!(catchup_window(200), MAX_CATCHUP);
        assert_eq!(catchup_window(10_000), MAX_CATCHUP);
        assert_eq!(catchup_window(u32::MAX), MAX_CATCHUP);
    }
}
