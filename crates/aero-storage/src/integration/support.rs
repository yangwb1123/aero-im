use super::{
    IntegrationInstallation, IntegrationPublishOutcome, IntegrationReplayProbe, IntegrationRepo,
    IntegrationTarget, NewIntegrationNotification, MAX_INTEGRATION_ROOMS,
};
use crate::DmWriteError;
use crate::MessageRepo;
use aero_common::{Error, MessageId, ParticipantId, RoomId, WorkspaceId};
use sha2::{Digest as _, Sha256};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

pub(super) const INSTALLATION_SELECT: &str = r"SELECT installation.id,
       installation.workspace_id,
       installation.bot_id,
       installation.issuer,
       installation.user_identity_issuer,
       installation.client_id,
       installation.name,
       installation.active,
       installation.allow_user_dm,
       installation.created_by,
       installation.created_at,
       installation.updated_at,
       COALESCE(
           array_agg(allowed.room_id ORDER BY allowed.room_id)
               FILTER (WHERE allowed.room_id IS NOT NULL),
           ARRAY[]::uuid[]
       ) AS room_ids
  FROM integration_installations installation
  LEFT JOIN integration_installation_rooms allowed
    ON allowed.installation_id = installation.id";

impl IntegrationTarget {
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Room(_) => "room",
            Self::SnaplinkUser(_) => "snaplink_user",
        }
    }

    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Room(room) => room.to_uuid().to_string(),
            Self::SnaplinkUser(subject) => {
                format!("sha256:{}", hex::encode(Sha256::digest(subject.as_bytes())))
            }
        }
    }
}

#[derive(sqlx::FromRow)]
pub(super) struct InstallationRow {
    id: Uuid,
    workspace_id: Uuid,
    bot_id: Uuid,
    issuer: String,
    user_identity_issuer: String,
    client_id: String,
    name: String,
    active: bool,
    allow_user_dm: bool,
    created_by: Option<Uuid>,
    created_at: time::OffsetDateTime,
    updated_at: time::OffsetDateTime,
    room_ids: Vec<Uuid>,
}

impl From<InstallationRow> for IntegrationInstallation {
    fn from(row: InstallationRow) -> Self {
        Self {
            id: row.id,
            workspace_id: WorkspaceId::from_uuid(row.workspace_id),
            bot_id: ParticipantId::from_uuid(row.bot_id),
            issuer: row.issuer,
            user_identity_issuer: row.user_identity_issuer,
            client_id: row.client_id,
            name: row.name,
            active: row.active,
            allow_user_dm: row.allow_user_dm,
            room_ids: row.room_ids.into_iter().map(RoomId::from_uuid).collect(),
            created_by: row.created_by.map(ParticipantId::from_uuid),
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

impl IntegrationRepo {
    pub(super) async fn load_existing(
        &self,
        reference: ExistingReceipt,
    ) -> Result<IntegrationPublishOutcome, Error> {
        let message = MessageRepo::new(self.pool.clone())
            .get(reference.message_id)
            .await?
            .filter(|message| {
                message.deleted_at.is_none()
                    && message.expires_at.map_or(true, |expires_at| {
                        expires_at > time::OffsetDateTime::now_utc()
                    })
            })
            .ok_or_else(|| {
                Error::Conflict("canonical integration message is unavailable".into())
            })?;
        Ok(IntegrationPublishOutcome {
            message,
            outbox_id: reference.outbox_id,
            deduplicated: true,
        })
    }
}

pub(super) async fn lock_installation(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    id: Uuid,
    exclusive: bool,
) -> Result<Option<IntegrationInstallation>, sqlx::Error> {
    let lock = if exclusive { "FOR UPDATE" } else { "FOR SHARE" };
    let row = sqlx::query_as::<_, InstallationBaseRow>(&format!(
        "SELECT id, workspace_id, bot_id, issuer, user_identity_issuer, client_id, name, active,
                allow_user_dm, created_by, created_at, updated_at
           FROM integration_installations
          WHERE workspace_id = $1 AND id = $2
          {lock}"
    ))
    .bind(workspace.to_uuid())
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let room_ids = sqlx::query_scalar::<_, Uuid>(
        "SELECT room_id FROM integration_installation_rooms
          WHERE installation_id = $1 ORDER BY room_id",
    )
    .bind(id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(Some(row.with_rooms(room_ids)))
}

#[derive(sqlx::FromRow)]
struct InstallationBaseRow {
    id: Uuid,
    workspace_id: Uuid,
    bot_id: Uuid,
    issuer: String,
    user_identity_issuer: String,
    client_id: String,
    name: String,
    active: bool,
    allow_user_dm: bool,
    created_by: Option<Uuid>,
    created_at: time::OffsetDateTime,
    updated_at: time::OffsetDateTime,
}

impl InstallationBaseRow {
    fn with_rooms(self, room_ids: Vec<Uuid>) -> IntegrationInstallation {
        IntegrationInstallation {
            id: self.id,
            workspace_id: WorkspaceId::from_uuid(self.workspace_id),
            bot_id: ParticipantId::from_uuid(self.bot_id),
            issuer: self.issuer,
            user_identity_issuer: self.user_identity_issuer,
            client_id: self.client_id,
            name: self.name,
            active: self.active,
            allow_user_dm: self.allow_user_dm,
            room_ids: room_ids.into_iter().map(RoomId::from_uuid).collect(),
            created_by: self.created_by.map(ParticipantId::from_uuid),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

pub(super) async fn lock_valid_bot(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    bot: ParticipantId,
) -> Result<(), Error> {
    let valid = sqlx::query_scalar::<_, bool>(
        r"SELECT true
            FROM bots bot
            JOIN participants participant
              ON participant.id = bot.id
             AND participant.kind = 'bot'
             AND participant.deleted_at IS NULL
            JOIN workspace_members membership
              ON membership.workspace_id = $1
             AND membership.participant_id = bot.id
            LEFT JOIN workspace_deactivations deactivated
              ON deactivated.workspace_id = $1
             AND deactivated.participant_id = bot.id
           WHERE bot.id = $2
             AND bot.workspace_id = $1
             AND deactivated.participant_id IS NULL
           FOR SHARE OF bot, participant, membership",
    )
    .bind(workspace.to_uuid())
    .bind(bot.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(false);
    if valid {
        Ok(())
    } else {
        Err(Error::Invalid(
            "bot must be an active member of the integration workspace".into(),
        ))
    }
}

pub(super) async fn lock_valid_rooms(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    bot: ParticipantId,
    rooms: &[RoomId],
) -> Result<(), Error> {
    let unique = canonical_room_ids(rooms)?;
    if unique.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = unique.iter().map(RoomId::to_uuid).collect();
    let found = sqlx::query_scalar::<_, Uuid>(
        r"SELECT room.id
            FROM rooms room
            JOIN room_members member
              ON member.room_id = room.id
             AND member.participant_id = $2
           WHERE room.id = ANY($1)
             AND room.workspace_id = $3
           ORDER BY room.id
           FOR SHARE OF room, member",
    )
    .bind(&ids)
    .bind(bot.to_uuid())
    .bind(workspace.to_uuid())
    .fetch_all(&mut **tx)
    .await?;
    if found.len() == ids.len() {
        Ok(())
    } else {
        Err(Error::Invalid(
            "every allowed room must belong to the workspace and contain the bot".into(),
        ))
    }
}

pub(super) async fn insert_rooms(
    tx: &mut Transaction<'_, Postgres>,
    installation: Uuid,
    rooms: &[RoomId],
) -> Result<(), sqlx::Error> {
    let rooms =
        canonical_room_ids(rooms).map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
    for room in rooms {
        sqlx::query(
            "INSERT INTO integration_installation_rooms (installation_id, room_id) VALUES ($1, $2)",
        )
        .bind(installation)
        .bind(room.to_uuid())
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

pub(super) fn canonical_room_ids(rooms: &[RoomId]) -> Result<Vec<RoomId>, Error> {
    if rooms.len() > MAX_INTEGRATION_ROOMS {
        return Err(Error::Invalid(format!(
            "too many integration rooms (max {MAX_INTEGRATION_ROOMS})"
        )));
    }
    let mut rooms = rooms.to_vec();
    rooms.sort_unstable_by_key(RoomId::to_uuid);
    rooms.dedup();
    Ok(rooms)
}

pub(super) fn validate_installation_fields(
    issuer: &str,
    user_identity_issuer: &str,
    client_id: &str,
    name: &str,
    rooms: &[RoomId],
) -> Result<(), Error> {
    validate_identity_component(issuer, 2_048, "issuer")?;
    validate_identity_component(user_identity_issuer, 2_048, "user_identity_issuer")?;
    validate_identity_component(client_id, 512, "client_id")?;
    validate_identity_component(name, 128, "name")?;
    canonical_room_ids(rooms)?;
    Ok(())
}

pub(super) fn validate_identity_component(
    value: &str,
    max: usize,
    label: &str,
) -> Result<(), Error> {
    if value.is_empty()
        || value != value.trim()
        || value.len() > max
        || value.chars().any(char::is_control)
    {
        return Err(Error::Invalid(format!(
            "{label} must be a non-blank exact value of at most {max} bytes"
        )));
    }
    Ok(())
}

pub(super) async fn validate_publish_target(
    tx: &mut Transaction<'_, Postgres>,
    installation: &IntegrationInstallation,
    new: &NewIntegrationNotification,
) -> Result<(), Error> {
    validate_target_binding(tx, installation, &new.target, new.room_id, new.recipient).await
}

pub(super) async fn validate_target_binding(
    tx: &mut Transaction<'_, Postgres>,
    installation: &IntegrationInstallation,
    target: &IntegrationTarget,
    room_id: RoomId,
    recipient: Option<ParticipantId>,
) -> Result<(), Error> {
    match (target, recipient) {
        (IntegrationTarget::Room(room), None) if *room == room_id => {
            let allowed = sqlx::query_scalar::<_, bool>(
                r"SELECT true
                    FROM integration_installation_rooms allowed
                    JOIN rooms room ON room.id = allowed.room_id
                   WHERE allowed.installation_id = $1
                     AND allowed.room_id = $2
                     AND room.workspace_id = $3
                   FOR SHARE OF allowed, room",
            )
            .bind(installation.id)
            .bind(room.to_uuid())
            .bind(installation.workspace_id.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .unwrap_or(false);
            if allowed {
                Ok(())
            } else {
                Err(Error::Forbidden(
                    "integration room target is not allowed".into(),
                ))
            }
        }
        (IntegrationTarget::SnaplinkUser(subject), Some(recipient))
            if installation.allow_user_dm =>
        {
            validate_identity_component(subject, 2_048, "subject")?;
            let valid = sqlx::query_scalar::<_, bool>(
                r"SELECT EXISTS (
                      SELECT 1
                        FROM sso_identities identity
                       WHERE identity.issuer = $1
                         AND identity.subject = $2
                         AND identity.participant_id = $3
                         AND aero_effective_workspace_access($4, identity.participant_id)
                  )
                  AND EXISTS (
                      SELECT 1
                        FROM rooms room
                       WHERE room.id = $5
                         AND room.workspace_id = $4
                         AND room.kind = 'direct'
                         AND (SELECT count(*) FROM room_members member WHERE member.room_id = room.id) = 2
                         AND EXISTS (
                             SELECT 1 FROM room_members member
                              WHERE member.room_id = room.id AND member.participant_id = $3
                         )
                         AND EXISTS (
                             SELECT 1 FROM room_members member
                              WHERE member.room_id = room.id AND member.participant_id = $6
                         )
                  )",
            )
            .bind(&installation.user_identity_issuer)
            .bind(subject)
            .bind(recipient.to_uuid())
            .bind(installation.workspace_id.to_uuid())
            .bind(room_id.to_uuid())
            .bind(installation.bot_id.to_uuid())
            .fetch_one(&mut **tx)
            .await?;
            if valid {
                Ok(())
            } else {
                Err(Error::Forbidden(
                    "integration user target is no longer valid".into(),
                ))
            }
        }
        _ => Err(Error::Invalid(
            "integration target resolution mismatch".into(),
        )),
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ExistingReceipt {
    pub(super) message_id: MessageId,
    pub(super) outbox_id: Uuid,
}

pub(super) async fn find_receipt(
    tx: &mut Transaction<'_, Postgres>,
    new: &NewIntegrationNotification,
) -> Result<Option<ExistingReceipt>, Error> {
    let row = sqlx::query_as::<_, (Vec<u8>, String, String, Uuid, Uuid, Uuid)>(
        r"SELECT request_hash, target_kind, target_key, room_id, message_id, outbox_id
            FROM integration_notification_receipts
           WHERE installation_id = $1 AND idempotency_key = $2
             AND expires_at > now()
           FOR UPDATE",
    )
    .bind(new.installation_id)
    .bind(new.idempotency_key)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((hash, kind, key, room, message, outbox)) = row else {
        return Ok(None);
    };
    if hash.as_slice() != new.request_hash
        || kind != new.target.kind()
        || key != new.target.key()
        || room != new.room_id.to_uuid()
    {
        return Err(Error::Conflict(
            "Idempotency-Key was already used for a different integration notification".into(),
        ));
    }
    Ok(Some(ExistingReceipt {
        message_id: MessageId::from_uuid(message),
        outbox_id: outbox,
    }))
}

pub(super) async fn find_probe_receipt(
    tx: &mut Transaction<'_, Postgres>,
    probe: &IntegrationReplayProbe,
) -> Result<Option<ExistingReceipt>, Error> {
    let row = sqlx::query_as::<_, (Vec<u8>, String, String, Uuid, Uuid)>(
        r"SELECT request_hash, target_kind, target_key, message_id, outbox_id
            FROM integration_notification_receipts
           WHERE installation_id = $1 AND idempotency_key = $2
             AND expires_at > now()
           FOR SHARE",
    )
    .bind(probe.installation_id)
    .bind(probe.idempotency_key)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((hash, kind, key, message, outbox)) = row else {
        return Ok(None);
    };
    if hash.as_slice() != probe.request_hash
        || kind != probe.target.kind()
        || key != probe.target.key()
    {
        return Err(Error::Conflict(
            "Idempotency-Key was already used for a different integration notification".into(),
        ));
    }
    Ok(Some(ExistingReceipt {
        message_id: MessageId::from_uuid(message),
        outbox_id: outbox,
    }))
}

pub(super) fn map_installation_write_error(error: sqlx::Error) -> Error {
    if let Some(database) = error.as_database_error() {
        if matches!(
            database.constraint(),
            Some(
                "integration_installations_workspace_id_issuer_client_id_key"
                    | "integration_installation_bot_scope"
                    | "integration_installation_room_scope"
            )
        ) {
            return Error::Conflict(
                "integration installation conflicts with existing state".into(),
            );
        }
    }
    Error::from(error)
}

pub(super) fn map_dm_error(error: DmWriteError) -> Error {
    match error {
        DmWriteError::Forbidden => Error::Forbidden("integration DM access was revoked".into()),
        DmWriteError::InformationBarrier => Error::Forbidden("information barrier".into()),
        DmWriteError::Blocked => Error::Forbidden("user block".into()),
        DmWriteError::Storage(error) => Error::from(error),
    }
}
