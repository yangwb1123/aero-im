//! Cross-node machine-request leases and integration-owned blob commits.

use aero_common::{BlobId, Error, ParticipantId, RoomId, WorkspaceId};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::support::{
    find_probe_receipt, lock_installation, map_dm_error, validate_identity_component,
};
use super::{
    IntegrationInstallation, IntegrationPublishOutcome, IntegrationReplayProbe, IntegrationRepo,
    IntegrationTarget, ResolvedIntegrationTarget,
};
use crate::DmRepo;

mod blob;
mod preauthorize;
mod sweep;
pub use blob::{
    IntegrationBlobCommit, IntegrationBlobCommitResolution, IntegrationBlobOutcome,
    IntegrationBlobQuotaReservation, MAX_INTEGRATION_BLOBS, MAX_INTEGRATION_BLOB_BYTES,
};
pub use sweep::IntegrationMachineSweep;

const MAX_INSTALLATION_REQUESTS_PER_MINUTE: i64 = 120;
const NOTIFICATION_LEASE_SECONDS: i32 = 30;
const BLOB_LEASE_SECONDS: i32 = 300;

#[derive(Clone)]
pub struct PreparedIntegrationTarget {
    pub installation: IntegrationInstallation,
    pub target: IntegrationTarget,
    /// Existing/room-policy room. `None` means a permitted DM still needs to be
    /// materialized after content and rate policy succeeds.
    pub room_id: Option<RoomId>,
    pub recipient: Option<ParticipantId>,
}

#[derive(Clone)]
pub enum IntegrationNotificationClaim {
    Acquired {
        lease_token: Uuid,
        target: PreparedIntegrationTarget,
    },
    Pending,
    Replay(IntegrationPublishOutcome),
}

#[derive(Clone)]
pub struct IntegrationBlobProbe {
    pub installation_id: Uuid,
    pub issuer: String,
    pub client_id: String,
    pub idempotency_key: Uuid,
    pub request_hash: [u8; 32],
    pub target: IntegrationTarget,
}

#[derive(Clone)]
pub enum IntegrationBlobClaim {
    Acquired {
        lease_token: Uuid,
        target: PreparedIntegrationTarget,
    },
    Pending,
    Replay(IntegrationBlobOutcome),
}

#[rustfmt::skip]
impl_redacted_debug!(PreparedIntegrationTarget, IntegrationNotificationClaim,
    IntegrationBlobProbe, IntegrationBlobClaim);

#[derive(sqlx::FromRow)]
struct MachineRequestRow {
    request_hash: Vec<u8>,
    target_kind: String,
    target_key: String,
    status: String,
    lease_live: bool,
    charged_current_window: bool,
}

impl IntegrationRepo {
    /// Acquire the notification's cross-node fencing lease before any
    /// side-effecting policy is consumed.
    pub async fn claim_notification(
        &self,
        probe: &IntegrationReplayProbe,
    ) -> Result<IntegrationNotificationClaim, Error> {
        let mut tx = self.pool.begin().await?;
        let installation = authorized_installation_in_tx(
            &mut tx,
            probe.installation_id,
            &probe.issuer,
            &probe.client_id,
        )
        .await?;
        lock_request_key(
            &mut tx,
            probe.installation_id,
            "notification",
            probe.idempotency_key,
        )
        .await?;
        expire_request_key_in_tx(
            &mut tx,
            probe.installation_id,
            "notification",
            probe.idempotency_key,
        )
        .await?;
        if let Some(existing) = find_probe_receipt(&mut tx, probe).await? {
            tx.commit().await?;
            return self
                .load_existing(existing)
                .await
                .map(IntegrationNotificationClaim::Replay);
        }
        match inspect_request(
            &mut tx,
            probe.installation_id,
            "notification",
            probe.idempotency_key,
            &probe.request_hash,
            &probe.target,
        )
        .await?
        {
            InspectRequest::Pending => {
                tx.commit().await?;
                Ok(IntegrationNotificationClaim::Pending)
            }
            InspectRequest::Acquire { charge } => {
                let target = prepare_target_in_tx(&mut tx, installation, &probe.target).await?;
                let lease_token = acquire_request_in_tx(
                    &mut tx,
                    probe.installation_id,
                    "notification",
                    probe.idempotency_key,
                    &probe.request_hash,
                    &probe.target,
                    NOTIFICATION_LEASE_SECONDS,
                    charge,
                )
                .await?;
                tx.commit().await?;
                Ok(IntegrationNotificationClaim::Acquired {
                    lease_token,
                    target,
                })
            }
        }
    }

    /// Acquire a blob request after multipart bytes have been bounded, scanned,
    /// and hashed. The target is authorized without creating a direct room.
    pub async fn claim_blob(
        &self,
        probe: &IntegrationBlobProbe,
    ) -> Result<IntegrationBlobClaim, Error> {
        let mut tx = self.pool.begin().await?;
        let installation = authorized_installation_in_tx(
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
        expire_request_key_in_tx(
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
                .map(IntegrationBlobClaim::Replay);
        }
        match inspect_request(
            &mut tx,
            probe.installation_id,
            "blob",
            probe.idempotency_key,
            &probe.request_hash,
            &probe.target,
        )
        .await?
        {
            InspectRequest::Pending => {
                tx.commit().await?;
                Ok(IntegrationBlobClaim::Pending)
            }
            InspectRequest::Acquire { charge } => {
                let target = prepare_target_in_tx(&mut tx, installation, &probe.target).await?;
                let lease_token = acquire_request_in_tx(
                    &mut tx,
                    probe.installation_id,
                    "blob",
                    probe.idempotency_key,
                    &probe.request_hash,
                    &probe.target,
                    BLOB_LEASE_SECONDS,
                    charge,
                )
                .await?;
                tx.commit().await?;
                Ok(IntegrationBlobClaim::Acquired {
                    lease_token,
                    target,
                })
            }
        }
    }

    /// Revalidate a prepared target and materialize a DM only after request
    /// content and both rate-limit tiers have passed.
    pub async fn materialize_target(
        &self,
        prepared: PreparedIntegrationTarget,
    ) -> Result<ResolvedIntegrationTarget, Error> {
        let current = self
            .authorize_client(
                prepared.installation.id,
                &prepared.installation.issuer,
                &prepared.installation.client_id,
            )
            .await?;
        if current.bot_id != prepared.installation.bot_id
            || current.workspace_id != prepared.installation.workspace_id
        {
            return Err(Error::Forbidden(
                "integration installation changed during request".into(),
            ));
        }
        match (&prepared.target, prepared.recipient) {
            (IntegrationTarget::Room(room), None) if prepared.room_id == Some(*room) => {
                Ok(ResolvedIntegrationTarget {
                    installation: current,
                    target: prepared.target.clone(),
                    room_id: *room,
                    recipient: None,
                    created_room: false,
                })
            }
            (IntegrationTarget::SnaplinkUser(_), Some(recipient)) => {
                let dms = DmRepo::new(self.pool.clone());
                let existed = dms
                    .find_direct_in_workspace(current.workspace_id, current.bot_id, recipient)
                    .await?
                    .is_some();
                let room = dms
                    .find_or_create_in_workspace(current.workspace_id, current.bot_id, recipient)
                    .await
                    .map_err(map_dm_error)?;
                Ok(ResolvedIntegrationTarget {
                    installation: current,
                    target: prepared.target,
                    room_id: room.id,
                    recipient: Some(recipient),
                    created_room: !existed,
                })
            }
            _ => Err(Error::Invalid(
                "integration target preparation mismatch".into(),
            )),
        }
    }

    /// Remove only a direct room materialized by this failed request and still
    /// containing no messages. Concurrent successful use therefore wins.
    pub async fn cleanup_empty_dm(
        &self,
        resolved: &ResolvedIntegrationTarget,
    ) -> Result<bool, Error> {
        let Some(recipient) = resolved.recipient else {
            return Ok(false);
        };
        if !resolved.created_room {
            return Ok(false);
        }
        let (low, high) = sorted_pair(resolved.installation.bot_id, recipient);
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT true FROM workspaces WHERE id = $1 FOR SHARE")
            .bind(resolved.installation.workspace_id.to_uuid())
            .fetch_optional(&mut *tx)
            .await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!(
                "aero:dm:{}:{low}:{high}",
                resolved.installation.workspace_id
            ))
            .execute(&mut *tx)
            .await?;
        let deleted = sqlx::query(
            r"DELETE FROM rooms room
                WHERE room.id = $1
                  AND room.workspace_id = $2
                  AND room.kind = 'direct'
                  AND NOT EXISTS (SELECT 1 FROM messages message WHERE message.room_id = room.id)
                  AND (SELECT count(*) FROM room_members member WHERE member.room_id = room.id) = 2
                  AND EXISTS (SELECT 1 FROM room_members member WHERE member.room_id = room.id AND member.participant_id = $3)
                  AND EXISTS (SELECT 1 FROM room_members member WHERE member.room_id = room.id AND member.participant_id = $4)",
        )
        .bind(resolved.room_id.to_uuid())
        .bind(resolved.installation.workspace_id.to_uuid())
        .bind(low.to_uuid())
        .bind(high.to_uuid())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        tx.commit().await?;
        Ok(deleted)
    }

    pub async fn release_notification_claim(
        &self,
        installation: Uuid,
        key: Uuid,
        lease_token: Uuid,
    ) -> Result<(), Error> {
        self.release_claim(installation, "notification", key, lease_token)
            .await
    }

    pub async fn release_blob_claim(
        &self,
        installation: Uuid,
        key: Uuid,
        lease_token: Uuid,
    ) -> Result<(), Error> {
        self.release_claim(installation, "blob", key, lease_token)
            .await
    }

    async fn release_claim(
        &self,
        installation: Uuid,
        operation: &str,
        key: Uuid,
        lease_token: Uuid,
    ) -> Result<(), Error> {
        sqlx::query(
            r"UPDATE integration_machine_requests
                  SET status = 'retryable', lease_token = NULL,
                      lease_expires_at = NULL, updated_at = now()
                WHERE installation_id = $1 AND operation = $2
                  AND idempotency_key = $3 AND status = 'processing'
                  AND lease_token = $4",
        )
        .bind(installation)
        .bind(operation)
        .bind(key)
        .bind(lease_token)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

enum InspectRequest {
    Pending,
    Acquire { charge: bool },
}

async fn inspect_request(
    tx: &mut Transaction<'_, Postgres>,
    installation: Uuid,
    operation: &str,
    key: Uuid,
    request_hash: &[u8; 32],
    target: &IntegrationTarget,
) -> Result<InspectRequest, Error> {
    let row = sqlx::query_as::<_, MachineRequestRow>(
        r"SELECT request_hash, target_kind, target_key, status,
                 COALESCE(lease_expires_at > now(), FALSE) AS lease_live,
                 charged_at >= date_trunc('minute', now()) AS charged_current_window
            FROM integration_machine_requests
           WHERE installation_id = $1 AND operation = $2 AND idempotency_key = $3
           FOR UPDATE",
    )
    .bind(installation)
    .bind(operation)
    .bind(key)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(InspectRequest::Acquire { charge: true });
    };
    validate_request_identity(
        &row.request_hash,
        &row.target_kind,
        &row.target_key,
        request_hash,
        target,
    )?;
    if row.status == "completed" {
        return Err(Error::Conflict(
            "completed integration request has no canonical receipt".into(),
        ));
    }
    if row.status == "processing" && row.lease_live {
        return Ok(InspectRequest::Pending);
    }
    Ok(InspectRequest::Acquire {
        charge: !row.charged_current_window,
    })
}

#[allow(clippy::too_many_arguments)] // One fenced row identity plus lease/rate policy.
async fn acquire_request_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    installation: Uuid,
    operation: &str,
    key: Uuid,
    request_hash: &[u8; 32],
    target: &IntegrationTarget,
    lease_seconds: i32,
    charge: bool,
) -> Result<Uuid, Error> {
    if charge {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("aero:integration-rate:{installation}"))
            .execute(&mut **tx)
            .await?;
        let count = sqlx::query_scalar::<_, i64>(
            r"SELECT count(*) FROM (
                  SELECT 1 FROM integration_machine_requests
                   WHERE installation_id = $1
                     AND charged_at >= date_trunc('minute', now())
                   LIMIT $2
              ) bounded",
        )
        .bind(installation)
        .bind(MAX_INSTALLATION_REQUESTS_PER_MINUTE)
        .fetch_one(&mut **tx)
        .await?;
        if count >= MAX_INSTALLATION_REQUESTS_PER_MINUTE {
            return Err(Error::RateLimited);
        }
    }
    let lease_token = Uuid::new_v4();
    sqlx::query(
        r"INSERT INTO integration_machine_requests
              (installation_id, operation, idempotency_key, request_hash,
               target_kind, target_key, status, lease_token, lease_expires_at,
               charged_at, created_at, updated_at, expires_at)
           VALUES ($1, $2, $3, $4, $5, $6, 'processing', $7,
                   now() + make_interval(secs => $8), now(), now(), now(),
                   now() + interval '7 days')
           ON CONFLICT (installation_id, operation, idempotency_key) DO UPDATE
             SET status = 'processing', lease_token = EXCLUDED.lease_token,
                 lease_expires_at = EXCLUDED.lease_expires_at,
                 charged_at = CASE WHEN $9 THEN now()
                                   ELSE integration_machine_requests.charged_at END,
                 updated_at = now(), expires_at = now() + interval '7 days'",
    )
    .bind(installation)
    .bind(operation)
    .bind(key)
    .bind(request_hash.as_slice())
    .bind(target.kind())
    .bind(target.key())
    .bind(lease_token)
    .bind(lease_seconds)
    .bind(charge)
    .execute(&mut **tx)
    .await?;
    Ok(lease_token)
}

pub(super) async fn assert_claim_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    installation: Uuid,
    operation: &str,
    key: Uuid,
    request_hash: &[u8; 32],
    target: &IntegrationTarget,
    lease_token: Uuid,
) -> Result<(), Error> {
    let row = sqlx::query_as::<_, (Vec<u8>, String, String)>(
        r"SELECT request_hash, target_kind, target_key
            FROM integration_machine_requests
           WHERE installation_id = $1 AND operation = $2 AND idempotency_key = $3
             AND status = 'processing' AND lease_token = $4
             AND lease_expires_at > now()
           FOR UPDATE",
    )
    .bind(installation)
    .bind(operation)
    .bind(key)
    .bind(lease_token)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| Error::Conflict("integration request lease was lost".into()))?;
    validate_request_identity(&row.0, &row.1, &row.2, request_hash, target)
}

pub(super) async fn complete_claim_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    installation: Uuid,
    operation: &str,
    key: Uuid,
    lease_token: Uuid,
) -> Result<(), Error> {
    let updated = sqlx::query(
        r"UPDATE integration_machine_requests
              SET status = 'completed', lease_token = NULL,
                  lease_expires_at = NULL, updated_at = now(),
                  expires_at = now() + interval '7 days'
            WHERE installation_id = $1 AND operation = $2
              AND idempotency_key = $3 AND status = 'processing'
              AND lease_token = $4",
    )
    .bind(installation)
    .bind(operation)
    .bind(key)
    .bind(lease_token)
    .execute(&mut **tx)
    .await?;
    if updated.rows_affected() == 1 {
        Ok(())
    } else {
        Err(Error::Conflict("integration request lease was lost".into()))
    }
}

async fn authorized_installation_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    issuer: &str,
    client_id: &str,
) -> Result<IntegrationInstallation, Error> {
    validate_identity_component(issuer, 2_048, "issuer")?;
    validate_identity_component(client_id, 512, "client_id")?;
    let workspace = sqlx::query_scalar::<_, Uuid>(
        "SELECT workspace_id FROM integration_installations WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .map(WorkspaceId::from_uuid)
    .ok_or_else(|| Error::Forbidden("integration installation is unavailable".into()))?;
    let workspace_exists =
        sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR SHARE")
            .bind(workspace.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .unwrap_or(false);
    if !workspace_exists {
        return Err(Error::Forbidden(
            "integration installation is unavailable".into(),
        ));
    }
    let installation = lock_installation(tx, workspace, id, false)
        .await?
        .filter(|row| row.active && row.issuer == issuer && row.client_id == client_id)
        .ok_or_else(|| Error::Forbidden("integration installation is unavailable".into()))?;
    let effective = sqlx::query_scalar::<_, bool>("SELECT aero_effective_workspace_access($1, $2)")
        .bind(workspace.to_uuid())
        .bind(installation.bot_id.to_uuid())
        .fetch_one(&mut **tx)
        .await?;
    if !effective {
        return Err(Error::Forbidden(
            "integration bot no longer has workspace access".into(),
        ));
    }
    Ok(installation)
}

async fn prepare_target_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    installation: IntegrationInstallation,
    target: &IntegrationTarget,
) -> Result<PreparedIntegrationTarget, Error> {
    match target {
        IntegrationTarget::Room(room) => {
            let allowed = sqlx::query_scalar::<_, bool>(
                r"SELECT aero_effective_room_access($1, $2, NULL)
                    FROM integration_installation_rooms allowed
                    JOIN rooms room ON room.id = allowed.room_id
                   WHERE allowed.installation_id = $3 AND allowed.room_id = $1
                     AND room.workspace_id = $4",
            )
            .bind(room.to_uuid())
            .bind(installation.bot_id.to_uuid())
            .bind(installation.id)
            .bind(installation.workspace_id.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .unwrap_or(false);
            if !allowed {
                return Err(Error::Forbidden(
                    "integration room target is not allowed".into(),
                ));
            }
            Ok(PreparedIntegrationTarget {
                installation,
                target: target.clone(),
                room_id: Some(*room),
                recipient: None,
            })
        }
        IntegrationTarget::SnaplinkUser(subject) => {
            if !installation.allow_user_dm {
                return Err(Error::Forbidden(
                    "integration user targets are disabled".into(),
                ));
            }
            validate_identity_component(subject, 2_048, "subject")?;
            let recipient = sqlx::query_scalar::<_, Uuid>(
                r"SELECT identity.participant_id
                    FROM sso_identities identity
                   WHERE identity.issuer = $1 AND identity.subject = $2
                     AND aero_effective_workspace_access($3, identity.participant_id)",
            )
            .bind(&installation.user_identity_issuer)
            .bind(subject)
            .bind(installation.workspace_id.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .map(ParticipantId::from_uuid)
            .ok_or_else(|| Error::NotFound("Snaplink user".into()))?;
            if recipient == installation.bot_id {
                return Err(Error::Invalid(
                    "integration cannot target its own bot".into(),
                ));
            }
            let forbidden = sqlx::query_scalar::<_, bool>(
                r"SELECT EXISTS (
                      SELECT 1 FROM user_blocks
                       WHERE (blocker_id = $1 AND blocked_id = $2)
                          OR (blocker_id = $2 AND blocked_id = $1)
                  ) OR EXISTS (
                      SELECT 1 FROM info_barriers barrier
                      JOIN user_group_members side_a ON side_a.group_id = barrier.group_a
                      JOIN user_group_members side_b ON side_b.group_id = barrier.group_b
                       WHERE barrier.workspace_id = $3
                         AND ((side_a.participant_id = $1 AND side_b.participant_id = $2)
                           OR (side_a.participant_id = $2 AND side_b.participant_id = $1))
                  )",
            )
            .bind(installation.bot_id.to_uuid())
            .bind(recipient.to_uuid())
            .bind(installation.workspace_id.to_uuid())
            .fetch_one(&mut **tx)
            .await?;
            if forbidden {
                return Err(Error::Forbidden(
                    "integration DM policy forbids this target".into(),
                ));
            }
            let room_id = find_direct_in_tx(
                tx,
                installation.workspace_id,
                installation.bot_id,
                recipient,
            )
            .await?;
            Ok(PreparedIntegrationTarget {
                installation,
                target: target.clone(),
                room_id,
                recipient: Some(recipient),
            })
        }
    }
}

async fn find_direct_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    first: ParticipantId,
    second: ParticipantId,
) -> Result<Option<RoomId>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        r"SELECT room.id FROM rooms room
           WHERE room.workspace_id = $1 AND room.kind = 'direct'
             AND (SELECT count(*) FROM room_members member WHERE member.room_id = room.id) = 2
             AND EXISTS (SELECT 1 FROM room_members member WHERE member.room_id = room.id AND member.participant_id = $2)
             AND EXISTS (SELECT 1 FROM room_members member WHERE member.room_id = room.id AND member.participant_id = $3)
           ORDER BY room.id LIMIT 1",
    )
    .bind(workspace.to_uuid())
    .bind(first.to_uuid())
    .bind(second.to_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map(|room| room.map(RoomId::from_uuid))
}

pub(super) async fn lock_request_key(
    tx: &mut Transaction<'_, Postgres>,
    installation: Uuid,
    operation: &str,
    key: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("aero:integration:{operation}:{installation}:{key}"))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn expire_request_key_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    installation: Uuid,
    operation: &str,
    key: Uuid,
) -> Result<(), sqlx::Error> {
    let receipt_table = match operation {
        "notification" => "integration_notification_receipts",
        "blob" => "integration_blob_receipts",
        _ => {
            return Err(sqlx::Error::Protocol(
                "unknown integration operation".into(),
            ))
        }
    };
    // Global lock order is machine request -> canonical receipt. The retention
    // sweep selects/locks the request first as well; reversing these two
    // deletes creates a deadlock at the seven-day expiry boundary.
    sqlx::query(
        r"DELETE FROM integration_machine_requests
            WHERE installation_id = $1 AND operation = $2 AND idempotency_key = $3
              AND status <> 'processing' AND expires_at <= now()",
    )
    .bind(installation)
    .bind(operation)
    .bind(key)
    .execute(&mut **tx)
    .await?;
    sqlx::query(&format!(
        "DELETE FROM {receipt_table} WHERE installation_id = $1 AND idempotency_key = $2 AND expires_at <= now()"
    ))
    .bind(installation)
    .bind(key)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn validate_request_identity(
    stored_hash: &[u8],
    stored_kind: &str,
    stored_key: &str,
    request_hash: &[u8; 32],
    target: &IntegrationTarget,
) -> Result<(), Error> {
    if stored_hash != request_hash || stored_kind != target.kind() || stored_key != target.key() {
        Err(Error::Conflict(
            "Idempotency-Key was already used for a different integration request".into(),
        ))
    } else {
        Ok(())
    }
}

async fn find_blob_receipt_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    probe: &IntegrationBlobProbe,
) -> Result<Option<BlobId>, Error> {
    let row = sqlx::query_as::<_, (Vec<u8>, String, String, Uuid)>(
        r"SELECT request_hash, target_kind, target_key, blob_id
            FROM integration_blob_receipts
           WHERE installation_id = $1 AND idempotency_key = $2
             AND expires_at > now()
           FOR SHARE",
    )
    .bind(probe.installation_id)
    .bind(probe.idempotency_key)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((hash, kind, key, blob)) = row else {
        return Ok(None);
    };
    validate_request_identity(&hash, &kind, &key, &probe.request_hash, &probe.target)?;
    Ok(Some(BlobId::from_uuid(blob)))
}

fn sorted_pair(first: ParticipantId, second: ParticipantId) -> (ParticipantId, ParticipantId) {
    if first.to_uuid() <= second.to_uuid() {
        (first, second)
    } else {
        (second, first)
    }
}
