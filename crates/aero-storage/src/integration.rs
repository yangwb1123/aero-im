//! Snaplink machine-client installations and durable notification publishing.
//!
//! Machine JWTs are authenticated at the HTTP edge, then matched here against
//! an administrator-owned installation. They never become `AuthUser`s. A
//! notification, its message event, post-commit jobs, idempotency receipt and
//! audit record commit in one transaction.

use aero_common::{
    Block, Error, Message, MessageEnvelope, ParticipantId, RoomEvent, RoomId, WorkspaceId,
};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::audit::AuditRepo;
use crate::blob::BlobRepo;
use crate::event_outbox::{EventOutboxKind, EventOutboxRepo, NewEventOutbox};
use crate::message::authorization::{lock_effective_message_write_access, PostPolicy};
use crate::message::{MessageRepo, NewMessage};
use crate::message_side_effect::{MessageSideEffectKind, MessageSideEffectRepo};
use crate::DmRepo;

#[rustfmt::skip]
macro_rules! impl_redacted_debug {
    ($($type_name:ident),+ $(,)?) => {$(
        impl std::fmt::Debug for $type_name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(concat!(stringify!($type_name), "([REDACTED])"))
            }
        }
    )+}; }

mod machine;
mod support;
pub use machine::{
    IntegrationBlobClaim, IntegrationBlobCommit, IntegrationBlobCommitResolution,
    IntegrationBlobOutcome, IntegrationBlobProbe, IntegrationBlobQuotaReservation,
    IntegrationMachineSweep, IntegrationNotificationClaim, PreparedIntegrationTarget,
    MAX_INTEGRATION_BLOBS, MAX_INTEGRATION_BLOB_BYTES,
};
#[cfg(test)]
use support::canonical_room_ids;
use support::{
    find_probe_receipt, find_receipt, insert_rooms, lock_installation, lock_valid_bot,
    lock_valid_rooms, map_dm_error, map_installation_write_error, validate_identity_component,
    validate_installation_fields, validate_publish_target, InstallationRow, INSTALLATION_SELECT,
};

pub const MAX_INTEGRATION_ROOMS: usize = 100;
pub const MAX_INTEGRATIONS_PER_WORKSPACE: i64 = 200;
const MAX_TRACEPARENT_BYTES: usize = 512;

#[derive(Clone, Serialize)]
pub struct IntegrationInstallation {
    pub id: Uuid,
    pub workspace_id: WorkspaceId,
    pub bot_id: ParticipantId,
    /// Exact issuer accepted for this installation's machine JWTs.
    pub issuer: String,
    /// Trusted human OIDC namespace used only for `snaplink_user` lookup.
    pub user_identity_issuer: String,
    pub client_id: String,
    pub name: String,
    pub active: bool,
    pub allow_user_dm: bool,
    pub room_ids: Vec<RoomId>,
    pub created_by: Option<ParticipantId>,
    pub created_at: time::OffsetDateTime,
    pub updated_at: time::OffsetDateTime,
}

#[derive(Clone)]
pub struct NewIntegrationInstallation {
    pub workspace_id: WorkspaceId,
    pub bot_id: ParticipantId,
    /// Exact issuer accepted for this installation's machine JWTs.
    pub issuer: String,
    /// Trusted human OIDC namespace used only for `snaplink_user` lookup.
    pub user_identity_issuer: String,
    pub client_id: String,
    pub name: String,
    pub allow_user_dm: bool,
    pub room_ids: Vec<RoomId>,
    pub created_by: ParticipantId,
}

#[derive(Clone, Default)]
pub struct UpdateIntegrationInstallation {
    pub bot_id: Option<ParticipantId>,
    /// Trusted control-plane issuer supplied by the server, never raw machine
    /// or administrator input.
    pub issuer: Option<String>,
    /// Trusted human OIDC namespace supplied by server configuration or an
    /// authenticated workspace administrator, never by a machine request.
    pub user_identity_issuer: Option<String>,
    pub client_id: Option<String>,
    pub name: Option<String>,
    pub active: Option<bool>,
    pub allow_user_dm: Option<bool>,
    pub room_ids: Option<Vec<RoomId>>,
}

#[derive(Clone)]
pub enum IntegrationTarget {
    Room(RoomId),
    SnaplinkUser(String),
}

#[derive(Clone)]
pub struct ResolvedIntegrationTarget {
    pub installation: IntegrationInstallation,
    pub target: IntegrationTarget,
    pub room_id: RoomId,
    pub recipient: Option<ParticipantId>,
    /// True only when this request materialized a previously absent direct room.
    /// HTTP callers use it to remove a still-empty room if a later policy or
    /// storage step fails.
    pub created_room: bool,
}

#[derive(Clone)]
pub struct NewIntegrationNotification {
    pub installation_id: Uuid,
    pub issuer: String,
    pub client_id: String,
    pub idempotency_key: Uuid,
    pub request_hash: [u8; 32],
    pub target: IntegrationTarget,
    pub room_id: RoomId,
    pub recipient: Option<ParticipantId>,
    pub blocks: Vec<Block>,
    pub traceparent: Option<String>,
    /// Cross-node fencing token acquired before request-side policies.  `None`
    /// is retained for trusted storage callers and migration tests; the machine
    /// HTTP surface always supplies a token.
    pub lease_token: Option<Uuid>,
}

/// Idempotency lookup that deliberately precedes target resolution. In
/// particular, a conflicting retry aimed at another Snaplink user must not
/// create an otherwise-unused DM before the conflict is reported.
#[derive(Clone)]
pub struct IntegrationReplayProbe {
    pub installation_id: Uuid,
    pub issuer: String,
    pub client_id: String,
    pub idempotency_key: Uuid,
    pub request_hash: [u8; 32],
    pub target: IntegrationTarget,
}

#[derive(Clone)]
pub struct IntegrationPublishOutcome {
    pub message: Message,
    pub outbox_id: Uuid,
    pub deduplicated: bool,
}

#[rustfmt::skip]
impl_redacted_debug!(IntegrationInstallation, NewIntegrationInstallation,
    UpdateIntegrationInstallation, IntegrationTarget, ResolvedIntegrationTarget,
    NewIntegrationNotification, IntegrationReplayProbe, IntegrationPublishOutcome);

#[derive(Clone)]
pub struct IntegrationRepo {
    pool: PgPool,
}

impl IntegrationRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(
        &self,
        new: NewIntegrationInstallation,
    ) -> Result<IntegrationInstallation, Error> {
        validate_installation_fields(
            &new.issuer,
            &new.user_identity_issuer,
            &new.client_id,
            &new.name,
            &new.room_ids,
        )?;
        let id = Uuid::new_v4();
        let mut tx = self.pool.begin().await?;
        crate::workspace::authz::assert_effective_admin_in_tx(
            &mut tx,
            new.workspace_id,
            new.created_by,
        )
        .await?;
        lock_valid_bot(&mut tx, new.workspace_id, new.bot_id).await?;
        lock_valid_rooms(&mut tx, new.workspace_id, new.bot_id, &new.room_ids).await?;
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM integration_installations WHERE workspace_id = $1",
        )
        .bind(new.workspace_id.to_uuid())
        .fetch_one(&mut *tx)
        .await?;
        if count >= MAX_INTEGRATIONS_PER_WORKSPACE {
            return Err(Error::Conflict(
                "integration installation quota exceeded".into(),
            ));
        }
        sqlx::query(
            r"INSERT INTO integration_installations
                  (id, workspace_id, bot_id, issuer, user_identity_issuer, client_id, name,
                   allow_user_dm, created_by)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(id)
        .bind(new.workspace_id.to_uuid())
        .bind(new.bot_id.to_uuid())
        .bind(&new.issuer)
        .bind(&new.user_identity_issuer)
        .bind(&new.client_id)
        .bind(&new.name)
        .bind(new.allow_user_dm)
        .bind(new.created_by.to_uuid())
        .execute(&mut *tx)
        .await
        .map_err(map_installation_write_error)?;
        insert_rooms(&mut tx, id, &new.room_ids).await?;
        AuditRepo::append_in_tx(
            &mut tx,
            new.workspace_id,
            Some(new.created_by),
            "integration.installation.created",
            Some(&id.to_string()),
            serde_json::json!({
                "bot_id": new.bot_id,
                "client_id": new.client_id,
                "room_count": new.room_ids.len(),
                "allow_user_dm": new.allow_user_dm,
            }),
        )
        .await?;
        tx.commit().await?;
        self.get(new.workspace_id, id)
            .await?
            .ok_or_else(|| Error::Internal(anyhow::anyhow!("created integration disappeared")))
    }

    pub async fn list(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<IntegrationInstallation>, sqlx::Error> {
        let rows = sqlx::query_as::<_, InstallationRow>(&format!(
            "{INSTALLATION_SELECT}
              WHERE installation.workspace_id = $1
              GROUP BY installation.id
              ORDER BY installation.created_at DESC
              LIMIT $2"
        ))
        .bind(workspace.to_uuid())
        .bind(MAX_INTEGRATIONS_PER_WORKSPACE)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn get(
        &self,
        workspace: WorkspaceId,
        id: Uuid,
    ) -> Result<Option<IntegrationInstallation>, sqlx::Error> {
        let row = sqlx::query_as::<_, InstallationRow>(&format!(
            "{INSTALLATION_SELECT}
              WHERE installation.workspace_id = $1 AND installation.id = $2
              GROUP BY installation.id"
        ))
        .bind(workspace.to_uuid())
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }

    pub async fn update(
        &self,
        workspace: WorkspaceId,
        id: Uuid,
        actor: ParticipantId,
        update: UpdateIntegrationInstallation,
    ) -> Result<IntegrationInstallation, Error> {
        let mut tx = self.pool.begin().await?;
        crate::workspace::authz::assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        let current = lock_installation(&mut tx, workspace, id, true)
            .await?
            .ok_or_else(|| Error::NotFound("integration installation".into()))?;

        let bot_id = update.bot_id.unwrap_or(current.bot_id);
        let issuer = update.issuer.as_deref().unwrap_or(&current.issuer);
        let user_identity_issuer = update
            .user_identity_issuer
            .as_deref()
            .unwrap_or(&current.user_identity_issuer);
        let client_id = update.client_id.as_deref().unwrap_or(&current.client_id);
        let name = update.name.as_deref().unwrap_or(&current.name);
        let room_ids = update.room_ids.clone().unwrap_or(current.room_ids);
        validate_installation_fields(issuer, user_identity_issuer, client_id, name, &room_ids)?;
        let issuer_rotated = update
            .issuer
            .as_deref()
            .is_some_and(|candidate| candidate != current.issuer);
        let user_identity_issuer_rotated = update
            .user_identity_issuer
            .as_deref()
            .is_some_and(|candidate| candidate != current.user_identity_issuer);
        let target_policy_changed = update.bot_id.is_some() || update.room_ids.is_some();
        let pure_revoke = update.active == Some(false)
            && update.bot_id.is_none()
            && update.issuer.is_none()
            && update.user_identity_issuer.is_none()
            && update.client_id.is_none()
            && update.name.is_none()
            && update.allow_user_dm.is_none()
            && update.room_ids.is_none();
        if !pure_revoke {
            lock_valid_bot(&mut tx, workspace, bot_id).await?;
            lock_valid_rooms(&mut tx, workspace, bot_id, &room_ids).await?;
        }

        if target_policy_changed {
            // Remove old policy rows before a bot rotation so the installation's
            // database trigger cannot evaluate the new bot against stale rooms.
            sqlx::query("DELETE FROM integration_installation_rooms WHERE installation_id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                r"UPDATE integration_installations
                      SET bot_id = $3, issuer = $4, user_identity_issuer = $5,
                          client_id = $6, name = $7,
                          active = COALESCE($8, active),
                          allow_user_dm = COALESCE($9, allow_user_dm),
                          updated_at = now()
                    WHERE id = $1 AND workspace_id = $2",
            )
            .bind(id)
            .bind(workspace.to_uuid())
            .bind(bot_id.to_uuid())
            .bind(issuer)
            .bind(user_identity_issuer)
            .bind(client_id)
            .bind(name)
            .bind(update.active)
            .bind(update.allow_user_dm)
            .execute(&mut *tx)
            .await
            .map_err(map_installation_write_error)?;
            insert_rooms(&mut tx, id, &room_ids).await?;
        } else {
            // Do not mention `bot_id` in a pure revoke UPDATE. The database bot
            // trigger intentionally rejects invalid current bots on ordinary
            // edits/reactivation, but revocation must remain available after the
            // bot was deleted, deactivated, or removed from its workspace.
            sqlx::query(
                r"UPDATE integration_installations
                      SET issuer = $3, user_identity_issuer = $4,
                          client_id = $5, name = $6,
                          active = COALESCE($7, active),
                          allow_user_dm = COALESCE($8, allow_user_dm),
                          updated_at = now()
                    WHERE id = $1 AND workspace_id = $2",
            )
            .bind(id)
            .bind(workspace.to_uuid())
            .bind(issuer)
            .bind(user_identity_issuer)
            .bind(client_id)
            .bind(name)
            .bind(update.active)
            .bind(update.allow_user_dm)
            .execute(&mut *tx)
            .await
            .map_err(map_installation_write_error)?;
        }
        AuditRepo::append_in_tx(
            &mut tx,
            workspace,
            Some(actor),
            if update.active == Some(false) {
                "integration.installation.revoked"
            } else if update.active == Some(true) {
                "integration.installation.reactivated"
            } else {
                "integration.installation.updated"
            },
            Some(&id.to_string()),
            serde_json::json!({
                "bot_id": bot_id,
                "client_id_rotated": update.client_id.is_some(),
                "issuer_rotated": issuer_rotated,
                "user_identity_issuer_rotated": user_identity_issuer_rotated,
                "bot_rotated": update.bot_id.is_some(),
                "room_count": room_ids.len(),
                "active": update.active,
            }),
        )
        .await?;
        tx.commit().await?;
        self.get(workspace, id)
            .await?
            .ok_or_else(|| Error::Internal(anyhow::anyhow!("updated integration disappeared")))
    }

    pub async fn authorize_client(
        &self,
        id: Uuid,
        issuer: &str,
        client_id: &str,
    ) -> Result<IntegrationInstallation, Error> {
        validate_identity_component(issuer, 2_048, "issuer")?;
        validate_identity_component(client_id, 512, "client_id")?;
        let workspace = sqlx::query_scalar::<_, Uuid>(
            r"SELECT workspace_id
                FROM integration_installations
               WHERE id = $1 AND issuer = $2 AND client_id = $3 AND active",
        )
        .bind(id)
        .bind(issuer)
        .bind(client_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| Error::Forbidden("integration installation is unavailable".into()))?;
        let installation = self
            .get(WorkspaceId::from_uuid(workspace), id)
            .await?
            .ok_or_else(|| Error::Forbidden("integration installation is unavailable".into()))?;
        let effective =
            sqlx::query_scalar::<_, bool>("SELECT aero_effective_workspace_access($1, $2)")
                .bind(installation.workspace_id.to_uuid())
                .bind(installation.bot_id.to_uuid())
                .fetch_one(&self.pool)
                .await?;
        if !effective {
            return Err(Error::Forbidden(
                "integration bot no longer has workspace access".into(),
            ));
        }
        Ok(installation)
    }

    pub async fn resolve_target(
        &self,
        id: Uuid,
        issuer: &str,
        client_id: &str,
        target: IntegrationTarget,
    ) -> Result<ResolvedIntegrationTarget, Error> {
        let installation = self.authorize_client(id, issuer, client_id).await?;
        match &target {
            IntegrationTarget::Room(room) => {
                let allowed = sqlx::query_scalar::<_, bool>(
                    r"SELECT aero_effective_room_access($1, $2, NULL)
                        FROM integration_installation_rooms allowed
                        JOIN rooms room ON room.id = allowed.room_id
                       WHERE allowed.installation_id = $3
                         AND allowed.room_id = $1
                         AND room.workspace_id = $4",
                )
                .bind(room.to_uuid())
                .bind(installation.bot_id.to_uuid())
                .bind(id)
                .bind(installation.workspace_id.to_uuid())
                .fetch_optional(&self.pool)
                .await?
                .unwrap_or(false);
                if !allowed {
                    return Err(Error::Forbidden(
                        "integration room target is not allowed".into(),
                    ));
                }
                Ok(ResolvedIntegrationTarget {
                    installation,
                    target: target.clone(),
                    room_id: *room,
                    recipient: None,
                    created_room: false,
                })
            }
            IntegrationTarget::SnaplinkUser(subject) => {
                if !installation.allow_user_dm {
                    return Err(Error::Forbidden(
                        "integration user targets are disabled".into(),
                    ));
                }
                validate_identity_component(subject, 2_048, "subject")?;
                let participant = sqlx::query_scalar::<_, Uuid>(
                    r"SELECT identity.participant_id
                        FROM sso_identities identity
                       WHERE identity.issuer = $1
                         AND identity.subject = $2
                         AND aero_effective_workspace_access($3, identity.participant_id)",
                )
                .bind(&installation.user_identity_issuer)
                .bind(subject)
                .bind(installation.workspace_id.to_uuid())
                .fetch_optional(&self.pool)
                .await?
                .map(ParticipantId::from_uuid)
                .ok_or_else(|| Error::NotFound("Snaplink user".into()))?;
                if participant == installation.bot_id {
                    return Err(Error::Invalid(
                        "integration cannot target its own bot".into(),
                    ));
                }
                let dms = DmRepo::new(self.pool.clone());
                let existed = dms
                    .find_direct_in_workspace(
                        installation.workspace_id,
                        installation.bot_id,
                        participant,
                    )
                    .await?
                    .is_some();
                let room = dms
                    .find_or_create_in_workspace(
                        installation.workspace_id,
                        installation.bot_id,
                        participant,
                    )
                    .await
                    .map_err(map_dm_error)?;
                Ok(ResolvedIntegrationTarget {
                    installation,
                    target: target.clone(),
                    room_id: room.id,
                    recipient: Some(participant),
                    created_room: !existed,
                })
            }
        }
    }

    /// Return a completed canonical result, or reject a reused key, without
    /// creating a DM or consuming any rate/slow-mode capacity.
    pub async fn probe_replay(
        &self,
        probe: &IntegrationReplayProbe,
    ) -> Result<Option<IntegrationPublishOutcome>, Error> {
        validate_identity_component(&probe.issuer, 2_048, "issuer")?;
        validate_identity_component(&probe.client_id, 512, "client_id")?;
        if let IntegrationTarget::SnaplinkUser(subject) = &probe.target {
            validate_identity_component(subject, 2_048, "subject")?;
        }
        let workspace = sqlx::query_scalar::<_, Uuid>(
            "SELECT workspace_id FROM integration_installations WHERE id = $1",
        )
        .bind(probe.installation_id)
        .fetch_optional(&self.pool)
        .await?
        .map(WorkspaceId::from_uuid)
        .ok_or_else(|| Error::Forbidden("integration installation is unavailable".into()))?;

        let mut tx = self.pool.begin().await?;
        let workspace_exists =
            sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR SHARE")
                .bind(workspace.to_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .unwrap_or(false);
        if !workspace_exists {
            return Err(Error::Forbidden(
                "integration installation is unavailable".into(),
            ));
        }
        let installation = lock_installation(&mut tx, workspace, probe.installation_id, false)
            .await?
            .filter(|installation| {
                installation.active
                    && installation.issuer == probe.issuer
                    && installation.client_id == probe.client_id
            })
            .ok_or_else(|| Error::Forbidden("integration installation is unavailable".into()))?;
        let effective =
            sqlx::query_scalar::<_, bool>("SELECT aero_effective_workspace_access($1, $2)")
                .bind(workspace.to_uuid())
                .bind(installation.bot_id.to_uuid())
                .fetch_one(&mut *tx)
                .await?;
        if !effective {
            return Err(Error::Forbidden(
                "integration bot no longer has workspace access".into(),
            ));
        }
        let existing = find_probe_receipt(&mut tx, probe).await?;
        tx.commit().await?;

        match existing {
            Some(existing) => self.load_existing(existing).await.map(Some),
            None => Ok(None),
        }
    }

    pub async fn publish(
        &self,
        new: NewIntegrationNotification,
    ) -> Result<IntegrationPublishOutcome, Error> {
        if new
            .traceparent
            .as_ref()
            .is_some_and(|value| value.len() > MAX_TRACEPARENT_BYTES)
        {
            return Err(Error::Invalid("traceparent is too large".into()));
        }
        let workspace = sqlx::query_scalar::<_, Uuid>(
            "SELECT workspace_id FROM integration_installations WHERE id = $1",
        )
        .bind(new.installation_id)
        .fetch_optional(&self.pool)
        .await?
        .map(WorkspaceId::from_uuid)
        .ok_or_else(|| Error::Forbidden("integration installation is unavailable".into()))?;

        let mut tx = self.pool.begin().await?;
        let workspace_exists =
            sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR SHARE")
                .bind(workspace.to_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .unwrap_or(false);
        if !workspace_exists {
            return Err(Error::Forbidden(
                "integration installation is unavailable".into(),
            ));
        }
        let installation = lock_installation(&mut tx, workspace, new.installation_id, false)
            .await?
            .filter(|installation| {
                installation.active
                    && installation.issuer == new.issuer
                    && installation.client_id == new.client_id
            })
            .ok_or_else(|| Error::Forbidden("integration installation is unavailable".into()))?;

        machine::lock_request_key(
            &mut tx,
            new.installation_id,
            "notification",
            new.idempotency_key,
        )
        .await?;
        if let Some(existing) = find_receipt(&mut tx, &new).await? {
            tx.commit().await?;
            return self.load_existing(existing).await;
        }

        if let Some(lease_token) = new.lease_token {
            machine::assert_claim_in_tx(
                &mut tx,
                new.installation_id,
                "notification",
                new.idempotency_key,
                &new.request_hash,
                &new.target,
                lease_token,
            )
            .await?;
        }

        validate_publish_target(&mut tx, &installation, &new).await?;
        let access = lock_effective_message_write_access(
            &mut tx,
            new.room_id,
            installation.bot_id,
            PostPolicy::Enforce,
        )
        .await?;
        if access.map_or(true, |access| access.workspace != installation.workspace_id) {
            return Err(Error::Forbidden(
                "integration bot lost message posting access".into(),
            ));
        }
        if !BlobRepo::lock_integration_attachments_in_tx(
            &mut tx,
            &new.blocks,
            installation.bot_id,
            new.room_id,
            new.installation_id,
        )
        .await?
        {
            return Err(Error::Forbidden(
                "message contains an unavailable attachment".into(),
            ));
        }

        let message = MessageRepo::insert_row_in_tx(
            &mut tx,
            NewMessage {
                room_id: new.room_id,
                sender_id: installation.bot_id,
                blocks: new.blocks,
                reply_to: None,
                metadata: serde_json::json!({
                    "source": "integration",
                    "installation_id": new.installation_id,
                }),
                expires_at: None,
            },
        )
        .await?;
        let payload = serde_json::to_value(RoomEvent::Message(MessageEnvelope {
            message: message.clone(),
            delivery_ordinal: None,
            client_message_id: Some(new.idempotency_key),
            recipients: Vec::new(),
        }))?;
        let outbox_id = EventOutboxRepo::insert_in_tx(
            &mut tx,
            NewEventOutbox {
                message_id: message.id,
                event_kind: EventOutboxKind::Message,
                subject: format!("im.room.{}", message.room_id),
                payload,
                traceparent: new.traceparent,
            },
        )
        .await?;
        MessageSideEffectRepo::insert_in_tx(
            &mut tx,
            message.id,
            message.version,
            &[
                MessageSideEffectKind::Notifications,
                MessageSideEffectKind::Embed,
                MessageSideEffectKind::Moderate,
            ],
        )
        .await?;
        sqlx::query(
            r"INSERT INTO integration_notification_receipts
                  (installation_id, idempotency_key, request_hash, target_kind,
                   target_key, room_id, message_id, outbox_id)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(new.installation_id)
        .bind(new.idempotency_key)
        .bind(new.request_hash.as_slice())
        .bind(new.target.kind())
        .bind(new.target.key())
        .bind(new.room_id.to_uuid())
        .bind(message.id.to_uuid())
        .bind(outbox_id)
        .execute(&mut *tx)
        .await?;
        AuditRepo::append_in_tx(
            &mut tx,
            installation.workspace_id,
            None,
            "integration.notification.published",
            Some(&message.id.to_string()),
            serde_json::json!({
                "installation_id": new.installation_id,
                "target_kind": new.target.kind(),
                "room_id": new.room_id,
            }),
        )
        .await?;
        if let Some(lease_token) = new.lease_token {
            machine::complete_claim_in_tx(
                &mut tx,
                new.installation_id,
                "notification",
                new.idempotency_key,
                lease_token,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(IntegrationPublishOutcome {
            message,
            outbox_id,
            deduplicated: false,
        })
    }

    /// Return the canonical result for a completed request before the caller
    /// consumes rate-limit or slow-mode capacity. Authorization and target
    /// resolution must still run first; [`Self::publish`] repeats every check
    /// and remains the concurrency-safe source of truth for a cache miss.
    pub async fn replay(
        &self,
        new: &NewIntegrationNotification,
    ) -> Result<Option<IntegrationPublishOutcome>, Error> {
        let workspace = sqlx::query_scalar::<_, Uuid>(
            "SELECT workspace_id FROM integration_installations WHERE id = $1",
        )
        .bind(new.installation_id)
        .fetch_optional(&self.pool)
        .await?
        .map(WorkspaceId::from_uuid)
        .ok_or_else(|| Error::Forbidden("integration installation is unavailable".into()))?;

        let mut tx = self.pool.begin().await?;
        let workspace_exists =
            sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR SHARE")
                .bind(workspace.to_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .unwrap_or(false);
        if !workspace_exists {
            return Err(Error::Forbidden(
                "integration installation is unavailable".into(),
            ));
        }
        lock_installation(&mut tx, workspace, new.installation_id, false)
            .await?
            .filter(|installation| {
                installation.active
                    && installation.issuer == new.issuer
                    && installation.client_id == new.client_id
            })
            .ok_or_else(|| Error::Forbidden("integration installation is unavailable".into()))?;
        let existing = find_receipt(&mut tx, new).await?;
        tx.commit().await?;

        match existing {
            Some(existing) => self.load_existing(existing).await.map(Some),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests;
