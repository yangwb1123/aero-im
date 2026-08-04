//! Transaction-owned integration blob publication and quota enforcement.

use aero_common::{Blob, BlobId, Error, FileKind, ParticipantId, RoomId, WorkspaceId};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::{
    assert_claim_in_tx, authorized_installation_in_tx, complete_claim_in_tx,
    find_blob_receipt_in_tx, lock_request_key, IntegrationBlobProbe,
};
use crate::audit::AuditRepo;
use crate::blob::BlobRepo;
use crate::integration::support::{validate_identity_component, validate_target_binding};
use crate::integration::{IntegrationRepo, IntegrationTarget};
use crate::message::authorization::{lock_effective_message_write_access, PostPolicy};

/// Maximum number of distinct blobs retained by one installation.
pub const MAX_INTEGRATION_BLOBS: i64 = 10_000;
/// Maximum aggregate bytes retained by one installation (10 GiB).
pub const MAX_INTEGRATION_BLOB_BYTES: i64 = 10 * 1024 * 1024 * 1024;

/// Fenced, fail-closed quota reservation created before bytes leave the
/// application process. The durable ledger row is deliberately retained if the
/// caller is cancelled after this transaction commits; stale-reservation GC is
/// then the only path that releases the charge.
#[derive(Clone)]
pub struct IntegrationBlobQuotaReservation {
    pub installation_id: Uuid,
    pub issuer: String,
    pub client_id: String,
    pub idempotency_key: Uuid,
    pub request_hash: [u8; 32],
    pub target: IntegrationTarget,
    pub room_id: RoomId,
    pub recipient: Option<ParticipantId>,
    pub lease_token: Uuid,
    pub blob_id: BlobId,
    pub content_sha256: String,
}

#[derive(Clone)]
pub struct IntegrationBlobCommit {
    pub installation_id: Uuid,
    pub issuer: String,
    pub client_id: String,
    pub idempotency_key: Uuid,
    pub request_hash: [u8; 32],
    pub target: IntegrationTarget,
    pub room_id: RoomId,
    pub recipient: Option<ParticipantId>,
    pub lease_token: Uuid,
    pub blob_id: BlobId,
    pub content_sha256: String,
    /// `Some` publishes an unfinished reservation; `None` registers a verified
    /// finalized content-dedup hit.
    pub storage_key: Option<String>,
}

#[derive(Clone)]
pub struct IntegrationBlobOutcome {
    pub blob: Blob,
    pub deduplicated: bool,
}

#[derive(Clone)]
pub enum IntegrationBlobCommitResolution {
    /// The transaction committed despite an ambiguous transport result.
    Completed(Box<IntegrationBlobOutcome>),
    /// No receipt committed and this exact fencing token was atomically
    /// released. The caller may safely delete its unpublished reservation.
    Released,
    /// Another owner holds the key, or state is otherwise not safe to clean.
    Lost,
}

#[rustfmt::skip]
impl_redacted_debug!(IntegrationBlobQuotaReservation, IntegrationBlobCommit,
    IntegrationBlobOutcome, IntegrationBlobCommitResolution);

#[derive(sqlx::FromRow)]
struct BlobCommitRow {
    id: Uuid,
    owner_id: Uuid,
    workspace_id: Option<Uuid>,
    storage_region: Option<String>,
    kind: String,
    name: String,
    mime: String,
    size: i64,
    sha256: Option<String>,
    storage_key: String,
    created_at: time::OffsetDateTime,
    finalized_at: Option<time::OffsetDateTime>,
}

impl BlobCommitRow {
    fn into_blob(self) -> Blob {
        Blob {
            id: BlobId::from_uuid(self.id),
            owner_id: ParticipantId::from_uuid(self.owner_id),
            workspace_id: self.workspace_id.map(WorkspaceId::from_uuid),
            storage_region: self.storage_region,
            kind: match self.kind.as_str() {
                "image" => FileKind::Image,
                "video" => FileKind::Video,
                "audio" => FileKind::Audio,
                "document" => FileKind::Document,
                _ => FileKind::Other,
            },
            name: self.name,
            mime: self.mime,
            size: u64::try_from(self.size).unwrap_or_default(),
            sha256: self.sha256,
            storage_key: self.storage_key,
            created_at: self.created_at,
            finalized_at: self.finalized_at,
        }
    }
}

impl IntegrationRepo {
    /// Reserve installation storage quota before writing an unfinished blob to
    /// object storage. Authorization, target policy, request identity, and the
    /// exact live fencing token are all rechecked in the same transaction that
    /// inserts the durable quota ledger row.
    pub async fn reserve_blob_upload_quota(
        &self,
        new: IntegrationBlobQuotaReservation,
    ) -> Result<(), Error> {
        validate_identity_component(&new.content_sha256, 64, "content_sha256")?;
        let mut tx = self.pool.begin().await?;
        let installation = authorized_installation_in_tx(
            &mut tx,
            new.installation_id,
            &new.issuer,
            &new.client_id,
        )
        .await?;
        lock_request_key(&mut tx, new.installation_id, "blob", new.idempotency_key).await?;
        assert_claim_in_tx(
            &mut tx,
            new.installation_id,
            "blob",
            new.idempotency_key,
            &new.request_hash,
            &new.target,
            new.lease_token,
        )
        .await?;
        validate_target_binding(
            &mut tx,
            &installation,
            &new.target,
            new.room_id,
            new.recipient,
        )
        .await?;
        let access = lock_effective_message_write_access(
            &mut tx,
            new.room_id,
            installation.bot_id,
            PostPolicy::Enforce,
        )
        .await?;
        if access.map_or(true, |access| access.workspace != installation.workspace_id) {
            return Err(Error::Forbidden(
                "integration bot lost blob target access".into(),
            ));
        }

        let row = lock_blob_reservation_in_tx(&mut tx, new.blob_id).await?;
        let queued = blob_is_gc_queued_in_tx(&mut tx, new.blob_id).await?;
        if row.owner_id != installation.bot_id.to_uuid()
            || row.workspace_id != Some(installation.workspace_id.to_uuid())
            || row.size < 0
            || row.sha256.as_deref() != Some(new.content_sha256.as_str())
            || row.finalized_at.is_some()
            || queued
        {
            return Err(Error::Conflict(
                "integration blob reservation changed before quota reservation".into(),
            ));
        }

        reserve_blob_quota_in_tx(&mut tx, new.installation_id, new.blob_id, row.size).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Resolve an ambiguous `commit_blob` result before the caller decides
    /// whether its object-store write is safe to delete.
    pub async fn resolve_blob_commit(
        &self,
        probe: &IntegrationBlobProbe,
        lease_token: Uuid,
    ) -> Result<IntegrationBlobCommitResolution, Error> {
        let mut tx = self.pool.begin().await?;
        authorized_installation_in_tx(
            &mut tx,
            probe.installation_id,
            &probe.issuer,
            &probe.client_id,
        )
        .await?;
        lock_request_key(
            &mut tx,
            probe.installation_id,
            "blob",
            probe.idempotency_key,
        )
        .await?;
        if let Some(blob_id) = find_blob_receipt_in_tx(&mut tx, probe).await? {
            tx.commit().await?;
            return self
                .load_blob_outcome(blob_id, true)
                .await
                .map(Box::new)
                .map(IntegrationBlobCommitResolution::Completed);
        }
        let released = sqlx::query(
            r"UPDATE integration_machine_requests
                  SET status = 'retryable', lease_token = NULL,
                      lease_expires_at = NULL, updated_at = now()
                WHERE installation_id = $1 AND operation = 'blob'
                  AND idempotency_key = $2 AND status = 'processing'
                  AND lease_token = $3 AND request_hash = $4
                  AND target_kind = $5 AND target_key = $6",
        )
        .bind(probe.installation_id)
        .bind(probe.idempotency_key)
        .bind(lease_token)
        .bind(probe.request_hash.as_slice())
        .bind(probe.target.kind())
        .bind(probe.target.key())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        tx.commit().await?;
        Ok(if released {
            IntegrationBlobCommitResolution::Released
        } else {
            IntegrationBlobCommitResolution::Lost
        })
    }

    /// Atomically recheck installation/bot/target/quota, publish an unfinished
    /// reservation when needed, and persist the canonical upload receipt.
    pub async fn commit_blob(
        &self,
        new: IntegrationBlobCommit,
    ) -> Result<IntegrationBlobOutcome, Error> {
        validate_identity_component(&new.content_sha256, 64, "content_sha256")?;
        let mut tx = self.pool.begin().await?;
        let installation = authorized_installation_in_tx(
            &mut tx,
            new.installation_id,
            &new.issuer,
            &new.client_id,
        )
        .await?;
        lock_request_key(&mut tx, new.installation_id, "blob", new.idempotency_key).await?;
        let probe = IntegrationBlobProbe {
            installation_id: new.installation_id,
            issuer: new.issuer.clone(),
            client_id: new.client_id.clone(),
            idempotency_key: new.idempotency_key,
            request_hash: new.request_hash,
            target: new.target.clone(),
        };
        if let Some(blob_id) = find_blob_receipt_in_tx(&mut tx, &probe).await? {
            tx.commit().await?;
            return self.load_blob_outcome(blob_id, true).await;
        }
        assert_claim_in_tx(
            &mut tx,
            new.installation_id,
            "blob",
            new.idempotency_key,
            &new.request_hash,
            &new.target,
            new.lease_token,
        )
        .await?;
        validate_target_binding(
            &mut tx,
            &installation,
            &new.target,
            new.room_id,
            new.recipient,
        )
        .await?;
        let access = lock_effective_message_write_access(
            &mut tx,
            new.room_id,
            installation.bot_id,
            PostPolicy::Enforce,
        )
        .await?;
        if access.map_or(true, |access| access.workspace != installation.workspace_id) {
            return Err(Error::Forbidden(
                "integration bot lost blob target access".into(),
            ));
        }

        // Acquire the blob lifecycle lock first, then evaluate predicates over
        // side tables in a fresh statement. This avoids READ COMMITTED retaining
        // a pre-wait `blob_gc_queue` result after a conflicting sweep commits.
        let row = lock_blob_reservation_in_tx(&mut tx, new.blob_id).await?;
        let queued = blob_is_gc_queued_in_tx(&mut tx, new.blob_id).await?;
        if row.owner_id != installation.bot_id.to_uuid()
            || row.workspace_id != Some(installation.workspace_id.to_uuid())
            || row.size < 0
            || row.sha256.as_deref() != Some(new.content_sha256.as_str())
            || queued
            || (new.storage_key.is_some() && row.finalized_at.is_some())
            || (new.storage_key.is_none() && row.finalized_at.is_none())
        {
            return Err(Error::Conflict(
                "integration blob reservation changed before commit".into(),
            ));
        }

        reserve_blob_quota_in_tx(&mut tx, new.installation_id, new.blob_id, row.size).await?;
        if let Some(storage_key) = &new.storage_key {
            let updated = sqlx::query(
                r"UPDATE blobs
                      SET storage_key = $2, finalized_at = now()
                    WHERE id = $1 AND finalized_at IS NULL
                      AND NOT EXISTS (SELECT 1 FROM blob_gc_queue queue WHERE queue.blob_id = blobs.id)",
            )
            .bind(new.blob_id.to_uuid())
            .bind(storage_key)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(Error::Conflict(
                    "integration blob reservation could not be finalized".into(),
                ));
            }
        }
        sqlx::query(
            r"INSERT INTO integration_blob_receipts
                  (installation_id, idempotency_key, request_hash, target_kind,
                   target_key, room_id, blob_id)
               VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(new.installation_id)
        .bind(new.idempotency_key)
        .bind(new.request_hash.as_slice())
        .bind(new.target.kind())
        .bind(new.target.key())
        .bind(new.room_id.to_uuid())
        .bind(new.blob_id.to_uuid())
        .execute(&mut *tx)
        .await?;
        AuditRepo::append_in_tx(
            &mut tx,
            installation.workspace_id,
            None,
            "integration.blob.uploaded",
            Some(&new.blob_id.to_string()),
            serde_json::json!({
                "installation_id": new.installation_id,
                "target_kind": new.target.kind(),
                "room_id": new.room_id,
                "size": row.size,
            }),
        )
        .await?;
        complete_claim_in_tx(
            &mut tx,
            new.installation_id,
            "blob",
            new.idempotency_key,
            new.lease_token,
        )
        .await?;
        let blob = sqlx::query_as::<_, BlobCommitRow>(
            r"SELECT id, owner_id, workspace_id, storage_region, kind, name, mime,
                     size, sha256, storage_key, created_at, finalized_at
                FROM blobs WHERE id = $1 AND finalized_at IS NOT NULL",
        )
        .bind(new.blob_id.to_uuid())
        .fetch_one(&mut *tx)
        .await?
        .into_blob();
        tx.commit().await?;
        Ok(IntegrationBlobOutcome {
            blob,
            deduplicated: false,
        })
    }

    pub(super) async fn load_blob_outcome(
        &self,
        blob_id: BlobId,
        deduplicated: bool,
    ) -> Result<IntegrationBlobOutcome, Error> {
        let blob = BlobRepo::new(self.pool.clone())
            .get(blob_id)
            .await?
            .ok_or_else(|| Error::Conflict("canonical integration blob is unavailable".into()))?;
        Ok(IntegrationBlobOutcome { blob, deduplicated })
    }
}

#[derive(sqlx::FromRow)]
struct LockedBlobReservation {
    owner_id: Uuid,
    workspace_id: Option<Uuid>,
    size: i64,
    sha256: Option<String>,
    finalized_at: Option<time::OffsetDateTime>,
}

async fn lock_blob_reservation_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    blob_id: BlobId,
) -> Result<LockedBlobReservation, Error> {
    sqlx::query_as::<_, LockedBlobReservation>(
        r"SELECT owner_id, workspace_id, size, sha256, finalized_at
            FROM blobs
           WHERE id = $1
           FOR UPDATE",
    )
    .bind(blob_id.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| Error::Conflict("integration blob reservation is unavailable".into()))
}

async fn blob_is_gc_queued_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    blob_id: BlobId,
) -> Result<bool, Error> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM blob_gc_queue WHERE blob_id = $1)")
        .bind(blob_id.to_uuid())
        .fetch_one(&mut **tx)
        .await
        .map_err(Error::from)
}

async fn reserve_blob_quota_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    installation: Uuid,
    blob_id: BlobId,
    size: i64,
) -> Result<(), Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("aero:integration-blob-quota:{installation}"))
        .execute(&mut **tx)
        .await?;
    let already_counted = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM integration_blob_ledger WHERE installation_id = $1 AND blob_id = $2)",
    )
    .bind(installation)
    .bind(blob_id.to_uuid())
    .fetch_one(&mut **tx)
    .await?;
    if already_counted {
        return Ok(());
    }
    let (count, bytes) = sqlx::query_as::<_, (i64, i64)>(
        r"SELECT count(*), COALESCE(sum(item.size), 0)::bigint
            FROM (
                SELECT ledger.blob_id, blob.size
                  FROM integration_blob_ledger ledger
                  JOIN blobs blob ON blob.id = ledger.blob_id
                 WHERE ledger.installation_id = $1
                 LIMIT $2
            ) item",
    )
    .bind(installation)
    .bind(MAX_INTEGRATION_BLOBS)
    .fetch_one(&mut **tx)
    .await?;
    if count >= MAX_INTEGRATION_BLOBS || bytes.saturating_add(size) > MAX_INTEGRATION_BLOB_BYTES {
        return Err(Error::Conflict(
            "integration blob storage quota exceeded".into(),
        ));
    }
    sqlx::query("INSERT INTO integration_blob_ledger (installation_id, blob_id) VALUES ($1, $2)")
        .bind(installation)
        .bind(blob_id.to_uuid())
        .execute(&mut **tx)
        .await?;
    Ok(())
}
