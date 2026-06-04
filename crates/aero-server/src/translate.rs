//! AI-native on-demand message translation.
//!
//! Translate any message's text into a target language. This reuses the exact
//! same [`AiBackend::translate`](crate::state::AiBackend::translate) seam that
//! powers live call-caption translation (P3): a thin HTTP shell resolves the
//! message + authorizes the caller, projects the message to plain text, and hands
//! it to the configured AI backend.
//!
//! Like every other AI capability (summarize / ask / moderate), this degrades
//! gracefully: when no LLM key is configured the backend echoes the source text,
//! so the route always returns a well-formed `{ original, translated }` payload —
//! the actual machine translation is a documented key-gated seam, not a separate
//! code path. When no AI service is wired at all (`AppState.ai == None`) it
//! returns `502 Upstream`, mirroring `ai_summarize` / `ai_ask`.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, MessageId};
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// Mount the translation route, folded into the gateway router by
/// [`crate::routes::build`].
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/messages/:id/translate", post(translate_message))
}

#[derive(Deserialize)]
struct TranslateReq {
    /// Target language — a BCP-47 tag or a language name the backend understands
    /// (e.g. `"es"`, `"zh-Hant"`, `"French"`). Trimmed; empty is rejected.
    target_lang: String,
}

/// `POST /api/messages/:id/translate` — translate a message's text into
/// `target_lang`.
///
/// Resolves the message (404 if unknown), authorizes the caller against the
/// message's room (`assert_room_access` → 403 for a non-member / cross-tenant),
/// projects the message to plain text via [`aero_common::Message::searchable_text`]
/// (400 if the message carries no translatable text — e.g. a bare file/card), then
/// runs it through the configured AI backend. Returns the original alongside the
/// translation so the client can show both.
async fn translate_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<TranslateReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let mid =
        MessageId::from_str(&id_str).map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let target = req.target_lang.trim();
    if target.is_empty() {
        return Err(AeroError::Invalid("target_lang must not be empty".into()).into());
    }

    let msg = s
        .messages
        .get(mid)
        .await?
        .ok_or_else(|| AeroError::NotFound(format!("message {mid}")))?;
    // Tenant + room-membership guard before exposing the message's content.
    s.im.assert_room_access(auth.participant_id, msg.room_id).await?;

    let source = msg.searchable_text();
    if source.trim().is_empty() {
        return Err(AeroError::Invalid("message has no translatable text".into()).into());
    }

    // Same backend seam as caption translation; `None` (no AI wired at all) is a
    // 502, matching the other AI routes. With a backend but no key, it echoes.
    let ai = s
        .ai
        .as_ref()
        .ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let translated = ai.translate(&source, target).await.map_err(AeroError::Upstream)?;

    Ok(Json(serde_json::json!({
        "message_id": mid,
        "target_lang": target,
        "original": source,
        "translated": translated,
    })))
}
