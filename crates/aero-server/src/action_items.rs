//! AI action-item extraction — pull the tasks / to-dos out of a channel.
//!
//! An AI-native convenience over the existing RAG path: instead of asking a free
//! form question (`POST /api/ai/ask`), the caller asks the server to mine a
//! channel for everything actionable — action items, tasks, decisions, and
//! to-dos — and gets back a concise markdown bullet list with the responsible
//! person where stated. It reuses the SAME retrieval-augmented
//! [`AiBackend::answer_question`](crate::state::AiBackend) call `POST /api/ai/ask`
//! uses (a fixed extraction prompt over the room's top-`k` relevant messages), so
//! there is no `AiBackend` trait change and the returned `citations` point back at
//! the source messages.
//!
//! Degrades exactly like `POST /api/ai/ask`: `502` when no AI backend is wired
//! ([`AppState::ai`] is `None`), and when a backend *is* wired but no LLM key is
//! configured it falls back to the backend's heuristic answer. Purely additive: a
//! thin handler over existing [`AppState`] state; no existing repo or service is
//! touched. Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, RoomId};
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All action-item routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/rooms/:id/action-items", post(action_items))
}

/// Default number of relevant messages retrieved when the caller omits `k`.
const DEFAULT_K: usize = 20;
/// Hard ceiling on the retrieval window, mirroring the search/RAG page caps.
const MAX_K: usize = 50;

/// The fixed extraction prompt fed to the RAG backend. Phrased so the model
/// returns a self-contained markdown bullet list (or an exact sentinel when the
/// channel holds nothing actionable).
const EXTRACT_PROMPT: &str = "Extract every action item, task, decision, and to-do mentioned in this channel as a concise markdown bullet list, each with the responsible person if stated. If there are none, reply exactly: No action items found.";

/// Clamp a requested retrieval window into `[1, MAX_K]`, defaulting to
/// [`DEFAULT_K`] when absent. Pure, so the cap/floor is unit-tested offline
/// (Postgres / AI backend absent in CI).
#[must_use]
fn action_item_k(requested: Option<usize>) -> usize {
    requested.unwrap_or(DEFAULT_K).clamp(1, MAX_K)
}

/// Parse a `RoomId` from a path segment, mapping a decode failure to `400`.
fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

#[derive(Deserialize)]
struct ActionItemsReq {
    /// How many relevant messages to retrieve for the extraction. Absent ⇒
    /// [`DEFAULT_K`]; clamped into `[1, MAX_K]`.
    #[serde(default)]
    k: Option<usize>,
}

/// `POST /api/rooms/:id/action-items` — extract the channel's action items,
/// tasks, decisions, and to-dos as a markdown bullet list. Requires room access.
/// Returns `{ "action_items": <markdown>, "citations": [<message id>, …] }`,
/// where `citations` are the source messages the extraction drew on. `502` when
/// no AI backend is configured; degrades to the backend's heuristic answer when a
/// backend is wired but no LLM key is set (exactly like `POST /api/ai/ask`).
async fn action_items(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<ActionItemsReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    // Tenant guard (workspace + room membership) before invoking the AI backend
    // over the room's messages — defense-in-depth on RAG output.
    s.im.assert_room_access(auth.participant_id, room).await?;

    let k = action_item_k(req.k);
    // Degrade exactly like `POST /api/ai/ask`: `502` when no AI backend is wired;
    // a wired backend falls back to its heuristic answer when no LLM key is set.
    let ai = s
        .ai
        .as_ref()
        .ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let answer = ai
        .answer_question(room, EXTRACT_PROMPT, k)
        .await
        .map_err(|e| AeroError::Upstream(format!("ai: {e}")))?;
    Ok(Json(serde_json::json!({
        "action_items": answer.answer,
        "citations": answer.citations,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure window-sizing logic: an absent `k` defaults to [`DEFAULT_K`]; a present
    /// value passes through until it hits the cap, then saturates at [`MAX_K`], and
    /// a zero floors to `1`.
    #[test]
    fn action_item_k_clamps_into_bounds() {
        assert_eq!(action_item_k(None), DEFAULT_K);
        assert_eq!(action_item_k(Some(0)), 1);
        assert_eq!(action_item_k(Some(1)), 1);
        assert_eq!(action_item_k(Some(20)), 20);
        assert_eq!(action_item_k(Some(50)), MAX_K);
        assert_eq!(action_item_k(Some(10_000)), MAX_K);
    }
}
