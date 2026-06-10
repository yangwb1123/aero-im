//! Find-expert — "who knows about X in this workspace?".
//!
//! An AI-native discovery surface over the workspace RAG path: instead of asking a
//! question, the caller names a TOPIC and gets back a ranked list of the workspace
//! members who have said the most relevant things about it. Retrieval is the SAME
//! membership- and workspace-bounded vector search the workspace ask uses
//! ([`MessageRepo::search_vector_workspace`](aero_storage::MessageRepo::search_vector_workspace)),
//! whose `JOIN room_members` is the security boundary — so a candidate can never be
//! ranked on a message in a room the caller isn't in, nor in another tenant's
//! channels. The hits are aggregated by author (summed relevance + a few citation
//! message ids) in the AI service.
//!
//! Workspace-member-gated (via the shared
//! [`WorkspaceRepo`](aero_storage::WorkspaceRepo), mirroring [`crate::workspace_ask`]):
//! `403` for a non-member. NO LLM call — purely retrieval + aggregation — so it
//! never errors on a missing LLM key: without embeddings it degrades to whatever
//! the (deterministic hash) embedder + vector index return, which is an empty list
//! rather than an error. `502` only when no AI backend is wired at all. Mounted via
//! [`routes`] and `.merge`d into the main router.

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

/// The find-expert route, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/workspaces/:id/find-expert", post(find_expert))
}

/// Default number of experts returned when the caller omits `k`.
const DEFAULT_K: usize = 5;
/// Hard ceiling on how many experts are returned.
const MAX_K: usize = 25;

/// Clamp a requested expert count into `[1, MAX_K]`, defaulting to [`DEFAULT_K`]
/// when absent. Pure, so the cap/floor is unit-tested offline.
#[must_use]
fn expert_k(requested: Option<usize>) -> usize {
    requested.unwrap_or(DEFAULT_K).clamp(1, MAX_K)
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

/// Assert the caller is a member of the workspace, rejecting non-members with a
/// `403`. Mirrors [`crate::workspace_ask`]'s membership gate.
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
struct FindExpertReq {
    /// The topic to find experts on. Trimmed; an empty/blank topic is `400`.
    topic: String,
    /// Optional number of experts to return; absent ⇒ [`DEFAULT_K`], clamped into
    /// `[1, MAX_K]`.
    #[serde(default)]
    k: Option<usize>,
}

/// `POST /api/workspaces/:id/find-expert` — rank the workspace members who have
/// said the most relevant things about `topic`. Members only (`403` otherwise);
/// a blank `topic` is `400`. Returns
/// `{ "experts": [{ "participant_id", "score", "citation_ids": [...] }, ...] }`
/// ordered strongest-first. Degrades to an empty list (never an error) without
/// embeddings; `502` only when no AI backend is wired.
async fn find_expert(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(ws_str): Path<String>,
    Json(req): Json<FindExpertReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&ws_str)?;
    assert_member(&s, ws, auth.participant_id).await?;

    let topic = req.topic.trim();
    if topic.is_empty() {
        return Err(AeroError::Invalid("topic must not be empty".into()).into());
    }
    let k = expert_k(req.k);

    // `502` only when no AI backend is wired; with a backend (even key-less) the
    // call runs through the deterministic embedder + vector index and returns a
    // (possibly empty) ranked list rather than erroring.
    let ai = s
        .ai
        .as_ref()
        .ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let experts = ai
        .find_expert(auth.participant_id, ws, topic, k)
        .await
        .map_err(|e| AeroError::Upstream(format!("ai: {e}")))?;

    let experts_json: Vec<serde_json::Value> = experts
        .into_iter()
        .map(|e| {
            serde_json::json!({
                "participant_id": e.participant,
                "score": e.score,
                "citation_ids": e.citations,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({ "experts": experts_json })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure expert-count logic: an absent `k` defaults to [`DEFAULT_K`]; a present
    /// value passes through until it hits the cap, then saturates at [`MAX_K`], and
    /// a zero floors to `1`.
    #[test]
    fn expert_k_clamps_into_bounds() {
        assert_eq!(expert_k(None), DEFAULT_K);
        assert_eq!(expert_k(Some(0)), 1);
        assert_eq!(expert_k(Some(1)), 1);
        assert_eq!(expert_k(Some(5)), 5);
        assert_eq!(expert_k(Some(25)), MAX_K);
        assert_eq!(expert_k(Some(10_000)), MAX_K);
    }
}
