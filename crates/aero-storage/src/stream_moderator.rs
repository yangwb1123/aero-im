//! Stream-moderator role repository (assign MOD ROLE on a stream's chat).
//!
//! Backs `migrations/0083_stream_moderators.sql`. DISTINCT from
//! [`StreamModRepo`](crate::StreamModRepo) (0026), which records *banned* chatters:
//! this assigns a moderator ROLE to a participant on a stream. A stream owner
//! adds/removes moderators; a moderator then gains the same chat-ban/timeout
//! authority as the owner on that stream's danmaku chat (the ban handler allows
//! owner OR [`StreamModeratorRepo::is_moderator`]).
//!
//! Management writes are owner-gated again inside the same transaction that
//! locks the canonical stream. That stream lock is also used by moderation
//! actions, so a role revocation cannot race a stale preflight and still allow a
//! later moderation commit. One row exists per `(stream, participant)`.

use aero_common::{Error as AeroError, ParticipantId, StreamModeratorId};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};
use ulid::Ulid;
use uuid::Uuid;

/// One stream-moderator assignment — a storage-layer projection of a
/// `stream_moderators` row.
#[derive(Debug, Clone, Serialize)]
pub struct StreamModerator {
    /// The assignment's unique id.
    pub id: StreamModeratorId,
    /// The stream the moderator role applies to.
    pub stream_id: Ulid,
    /// The participant granted the moderator role.
    pub participant_id: ParticipantId,
    /// The participant (stream owner) who granted the role.
    pub created_by: ParticipantId,
    /// When the role was granted (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

const COLUMNS: &str = "id, stream_id, participant_id, created_by, created_at";

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: Uuid,
    stream_id: Uuid,
    participant_id: Uuid,
    created_by: Uuid,
    created_at: time::OffsetDateTime,
}

fn row_to_model(r: &Row) -> StreamModerator {
    StreamModerator {
        id: StreamModeratorId::from_uuid(r.id),
        stream_id: Ulid(r.stream_id.as_u128()),
        participant_id: ParticipantId::from_uuid(r.participant_id),
        created_by: ParticipantId::from_uuid(r.created_by),
        created_at: r.created_at,
    }
}

/// Repository over the `stream_moderators` table.
///
/// Cheap to clone — wraps a [`PgPool`]; feature modules build one inline via
/// [`StreamModeratorRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct StreamModeratorRepo {
    pool: PgPool,
}

#[derive(Clone, Copy)]
pub(crate) enum RequiredStreamAuthority {
    Owner,
    OwnerOrModerator,
}

/// Prove the actor may still access the stream's canonical scope and hold the
/// stream row until the surrounding transaction commits. The returned
/// participant is the current canonical owner.
///
/// Room-linked streams enter the global workspace -> room -> membership lock
/// order before taking the stream lock. Unlinked streams fence the actor's
/// active identity before the stream.
pub(crate) async fn lock_stream_access_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    stream: Ulid,
    actor: ParticipantId,
) -> Result<ParticipantId, AeroError> {
    let stream_id = Uuid::from_u128(stream.0);
    let resolved = sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
        "SELECT owner_id, room_id FROM streams WHERE id = $1",
    )
    .bind(stream_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| AeroError::NotFound(format!("stream {stream}")))?;

    if let Some(room_id) = resolved.1 {
        let effective: bool = sqlx::query_scalar("SELECT aero_effective_room_access($1, $2, NULL)")
            .bind(room_id)
            .bind(actor.to_uuid())
            .fetch_one(&mut **tx)
            .await?;
        if !effective {
            return Err(AeroError::Forbidden(
                "stream actor lacks effective room access".into(),
            ));
        }
    } else {
        let active = sqlx::query_scalar::<_, bool>(
            "SELECT deleted_at IS NULL FROM participants WHERE id = $1 FOR SHARE",
        )
        .bind(actor.to_uuid())
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or(false);
        if !active {
            return Err(AeroError::Forbidden(
                "stream actor identity is inactive".into(),
            ));
        }
    }

    let locked = sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
        "SELECT owner_id, room_id FROM streams WHERE id = $1 FOR UPDATE",
    )
    .bind(stream_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| AeroError::NotFound(format!("stream {stream}")))?;
    if locked != resolved {
        return Err(AeroError::Conflict(
            "stream scope changed during authorization".into(),
        ));
    }

    Ok(ParticipantId::from_uuid(locked.0))
}

/// Resolve current actor authority and hold the canonical stream row until the
/// surrounding transaction commits.
///
/// All live-governance mutations take the stream lock before moderator-edge
/// locks, making grants/revocations deterministic.
pub(crate) async fn lock_stream_authority_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    stream: Ulid,
    actor: ParticipantId,
    required: RequiredStreamAuthority,
) -> Result<(), AeroError> {
    let owner_id = lock_stream_access_in_tx(tx, stream, actor).await?;
    if owner_id != actor {
        if matches!(required, RequiredStreamAuthority::Owner) {
            return Err(AeroError::Forbidden(
                "only the stream owner may manage moderators".into(),
            ));
        }
        let moderator = sqlx::query_scalar::<_, Uuid>(
            r"SELECT id
                FROM stream_moderators
               WHERE stream_id = $1
                 AND participant_id = $2
               FOR SHARE",
        )
        .bind(Uuid::from_u128(stream.0))
        .bind(actor.to_uuid())
        .fetch_optional(&mut **tx)
        .await?;
        if moderator.is_none() {
            return Err(AeroError::Forbidden(
                "only the stream owner or a moderator may moderate".into(),
            ));
        }
    }

    Ok(())
}

pub(crate) async fn set_live_governance_actor(
    tx: &mut Transaction<'_, Postgres>,
    actor: ParticipantId,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config('aero.live_governance_actor', $1, true)")
        .bind(actor.to_uuid().to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn lock_active_target(
    tx: &mut Transaction<'_, Postgres>,
    target: ParticipantId,
) -> Result<(), AeroError> {
    let active = sqlx::query_scalar::<_, bool>(
        "SELECT deleted_at IS NULL FROM participants WHERE id = $1 FOR SHARE",
    )
    .bind(target.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(false);
    if !active {
        return Err(AeroError::NotFound(format!("participant {target}")));
    }
    Ok(())
}

impl StreamModeratorRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Grant `participant` the moderator role on `stream`, proving `actor` is
    /// still its owner at commit time. Re-adding is idempotent and preserves the
    /// original audit row.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert.
    pub async fn add_authorized(
        &self,
        stream: Ulid,
        participant: ParticipantId,
        actor: ParticipantId,
    ) -> Result<StreamModeratorId, AeroError> {
        let mut tx = self.pool.begin().await?;
        lock_stream_authority_in_tx(&mut tx, stream, actor, RequiredStreamAuthority::Owner).await?;
        lock_active_target(&mut tx, participant).await?;
        set_live_governance_actor(&mut tx, actor).await?;

        let id = StreamModeratorId::new();
        let inserted = sqlx::query_scalar::<_, Uuid>(
            r"INSERT INTO stream_moderators (id, stream_id, participant_id, created_by)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (stream_id, participant_id)
               DO NOTHING
               RETURNING id",
        )
        .bind(id.to_uuid())
        .bind(Uuid::from_u128(stream.0))
        .bind(participant.to_uuid())
        .bind(actor.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let surviving = if let Some(inserted) = inserted {
            inserted
        } else {
            sqlx::query_scalar::<_, Uuid>(
                r"SELECT id
                    FROM stream_moderators
                   WHERE stream_id = $1
                     AND participant_id = $2",
            )
            .bind(Uuid::from_u128(stream.0))
            .bind(participant.to_uuid())
            .fetch_one(&mut *tx)
            .await?
        };
        tx.commit().await?;
        Ok(StreamModeratorId::from_uuid(surviving))
    }

    /// Revoke `participant`'s moderator role while holding the same stream fence
    /// used by in-flight moderator actions.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn remove_authorized(
        &self,
        stream: Ulid,
        participant: ParticipantId,
        actor: ParticipantId,
    ) -> Result<bool, AeroError> {
        let mut tx = self.pool.begin().await?;
        lock_stream_authority_in_tx(&mut tx, stream, actor, RequiredStreamAuthority::Owner).await?;
        set_live_governance_actor(&mut tx, actor).await?;
        let result = sqlx::query(
            r"DELETE FROM stream_moderators WHERE stream_id = $1 AND participant_id = $2",
        )
        .bind(Uuid::from_u128(stream.0))
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }

    /// A stream's moderators, newest first, visible only to its current owner.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_authorized(
        &self,
        stream: Ulid,
        actor: ParticipantId,
    ) -> Result<Vec<StreamModerator>, AeroError> {
        let mut tx = self.pool.begin().await?;
        lock_stream_authority_in_tx(&mut tx, stream, actor, RequiredStreamAuthority::Owner).await?;
        let sql = format!(
            "SELECT {COLUMNS}
               FROM stream_moderators
              WHERE stream_id = $1
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(Uuid::from_u128(stream.0))
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows.iter().map(row_to_model).collect())
    }

    /// Whether `participant` holds the moderator role on `stream`. Used by the
    /// ban handler to grant a moderator the same chat-ban authority as the owner.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn is_moderator(
        &self,
        stream: Ulid,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let exists = sqlx::query_scalar::<_, bool>(
            r"SELECT EXISTS(
                SELECT 1 FROM stream_moderators
                 WHERE stream_id = $1 AND participant_id = $2
              )",
        )
        .bind(Uuid::from_u128(stream.0))
        .bind(participant.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(exists)
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored stream_moderator_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn participant(p: &PgPool, label: &str) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(id.to_uuid())
            .bind(format!("{label}-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    async fn stream(p: &PgPool, owner: ParticipantId) -> Ulid {
        let id = Ulid::new();
        sqlx::query(
            r"INSERT INTO streams
                    (id, owner_id, title, stream_key, status, protocol)
              VALUES ($1, $2, $3, $4, 'idle', 'rtmp')",
        )
        .bind(Uuid::from_u128(id.0))
        .bind(owner.to_uuid())
        .bind(format!("moderator-test-{id}"))
        .bind(format!("moderator-test-key-{id}"))
        .execute(p)
        .await
        .expect("insert stream");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn stream_moderator_add_list_remove_and_idempotent() {
        let p = pool();
        let repo = StreamModeratorRepo::new(p.clone());
        let owner = participant(&p, "smod-owner").await;
        let mod1 = participant(&p, "smod-mod").await;
        let stranger = participant(&p, "smod-stranger").await;
        let stream = stream(&p, owner).await;

        assert!(
            !repo.is_moderator(stream, mod1).await.unwrap(),
            "not a mod yet"
        );

        // Add grants the role; re-add is a no-op upsert (same id).
        assert!(matches!(
            repo.add_authorized(stream, mod1, stranger).await,
            Err(AeroError::Forbidden(_))
        ));
        let id = repo.add_authorized(stream, mod1, owner).await.unwrap();
        let id2 = repo.add_authorized(stream, mod1, owner).await.unwrap();
        assert_eq!(id, id2, "re-add keeps the same row id");
        assert!(repo.is_moderator(stream, mod1).await.unwrap(), "now a mod");

        assert!(matches!(
            repo.list_authorized(stream, stranger).await,
            Err(AeroError::Forbidden(_))
        ));
        let listed = repo.list_authorized(stream, owner).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].participant_id, mod1);
        assert_eq!(listed[0].created_by, owner);

        // Remove revokes; the second remove is a no-op.
        assert!(
            repo.remove_authorized(stream, mod1, owner).await.unwrap(),
            "removed"
        );
        assert!(
            !repo.remove_authorized(stream, mod1, owner).await.unwrap(),
            "second is a no-op"
        );
        assert!(
            !repo.is_moderator(stream, mod1).await.unwrap(),
            "no longer a mod"
        );

        // Cleanup.
        sqlx::query("DELETE FROM stream_moderators WHERE created_by = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM streams WHERE id = $1")
            .bind(Uuid::from_u128(stream.0))
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
            .bind([owner.to_uuid(), mod1.to_uuid(), stranger.to_uuid()])
            .execute(&p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn stream_moderator_raw_sql_cannot_forge_scope_or_actor() {
        let p = pool();
        let repo = StreamModeratorRepo::new(p.clone());
        let owner = participant(&p, "smod-raw-owner").await;
        let target = participant(&p, "smod-raw-target").await;
        let outsider = participant(&p, "smod-raw-outsider").await;
        let stream = stream(&p, owner).await;

        let missing_context = sqlx::query(
            r"INSERT INTO stream_moderators (id, stream_id, participant_id, created_by)
               VALUES ($1, $2, $3, $4)",
        )
        .bind(StreamModeratorId::new().to_uuid())
        .bind(Uuid::from_u128(stream.0))
        .bind(target.to_uuid())
        .bind(owner.to_uuid())
        .execute(&p)
        .await;
        assert!(
            missing_context.is_err(),
            "raw SQL cannot copy the canonical owner into audit metadata without actor context"
        );

        let mut forged = p.begin().await.unwrap();
        set_live_governance_actor(&mut forged, outsider)
            .await
            .unwrap();
        let forged_owner = sqlx::query(
            r"INSERT INTO stream_moderators (id, stream_id, participant_id, created_by)
               VALUES ($1, $2, $3, $4)",
        )
        .bind(StreamModeratorId::new().to_uuid())
        .bind(Uuid::from_u128(stream.0))
        .bind(target.to_uuid())
        .bind(outsider.to_uuid())
        .execute(&mut *forged)
        .await;
        assert!(
            forged_owner.is_err(),
            "an authenticated outsider cannot forge source ownership"
        );
        forged.rollback().await.unwrap();

        repo.add_authorized(stream, target, owner).await.unwrap();
        let raw_remove = sqlx::query(
            "DELETE FROM stream_moderators WHERE stream_id = $1 AND participant_id = $2",
        )
        .bind(Uuid::from_u128(stream.0))
        .bind(target.to_uuid())
        .execute(&p)
        .await;
        assert!(
            raw_remove.is_err(),
            "moderator assignment deletion requires owner actor context"
        );
        let tamper = sqlx::query(
            r"UPDATE stream_moderators
                  SET participant_id = $1
                WHERE stream_id = $2
                  AND participant_id = $3",
        )
        .bind(outsider.to_uuid())
        .bind(Uuid::from_u128(stream.0))
        .bind(target.to_uuid())
        .execute(&p)
        .await;
        assert!(
            tamper.is_err(),
            "persisted moderator aggregate identity is immutable"
        );

        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(target.to_uuid())
            .execute(&p)
            .await
            .expect("participant FK cascade may remove its moderator assignment");
        assert!(!repo.is_moderator(stream, target).await.unwrap());
    }
}
