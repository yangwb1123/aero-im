//! Atomic idempotent persistence for AI-extracted action-item batches.

use aero_common::{ParticipantId, RoomId, TaskId};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, QueryBuilder};

use super::TaskRepo;

/// Maximum number of AI action items persisted by one request.
pub const MAX_ACTION_ITEM_BATCH_SIZE: usize = 20;
/// Maximum UTF-8 byte length of the caller-provided idempotency key.
pub const MAX_ACTION_ITEM_BATCH_KEY_LEN: usize = 128;

/// A durable action-item batch could not be created or replayed safely.
#[derive(Debug, thiserror::Error)]
pub enum ActionItemBatchError {
    /// The caller no longer has effective access to the target room.
    #[error("action-item batch creator lacks current effective room access")]
    ActorNotMember,
    /// The idempotency key is empty or exceeds the storage contract.
    #[error("invalid action-item batch idempotency key")]
    InvalidIdempotencyKey,
    /// The caller attempted to persist more than the bounded batch size.
    #[error("action-item batch exceeds the maximum size")]
    TooManyItems,
    /// `PostgreSQL` rejected or could not complete the atomic batch transaction.
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl TaskRepo {
    /// Atomically persist one AI action-item batch, or replay its original ids.
    ///
    /// The key is scoped by `(creator, room)` and only its SHA-256 digest is
    /// persisted. Effective room access is fenced before a transaction-local
    /// advisory lock serializes that exact digest. The receipt and every task
    /// then commit together; retries return the receipt's original ordered ids
    /// without consulting the new model output. Empty batches are also
    /// receipted.
    ///
    /// # Errors
    /// Returns [`ActionItemBatchError::ActorNotMember`] after access revocation,
    /// validation variants for an invalid key or oversized batch, and
    /// [`ActionItemBatchError::Database`] for an atomic storage failure.
    pub async fn create_action_item_batch(
        &self,
        room: RoomId,
        creator: ParticipantId,
        idempotency_key: &str,
        titles: &[String],
    ) -> Result<Vec<TaskId>, ActionItemBatchError> {
        if titles.len() > MAX_ACTION_ITEM_BATCH_SIZE {
            return Err(ActionItemBatchError::TooManyItems);
        }
        let idempotency_key = normalize_idempotency_key(idempotency_key)?;
        // Idempotency tokens are bearer-adjacent client material: retain only
        // the deterministic digest needed for matching/replay.  This also keeps
        // task exports and participant tombstones from exposing the raw token.
        let idempotency_key_digest = action_item_batch_key_digest(idempotency_key);
        let mut tx = self.pool.begin().await?;

        let has_access: bool =
            sqlx::query_scalar("SELECT aero_effective_room_access($1, $2, NULL)")
                .bind(room.to_uuid())
                .bind(creator.to_uuid())
                .fetch_one(&mut *tx)
                .await?;
        if !has_access {
            return Err(ActionItemBatchError::ActorNotMember);
        }

        // The database unique key is the durable backstop; this advisory lock
        // gives concurrent API calls a cheap serialization point that also works
        // for an empty batch (which has no child task row to lock).
        let lock_key = format!(
            "aero:task-action-items:{}:{}:{idempotency_key_digest}",
            creator.to_uuid(),
            room.to_uuid()
        );
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 224))")
            .bind(lock_key)
            .execute(&mut *tx)
            .await?;

        let existing = sqlx::query_scalar::<_, Vec<uuid::Uuid>>(
            r"SELECT task_ids
                FROM task_action_item_batches
               WHERE participant_id = $1
                 AND room_id = $2
                 AND idempotency_key = $3
               FOR SHARE",
        )
        .bind(creator.to_uuid())
        .bind(room.to_uuid())
        .bind(&idempotency_key_digest)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(task_ids) = existing {
            tx.commit().await?;
            return Ok(task_ids.into_iter().map(TaskId::from_uuid).collect());
        }

        let task_ids: Vec<uuid::Uuid> =
            (0..titles.len()).map(|_| TaskId::new().to_uuid()).collect();
        sqlx::query(
            r"INSERT INTO task_action_item_batches
                  (participant_id, room_id, idempotency_key, task_ids)
               VALUES ($1, $2, $3, $4)",
        )
        .bind(creator.to_uuid())
        .bind(room.to_uuid())
        .bind(&idempotency_key_digest)
        .bind(&task_ids)
        .execute(&mut *tx)
        .await?;

        if !task_ids.is_empty() {
            let mut insert = QueryBuilder::<Postgres>::new(
                "INSERT INTO tasks (
                    id,
                    room_id,
                    creator_id,
                    title,
                    action_item_batch_key,
                    action_item_batch_index
                ) ",
            );
            insert.push_values(
                task_ids.iter().zip(titles).enumerate(),
                |mut row, (index, (task_id, title))| {
                    row.push_bind(task_id)
                        .push_bind(room.to_uuid())
                        .push_bind(creator.to_uuid())
                        .push_bind(title)
                        .push_bind(&idempotency_key_digest)
                        .push_bind(i16::try_from(index).expect("batch size is at most 20"));
                },
            );
            insert.build().execute(&mut *tx).await?;
        }

        tx.commit().await?;
        Ok(task_ids.into_iter().map(TaskId::from_uuid).collect())
    }
}

fn normalize_idempotency_key(key: &str) -> Result<&str, ActionItemBatchError> {
    let key = key.trim();
    if key.is_empty() || key.len() > MAX_ACTION_ITEM_BATCH_KEY_LEN {
        return Err(ActionItemBatchError::InvalidIdempotencyKey);
    }
    Ok(key)
}

fn action_item_batch_key_digest(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

#[cfg(test)]
#[path = "action_item_erasure_tests.rs"]
mod erasure_tests;
#[cfg(test)]
#[path = "action_item_batch_tests.rs"]
mod tests;
