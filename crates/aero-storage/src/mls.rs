//! MLS storage — KeyPackage publish/fetch and group state persistence.
//!
//! Server is **not** an MLS participant; it stores opaque payloads and routes
//! handshake messages. The cryptographic library (openmls) lives client-side.

use aero_common::{
    mls::{KeyPackage, MlsGroupId, MlsGroupState},
    ParticipantId, RoomId,
};
use sqlx::PgPool;

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
            r#"INSERT INTO mls_key_packages (id, participant_id, ciphersuite, payload, created_at)
               VALUES ($1, $2, $3, $4, $5)"#,
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

    /// Atomically pop an unused KeyPackage for a target participant. Returns
    /// `None` if there are no fresh packages left — the consumer should ask
    /// the client to upload more.
    pub async fn consume_one(
        &self,
        target: ParticipantId,
    ) -> Result<Option<KeyPackage>, sqlx::Error> {
        let row = sqlx::query_as::<_, KpRow>(
            r#"UPDATE mls_key_packages
                  SET consumed_at = NOW()
                WHERE id = (
                    SELECT id FROM mls_key_packages
                     WHERE participant_id = $1 AND consumed_at IS NULL
                     ORDER BY created_at ASC
                     FOR UPDATE SKIP LOCKED
                     LIMIT 1
                )
                RETURNING id, participant_id, ciphersuite, payload, created_at, consumed_at"#,
        )
        .bind(target.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(KeyPackage::from))
    }

    pub async fn pending_count(&self, target: ParticipantId) -> Result<i64, sqlx::Error> {
        let (n,) = sqlx::query_as::<_, (i64,)>(
            r#"SELECT COUNT(*) FROM mls_key_packages
               WHERE participant_id = $1 AND consumed_at IS NULL"#,
        )
        .bind(target.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(n)
    }
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

    pub async fn upsert(&self, group: &MlsGroupState) -> Result<(), sqlx::Error> {
        let epoch_i = i64::try_from(group.epoch).unwrap_or(i64::MAX);
        sqlx::query(
            r#"INSERT INTO mls_groups (group_id, room_id, ciphersuite, epoch, state, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6)
               ON CONFLICT (group_id) DO UPDATE
                   SET epoch = EXCLUDED.epoch,
                       state = EXCLUDED.state,
                       updated_at = EXCLUDED.updated_at"#,
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

    pub async fn get(&self, group_id: &MlsGroupId) -> Result<Option<MlsGroupState>, sqlx::Error> {
        let row = sqlx::query_as::<_, (Vec<u8>, Option<uuid::Uuid>, String, i64, Vec<u8>, time::OffsetDateTime)>(
            r#"SELECT group_id, room_id, ciphersuite, epoch, state, updated_at
               FROM mls_groups WHERE group_id = $1"#,
        )
        .bind(group_id.as_bytes())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(g, r, c, e, s, u)| MlsGroupState {
            group_id: MlsGroupId::new(g),
            room_id: r.map(RoomId::from_uuid),
            ciphersuite: c,
            epoch: u64::try_from(e).unwrap_or(0),
            state: s,
            updated_at: u,
        }))
    }
}
