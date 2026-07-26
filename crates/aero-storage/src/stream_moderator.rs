//! Stream-moderator role repository (assign MOD ROLE on a stream's chat).
//!
//! Backs `migrations/0083_stream_moderators.sql`. DISTINCT from
//! [`StreamModRepo`](crate::StreamModRepo) (0026), which records *banned* chatters:
//! this assigns a moderator ROLE to a participant on a stream. A stream owner
//! adds/removes moderators; a moderator then gains the same chat-ban/timeout
//! authority as the owner on that stream's danmaku chat (the ban handler allows
//! owner OR [`StreamModeratorRepo::is_moderator`]).
//!
//! Add/remove are owner-gated by the HTTP layer; this repo persists what it is
//! given. Purely additive: a NEW [`StreamModeratorRepo`]; no existing repo is
//! touched. One row per `(stream, participant)` — the UNIQUE constraint makes a
//! re-add a no-op upsert. Stream ids are [`Ulid`]s stored as UUID, bound via
//! `Uuid::from_u128(ulid.0)` to match [`StreamRepo`](crate::StreamRepo).

use aero_common::{ParticipantId, StreamModeratorId};
use serde::Serialize;
use sqlx::PgPool;
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

fn row_to_model(r: Row) -> StreamModerator {
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

impl StreamModeratorRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Grant `participant` the moderator role on `stream` (granted by `created_by`,
    /// the owner), returning the surviving row's id. Idempotent: a re-add upserts
    /// (keeping the original id), so adding twice is a no-op.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert.
    pub async fn add(
        &self,
        stream: Ulid,
        participant: ParticipantId,
        created_by: ParticipantId,
    ) -> Result<StreamModeratorId, sqlx::Error> {
        let id = StreamModeratorId::new();
        let row: (Uuid,) = sqlx::query_as(
            r"INSERT INTO stream_moderators (id, stream_id, participant_id, created_by)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (stream_id, participant_id)
               DO UPDATE SET created_by = EXCLUDED.created_by
               RETURNING id",
        )
        .bind(id.to_uuid())
        .bind(Uuid::from_u128(stream.0))
        .bind(participant.to_uuid())
        .bind(created_by.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(StreamModeratorId::from_uuid(row.0))
    }

    /// Revoke `participant`'s moderator role on `stream`. Returns `true` iff a row
    /// was removed (idempotent: `false` when they weren't a moderator).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn remove(
        &self,
        stream: Ulid,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"DELETE FROM stream_moderators WHERE stream_id = $1 AND participant_id = $2",
        )
        .bind(Uuid::from_u128(stream.0))
        .bind(participant.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// A stream's moderators, newest first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list(&self, stream: Ulid) -> Result<Vec<StreamModerator>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM stream_moderators
              WHERE stream_id = $1
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(Uuid::from_u128(stream.0))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
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

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn stream_moderator_add_list_remove_and_idempotent() {
        let p = pool();
        let repo = StreamModeratorRepo::new(p.clone());
        let stream = Ulid::new();
        let owner = participant(&p, "smod-owner").await;
        let mod1 = participant(&p, "smod-mod").await;

        assert!(!repo.is_moderator(stream, mod1).await.unwrap(), "not a mod yet");

        // Add grants the role; re-add is a no-op upsert (same id).
        let id = repo.add(stream, mod1, owner).await.unwrap();
        let id2 = repo.add(stream, mod1, owner).await.unwrap();
        assert_eq!(id, id2, "re-add keeps the same row id");
        assert!(repo.is_moderator(stream, mod1).await.unwrap(), "now a mod");

        let listed = repo.list(stream).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].participant_id, mod1);
        assert_eq!(listed[0].created_by, owner);

        // Remove revokes; the second remove is a no-op.
        assert!(repo.remove(stream, mod1).await.unwrap(), "removed");
        assert!(!repo.remove(stream, mod1).await.unwrap(), "second is a no-op");
        assert!(!repo.is_moderator(stream, mod1).await.unwrap(), "no longer a mod");

        // Cleanup.
        sqlx::query("DELETE FROM stream_moderators WHERE created_by = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
