//! Thread-level unread count tracking repository.
//!
//! Backs `migrations/0101_thread_read_state.sql`. Each row records when a
//! participant last read the replies in a thread (identified by its root
//! message id). An unread count is then computed as the number of reply
//! messages posted *after* the stored cursor by *other* participants.
//!
//! Purely additive: a NEW [`ThreadReadStateRepo`]; no existing repo is touched.

use std::collections::HashMap;

use aero_common::{Error as AeroError, MessageId, ParticipantId};
use sqlx::PgPool;

/// Repository for per-participant thread read state.
#[derive(Clone)]
pub struct ThreadReadStateRepo {
    pub pg: PgPool,
}

impl ThreadReadStateRepo {
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Mark the thread rooted at `root` as read for `participant` (UPSERT).
    /// Sets / refreshes `last_read_at` to now().
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert.
    pub async fn mark_read(
        &self,
        participant: ParticipantId,
        root: MessageId,
    ) -> Result<(), AeroError> {
        sqlx::query(
            r"INSERT INTO thread_read_state (participant_id, root_message_id, last_read_at)
               VALUES ($1, $2, now())
               ON CONFLICT (participant_id, root_message_id)
               DO UPDATE SET last_read_at = now()",
        )
        .bind(participant.to_uuid())
        .bind(root.to_uuid())
        .execute(&self.pg)
        .await
        .map_err(AeroError::from)?;
        Ok(())
    }

    /// The number of unread replies in the thread rooted at `root` for
    /// `participant`: replies posted after the participant's `last_read_at`
    /// cursor (defaulting to the Unix epoch when no cursor exists) by
    /// participants *other than* `participant` themselves.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn unread_count(
        &self,
        participant: ParticipantId,
        root: MessageId,
    ) -> Result<i64, AeroError> {
        let row = sqlx::query_as::<_, (i64,)>(
            r"SELECT COUNT(*)
               FROM messages
               WHERE reply_to = $1
                 AND sender_id != $2
                 AND deleted_at IS NULL
                 AND created_at > COALESCE(
                       (SELECT last_read_at
                          FROM thread_read_state
                         WHERE participant_id = $2
                           AND root_message_id = $1),
                       TIMESTAMP WITH TIME ZONE 'epoch'
                 )",
        )
        .bind(root.to_uuid())
        .bind(participant.to_uuid())
        .fetch_one(&self.pg)
        .await
        .map_err(AeroError::from)?;
        Ok(row.0)
    }

    /// Batch-fetch unread counts for multiple threads rooted at `roots` for
    /// `participant` in two round-trips (one for the cursors, one for the
    /// counts). Returns a map of `root_message_id -> unread_count`; threads
    /// with no unread messages are omitted (callers treat a miss as 0).
    /// Empty input short-circuits to an empty map without touching the DB.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the queries.
    pub async fn unread_counts_batch(
        &self,
        participant: ParticipantId,
        roots: &[MessageId],
    ) -> Result<HashMap<MessageId, i64>, AeroError> {
        if roots.is_empty() {
            return Ok(HashMap::new());
        }
        let root_uuids: Vec<uuid::Uuid> = roots.iter().map(MessageId::to_uuid).collect();
        // Fetch per-thread unread counts in one aggregated query.
        let rows = sqlx::query_as::<_, (uuid::Uuid, i64)>(
            r"SELECT m.reply_to, COUNT(*) AS unread
               FROM messages m
               WHERE m.reply_to = ANY($1)
                 AND m.sender_id != $2
                 AND m.deleted_at IS NULL
                 AND m.created_at > COALESCE(
                       (SELECT trs.last_read_at
                          FROM thread_read_state trs
                         WHERE trs.participant_id = $2
                           AND trs.root_message_id = m.reply_to),
                       TIMESTAMP WITH TIME ZONE 'epoch'
                 )
               GROUP BY m.reply_to
               HAVING COUNT(*) > 0",
        )
        .bind(&root_uuids)
        .bind(participant.to_uuid())
        .fetch_all(&self.pg)
        .await
        .map_err(AeroError::from)?;

        Ok(rows
            .into_iter()
            .map(|(id, count)| (MessageId::from_uuid(id), count))
            .collect())
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{MessageId, ParticipantId};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway workspace + channel room + participant so messages can
    /// be inserted. Returns `(room_id, participant_id)` for use in tests.
    async fn fixture(p: &PgPool) -> (uuid::Uuid, ParticipantId) {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(actor.to_uuid())
            .bind(format!("thread-rs-actor-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");
        let room_id = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
             VALUES ($1,'channel',$2,$3,now(),'00000000-0000-0000-0000-000000000000')",
        )
        .bind(room_id)
        .bind(format!("thread-rs-room-{room_id}"))
        .bind(actor.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        (room_id, actor)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn thread_read_state_mark_and_unread_count() {
        let p = pool();
        let repo = ThreadReadStateRepo::new(p.clone());
        let (room_id, reader) = fixture(&p).await;

        // A second participant who posts replies.
        let sender = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(sender.to_uuid())
            .bind(format!("thread-rs-sender-{sender}"))
            .execute(&p)
            .await
            .expect("insert sender");

        // Root message.
        let root = MessageId::new();
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, body, created_at)
             VALUES ($1, $2, $3, 'root msg', now())",
        )
        .bind(root.to_uuid())
        .bind(room_id)
        .bind(reader.to_uuid())
        .execute(&p)
        .await
        .expect("insert root message");

        // Initially 0 unread (no replies yet).
        let count = repo.unread_count(reader, root).await.unwrap();
        assert_eq!(count, 0, "no replies yet");

        // Post two replies from `sender`.
        for i in 0..2 {
            let reply = MessageId::new();
            sqlx::query(
                "INSERT INTO messages (id, room_id, sender_id, reply_to, body, created_at)
                 VALUES ($1, $2, $3, $4, $5, now())",
            )
            .bind(reply.to_uuid())
            .bind(room_id)
            .bind(sender.to_uuid())
            .bind(root.to_uuid())
            .bind(format!("reply {i}"))
            .execute(&p)
            .await
            .expect("insert reply");
        }

        let count = repo.unread_count(reader, root).await.unwrap();
        assert_eq!(count, 2, "two unread replies");

        // Mark thread as read → count drops to 0.
        repo.mark_read(reader, root).await.unwrap();
        // Second mark_read is idempotent.
        repo.mark_read(reader, root).await.unwrap();

        let count = repo.unread_count(reader, root).await.unwrap();
        assert_eq!(count, 0, "zero after marking read");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn thread_read_state_unread_counts_batch() {
        let p = pool();
        let repo = ThreadReadStateRepo::new(p.clone());
        let (room_id, reader) = fixture(&p).await;

        let sender = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(sender.to_uuid())
            .bind(format!("thread-rs-batch-sender-{sender}"))
            .execute(&p)
            .await
            .expect("insert sender");

        // Two thread roots.
        let root_a = MessageId::new();
        let root_b = MessageId::new();
        for root in [root_a, root_b] {
            sqlx::query(
                "INSERT INTO messages (id, room_id, sender_id, body, created_at)
                 VALUES ($1, $2, $3, 'root', now())",
            )
            .bind(root.to_uuid())
            .bind(room_id)
            .bind(reader.to_uuid())
            .execute(&p)
            .await
            .expect("insert root message");
        }

        // 1 reply in root_a, 3 in root_b.
        let reply_a = MessageId::new();
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, reply_to, body, created_at)
             VALUES ($1, $2, $3, $4, 'reply-a', now())",
        )
        .bind(reply_a.to_uuid())
        .bind(room_id)
        .bind(sender.to_uuid())
        .bind(root_a.to_uuid())
        .execute(&p)
        .await
        .expect("insert reply_a");

        for i in 0..3 {
            let reply = MessageId::new();
            sqlx::query(
                "INSERT INTO messages (id, room_id, sender_id, reply_to, body, created_at)
                 VALUES ($1, $2, $3, $4, $5, now())",
            )
            .bind(reply.to_uuid())
            .bind(room_id)
            .bind(sender.to_uuid())
            .bind(root_b.to_uuid())
            .bind(format!("reply-b-{i}"))
            .execute(&p)
            .await
            .expect("insert reply_b");
        }

        let map = repo.unread_counts_batch(reader, &[root_a, root_b]).await.unwrap();
        assert_eq!(map.get(&root_a).copied(), Some(1));
        assert_eq!(map.get(&root_b).copied(), Some(3));

        // Empty input short-circuits.
        let empty = repo.unread_counts_batch(reader, &[]).await.unwrap();
        assert!(empty.is_empty());
    }
}
