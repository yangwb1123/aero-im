//! Workspace-wide RAG ask — "ask your workspace".
//!
//! The flagship enterprise-AI surface: instead of grounding an answer in a single
//! room (like `POST /api/ai/ask`), the caller asks a question across EVERY channel
//! they belong to within a workspace. Retrieval is a membership- and
//! workspace-bounded vector search
//! ([`MessageRepo::search_vector_workspace`](aero_storage::MessageRepo::search_vector_workspace)),
//! whose `JOIN room_members` is the security boundary — so the answer can never be
//! grounded in a room the caller isn't in, nor in another tenant's channels. The
//! retrieved context is then handed to the same LLM answer step `POST /api/ai/ask`
//! uses, via [`AiBackend::answer_question_workspace`](crate::state::AiBackend).
//!
//! Workspace-member-gated (via the shared
//! [`WorkspaceRepo`](aero_storage::WorkspaceRepo), mirroring
//! [`crate::saved_searches`]). Degrades exactly like `POST /api/ai/ask`: `502`
//! when no AI backend is wired, and an LLM-key-less backend returns the retrieved
//! context block as the answer. Mounted via [`routes`] and `.merge`d into the main
//! router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, WorkspaceId};
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// The workspace-ask route, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/workspaces/:id/ask", post(workspace_ask))
}

/// Default number of context hits to retrieve when the caller omits `k`
/// (mirrors `POST /api/ai/ask`).
const DEFAULT_K: usize = 8;

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

/// Assert the caller is a member of the workspace, rejecting non-members with a
/// `403`. Mirrors `crate::saved_searches::assert_member`.
async fn assert_member(
    s: &AppState,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    s.workspaces
        .member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))?;
    Ok(())
}

#[derive(Deserialize)]
struct WorkspaceAskReq {
    /// The natural-language question to answer across the caller's channels.
    question: String,
    /// Optional retrieval breadth (top-k context hits); defaults to [`DEFAULT_K`].
    #[serde(default)]
    k: Option<usize>,
}

/// `POST /api/workspaces/:id/ask` — answer `question` grounded in every channel
/// the caller belongs to in this workspace. Members only (`403` otherwise).
/// Returns `{ answer, citations }` exactly like `POST /api/ai/ask`; `502` when no
/// AI backend is configured.
async fn workspace_ask(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Json(req): Json<WorkspaceAskReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_member(&s, ws, auth.participant_id).await?;

    let k = req.k.unwrap_or(DEFAULT_K);
    // Degrade exactly like `POST /api/ai/ask`: `502` when no AI backend is wired.
    let ai = s
        .ai
        .as_ref()
        .ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let answer = ai
        .answer_question_workspace(auth.participant_id, ws, &req.question, k)
        .await
        .map_err(|e| AeroError::Upstream(format!("ai: {e}")))?;

    Ok(Json(serde_json::json!({
        "answer": answer.answer,
        "citations": answer.citations,
    })))
}
