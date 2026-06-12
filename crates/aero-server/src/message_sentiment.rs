//! Per-message sentiment / toxicity scoring — "how does this message read".
//!
//! An AI-native, ADDITIVE read on a single message's affect. Distinct from
//! moderation ([`crate::moderation_bot`] / the synchronous block-word gate): this
//! NEVER blocks anything — it returns a descriptive
//! `{ sentiment, toxicity, tone }` for a UI affordance (a tone chip on a message).
//!
//! The target message is resolved first (`404` if unknown / soft-deleted), then
//! access is asserted against its room
//! ([`assert_room_access`](aero_im_core::ImService::assert_room_access)) — the same
//! workspace + room membership guard the rest of the read routes use — before the
//! AI backend sees the text. Degrades SAFELY: `502` when no AI backend is wired
//! ([`AppState::ai`] is `None`), and when a backend IS wired but no LLM key is
//! configured it scores via a deterministic keyword/punctuation heuristic — so the
//! route is verifiable as 200-with-heuristic. Purely additive: a thin handler over
//! existing [`AppState`] state; no existing repo or service is touched. Mirrors the
//! structure of [`crate::message_context`]. Mounted via [`routes`] and `.merge`d
//! into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, MessageId};
use aero_storage::MessageRepo;
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// All message-sentiment routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/messages/:id/sentiment", post(message_sentiment))
}

/// Build a [`MessageRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> MessageRepo {
    MessageRepo::new(s.pg.clone())
}

fn parse_message(s: &str) -> Result<MessageId, AeroError> {
    MessageId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("message id: {e}")))
}

/// `POST /api/messages/:id/sentiment` — score a single message's affect. Resolves
/// the message (`404` if unknown or soft-deleted), asserts read access to its room
/// (`403` for non-members), then scores its text. Returns
/// `{ "sentiment": "negative"|"neutral"|"positive", "toxicity": <0..1>,
/// "tone": <label> }`. `502` when no AI backend is configured; degrades to a
/// deterministic keyword/punctuation heuristic when a backend is wired but no LLM
/// key is set (never errors on a missing key).
async fn message_sentiment(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_message(&id_str)?;
    let r = repo(&s);

    // Resolve the message first so access can be membership-gated; an unknown or
    // soft-deleted message is a `404`.
    let target = r
        .get(id)
        .await
        .map_err(AeroError::from)?
        .filter(|m| m.deleted_at.is_none())
        .ok_or_else(|| AeroError::NotFound(format!("message {id}")))?;
    // Workspace + room membership guard (read-only; `ImService` is not mutated).
    s.im.assert_room_access(auth.participant_id, target.room_id).await?;

    let text = target.searchable_text();

    // Degrade like the other AI routes: `502` when no AI backend is wired; a wired
    // backend scores heuristically when no LLM key is set.
    let ai = s
        .ai
        .as_ref()
        .ok_or_else(|| AeroError::Upstream("AI not configured".into()))?;
    let score = ai
        .score_sentiment(&text)
        .await
        .map_err(|e| AeroError::Upstream(format!("ai: {e}")))?;

    Ok(Json(serde_json::json!({
        "message_id": id,
        "sentiment": score.sentiment,
        "toxicity": score.toxicity,
        "tone": score.tone,
    })))
}

#[cfg(test)]
mod db_tests {
    //! Live-DB tests for per-message sentiment scoring. They COMPILE here but only
    //! RUN against a live Postgres (the integrator runs them). They exercise the
    //! degrade-safe path: with NO Anthropic key configured the scorer must return a
    //! well-formed score for an inserted message.

    use aero_ai::{AiService, Sentiment};
    use aero_common::{Block, ParticipantId, RoomId, WorkspaceId};
    use aero_storage::{AiJobRepo, MessageRepo, NewMessage, RoomRepo};
    use sqlx::postgres::PgPoolOptions;

    async fn pool() -> aero_storage::PgPool {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        PgPoolOptions::new().max_connections(2).connect(&url).await.expect("connect")
    }

    // Seed a throwaway participant + room (in the all-zero default workspace) so a
    // message insert satisfies its participants/rooms foreign keys.
    async fn seed(pg: &aero_storage::PgPool) -> (RoomId, ParticipantId) {
        let sender = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(sender.to_uuid())
            .bind(format!("sentiment-actor-{sender}"))
            .execute(pg)
            .await
            .expect("insert participant");
        let room = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id) VALUES ($1,'group',$2,$3,$4)",
        )
        .bind(room.to_uuid())
        .bind(format!("sentiment-room-{room}"))
        .bind(sender.to_uuid())
        .bind(WorkspaceId(ulid::Ulid(0)).to_uuid())
        .execute(pg)
        .await
        .expect("insert room");
        (room, sender)
    }

    fn new_msg(room: RoomId, sender: ParticipantId, text: &str) -> NewMessage {
        NewMessage {
            room_id: room,
            sender_id: sender,
            blocks: vec![Block::text(text)],
            reply_to: None,
            metadata: serde_json::Value::Null,
            expires_at: None,
        }
    }

    fn ai_no_key(pg: &aero_storage::PgPool) -> AiService {
        AiService::new(
            None,
            aero_ai::default_embedder(),
            aero_ai::default_transcriber(),
            AiJobRepo::new(pg.clone()),
            MessageRepo::new(pg.clone()),
            RoomRepo::new(pg.clone()),
            None,
        )
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn sentiment_scores_inserted_message_without_key() {
        let pg = pool().await;
        let messages = MessageRepo::new(pg.clone());
        let (room, sender) = seed(&pg).await;

        // A clearly positive message — heuristic must read positive with low toxicity.
        let m = messages
            .insert(new_msg(room, sender, "thanks so much, great job on this!"))
            .await
            .expect("insert");

        let ai = ai_no_key(&pg);
        // Mirror the handler: resolve the message text, then score it.
        let loaded = messages.get(m.id).await.expect("get").expect("exists");
        let score = ai
            .score_message_sentiment(&loaded.searchable_text())
            .await
            .expect("scoring is infallible without a key");

        assert_eq!(score.sentiment, Sentiment::Positive, "positive words read positive");
        assert!(score.toxicity < 0.1, "positive message is low toxicity, got {}", score.toxicity);
        assert!(!score.tone.is_empty(), "tone label is well-formed");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn sentiment_flags_toxic_message_without_key() {
        let pg = pool().await;
        let messages = MessageRepo::new(pg.clone());
        let (room, sender) = seed(&pg).await;

        let m = messages
            .insert(new_msg(room, sender, "you are an idiot and a loser"))
            .await
            .expect("insert");

        let ai = ai_no_key(&pg);
        let loaded = messages.get(m.id).await.expect("get").expect("exists");
        let score = ai
            .score_message_sentiment(&loaded.searchable_text())
            .await
            .expect("infallible");

        assert_eq!(score.sentiment, Sentiment::Negative);
        assert!(score.toxicity >= 0.8, "insult => high toxicity, got {}", score.toxicity);
        assert_eq!(score.tone, "angry");
    }
}
