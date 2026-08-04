//! Per-message read-receipt repository ("Seen by").
//!
//! Backs `migrations/0077_message_receipts.sql`. One row per (message,
//! participant) recording the timestamp at which a participant acknowledged
//! seeing a SPECIFIC message (Slack/Teams "Seen by …"). This is distinct from
//! the per-room [`ReceiptRepo`](crate::ReceiptRepo), which keeps a single
//! monotonic cursor per room for unread counts and cannot answer "who has seen
//! this one message". Purely additive: a NEW [`MessageReceiptRepo`]; no existing
//! repo is touched.

use aero_common::{MessageId, ParticipantId};
use sqlx::PgPool;
use time::OffsetDateTime;

/// One reader of a message: who saw it and when they first acknowledged it.
pub type MessageReader = (ParticipantId, OffsetDateTime);

#[derive(Clone)]
pub struct MessageReceiptRepo {
    pool: PgPool,
}

impl MessageReceiptRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record that `participant` has seen `message`. Idempotent: re-marking an
    /// already-seen message is a no-op that keeps the original `read_at`
    /// (first-seen provenance). Returns `true` iff a NEW receipt row was created
    /// (so the caller can skip a redundant broadcast on a repeat mark). The
    /// caller is responsible for room-access gating.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert.
    pub async fn mark_read(
        &self,
        message: MessageId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"INSERT INTO message_receipts (message_id, participant_id, read_at)
               VALUES ($1, $2, now())
               ON CONFLICT (message_id, participant_id) DO NOTHING",
        )
        .bind(message.to_uuid())
        .bind(participant.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// The reader list for a message — every participant who has acknowledged it,
    /// each with the time they first saw it, ordered oldest-seen first (the
    /// natural "Seen by" ordering).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_readers(
        &self,
        message: MessageId,
    ) -> Result<Vec<MessageReader>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid, OffsetDateTime)>(
            r"SELECT participant_id, read_at
               FROM message_receipts
              WHERE message_id = $1
              ORDER BY read_at ASC, participant_id ASC",
        )
        .bind(message.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(pid, at)| (ParticipantId::from_uuid(pid), at))
            .collect())
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored message_receipt_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::RoomId;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// A self-contained (room, message, two participants) fixture.
    async fn fixture(p: &PgPool) -> (MessageId, ParticipantId, ParticipantId) {
        let author = ParticipantId::new();
        let reader = ParticipantId::new();
        for (id, who) in [(author, "mr-author"), (reader, "mr-reader")] {
            sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
                .bind(id.to_uuid())
                .bind(format!("{who}-{id}"))
                .execute(p)
                .await
                .expect("insert participant");
        }
        let room = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
             VALUES ($1,'group',$2,$3, now(), '00000000-0000-0000-0000-000000000000')",
        )
        .bind(room.to_uuid())
        .bind("mr-room")
        .bind(author.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        let message = MessageId::new();
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks, searchable_text, created_at)
             VALUES ($1,$2,$3,'[{\"type\":\"text\",\"content\":\"hi\"}]'::jsonb,'hi', now())",
        )
        .bind(message.to_uuid())
        .bind(room.to_uuid())
        .bind(author.to_uuid())
        .execute(p)
        .await
        .expect("insert message");
        (message, author, reader)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn message_receipt_mark_and_list_two_readers() {
        let p = pool();
        let repo = MessageReceiptRepo::new(p.clone());
        let (message, author, reader) = fixture(&p).await;

        // No readers yet.
        assert!(repo.list_readers(message).await.unwrap().is_empty());

        // Two distinct users mark seen → both appear.
        assert!(
            repo.mark_read(message, author).await.unwrap(),
            "first mark created"
        );
        assert!(
            repo.mark_read(message, reader).await.unwrap(),
            "second reader created"
        );

        let readers = repo.list_readers(message).await.unwrap();
        assert_eq!(readers.len(), 2, "both readers listed");
        let ids: Vec<ParticipantId> = readers.iter().map(|(p, _)| *p).collect();
        assert!(ids.contains(&author) && ids.contains(&reader));
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn message_receipt_remark_is_idempotent() {
        let p = pool();
        let repo = MessageReceiptRepo::new(p.clone());
        let (message, _author, reader) = fixture(&p).await;

        assert!(
            repo.mark_read(message, reader).await.unwrap(),
            "first mark created"
        );
        // Re-marking is a no-op (returns false) and does not duplicate the row.
        assert!(
            !repo.mark_read(message, reader).await.unwrap(),
            "re-mark is a no-op"
        );

        let readers = repo.list_readers(message).await.unwrap();
        assert_eq!(readers.len(), 1, "still exactly one reader after re-mark");
    }
}
