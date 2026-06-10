//! Thread-scoped AI summarization — "summarize this thread".
//!
//! An AI-native convenience over the room summarizer ([`crate::catchup`] /
//! `POST /api/ai/summarize`): instead of summarizing a room's recent window, the
//! caller points at a thread's ROOT message and gets back a bullet summary of the
//! reply chain hanging off it. The reply set is the SAME flat-thread set the
//! "N replies" affordance counts
//! ([`MessageRepo::thread_replies`](aero_storage::MessageRepo::thread_replies)), so
//! it already excludes deleted replies and is keyset-paginated.
//!
//! The root message is resolved first (`404` if it doesn't exist), then access is
//! asserted against the root's room ([`assert_room_access`](aero_im_core::ImService::assert_room_access))
//! before the AI backend ever sees the thread. Degrades exactly like
//! `POST /api/ai/summarize`: `502` when no AI backend is wired ([`AppState::ai`] is
//! `None`), and when a backend IS wired but no LLM key is configured it falls back
//! to the deterministic heuristic summary. A thread with no replies short-circuits
//! with an empty summary and never spends an LLM call. Purely additive: a thin
//! handler over existing [`AppState`] state; no existing repo or service is touched.
//! Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, MessageId};
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// All thread-summary routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/messages/:id/thread-summary", post(thread_summary))
}

/// Upper bound on how many replies a single thread summary reads. A large thread
/// is clamped to this so the backend is asked for a bounded window (mirrors the
/// history/summarize page caps).
const MAX_REPLIES: usize = 200;

/// Parse a `MessageId` from a path segment, mapping a decode failure to `400`.
fn parse_message(s: &str) -> Result<MessageId, AeroError> {
    MessageId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("message id: {e}")))
}

/// `POST /api/messages/:id/thread-summary` — summarize the thread rooted at the
/// given message. Resolves the root (`404` if missing), asserts access to its room,
/// then summarizes the reply chain. Returns
/// `{ "summary": <text>, "root_id": <id>, "replies": <n> }` where `n` is the
/// (clamped) reply window summarized; an empty thread returns an empty summary
/// without an LLM call. `502` when no AI backend is configured; degrades to the
/// backend's heuristic summary when a backend is wired but no LLM key is set.
async fn thread_summary(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let root_id = parse_message(&id_str)?;

    // Resolve the root message to learn its room, then gate on room access — the
    // tenant + membership guard runs before the AI backend reads the thread.
    let root = s
        .messages
        .get(root_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("message {root_id}")))?;
    s.im.assert_room_access(auth.participant_id, root.room_id).await?;

    // Degrade exactly like `POST /api/ai/summarize`: `502` when no AI backend is
    // wired; a wired backend heuristically summarizes when no LLM key is set. An
    // empty thread returns an empty summary (the backend short-circuits).
    let ai = s
        .ai
        .as_ref()
        .ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let summary = ai
        .summarize_thread(root_id, MAX_REPLIES)
        .await
        .map_err(|e| AeroError::Upstream(format!("ai: {e}")))?;

    Ok(Json(serde_json::json!({
        "summary": summary,
        "root_id": root_id,
        "replies": MAX_REPLIES,
    })))
}
