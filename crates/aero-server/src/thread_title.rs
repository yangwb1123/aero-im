//! Thread auto-titling — "name this thread".
//!
//! An AI-native convenience alongside the thread summarizer
//! ([`crate::thread_summarize`]): instead of a bullet summary of a thread's reply
//! chain, the caller points at a thread's ROOT message and gets back a short
//! (5-10 word) title concisely naming the topic. The title is anchored on the root
//! message; the reply chain (the SAME flat-thread set the "N replies" affordance
//! counts —
//! [`MessageRepo::thread_replies`](aero_storage::MessageRepo::thread_replies),
//! deleted replies excluded) is supporting context.
//!
//! The root message is resolved first (`404` if it doesn't exist), then access is
//! asserted against the root's room
//! ([`assert_room_access`](aero_im_core::ImService::assert_room_access)) before the
//! AI backend ever sees the thread. Degrades exactly like
//! `POST /api/messages/:id/thread-summary`: `502` when no AI backend is wired
//! ([`AppState::ai`] is `None`), and when a backend IS wired but no LLM key is
//! configured it falls back to the deterministic heuristic title (the first ~8
//! words of the root message) — so the route is verifiable as 200-with-heuristic.
//! Purely additive: a thin handler over existing [`AppState`] state; no existing
//! repo or service is touched. Mounted via [`routes`] and `.merge`d into the main
//! router.

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

/// All thread-title routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/messages/:id/thread-title", post(thread_title))
}

/// Upper bound on how many replies inform a single thread title. A large thread is
/// clamped to this so the backend is asked for a bounded window (mirrors
/// [`crate::thread_summarize`]).
const MAX_REPLIES: usize = 200;

/// Parse a `MessageId` from a path segment, mapping a decode failure to `400`.
fn parse_message(s: &str) -> Result<MessageId, AeroError> {
    MessageId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("message id: {e}")))
}

/// `POST /api/messages/:id/thread-title` — generate a short title for the thread
/// rooted at the given message. Resolves the root (`404` if missing), asserts
/// access to its room, then asks the AI backend for a title. Returns
/// `{ "title": <text>, "root_id": <id> }`. `502` when no AI backend is configured;
/// degrades to the backend's heuristic title (first ~8 words of the root) when a
/// backend is wired but no LLM key is set.
async fn thread_title(
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

    // Degrade exactly like `POST /api/messages/:id/thread-summary`: `502` when no AI
    // backend is wired; a wired backend heuristically titles when no LLM key is set.
    let ai = s
        .ai
        .as_ref()
        .ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let title = ai
        .generate_thread_title(root_id, MAX_REPLIES)
        .await
        .map_err(|e| AeroError::Upstream(format!("ai: {e}")))?;

    Ok(Json(serde_json::json!({
        "title": title,
        "root_id": root_id,
    })))
}

#[cfg(test)]
mod db_tests {
    //! Live-DB tests for thread auto-titling. They COMPILE here but only RUN
    //! against a live Postgres (the integrator runs them). They exercise the
    //! degrade-safe path: with NO Anthropic key configured the handler logic must
    //! return a non-empty heuristic title derived from the root message.

    use aero_ai::AiService;
    use aero_common::{Block, ParticipantId, RoomId, WorkspaceId};
    use aero_storage::{AiJobRepo, MessageRepo, NewMessage, RoomRepo};
    use sqlx::postgres::PgPoolOptions;

    async fn pool() -> aero_storage::PgPool {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        PgPoolOptions::new().max_connections(2).connect(&url).await.expect("connect")
    }

    // Seed a throwaway participant + room (all-zero default workspace) so a message
    // insert satisfies its participants/rooms foreign keys.
    async fn seed(pg: &aero_storage::PgPool) -> (RoomId, ParticipantId) {
        let sender = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(sender.to_uuid())
            .bind(format!("title-actor-{sender}"))
            .execute(pg)
            .await
            .expect("insert participant");
        let room = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id) VALUES ($1,'group',$2,$3,$4)",
        )
        .bind(room.to_uuid())
        .bind(format!("title-room-{room}"))
        .bind(sender.to_uuid())
        .bind(WorkspaceId(ulid::Ulid(0)).to_uuid())
        .execute(pg)
        .await
        .expect("insert room");
        (room, sender)
    }

    fn new_msg(
        room: RoomId,
        sender: ParticipantId,
        text: &str,
        reply_to: Option<aero_common::MessageId>,
    ) -> NewMessage {
        NewMessage {
            room_id: room,
            sender_id: sender,
            blocks: vec![Block::text(text)],
            reply_to,
            metadata: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn thread_title_degrades_to_heuristic_without_key() {
        let pg = pool().await;
        let messages = MessageRepo::new(pg.clone());

        let (room, sender) = seed(&pg).await;
        // Root message with a clear first-8-words signal.
        let root_msg = messages
            .insert(new_msg(
                room,
                sender,
                "we should ship the new billing flow before the next release",
                None,
            ))
            .await
            .expect("insert root");
        // A reply for context (the heuristic ignores it, but the path still reads it).
        messages
            .insert(new_msg(room, sender, "agreed, lets schedule it", Some(root_msg.id)))
            .await
            .expect("insert reply 1");

        // Build an AiService with NO Anthropic key (degrade-safe path).
        let ai = AiService::new(
            None,
            aero_ai::default_embedder(),
            aero_ai::default_transcriber(),
            AiJobRepo::new(pg.clone()),
            messages.clone(),
            RoomRepo::new(pg.clone()),
            None,
        );

        let title = ai
            .generate_thread_title(root_msg.id, 200)
            .await
            .expect("title is infallible without a key");
        assert!(!title.is_empty(), "heuristic title must be non-empty");
        assert!(
            title.starts_with("we should ship"),
            "heuristic derives from the root's first words, got {title}"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn thread_title_empty_for_missing_root() {
        let pg = pool().await;
        let messages = MessageRepo::new(pg.clone());
        let ai = AiService::new(
            None,
            aero_ai::default_embedder(),
            aero_ai::default_transcriber(),
            AiJobRepo::new(pg.clone()),
            messages.clone(),
            RoomRepo::new(pg.clone()),
            None,
        );
        // Unknown root id => empty title, no panic.
        let title = ai
            .generate_thread_title(aero_common::MessageId::new(), 200)
            .await
            .expect("infallible");
        assert!(title.is_empty(), "missing root yields an empty title");
    }
}
