#![allow(unused_imports)]
//! Search route handlers.
use crate::error::ApiResult;
use crate::routes::helpers::{merge_hits, parse_room_id};
use crate::state::AppState;
use aero_auth::AuthUser;
use aero_common::{Error as AeroError, MessageId, ParticipantId, Result as AeroResult, RoomId};
use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use std::str::FromStr;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/rooms/:id/search", post(room_search))
}

// ----- Search -----

#[derive(Deserialize)]
pub(crate) struct SearchReq {
    query: String,
    #[serde(default)]
    limit: Option<i64>,
    /// "fts" (default) | "vector" | "auto"
    #[serde(default)]
    mode: Option<String>,
}

pub(crate) async fn room_search(
    State(s): State<AppState>,
    auth: AuthUser,
    headers: HeaderMap,
    Path(room_str): Path<String>,
    Json(req): Json<SearchReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&room_str)?;
    // Tenant guard (workspace + room membership) — supersedes the prior bare
    // room-membership check before searching the room's messages.
    s.im.assert_room_access(auth.participant_id, room).await?;
    // Tenant fairness (ROADMAP3 方向五): search is one of the most expensive
    // per-request PG paths, so it is a charged choke point.
    crate::ws_rate::check_ws_rate_room(&s, room).await?;
    if req.query.trim().is_empty() {
        return Err(AeroError::Invalid("empty query".into()).into());
    }
    let limit = req.limit.unwrap_or(20);
    let mode = req.mode.as_deref().unwrap_or("auto");

    let hits = match (mode, &s.ai) {
        ("vector", Some(ai)) => {
            let workspace = s
                .rooms
                .room_workspace(room)
                .await
                .map_err(AeroError::from)?
                .map(|value| value.to_uuid());
            let embedding = ai
                .embed_text_with_usage_context(
                    &req.query,
                    crate::ai_usage::request_usage_context(
                        &headers,
                        auth.participant_id,
                        workspace,
                        &format!("room_search_vector:{room}:{limit}:{}", req.query),
                    ),
                    "voyage_room_search_vector",
                )
                .await
                .map_err(|e| AeroError::Upstream(format!("ai embed: {e}")))?;
            s.messages
                .search_vector(room, embedding, limit)
                .await
                .map_err(AeroError::from)?
        }
        ("hybrid", Some(ai)) => {
            // FTS + vector fused with simple max-score merge.
            let mut fts = s
                .messages
                .search_fts(room, &req.query, limit)
                .await
                .map_err(AeroError::from)?;
            let workspace = s
                .rooms
                .room_workspace(room)
                .await
                .map_err(AeroError::from)?
                .map(|value| value.to_uuid());
            let embedding = ai
                .embed_text_with_usage_context(
                    &req.query,
                    crate::ai_usage::request_usage_context(
                        &headers,
                        auth.participant_id,
                        workspace,
                        &format!("room_search_hybrid:{room}:{limit}:{}", req.query),
                    ),
                    "voyage_room_search_hybrid",
                )
                .await;
            match embedding {
                Ok(embedding) => {
                    let vec_hits = s
                        .messages
                        .search_vector(room, embedding, limit)
                        .await
                        .map_err(AeroError::from)?;
                    fts = merge_hits(fts, vec_hits, limit);
                }
                Err(error) if error.contains("AI usage accounting:") => {
                    return Err(AeroError::Upstream(format!("ai embed: {error}")).into());
                }
                Err(error) => {
                    tracing::warn!(%error, %room, "hybrid search embedding failed; using FTS");
                }
            }
            fts
        }
        _ => s
            .messages
            .search_fts(room, &req.query, limit)
            .await
            .map_err(AeroError::from)?,
    };

    Ok(Json(serde_json::json!({
        "query": req.query,
        "mode": mode,
        "results": hits.into_iter().map(|h| {
            serde_json::json!({
                "score": h.score,
                "message": h.message,
            })
        }).collect::<Vec<_>>(),
    })))
}
