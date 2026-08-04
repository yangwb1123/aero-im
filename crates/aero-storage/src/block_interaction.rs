//! Block-interaction repository — records interactions with interactive message
//! blocks (Slack Block Kit-lite `Button` / `Select`).
//!
//! Backs `migrations/0086_block_interactions.sql`. When a participant clicks a
//! button or picks a `Select` option on a message, the interactions HTTP surface
//! ([`crate`'s caller in `aero-server`]) records it here so the message's poster —
//! typically a bot/webhook/app integration — can collect actionable replies.
//!
//! One row per interaction, NOT deduplicated: a participant may click the same
//! button repeatedly or pick different `Select` values, and each is a distinct,
//! time-ordered event ([`record`](BlockInteractionRepo::record) always inserts;
//! [`list_for_message`](BlockInteractionRepo::list_for_message) returns them
//! oldest-first). Purely additive — a NEW repo; no existing repo is touched. The
//! [`BlockInteraction`] model lives here (and is re-exported from the crate root)
//! rather than in `aero-common`, since it is a storage-layer projection.

use aero_common::{BlockInteractionId, MessageId, ParticipantId, RoomId};
use serde::Serialize;
use sqlx::PgPool;

/// One recorded interaction with an interactive message block.
///
/// A storage-layer projection of a `block_interactions` row. `Serialize` so a
/// handler can hand the row straight back as JSON; `created_at` renders as
/// RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct BlockInteraction {
    /// The interaction's unique id.
    pub id: BlockInteractionId,
    /// The message whose interactive block was hit.
    pub message_id: MessageId,
    /// The room the message lives in (denormalized for room-scoped queries).
    pub room_id: RoomId,
    /// The participant who clicked / picked.
    pub participant_id: ParticipantId,
    /// The `action_id` of the interactive block that was hit.
    pub action_id: String,
    /// The chosen `Select` option value / button payload; `None` for a value-less
    /// button click.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// When the interaction was recorded (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// The columns a [`BlockInteraction`] is built from, in select order. Shared by
/// every query so the row decoding stays in one place.
const COLUMNS: &str = "id, message_id, room_id, participant_id, action_id, value, created_at";

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: uuid::Uuid,
    message_id: uuid::Uuid,
    room_id: uuid::Uuid,
    participant_id: uuid::Uuid,
    action_id: String,
    value: Option<String>,
    created_at: time::OffsetDateTime,
}

fn row_to_model(r: Row) -> BlockInteraction {
    BlockInteraction {
        id: BlockInteractionId::from_uuid(r.id),
        message_id: MessageId::from_uuid(r.message_id),
        room_id: RoomId::from_uuid(r.room_id),
        participant_id: ParticipantId::from_uuid(r.participant_id),
        action_id: r.action_id,
        value: r.value,
        created_at: r.created_at,
    }
}

/// Repository over the `block_interactions` table (interactive-block clicks).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`BlockInteractionRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct BlockInteractionRepo {
    pool: PgPool,
}

impl BlockInteractionRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record one interaction with an interactive block, returning the freshly
    /// inserted row. Always inserts — interactions are NOT deduplicated, so a
    /// repeated click or a changed `Select` value each lands as a distinct,
    /// time-ordered event. The caller is responsible for verifying the message
    /// actually contains a block with `action_id` (and for room access).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert / read-back.
    pub async fn record(
        &self,
        message: MessageId,
        room: RoomId,
        participant: ParticipantId,
        action_id: &str,
        value: Option<&str>,
    ) -> Result<BlockInteraction, sqlx::Error> {
        let id = BlockInteractionId::new();
        let sql = format!(
            "INSERT INTO block_interactions
                 (id, message_id, room_id, participant_id, action_id, value)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING {COLUMNS}"
        );
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .bind(message.to_uuid())
            .bind(room.to_uuid())
            .bind(participant.to_uuid())
            .bind(action_id)
            .bind(value)
            .fetch_one(&self.pool)
            .await?;
        Ok(row_to_model(row))
    }

    /// List every interaction recorded on `message`, oldest first (so the poster
    /// reads a chronological tally of who clicked/picked what).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_message(
        &self,
        message: MessageId,
    ) -> Result<Vec<BlockInteraction>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM block_interactions
              WHERE message_id = $1
              ORDER BY created_at ASC, id ASC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(message.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored block_interaction
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::message::{MessageRepo, NewMessage};
    use aero_common::Block;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// The reserved all-zero default workspace, guaranteed to exist by migration
    /// 0006's backfill — reused so the room is well-scoped.
    const DEFAULT_WS: &str = "00000000-0000-0000-0000-000000000000";

    /// Create a throwaway participant so the test is self-contained.
    async fn participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("block-interaction-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    /// Create a throwaway room in the default workspace, created by `by`.
    async fn room(p: &PgPool, by: ParticipantId) -> RoomId {
        let id = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
             VALUES ($1, 'group', $2, $3, now(), $4::uuid)",
        )
        .bind(id.to_uuid())
        .bind(format!("bi-room-{id}"))
        .bind(by.to_uuid())
        .bind(DEFAULT_WS)
        .execute(p)
        .await
        .expect("insert room");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn record_and_list_for_message_ordered() {
        let p = pool();
        let repo = BlockInteractionRepo::new(p.clone());
        let msgs = MessageRepo::new(p.clone());
        let sender = participant(&p).await;
        let clicker = participant(&p).await;
        let room = room(&p, sender).await;

        // A message carrying an interactive button.
        let blocks = vec![
            Block::text("approve?"),
            Block::Button {
                action_id: "approve".into(),
                label: "Approve".into(),
                style: Some("primary".into()),
                url: None,
            },
        ];
        let msg = msgs
            .insert(NewMessage {
                room_id: room,
                sender_id: sender,
                blocks,
                reply_to: None,
                metadata: serde_json::Value::Null,
                expires_at: None,
            })
            .await
            .expect("insert message");

        // No interactions yet.
        assert!(repo.list_for_message(msg.id).await.unwrap().is_empty());

        // Record two clicks (NOT deduplicated).
        let first = repo
            .record(msg.id, room, clicker, "approve", None)
            .await
            .unwrap();
        assert_eq!(first.action_id, "approve");
        assert_eq!(first.value, None);
        assert_eq!(first.room_id, room);
        let _second = repo
            .record(msg.id, room, clicker, "approve", Some("again"))
            .await
            .unwrap();

        // Both rows show, oldest first.
        let listed = repo.list_for_message(msg.id).await.unwrap();
        assert_eq!(listed.len(), 2, "both interactions recorded (no dedup)");
        assert_eq!(listed[0].id, first.id, "oldest first");
        assert_eq!(listed[1].value.as_deref(), Some("again"));

        // Cleanup so reruns stay self-contained (cascades clear interactions).
        sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(msg.id.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
