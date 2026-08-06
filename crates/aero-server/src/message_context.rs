//! Message permalink / jump-to-message context (Slack "jump to message").
//!
//! When a client follows a permalink to a single message, it wants not just that
//! message but the surrounding conversation so the message can be rendered in
//! context. This module exposes the READ side: resolve the target message
//! (`404` if unknown / deleted), assert the caller may see its room (`403` for
//! non-members), then return the target plus a centered window of the messages
//! immediately before and after it.
//!
//! Thin handler over [`MessageRepo`](aero_storage::MessageRepo): it resolves the
//! message via `get` (so the room can be derived and access membership-gated),
//! asserts room access via the shared
//! [`ImService::assert_room_access`](aero_im_core::ImService::assert_room_access)
//! (the same workspace + room membership guard `crate::routes` uses), then reads
//! the window via
//! [`MessageRepo::messages_around`](aero_storage::MessageRepo::messages_around).
//! Mounted via [`routes`] and `.merge`d into the main router. Mirrors the
//! structure of [`crate::message_history`].

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, Message, MessageId};
use aero_storage::MessageRepo;
use axum::{
    extract::{Path, Query, State},
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// Default half-window when the caller omits `?window=`: 25 messages on each
/// side of the target, matching a comfortable "jump to message" viewport.
const DEFAULT_WINDOW: i64 = 25;

/// All message-context routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/messages/:id/context", get(message_context))
}

fn parse_message(s: &str) -> Result<MessageId, AeroError> {
    MessageId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("message id: {e}")))
}

#[derive(Debug, Deserialize)]
struct ContextQuery {
    /// Half-window: how many messages to fetch on each side of the target. The
    /// repo clamps this to `[1, 100]`; omitted defaults to [`DEFAULT_WINDOW`].
    window: Option<i64>,
    /// `eventual` explicitly routes only the surrounding window to a configured
    /// replica. Omitted/`strong` keeps the window on primary.
    consistency: Option<String>,
}

fn context_consistency(raw: Option<&str>) -> Result<aero_storage::QueryConsistency, AeroError> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None | Some("strong") => Ok(aero_storage::QueryConsistency::Strong),
        Some("eventual") => Ok(aero_storage::QueryConsistency::Eventual),
        Some(other) => Err(AeroError::Invalid(format!(
            "consistency must be `strong` or `eventual`, got `{other}`"
        ))),
    }
}

fn target_is_visible(message: &Message, now: time::OffsetDateTime) -> bool {
    message.deleted_at.is_none()
        && message
            .expires_at
            .map_or(true, |expires_at| expires_at > now)
}

fn context_response_value(
    target: &Message,
    messages: &[Message],
    has_more: bool,
) -> serde_json::Value {
    serde_json::json!({
        "target": target,
        "messages": messages,
        "has_more": has_more,
    })
}

/// `GET /api/messages/:id/context?window=N&consistency=eventual` — the target
/// message plus a centered window before and after it (Slack "jump to message").
/// Target resolution and authorization always use primary. The surrounding
/// window defaults to `strong`; callers that explicitly tolerate replication
/// lag may pass `consistency=eventual`. `404` means the target is unknown,
/// deleted, or expired; `403` means the caller cannot access its room. Returns
/// `{ "target": <msg>, "messages": [<window before..target..after>], "has_more": bool }`.
async fn message_context(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<ContextQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_message(&id_str)?;
    // Resolve the message first so access can be membership-gated; an unknown,
    // soft-deleted, or expired message is a `404`. This lookup and the security
    // decision deliberately use primary.
    let target = s
        .messages
        .get(id)
        .await
        .map_err(AeroError::from)?
        .filter(|message| target_is_visible(message, time::OffsetDateTime::now_utc()))
        .ok_or_else(|| AeroError::NotFound(format!("message {id}")))?;
    // Workspace + room membership guard (read-only; `ImService` is not mutated).
    s.im.assert_room_access(auth.participant_id, target.room_id)
        .await?;
    let half_window = q.window.unwrap_or(DEFAULT_WINDOW);
    let consistency = context_consistency(q.consistency.as_deref())?;
    let read_repo = MessageRepo::new(s.query_router.repo_pool(consistency));
    let (messages, has_more) = match read_repo
        .messages_around(target.room_id, id, half_window)
        .await
    {
        Ok(messages) => messages,
        Err(error)
            if consistency == aero_storage::QueryConsistency::Eventual
                && s.query_router.has_replica() =>
        {
            tracing::warn!(
                room = %target.room_id,
                ?error,
                "message context replica failed; retrying on primary"
            );
            s.messages
                .messages_around(target.room_id, id, half_window)
                .await
                .map_err(AeroError::from)?
        }
        Err(error) => return Err(AeroError::from(error).into()),
    };
    Ok(Json(context_response_value(&target, &messages, has_more)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::{ParticipantId, RoomId};
    use time::macros::datetime;

    fn message(
        deleted_at: Option<time::OffsetDateTime>,
        expires_at: Option<time::OffsetDateTime>,
    ) -> Message {
        Message {
            id: MessageId::new(),
            room_id: RoomId::new(),
            sender_id: ParticipantId::new(),
            blocks: Vec::new(),
            reply_to: None,
            metadata: serde_json::json!({}),
            created_at: datetime!(2026-01-01 00:00:00 UTC),
            edited_at: None,
            deleted_at,
            recalled_at: None,
            recalled_by: None,
            expires_at,
            version: 1,
        }
    }

    #[test]
    fn context_defaults_to_strong_but_accepts_eventual_opt_in() {
        assert_eq!(
            context_consistency(None).unwrap(),
            aero_storage::QueryConsistency::Strong
        );
        assert_eq!(
            context_consistency(Some("strong")).unwrap(),
            aero_storage::QueryConsistency::Strong
        );
        assert_eq!(
            context_consistency(Some(" eventual ")).unwrap(),
            aero_storage::QueryConsistency::Eventual
        );
        assert!(context_consistency(Some("session")).is_err());
    }

    #[test]
    fn target_visibility_rejects_deleted_and_expired_messages() {
        let now = datetime!(2026-01-02 00:00:00 UTC);
        assert!(target_is_visible(&message(None, None), now));
        assert!(target_is_visible(
            &message(None, Some(datetime!(2026-01-02 00:00:01 UTC))),
            now
        ));
        assert!(!target_is_visible(
            &message(None, Some(datetime!(2026-01-02 00:00:00 UTC))),
            now
        ));
        assert!(!target_is_visible(
            &message(None, Some(datetime!(2026-01-01 23:59:59 UTC))),
            now
        ));
        assert!(!target_is_visible(
            &message(Some(datetime!(2026-01-01 12:00:00 UTC)), None),
            now
        ));
    }

    #[test]
    fn context_response_keeps_messages_as_an_array_and_exposes_has_more() {
        let target = message(None, None);
        let context = message(None, None);
        let value = context_response_value(&target, std::slice::from_ref(&context), true);

        assert!(value["messages"].is_array());
        assert_eq!(value["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(value["has_more"], serde_json::Value::Bool(true));
    }
}
