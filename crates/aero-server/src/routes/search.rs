//! Search route handlers.
use std::str::FromStr;
use axum::{extract::{Path, Query, State}, routing::{get, post}, Json, Router};
use aero_auth::AuthUser;
use aero_common::{Error as AeroError, MessageId, ParticipantId, RoomId, Result as AeroResult};
use serde::Deserialize;
use crate::error::ApiResult;
use crate::routes::helpers::{merge_hits, parse_room_id};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/rooms/:id/search", post(room_search))
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
            let embedding = ai
                .embed_text(&req.query)
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
            if let Ok(embedding) = ai.embed_text(&req.query).await {
                let vec_hits = s
                    .messages
                    .search_vector(room, embedding, limit)
                    .await
                    .map_err(AeroError::from)?;
                fts = merge_hits(fts, vec_hits, limit);
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

