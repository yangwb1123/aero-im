//! Expiry sweep: ephemeral message deletion.
//!
//! Extracted from `message/orig.rs` as part of REFACTOR_PLAN.md Step 2.

use aero_common::{MessageId, RoomId};

use super::MessageRepo;

impl MessageRepo {
    /// Delete expired ephemeral messages (those with `expires_at <= NOW()`) and
    /// return their (id, room_id) pairs so the caller can emit Deleted frames. The
    /// read queries (`query.rs` / `search.rs`) ALSO filter `expires_at > now()`, so
    /// an expired message is invisible the instant it lapses; this sweep is the
    /// eventual hard-delete that reclaims the row + announces the Deleted event.
    pub async fn sweep_ephemeral(&self) -> Result<Vec<(MessageId, RoomId)>, sqlx::Error> {
        let rows: Vec<(uuid::Uuid, uuid::Uuid)> = sqlx::query_as(
            "DELETE FROM messages WHERE expires_at IS NOT NULL AND expires_at <= NOW() \
             RETURNING id, room_id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, room)| (MessageId::from_uuid(id), RoomId::from_uuid(room)))
            .collect())
    }

    /// ADDITIVE partition prep — run ONE batch of the `messages` →
    /// `messages_partitioned` backfill (see migration 0148 and
    /// `docs/runbooks/messages-partitioning.md`).
    ///
    /// This is the thin operator-facing wrapper over the SQL function
    /// `backfill_messages_partition(batch_size, from_id)`. It copies up to
    /// `batch_size` live rows into the shadow partitioned table starting strictly
    /// after `from_id`, idempotently (`ON CONFLICT DO NOTHING`), and returns
    /// `(rows_copied, last_id)`:
    ///   * `rows_copied` — rows actually inserted this batch (0 ⇒ caught up to the
    ///     current tail of `messages`),
    ///   * `last_id` — the new high-water mark to thread back as `from_id`.
    ///
    /// It is **read-only** w.r.t. `messages` (a plain `SELECT`); it never locks or
    /// mutates the live table and is NOT a dual-write. Drive it from an offline
    /// ops loop (throttled) BEFORE the cutover window so the in-window final sync
    /// is tiny. The destructive cutover (FK repointing + table swap) is NOT done
    /// here — it stays in the maintenance-window runbook.
    pub async fn backfill_messages_partition(
        &self,
        batch_size: i32,
        from_id: MessageId,
    ) -> Result<(i64, MessageId), sqlx::Error> {
        let (rows_copied, last_id): (i64, uuid::Uuid) = sqlx::query_as(
            "SELECT rows_copied, last_id \
             FROM backfill_messages_partition($1, $2)",
        )
        .bind(batch_size)
        .bind(from_id.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok((rows_copied, MessageId::from_uuid(last_id)))
    }
}
