//! Blob garbage-collection queue — GDPR right-to-erasure for binary content.
//!
//! When a participant's account is deleted, their blob IDs are enqueued here.
//! A background job drains the queue, deletes the storage objects, and acks
//! (removes) each entry. The queue is also safe to populate for any other
//! reason a blob's bytes should be removed (e.g. moderation purge).

use aero_common::{BlobId, ParticipantId};
use sqlx::PgPool;

#[derive(Clone)]
pub struct BlobGcRepo {
    pool: PgPool,
}

impl BlobGcRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Enqueue all blobs owned by `owner` for garbage collection. Called
    /// inside the participant-delete transaction via a raw query; this method
    /// is the public equivalent for ad-hoc use outside a transaction.
    ///
    /// Uses `INSERT … ON CONFLICT DO NOTHING` so it is safe to call more than
    /// once (e.g. retry after a partial failure).
    pub async fn enqueue_for_owner(
        &self,
        owner: ParticipantId,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r"INSERT INTO blob_gc_queue (blob_id)
              SELECT id FROM blobs WHERE owner_id = $1
              ON CONFLICT (blob_id) DO NOTHING",
        )
        .bind(owner.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Return up to `limit` blob IDs that need storage deletion, oldest first.
    pub async fn drain(&self, limit: i64) -> Result<Vec<BlobId>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT blob_id FROM blob_gc_queue ORDER BY enqueued_at ASC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id,)| BlobId::from_uuid(id)).collect())
    }

    /// Mark a blob as successfully deleted — remove it from the queue.
    pub async fn ack(&self, blob_id: BlobId) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM blob_gc_queue WHERE blob_id = $1")
            .bind(blob_id.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_gc_repo_is_clone() {
        fn assert_clone<T: Clone>() {}
        assert_clone::<BlobGcRepo>();
    }
}
