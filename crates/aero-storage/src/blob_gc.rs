//! Blob garbage-collection queue — GDPR right-to-erasure for binary content.
//!
//! When a participant's account is deleted, their blob IDs are enqueued here.
//! A background job drains the queue, deletes the storage objects, and acks
//! (removes) each entry. The queue is also safe to populate for any other
//! reason a blob's bytes should be removed (e.g. moderation purge).

use aero_common::{BlobId, ParticipantId};
use sqlx::PgPool;

/// A queued object deletion and the policy that put it there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobGcItem {
    pub blob_id: BlobId,
    /// Mandatory erasure/expiry ignores live message references. Ordinary
    /// message cleanup is cancelled if a new live reference won the race.
    pub force_delete: bool,
}

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
    /// A GDPR erasure is mandatory even if another message still references the
    /// object. An existing ordinary cleanup row is therefore upgraded to forced.
    pub async fn enqueue_for_owner(&self, owner: ParticipantId) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r"INSERT INTO blob_gc_queue (blob_id, force_delete)
              SELECT id, TRUE FROM blobs WHERE owner_id = $1
              ON CONFLICT (blob_id) DO UPDATE
                  SET force_delete = TRUE",
        )
        .bind(owner.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Enqueue a single blob for garbage collection (idempotent). Used to expire
    /// a one-off artefact like a stale async-export archive — distinct from
    /// [`Self::enqueue_for_owner`], which sweeps every blob an owner has.
    pub async fn enqueue_one(&self, blob: BlobId) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO blob_gc_queue (blob_id, force_delete)
              VALUES ($1, TRUE)
              ON CONFLICT (blob_id) DO UPDATE
                  SET force_delete = TRUE",
        )
        .bind(blob.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Queue abandoned upload reservations older than `cutoff`.
    ///
    /// A process can stop after object-store `put` but before metadata finalize.
    /// Such rows are intentionally invisible to normal reads; this turns them
    /// into the same delete-then-ack workflow used by GDPR erasure.
    pub async fn enqueue_stale_reservations(
        &self,
        cutoff: time::OffsetDateTime,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r"INSERT INTO blob_gc_queue (blob_id, force_delete)
              SELECT id, TRUE
                FROM blobs
               WHERE finalized_at IS NULL
                 AND created_at < $1
              ON CONFLICT (blob_id) DO UPDATE
                  SET force_delete = TRUE",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Return up to `limit` queued deletions, oldest first.
    pub async fn drain(&self, limit: i64) -> Result<Vec<BlobGcItem>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid, bool)>(
            r"SELECT blob_id, force_delete
                FROM blob_gc_queue
               ORDER BY enqueued_at ASC
               LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, force_delete)| BlobGcItem {
                blob_id: BlobId::from_uuid(id),
                force_delete,
            })
            .collect())
    }

    /// Mark a blob as successfully deleted.
    ///
    /// The metadata row is removed too; the queue row disappears through its
    /// `ON DELETE CASCADE` foreign key. Keeping finalized metadata after deleting
    /// bytes would make owner-scoped SHA-256 dedup return a permanently broken
    /// attachment.
    pub async fn ack(&self, blob_id: BlobId) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM blobs WHERE id = $1")
            .bind(blob_id.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Cancel an ordinary queued deletion when the blob acquired a live
    /// reference before the worker reached it. The predicate prevents a
    /// concurrent GDPR/expiry request from being accidentally cancelled.
    pub async fn cancel(&self, blob_id: BlobId) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM blob_gc_queue WHERE blob_id = $1 AND force_delete = FALSE")
            .bind(blob_id.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlobRepo, NewBlob};
    use aero_common::{FileKind, WorkspaceId};
    use sqlx::PgPool;

    #[test]
    fn blob_gc_repo_is_clone() {
        fn assert_clone<T: Clone>() {}
        assert_clone::<BlobGcRepo>();
    }

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("valid DATABASE_URL")
    }

    async fn participant(pool: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("blob-gc-{id}"))
            .execute(pool)
            .await
            .expect("insert participant");
        id
    }

    async fn reservation(pool: &PgPool, owner: ParticipantId) -> BlobId {
        BlobRepo::new(pool.clone())
            .reserve(NewBlob {
                owner_id: owner,
                kind: FileKind::Document,
                name: "archive.json".into(),
                mime: "application/json".into(),
                size: 2,
                sha256: Some(format!("sha-{owner}")),
                storage_key: "pending:test".into(),
            })
            .await
            .expect("reserve")
            .id
    }

    #[tokio::test]
    #[ignore = "requires migrated Postgres"]
    async fn force_upgrade_survives_an_ordinary_cancel() {
        let pool = pool();
        let owner = participant(&pool).await;
        let blob = reservation(&pool, owner).await;
        sqlx::query("INSERT INTO blob_gc_queue (blob_id, force_delete) VALUES ($1, FALSE)")
            .bind(blob.to_uuid())
            .execute(&pool)
            .await
            .expect("ordinary enqueue");

        let gc = BlobGcRepo::new(pool.clone());
        gc.enqueue_one(blob).await.expect("upgrade to forced");
        gc.cancel(blob)
            .await
            .expect("ordinary cancel is conditional");

        let forced: bool =
            sqlx::query_scalar("SELECT force_delete FROM blob_gc_queue WHERE blob_id = $1")
                .bind(blob.to_uuid())
                .fetch_one(&pool)
                .await
                .expect("forced row retained");
        assert!(forced);

        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(owner.to_uuid())
            .execute(&pool)
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    #[ignore = "requires migrated Postgres"]
    async fn queued_reservation_cannot_finalize_or_become_a_dedup_hit() {
        let pool = pool();
        let owner = participant(&pool).await;
        let blob = reservation(&pool, owner).await;
        let gc = BlobGcRepo::new(pool.clone());
        gc.enqueue_one(blob).await.expect("enqueue forced cleanup");

        let blobs = BlobRepo::new(pool.clone());
        assert!(
            blobs
                .finalize(blob, "test:key")
                .await
                .expect("finalize query")
                .is_none(),
            "queued upload must not be resurrected"
        );
        assert!(blobs.get(blob).await.expect("get").is_none());
        assert!(blobs
            .find_by_owner_sha256(owner, &format!("sha-{owner}"))
            .await
            .expect("dedup query")
            .is_none());
        assert!(gc.drain(1_000).await.expect("drain").contains(&BlobGcItem {
            blob_id: blob,
            force_delete: true
        }));

        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(owner.to_uuid())
            .execute(&pool)
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    #[ignore = "requires migrated Postgres"]
    async fn queued_blob_retains_region_until_delete_then_ack() {
        let pool = pool();
        let owner = participant(&pool).await;
        let workspace = WorkspaceId::new();
        let blobs = BlobRepo::new(pool.clone());
        let blob = blobs
            .reserve_in_scope(
                NewBlob {
                    owner_id: owner,
                    kind: FileKind::Document,
                    name: "regional.json".into(),
                    mime: "application/json".into(),
                    size: 2,
                    sha256: Some(format!("regional-{owner}")),
                    storage_key: "pending:test".into(),
                },
                Some(workspace),
                Some("eu-west-1"),
            )
            .await
            .expect("reserve")
            .id;
        let gc = BlobGcRepo::new(pool.clone());
        gc.enqueue_one(blob).await.expect("enqueue");

        let scope = blobs
            .storage_scope(blob)
            .await
            .expect("scope lookup")
            .expect("metadata retained");
        assert_eq!(scope.workspace_id, Some(workspace));
        assert_eq!(scope.storage_region.as_deref(), Some("eu-west-1"));

        gc.ack(blob).await.expect("delete metadata after bytes");
        assert!(blobs.storage_scope(blob).await.expect("scope").is_none());
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(owner.to_uuid())
            .execute(&pool)
            .await
            .expect("cleanup");
    }
}
