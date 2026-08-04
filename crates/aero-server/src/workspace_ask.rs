//! Workspace-wide RAG ask — "ask your workspace".
//!
//! The flagship enterprise-AI surface: instead of grounding an answer in a single
//! room (like `POST /api/ai/ask`), the caller asks a question across EVERY channel
//! they belong to within a workspace. Retrieval is a membership- and
//! workspace-bounded vector search
//! ([`MessageRepo::search_vector_workspace`](aero_storage::MessageRepo::search_vector_workspace)),
//! whose effective-access joins cover room/workspace membership, account status,
//! deactivation, and mandatory 2FA — so the answer cannot be grounded in revoked
//! or cross-tenant content. The retrieved context is then handed to the same LLM
//! answer step `POST /api/ai/ask` uses, via
//! [`AiBackend::answer_question_workspace`](crate::state::AiBackend).
//!
//! Effective-workspace-access-gated (via the shared
//! [`WorkspaceRepo`](aero_storage::WorkspaceRepo), mirroring
//! [`crate::saved_searches`]): membership alone is insufficient; a deleted or
//! workspace-deactivated account, or one that has not satisfied mandatory 2FA,
//! is denied before any embedding/model work. Degrades exactly like
//! `POST /api/ai/ask`: `502`
//! when no AI backend is wired, and an LLM-key-less backend returns the retrieved
//! context block as the answer. Mounted via [`routes`] and `.merge`d into the main
//! router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, WorkspaceId};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
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

/// Assert the caller has current effective access to the workspace, rejecting
/// non-members, deleted/deactivated accounts and unmet mandatory-2FA enrollment
/// with a `403`.
async fn assert_member(
    s: &AppState,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    s.workspaces
        .effective_member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("no effective workspace access".into()))?;
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
    headers: HeaderMap,
    Path(ws_str): Path<String>,
    Json(req): Json<WorkspaceAskReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_member(&s, ws, auth.participant_id).await?;

    let k = req.k.unwrap_or(DEFAULT_K);
    // Degrade exactly like `POST /api/ai/ask`: `502` when no AI backend is wired.
    let ai =
        s.ai.as_ref()
            .ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let usage_context = crate::ai_usage::request_usage_context(
        &headers,
        auth.participant_id,
        Some(ws.to_uuid()),
        &format!("workspace_ask:{ws}:{k}:{}", req.question),
    );
    let answer = ai
        .answer_question_workspace_with_usage_context(
            auth.participant_id,
            ws,
            &req.question,
            k,
            usage_context,
        )
        .await
        .map_err(|e| AeroError::Upstream(format!("ai: {e}")))?;

    Ok(Json(serde_json::json!({
        "answer": answer.answer,
        "citations": answer.citations,
    })))
}
