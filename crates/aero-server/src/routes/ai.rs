//! AI route handlers — summarize, ask, stream, context.
//!
//! Extracted from the monolithic `routes.rs` (ROADMAP 方向二: 路由拆分).

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, RoomId};
use axum::{
    extract::{Path as AxumPath, Query, State},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    routing::post,
    Json, Router,
};
use futures::StreamExt as _;
use serde::Deserialize;

use crate::error::ApiResult;
use crate::routes::helpers::parse_room_id;
use crate::state::AppState;

/// Mount AI routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/ai/summarize", post(ai_summarize))
        .route("/api/ai/ask", post(ai_ask))
        .route("/api/ai/ask/stream", post(ai_ask_stream))
        .route("/api/ai/ask/context", post(ai_ask_context))
}

#[derive(Deserialize)]
struct AiSummarizeReq {
    room_id: String,
    #[serde(default)]
    last_n: Option<usize>,
}

async fn ai_summarize(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<AiSummarizeReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&req.room_id)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let last_n = req.last_n.unwrap_or(50);
    let ai = s.ai.as_ref().ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let summary = ai
        .summarize_room(room, last_n)
        .await
        .map_err(|e| AeroError::Upstream(format!("ai: {e}")))?;
    Ok(Json(serde_json::json!({"summary": summary})))
}

#[derive(Deserialize)]
struct AiAskReq {
    room_id: String,
    question: String,
    #[serde(default)]
    k: Option<usize>,
    #[serde(default)]
    agentic: Option<bool>,
}

async fn ai_ask(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<AiAskReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&req.room_id)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let k = req.k.unwrap_or(8);
    let ai = s.ai.as_ref().ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let answer = if req.agentic.unwrap_or(false) && std::env::var("AERO_AGENTIC_ANSWERS").is_ok() {
        ai.answer_question_agentic(room, &req.question, 4)
            .await.map_err(|e| AeroError::Upstream(format!("ai: {e}")))?
    } else {
        ai.answer_question(room, &req.question, k)
            .await.map_err(|e| AeroError::Upstream(format!("ai: {e}")))?
    };
    Ok(Json(serde_json::json!({"answer": answer.answer, "citations": answer.citations})))
}

async fn ai_ask_stream(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<AiAskReq>,
) -> impl IntoResponse {
    let room = match parse_room_id(&req.room_id) {
        Ok(r) => r,
        Err(e) => return crate::error::ApiError::from(e).into_response(),
    };
    if let Err(e) = s.im.assert_room_access(auth.participant_id, room).await {
        return crate::error::ApiError::from(e).into_response();
    }
    let k = req.k.unwrap_or(8);
    let Some(ai) = s.ai.as_ref() else {
        return Sse::new(futures::stream::once(async {
            Ok::<_, std::convert::Infallible>(Event::default().event("done").data("AI not configured"))
        })).into_response();
    };
    let ai = ai.clone();
    let (citations, text_stream) = match ai.answer_question_stream(room, &req.question, k).await {
        Ok(v) => v,
        Err(e) => return crate::error::ApiError::from(AeroError::Upstream(format!("ai: {e}"))).into_response(),
    };
    let citations_json = serde_json::to_string(&citations.iter().map(ToString::to_string).collect::<Vec<_>>())
        .unwrap_or_else(|_| "[]".to_string());
    let event_stream = futures::stream::once(async {
        Ok::<_, std::convert::Infallible>(Event::default().event("citations").data(citations_json))
    })
    .chain(text_stream.map(|r| match r {
        Ok(text) => Ok(Event::default().event("delta").data(text)),
        Err(e) => { tracing::warn!(error = %e, "ai stream chunk"); Ok(Event::default().event("error").data(e)) }
    }))
    .chain(futures::stream::once(async {
        Ok::<_, std::convert::Infallible>(Event::default().event("done").data(""))
    }));
    Sse::new(event_stream).keep_alive(KeepAlive::default()).into_response()
}

async fn ai_ask_context(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<AiAskReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&req.room_id)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let k = req.k.unwrap_or(8);
    let ai = s.ai.as_ref().ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let answer = ai.ask_with_context(auth.participant_id, room, &req.question, k)
        .await.map_err(|e| AeroError::Upstream(format!("ai: {e}")))?;
    Ok(Json(serde_json::json!({"answer": answer.answer, "citations": answer.citations})))
}
