//! AI smart replies — suggest a few short reply options for a room.
//!
//! An AI-native convenience over the existing RAG path: instead of asking a
//! free-form question (`POST /api/ai/ask`), the caller asks the server to draft a
//! handful of short reply options they could send next. The server poses a fixed
//! prompt to the SAME retrieval-augmented backend
//! ([`AiBackend::answer_question`](crate::state::AiBackend)) over the room's
//! recent messages, so the suggestions are grounded in the actual conversation —
//! and the returned `citations` point at the messages they were drawn from.
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
    http::HeaderMap,
    routing::post,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All smart-reply routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/rooms/:id/suggest-replies", post(suggest_replies))
}

/// Default retrieval window (recent messages fed to the RAG backend) when the
/// caller omits `k`. Mirrors the modest default the rest of the AI surface uses
/// while staying well within the clamp below.
const DEFAULT_K: usize = 12;

/// The fixed prompt that turns the RAG backend into a reply suggester: ground the
/// answer in the recent conversation and emit exactly three brief options as a
/// markdown numbered list, no preamble.
const SUGGEST_PROMPT: &str = "Based on the recent conversation, suggest exactly 3 brief, distinct reply options the current user could send next. Return them as a markdown numbered list (1., 2., 3.), each one sentence, no preamble.";

/// Clamp a requested retrieval window into the bounds the backend is asked for.
/// Pure, so the cap/floor is unit-tested offline (no AI backend or database).
#[must_use]
fn suggest_k(requested: Option<usize>) -> usize {
    requested.unwrap_or(DEFAULT_K).clamp(1, 40)
}

/// Parse a `RoomId` from a path segment, mapping a decode failure to `400`.
fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

#[derive(Deserialize)]
struct SuggestRepliesReq {
    /// How many recent messages to retrieve for grounding (clamped to `[1, 40]`).
    #[serde(default)]
    k: Option<usize>,
}

/// `POST /api/rooms/:id/suggest-replies` — draft a few short reply options the
/// caller could send next, grounded in the room's recent conversation. Requires
/// room access. Returns `{ "suggestions": <markdown text>, "citations": [...] }`
/// where `suggestions` is a markdown numbered list of three options and
/// `citations` point at the messages they were drawn from. `502` when no AI
/// backend is configured; with a backend but no LLM key the suggestions fall back
/// to the backend's heuristic answer (exactly like `POST /api/ai/ask`).
async fn suggest_replies(
    State(s): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(room_str): Path<String>,
    Json(req): Json<SuggestRepliesReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    // Tenant guard (workspace + room membership) before reading the room's
    // messages or invoking the AI backend over them.
    s.im.assert_room_access(auth.participant_id, room).await?;

    let k = suggest_k(req.k);
    // Degrade exactly like `POST /api/ai/ask`: `502` when no AI backend is wired;
    // a wired backend answers heuristically when no LLM key is configured.
    let ai =
        s.ai.as_ref()
            .ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let usage_context = crate::ai_usage::room_request_usage_context(
        &s,
        &headers,
        auth.participant_id,
        room,
        &format!("suggest_replies:{room}:{k}"),
    )
    .await?;
    let answer = ai
        .answer_question_with_usage_context(room, SUGGEST_PROMPT, k, usage_context)
        .await
        .map_err(|e| AeroError::Upstream(format!("ai: {e}")))?;
    Ok(Json(serde_json::json!({
        "suggestions": answer.answer,
        "citations": answer.citations,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure window-sizing logic: an absent `k` uses the default; a request passes
    /// through until it hits the bounds, then saturates at the `[1, 40]` clamp.
    #[test]
    fn suggest_k_clamps_into_bounds() {
        assert_eq!(suggest_k(None), DEFAULT_K);
        assert_eq!(suggest_k(Some(0)), 1);
        assert_eq!(suggest_k(Some(1)), 1);
        assert_eq!(suggest_k(Some(12)), 12);
        assert_eq!(suggest_k(Some(40)), 40);
        assert_eq!(suggest_k(Some(41)), 40);
        assert_eq!(suggest_k(Some(10_000)), 40);
    }
}
