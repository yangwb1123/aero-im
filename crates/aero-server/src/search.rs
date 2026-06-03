//! Cross-room (workspace-wide) message search — the global counterpart to the
//! per-room `POST /api/rooms/:id/search` in [`crate::routes`].
//!
//! Searches every room the caller belongs to (Slack-style global search). The
//! membership scoping is enforced in SQL by
//! [`MessageRepo::search_all_rooms`](aero_storage::MessageRepo::search_all_rooms)
//! (a `JOIN room_members`), so results can never leak a room the caller isn't in
//! — there is no post-filter to get wrong. Mounted via [`routes`] and `.merge`d
//! into the gateway router, mirroring [`crate::collab`].

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, WorkspaceId};
use axum::{extract::State, routing::post, Json, Router};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// The cross-room search route, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/search", post(search_all))
}

#[derive(Deserialize)]
struct SearchAllReq {
    query: String,
    #[serde(default)]
    limit: Option<i64>,
    /// Optional tenant scope. When present, only the caller's rooms in that
    /// workspace are searched (`search_all_rooms_in_workspace`); absent ⇒ every
    /// room the caller belongs to, across tenants.
    #[serde(default)]
    workspace_id: Option<String>,
}

/// `POST /api/search` — full-text/trigram search across all rooms the caller is
/// a member of. The repository's `JOIN room_members` is the security boundary;
/// no room the caller isn't in can appear in the results.
async fn search_all(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<SearchAllReq>,
) -> ApiResult<Json<serde_json::Value>> {
    if req.query.trim().is_empty() {
        return Err(AeroError::Invalid("empty query".into()).into());
    }
    let limit = req.limit.unwrap_or(20);

    let hits = match req.workspace_id.as_deref() {
        Some(raw) => {
            let ws = WorkspaceId::from_str(raw.trim())
                .map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))?;
            s.messages
                .search_all_rooms_in_workspace(auth.participant_id, ws, &req.query, limit)
                .await
                .map_err(AeroError::from)?
        }
        None => s
            .messages
            .search_all_rooms(auth.participant_id, &req.query, limit)
            .await
            .map_err(AeroError::from)?,
    };

    Ok(Json(serde_json::json!({
        "query": req.query,
        "results": hits.into_iter().map(|h| {
            serde_json::json!({
                "score": h.score,
                "message": h.message,
            })
        }).collect::<Vec<_>>(),
    })))
}
