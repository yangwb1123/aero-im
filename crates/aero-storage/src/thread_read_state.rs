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

use crate::thread_subscription::lock_effective_live_thread_root_in_tx;

/// Repository for per-participant thread read state.
#[derive(Clone)]
pub struct ThreadReadStateRepo {
    pg: PgPool,
}

impl ThreadReadStateRepo {
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Mark the thread rooted at `root` as read for `participant` (UPSERT).
    /// Sets / refreshes `last_read_at` to `now()`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert.
    #[cfg(test)]
    pub(crate) async fn mark_read(
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
    #[cfg(test)]
    pub(crate) async fn unread_count(
        &self,
        participant: ParticipantId,
        root: MessageId,
    ) -> Result<i64, AeroError> {
        let row = sqlx::query_as::<_, (i64,)>(
            r"SELECT COUNT(*)
               FROM messages AS reply
               JOIN messages AS root
                 ON root.id = $1 AND root.room_id = reply.room_id
               WHERE reply.reply_to = $1
                 AND reply.sender_id != $2
                 AND reply.deleted_at IS NULL
                 AND reply.created_at > COALESCE(
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
    #[cfg(test)]
    pub(crate) async fn unread_counts_batch(
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
            r"SELECT reply.reply_to, COUNT(*) AS unread
               FROM messages AS reply
               JOIN messages AS root
                 ON root.id = reply.reply_to
                AND root.room_id = reply.room_id
               WHERE reply.reply_to = ANY($1)
                 AND reply.sender_id != $2
                 AND reply.deleted_at IS NULL
                 AND reply.created_at > COALESCE(
                       (SELECT trs.last_read_at
                          FROM thread_read_state trs
                         WHERE trs.participant_id = $2
                           AND trs.root_message_id = reply.reply_to),
                       TIMESTAMP WITH TIME ZONE 'epoch'
                 )
               GROUP BY reply.reply_to
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

    /// Mark a live canonical thread read under the caller's current room-access
    /// fence.
    ///
    /// # Errors
    /// Returns an opaque root/access error or a database error.
    pub async fn mark_read_authorized(
        &self,
        participant: ParticipantId,
        root: MessageId,
    ) -> Result<(), AeroError> {
        let mut tx = self.pg.begin().await?;
        lock_effective_live_thread_root_in_tx(&mut tx, participant, root).await?;
        sqlx::query(
            r"INSERT INTO thread_read_state
                  (participant_id, root_message_id, last_read_at)
               VALUES ($1, $2, now())
               ON CONFLICT (participant_id, root_message_id)
               DO UPDATE SET last_read_at = now()",
        )
        .bind(participant.to_uuid())
        .bind(root.to_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Count unread replies under a live-root/current-access fence.
    ///
    /// # Errors
    /// Returns an opaque root/access error or a database error.
    pub async fn unread_count_authorized(
        &self,
        participant: ParticipantId,
        root: MessageId,
    ) -> Result<i64, AeroError> {
        let mut tx = self.pg.begin().await?;
        lock_effective_live_thread_root_in_tx(&mut tx, participant, root).await?;
        let count = sqlx::query_scalar::<_, i64>(
            r"SELECT COUNT(*)
                FROM messages AS reply
               WHERE reply.reply_to = $1
                 AND reply.sender_id != $2
                 AND reply.deleted_at IS NULL
                 AND reply.created_at > COALESCE(
                       (SELECT last_read_at
                          FROM thread_read_state
                         WHERE participant_id = $2
                           AND root_message_id = $1),
                       TIMESTAMP WITH TIME ZONE 'epoch'
                 )",
        )
        .bind(root.to_uuid())
        .bind(participant.to_uuid())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(count)
    }

    /// Batch-count only live roots in rooms the caller can currently access.
    /// Roots with no unread replies remain absent from the returned map.
    ///
    /// # Errors
    /// Propagates database errors.
    pub async fn unread_counts_accessible(
        &self,
        participant: ParticipantId,
        roots: &[MessageId],
    ) -> Result<HashMap<MessageId, i64>, AeroError> {
        if roots.is_empty() {
            return Ok(HashMap::new());
        }
        let root_uuids: Vec<uuid::Uuid> = roots.iter().map(MessageId::to_uuid).collect();
        let rows = sqlx::query_as::<_, (uuid::Uuid, i64)>(
            r"SELECT reply.reply_to, COUNT(*) AS unread
                FROM messages AS reply
                JOIN messages AS root
                  ON root.id = reply.reply_to
                 AND root.room_id = reply.room_id
                 AND root.reply_to IS NULL
                 AND root.deleted_at IS NULL
               WHERE reply.reply_to = ANY($1)
                 AND reply.sender_id != $2
                 AND reply.deleted_at IS NULL
                 AND aero_effective_room_access(root.room_id, $2, NULL)
                 AND reply.created_at > COALESCE(
                       (SELECT state.last_read_at
                          FROM thread_read_state AS state
                         WHERE state.participant_id = $2
                           AND state.root_message_id = reply.reply_to),
                       TIMESTAMP WITH TIME ZONE 'epoch'
                 )
               GROUP BY reply.reply_to
              HAVING COUNT(*) > 0",
        )
        .bind(&root_uuids)
        .bind(participant.to_uuid())
        .fetch_all(&self.pg)
        .await?;
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

    struct Fixture {
        room_id: uuid::Uuid,
        reader: ParticipantId,
        sender: ParticipantId,
    }

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn fixture(p: &PgPool) -> Fixture {
        let reader = ParticipantId::new();
        let sender = ParticipantId::new();
        let workspace_id = uuid::Uuid::new_v4();
        let room_id = uuid::Uuid::new_v4();
        let mut tx = p.begin().await.expect("begin thread read-state fixture");

        for participant in [reader, sender] {
            sqlx::query(
                "INSERT INTO participants (id, kind, display_name)
                 VALUES ($1, 'human', $2)",
            )
            .bind(participant.to_uuid())
            .bind(format!("thread-rs-{participant}"))
            .execute(&mut *tx)
            .await
            .expect("insert participant");
        }
        sqlx::query(
            "INSERT INTO workspaces (id, name, slug, created_by)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(workspace_id)
        .bind(format!("Thread read state {workspace_id}"))
        .bind(format!("thread-read-state-{workspace_id}"))
        .bind(reader.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert workspace");
        for (participant, role) in [(reader, "owner"), (sender, "member")] {
            sqlx::query(
                "INSERT INTO workspace_members (workspace_id, participant_id, role)
                 VALUES ($1, $2, $3)",
            )
            .bind(workspace_id)
            .bind(participant.to_uuid())
            .bind(role)
            .execute(&mut *tx)
            .await
            .expect("insert workspace member");
        }
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
             VALUES ($1, 'group', $2, $3, now(), $4)",
        )
        .bind(room_id)
        .bind(format!("thread-rs-room-{room_id}"))
        .bind(reader.to_uuid())
        .bind(workspace_id)
        .execute(&mut *tx)
        .await
        .expect("insert room");
        for (participant, role) in [(reader, "owner"), (sender, "member")] {
            sqlx::query(
                "INSERT INTO room_members (room_id, participant_id, role)
                 VALUES ($1, $2, $3)",
            )
            .bind(room_id)
            .bind(participant.to_uuid())
            .bind(role)
            .execute(&mut *tx)
            .await
            .expect("insert room member");
        }
        tx.commit().await.expect("commit thread read-state fixture");

        Fixture {
            room_id,
            reader,
            sender,
        }
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn thread_read_state_mark_and_unread_count() {
        let p = pool();
        let repo = ThreadReadStateRepo::new(p.clone());
        let Fixture {
            room_id,
            reader,
            sender,
        } = fixture(&p).await;

        // Root message.
        let root = MessageId::new();
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks, searchable_text, created_at)
             VALUES ($1, $2, $3, '[]'::jsonb, 'root msg', now())",
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
                "INSERT INTO messages (id, room_id, sender_id, reply_to, blocks, searchable_text, created_at)
                 VALUES ($1, $2, $3, $4, '[]'::jsonb, $5, now())",
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
        let Fixture {
            room_id,
            reader,
            sender,
        } = fixture(&p).await;

        // Two thread roots.
        let root_a = MessageId::new();
        let root_b = MessageId::new();
        for root in [root_a, root_b] {
            sqlx::query(
                "INSERT INTO messages (id, room_id, sender_id, blocks, searchable_text, created_at)
                 VALUES ($1, $2, $3, '[]'::jsonb, 'root', now())",
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
            "INSERT INTO messages (id, room_id, sender_id, reply_to, blocks, searchable_text, created_at)
             VALUES ($1, $2, $3, $4, '[]'::jsonb, 'reply-a', now())",
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
                "INSERT INTO messages (id, room_id, sender_id, reply_to, blocks, searchable_text, created_at)
                 VALUES ($1, $2, $3, $4, '[]'::jsonb, $5, now())",
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

        let map = repo
            .unread_counts_batch(reader, &[root_a, root_b])
            .await
            .unwrap();
        assert_eq!(map.get(&root_a).copied(), Some(1));
        assert_eq!(map.get(&root_b).copied(), Some(3));

        // Empty input short-circuits.
        let empty = repo.unread_counts_batch(reader, &[]).await.unwrap();
        assert!(empty.is_empty());
    }
}
