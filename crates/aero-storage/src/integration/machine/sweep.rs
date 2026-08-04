//! Bounded retention sweep for integration idempotency state.

use sqlx::FromRow;
use uuid::Uuid;

use crate::integration::IntegrationRepo;

const MAX_SWEEP_BATCH: i64 = 1_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IntegrationMachineSweep {
    pub requests: u64,
    pub notification_receipts: u64,
    pub blob_receipts: u64,
    pub blob_ledger_releases: u64,
}

#[derive(FromRow)]
struct RequestCandidate {
    installation_id: Uuid,
    operation: String,
    idempotency_key: Uuid,
}

impl IntegrationRepo {
    /// Remove expired canonical receipts and request leases in bounded batches.
    /// Each cleanup phase gets its own bounded batch so a hot request backlog
    /// cannot indefinitely starve orphan receipts or blob-ledger release.
    /// A live processing lease is never selected. An abandoned lease is eligible
    /// only after both its short ownership lease and seven-day retention window
    /// have expired.
    pub async fn sweep_expired_machine_state(
        &self,
        limit: i64,
    ) -> Result<IntegrationMachineSweep, sqlx::Error> {
        let limit = limit.clamp(1, MAX_SWEEP_BATCH);
        let mut tx = self.pool.begin().await?;
        let candidates = sqlx::query_as::<_, RequestCandidate>(
            r"SELECT installation_id, operation, idempotency_key
                FROM integration_machine_requests
               WHERE expires_at <= now()
                 AND (status <> 'processing' OR lease_expires_at <= now())
               ORDER BY expires_at, installation_id, operation, idempotency_key
               LIMIT $1
               FOR UPDATE SKIP LOCKED",
        )
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;

        let mut swept = IntegrationMachineSweep::default();
        for candidate in &candidates {
            match candidate.operation.as_str() {
                "notification" => {
                    let rows = sqlx::query(
                        "DELETE FROM integration_notification_receipts WHERE installation_id = $1 AND idempotency_key = $2",
                    )
                    .bind(candidate.installation_id)
                    .bind(candidate.idempotency_key)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                    swept.notification_receipts += rows;
                    rows
                }
                "blob" => {
                    let rows = sqlx::query(
                        "DELETE FROM integration_blob_receipts WHERE installation_id = $1 AND idempotency_key = $2",
                    )
                    .bind(candidate.installation_id)
                    .bind(candidate.idempotency_key)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                    swept.blob_receipts += rows;
                    rows
                }
                _ => 0,
            };
            swept.requests += sqlx::query(
                r"DELETE FROM integration_machine_requests
                    WHERE installation_id = $1 AND operation = $2 AND idempotency_key = $3
                      AND expires_at <= now()
                      AND (status <> 'processing' OR lease_expires_at <= now())",
            )
            .bind(candidate.installation_id)
            .bind(&candidate.operation)
            .bind(candidate.idempotency_key)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }

        swept.notification_receipts += delete_orphan_notifications(&mut tx, limit).await?;
        swept.blob_receipts += delete_orphan_blobs(&mut tx, limit).await?;
        swept.blob_ledger_releases = release_unreferenced_blob_ledger(&mut tx, limit).await?;
        tx.commit().await?;
        Ok(swept)
    }
}

async fn delete_orphan_notifications(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    limit: i64,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        r"WITH candidates AS (
              SELECT receipt.ctid
                FROM integration_notification_receipts receipt
               WHERE receipt.expires_at <= now()
                 AND NOT EXISTS (
                     SELECT 1 FROM integration_machine_requests request
                      WHERE request.installation_id = receipt.installation_id
                        AND request.operation = 'notification'
                        AND request.idempotency_key = receipt.idempotency_key
                 )
               ORDER BY receipt.expires_at, receipt.installation_id
               LIMIT $1 FOR UPDATE SKIP LOCKED
          )
          DELETE FROM integration_notification_receipts receipt
           USING candidates WHERE receipt.ctid = candidates.ctid",
    )
    .bind(limit)
    .execute(&mut **tx)
    .await?
    .rows_affected())
}

async fn delete_orphan_blobs(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    limit: i64,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query(
        r"WITH candidates AS (
              SELECT receipt.ctid
                FROM integration_blob_receipts receipt
               WHERE receipt.expires_at <= now()
                 AND NOT EXISTS (
                     SELECT 1 FROM integration_machine_requests request
                      WHERE request.installation_id = receipt.installation_id
                        AND request.operation = 'blob'
                        AND request.idempotency_key = receipt.idempotency_key
                 )
               ORDER BY receipt.expires_at, receipt.installation_id
               LIMIT $1 FOR UPDATE SKIP LOCKED
          )
          DELETE FROM integration_blob_receipts receipt
           USING candidates WHERE receipt.ctid = candidates.ctid",
    )
    .bind(limit)
    .execute(&mut **tx)
    .await?
    .rows_affected())
}

async fn release_unreferenced_blob_ledger(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    limit: i64,
) -> Result<u64, sqlx::Error> {
    let released = sqlx::query_as::<_, (Uuid, Uuid)>(
        r"SELECT ledger.installation_id, ledger.blob_id
            FROM integration_blob_ledger ledger
            JOIN blobs blob ON blob.id = ledger.blob_id
           WHERE blob.finalized_at IS NOT NULL
             AND NOT EXISTS (
                     SELECT 1 FROM integration_blob_receipts receipt
                      WHERE receipt.installation_id = ledger.installation_id
                        AND receipt.blob_id = ledger.blob_id
                 )
             AND NOT EXISTS (
                     SELECT 1 FROM messages message
                      WHERE message.deleted_at IS NULL
                        AND (message.expires_at IS NULL OR message.expires_at > now())
                        AND message.blocks @> jsonb_build_array(
                            jsonb_build_object('blob_id', aero_uuid_to_ulid(ledger.blob_id))
                        )
                 )
             AND NOT EXISTS (SELECT 1 FROM custom_emoji emoji WHERE emoji.blob_id = ledger.blob_id)
             AND NOT EXISTS (SELECT 1 FROM workspace_emoji emoji WHERE emoji.blob_id = ledger.blob_id)
           ORDER BY ledger.created_at, ledger.installation_id, ledger.blob_id
           LIMIT $1
           FOR UPDATE OF ledger, blob SKIP LOCKED",
    )
    .bind(limit)
    .fetch_all(&mut **tx)
    .await?;
    let mut released_count = 0_u64;
    for (installation, blob) in &released {
        // Candidate predicates above are only a bounded prefilter. The blob row
        // is now locked, so re-read every lifecycle side table in a fresh
        // READ COMMITTED statement before releasing the durable charge.
        let still_releasable = sqlx::query_scalar::<_, bool>(
            r"SELECT blob.finalized_at IS NOT NULL
                       AND NOT EXISTS (
                           SELECT 1 FROM integration_blob_receipts receipt
                            WHERE receipt.installation_id = $1
                              AND receipt.blob_id = blob.id
                       )
                       AND NOT EXISTS (
                           SELECT 1 FROM messages message
                            WHERE message.deleted_at IS NULL
                              AND (message.expires_at IS NULL OR message.expires_at > now())
                              AND message.blocks @> jsonb_build_array(
                                  jsonb_build_object('blob_id', aero_uuid_to_ulid(blob.id))
                              )
                       )
                       AND NOT EXISTS (SELECT 1 FROM custom_emoji emoji WHERE emoji.blob_id = blob.id)
                       AND NOT EXISTS (SELECT 1 FROM workspace_emoji emoji WHERE emoji.blob_id = blob.id)
                  FROM blobs blob
                 WHERE blob.id = $2",
        )
        .bind(installation)
        .bind(blob)
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or(false);
        if !still_releasable {
            continue;
        }
        sqlx::query(
            "DELETE FROM integration_blob_ledger WHERE installation_id = $1 AND blob_id = $2",
        )
        .bind(installation)
        .bind(blob)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            r"INSERT INTO blob_gc_queue (blob_id, force_delete)
              SELECT $1, FALSE
               WHERE NOT EXISTS (
                         SELECT 1 FROM integration_blob_ledger other
                          WHERE other.blob_id = $1
                     )
                 AND NOT EXISTS (
                         SELECT 1 FROM integration_blob_receipts receipt
                          WHERE receipt.blob_id = $1
                     )
              ON CONFLICT (blob_id) DO UPDATE
                  SET force_delete = blob_gc_queue.force_delete OR EXCLUDED.force_delete",
        )
        .bind(blob)
        .execute(&mut **tx)
        .await?;
        released_count = released_count.saturating_add(1);
    }
    Ok(released_count)
}
