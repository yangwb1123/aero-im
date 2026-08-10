//! MLS storage — `KeyPackage` publish/fetch and group state persistence.
//!
//! Server is **not** an MLS participant; it stores opaque payloads and routes
//! handshake messages. The cryptographic library (openmls) lives client-side.

use aero_common::{
    mls::{KeyPackage, MlsGroupId, MlsGroupState},
    Error, ParticipantId, RoomId,
};
use sqlx::{PgPool, Postgres, Transaction};

/// Maximum opaque MLS group-id size accepted by production writes.
pub const MAX_MLS_GROUP_ID_BYTES: usize = 255;
/// Maximum ciphersuite identifier size accepted by production writes.
pub const MAX_MLS_CIPHERSUITE_BYTES: usize = 128;
/// Maximum opaque persisted group-state size accepted by production writes.
pub const MAX_MLS_GROUP_STATE_BYTES: usize = 1024 * 1024;
/// Maximum opaque `KeyPackage` payload accepted by the HTTP relay.
pub const MAX_MLS_KEY_PACKAGE_BYTES: usize = 16 * 1024;

#[derive(Clone)]
pub struct KeyPackageRepo {
    pool: PgPool,
}

impl KeyPackageRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn publish(
        &self,
        participant: ParticipantId,
        ciphersuite: &str,
        payload: Vec<u8>,
    ) -> Result<KeyPackage, sqlx::Error> {
        let id = uuid::Uuid::new_v4();
        let created_at = time::OffsetDateTime::now_utc();
        sqlx::query(
            r"INSERT INTO mls_key_packages (id, participant_id, ciphersuite, payload, created_at)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(participant.to_uuid())
        .bind(ciphersuite)
        .bind(&payload)
        .bind(created_at)
        .execute(&self.pool)
        .await?;
        Ok(KeyPackage {
            id,
            participant_id: participant,
            ciphersuite: ciphersuite.to_owned(),
            payload,
            created_at,
            consumed_at: None,
        })
    }

    /// Atomically consume one of the caller's own unused `KeyPackages`.
    ///
    /// This preserves the legacy non-room endpoint without allowing an
    /// authenticated caller to drain another participant's packages.
    pub async fn consume_own(
        &self,
        requester: ParticipantId,
        target: ParticipantId,
    ) -> Result<Option<KeyPackage>, Error> {
        if requester != target {
            return Err(Error::Forbidden(
                "the legacy KeyPackage endpoint only permits self-consumption".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        let package = claim_one_in_tx(&mut tx, target).await?;
        tx.commit().await?;
        Ok(package)
    }

    /// Atomically consume a target's unused `KeyPackage` while both participants
    /// retain effective access to the canonical room.
    ///
    /// Participant authorization edges are locked in UUID order. This makes
    /// opposite-direction concurrent claims use one deterministic lock order,
    /// while the package claim remains in the same transaction.
    pub async fn consume_one_authorized(
        &self,
        requester: ParticipantId,
        target: ParticipantId,
        room: RoomId,
    ) -> Result<Option<KeyPackage>, Error> {
        let mut tx = self.pool.begin().await?;
        let workspace =
            sqlx::query_scalar::<_, uuid::Uuid>("SELECT workspace_id FROM rooms WHERE id = $1")
                .bind(room.to_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or_else(|| Error::NotFound(format!("room {room}")))?;

        let mut participants = [requester.to_uuid(), target.to_uuid()];
        participants.sort_unstable();
        let mut previous = None;
        for participant in participants {
            if previous == Some(participant) {
                continue;
            }
            let allowed: bool = sqlx::query_scalar("SELECT aero_effective_room_access($1, $2, $3)")
                .bind(room.to_uuid())
                .bind(participant)
                .bind(workspace)
                .fetch_one(&mut *tx)
                .await?;
            if !allowed {
                return Err(Error::Forbidden(
                    "requester and target must have current room access".into(),
                ));
            }
            previous = Some(participant);
        }

        let package = claim_one_in_tx(&mut tx, target).await?;
        tx.commit().await?;
        Ok(package)
    }

    /// Low-level test seam for the original atomic claim primitive.
    #[cfg(test)]
    pub async fn consume_one(
        &self,
        target: ParticipantId,
    ) -> Result<Option<KeyPackage>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let package = claim_one_in_tx(&mut tx, target).await?;
        tx.commit().await?;
        Ok(package)
    }

    pub async fn pending_count(&self, target: ParticipantId) -> Result<i64, sqlx::Error> {
        let (n,) = sqlx::query_as::<_, (i64,)>(
            r"SELECT COUNT(*) FROM mls_key_packages
               WHERE participant_id = $1 AND consumed_at IS NULL",
        )
        .bind(target.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(n)
    }
}

async fn claim_one_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    target: ParticipantId,
) -> Result<Option<KeyPackage>, sqlx::Error> {
    let row = sqlx::query_as::<_, KpRow>(
        r"UPDATE mls_key_packages
              SET consumed_at = NOW()
            WHERE id = (
                SELECT id FROM mls_key_packages
                 WHERE participant_id = $1 AND consumed_at IS NULL
                 ORDER BY created_at ASC, id ASC
                 FOR UPDATE SKIP LOCKED
                 LIMIT 1
            )
            RETURNING id, participant_id, ciphersuite, payload, created_at, consumed_at",
    )
    .bind(target.to_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(KeyPackage::from))
}

#[derive(sqlx::FromRow)]
struct KpRow {
    id: uuid::Uuid,
    participant_id: uuid::Uuid,
    ciphersuite: String,
    payload: Vec<u8>,
    created_at: time::OffsetDateTime,
    consumed_at: Option<time::OffsetDateTime>,
}

impl From<KpRow> for KeyPackage {
    fn from(r: KpRow) -> Self {
        Self {
            id: r.id,
            participant_id: ParticipantId::from_uuid(r.participant_id),
            ciphersuite: r.ciphersuite,
            payload: r.payload,
            created_at: r.created_at,
            consumed_at: r.consumed_at,
        }
    }
}

#[derive(Clone)]
pub struct MlsGroupRepo {
    pool: PgPool,
}

impl MlsGroupRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create or advance opaque group state while holding canonical room access.
    ///
    /// Existing groups are authorized against their stored room before the row
    /// is locked and rechecked. First creation is an insert-with-conflict
    /// arbitration, so concurrent attempts to bind the same group id to
    /// different rooms cannot both succeed.
    pub async fn upsert_authorized(
        &self,
        actor: ParticipantId,
        group: &MlsGroupState,
    ) -> Result<(), Error> {
        validate_group_state(group)?;
        let requested_room = group
            .room_id
            .ok_or_else(|| Error::Invalid("MLS group must be room-bound".into()))?;
        let epoch = i64::try_from(group.epoch)
            .map_err(|_| Error::Invalid("MLS epoch exceeds storage range".into()))?;

        let mut tx = self.pool.begin().await?;
        // Resolve the immutable tenant edge without a group lock. Governance
        // writers take workspace/room locks first, so taking the group lock
        // before authorization would invert the global order.
        let resolved_room = sqlx::query_scalar::<_, Option<uuid::Uuid>>(
            "SELECT room_id FROM mls_groups WHERE group_id = $1",
        )
        .bind(group.group_id.as_bytes())
        .fetch_optional(&mut *tx)
        .await?;
        let existed = resolved_room.is_some();
        let canonical_room = match resolved_room {
            Some(Some(room)) => RoomId::from_uuid(room),
            Some(None) => {
                return Err(Error::NotFound(
                    "MLS group has no canonical room binding".into(),
                ));
            }
            None => requested_room,
        };

        assert_effective_room_access(&mut tx, canonical_room, actor).await?;

        if !existed {
            sqlx::query(
                r"INSERT INTO mls_groups
                       (group_id, room_id, ciphersuite, epoch, state, updated_at, updated_by)
                   VALUES ($1, $2, $3, $4, $5, CURRENT_TIMESTAMP, $6)
                   ON CONFLICT (group_id) DO NOTHING",
            )
            .bind(group.group_id.as_bytes())
            .bind(requested_room.to_uuid())
            .bind(&group.ciphersuite)
            .bind(epoch)
            .bind(&group.state)
            .bind(actor.to_uuid())
            .execute(&mut *tx)
            .await?;
        }

        let locked = fetch_group_for_update(&mut tx, &group.group_id)
            .await?
            .ok_or_else(|| Error::NotFound("MLS group".into()))?;
        if existed && locked.room_id != Some(canonical_room.to_uuid()) {
            return Err(Error::Database(sqlx::Error::Protocol(
                "MLS group room identity changed during authorization".into(),
            )));
        }
        if locked.room_id != Some(requested_room.to_uuid()) {
            return Err(Error::Conflict(
                "cannot reassign an MLS group to another room".into(),
            ));
        }
        if locked.ciphersuite != group.ciphersuite {
            return Err(Error::Conflict(
                "cannot change an MLS group's ciphersuite".into(),
            ));
        }
        if epoch < locked.epoch {
            return Err(Error::Conflict("MLS epoch cannot move backwards".into()));
        }

        let updated = sqlx::query(
            r"UPDATE mls_groups
                  SET epoch = $2,
                      state = $3,
                      updated_at = CURRENT_TIMESTAMP,
                      updated_by = $4
                WHERE group_id = $1",
        )
        .bind(group.group_id.as_bytes())
        .bind(epoch)
        .bind(&group.state)
        .bind(actor.to_uuid())
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(Error::NotFound("MLS group".into()));
        }
        tx.commit().await?;
        Ok(())
    }

    /// Read opaque group state while holding current access to its canonical
    /// room through the row read.
    pub async fn get_authorized(
        &self,
        actor: ParticipantId,
        group_id: &MlsGroupId,
    ) -> Result<MlsGroupState, Error> {
        if group_id.as_bytes().is_empty() {
            return Err(Error::Invalid("MLS group id must not be empty".into()));
        }
        if group_id.as_bytes().len() > MAX_MLS_GROUP_ID_BYTES {
            return Err(Error::Invalid("MLS group id is too large".into()));
        }

        let mut tx = self.pool.begin().await?;
        let room = sqlx::query_scalar::<_, Option<uuid::Uuid>>(
            "SELECT room_id FROM mls_groups WHERE group_id = $1",
        )
        .bind(group_id.as_bytes())
        .fetch_optional(&mut *tx)
        .await?
        .flatten()
        .map(RoomId::from_uuid)
        .ok_or_else(|| Error::NotFound("MLS group".into()))?;
        assert_effective_room_access(&mut tx, room, actor).await?;

        let row = sqlx::query_as::<_, GroupRow>(
            r"SELECT group_id, room_id, ciphersuite, epoch, state, updated_at
                 FROM mls_groups
                WHERE group_id = $1
                FOR SHARE",
        )
        .bind(group_id.as_bytes())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::NotFound("MLS group".into()))?;
        if row.room_id != Some(room.to_uuid()) {
            return Err(Error::Database(sqlx::Error::Protocol(
                "MLS group room identity changed during authorization".into(),
            )));
        }
        let group = row.into();
        tx.commit().await?;
        Ok(group)
    }

    /// Low-level compatibility seam used only by storage tests.
    #[cfg(test)]
    pub async fn upsert(&self, group: &MlsGroupState) -> Result<(), sqlx::Error> {
        let epoch_i = i64::try_from(group.epoch).unwrap_or(i64::MAX);
        sqlx::query(
            r"INSERT INTO mls_groups (group_id, room_id, ciphersuite, epoch, state, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6)
               ON CONFLICT (group_id) DO UPDATE
                   SET epoch = EXCLUDED.epoch,
                       state = EXCLUDED.state,
                       updated_at = EXCLUDED.updated_at",
        )
        .bind(group.group_id.as_bytes())
        .bind(group.room_id.map(|r| r.to_uuid()))
        .bind(&group.ciphersuite)
        .bind(epoch_i)
        .bind(&group.state)
        .bind(group.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Low-level compatibility seam used only by storage tests.
    #[cfg(test)]
    pub async fn get(&self, group_id: &MlsGroupId) -> Result<Option<MlsGroupState>, sqlx::Error> {
        let row = sqlx::query_as::<_, GroupRow>(
            r"SELECT group_id, room_id, ciphersuite, epoch, state, updated_at
               FROM mls_groups WHERE group_id = $1",
        )
        .bind(group_id.as_bytes())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(Into::into))
    }
}

#[derive(sqlx::FromRow)]
struct GroupRow {
    group_id: Vec<u8>,
    room_id: Option<uuid::Uuid>,
    ciphersuite: String,
    epoch: i64,
    state: Vec<u8>,
    updated_at: time::OffsetDateTime,
}

impl From<GroupRow> for MlsGroupState {
    fn from(row: GroupRow) -> Self {
        Self {
            group_id: MlsGroupId::new(row.group_id),
            room_id: row.room_id.map(RoomId::from_uuid),
            ciphersuite: row.ciphersuite,
            epoch: u64::try_from(row.epoch).unwrap_or(0),
            state: row.state,
            updated_at: row.updated_at,
        }
    }
}

fn validate_group_state(group: &MlsGroupState) -> Result<(), Error> {
    if group.group_id.as_bytes().is_empty() {
        return Err(Error::Invalid("MLS group id must not be empty".into()));
    }
    if group.group_id.as_bytes().len() > MAX_MLS_GROUP_ID_BYTES {
        return Err(Error::Invalid("MLS group id is too large".into()));
    }
    if group.ciphersuite.is_empty()
        || group.ciphersuite.trim() != group.ciphersuite
        || group.ciphersuite.len() > MAX_MLS_CIPHERSUITE_BYTES
    {
        return Err(Error::Invalid("invalid MLS ciphersuite".into()));
    }
    if group.state.is_empty() {
        return Err(Error::Invalid("MLS group state must not be empty".into()));
    }
    if group.state.len() > MAX_MLS_GROUP_STATE_BYTES {
        return Err(Error::Invalid("MLS group state is too large".into()));
    }
    Ok(())
}

async fn assert_effective_room_access(
    tx: &mut Transaction<'_, Postgres>,
    room: RoomId,
    actor: ParticipantId,
) -> Result<(), Error> {
    let allowed: bool = sqlx::query_scalar("SELECT aero_effective_room_access($1, $2, NULL)")
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .fetch_one(&mut **tx)
        .await?;
    if !allowed {
        return Err(Error::Forbidden("current room access required".into()));
    }
    Ok(())
}

async fn fetch_group_for_update(
    tx: &mut Transaction<'_, Postgres>,
    group_id: &MlsGroupId,
) -> Result<Option<GroupRow>, sqlx::Error> {
    sqlx::query_as::<_, GroupRow>(
        r"SELECT group_id, room_id, ciphersuite, epoch, state, updated_at
             FROM mls_groups
            WHERE group_id = $1
            FOR UPDATE",
    )
    .bind(group_id.as_bytes())
    .fetch_optional(&mut **tx)
    .await
}

#[cfg(test)]
#[path = "mls/security_tests.rs"]
mod security_tests;
