//! Expiry sweep: ephemeral message deletion.
//!
//! Extracted from `message/orig.rs` as part of REFACTOR_PLAN.md Step 2.

use aero_common::{MessageId, RoomId};

use super::MessageRepo;

impl MessageRepo {
    /// Delete expired ephemeral messages (those with `expires_at <= NOW()`) and
    /// return their (id, `room_id`) pairs so the caller can emit Deleted frames. The
    /// read queries (`query.rs` / `search.rs`) ALSO filter `expires_at > now()`, so
    /// an expired message is invisible the instant it lapses; this sweep is the
    /// eventual hard-delete that reclaims the row + announces the Deleted event.
    pub async fn sweep_ephemeral(&self) -> Result<Vec<(MessageId, RoomId)>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let rows: Vec<(uuid::Uuid, uuid::Uuid, serde_json::Value)> = sqlx::query_as(
            r"SELECT id, room_id, blocks
                FROM messages
               WHERE expires_at IS NOT NULL
                 AND expires_at <= now()
               FOR UPDATE",
        )
        .fetch_all(&mut *tx)
        .await?;
        let ids = rows.iter().map(|(id, _, _)| *id).collect::<Vec<_>>();
        if !ids.is_empty() {
            // Append the tombstones before removing the aggregate rows. Both
            // operations commit together, and event_outbox intentionally has no
            // message FK so a delayed relay survives the hard delete.
            for (message_id, room_id, _) in &rows {
                Self::append_deleted_event_in_tx(
                    &mut tx,
                    MessageId::from_uuid(*message_id),
                    RoomId::from_uuid(*room_id),
                )
                .await?;
            }
            sqlx::query("DELETE FROM messages WHERE id = ANY($1)")
                .bind(&ids)
                .execute(&mut *tx)
                .await?;
            let blobs = rows
                .iter()
                .flat_map(|(_, _, blocks)| super::attached_blob_ids(blocks))
                .collect::<Vec<_>>();
            Self::enqueue_unreferenced_blobs_in_tx(&mut tx, &blobs).await?;
        }
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .map(|(id, room, _)| (MessageId::from_uuid(id), RoomId::from_uuid(room)))
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
