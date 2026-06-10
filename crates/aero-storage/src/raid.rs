//! Raid repository (creator sends viewers from one stream to another at end).
//!
//! Backs `migrations/0080_raids.sql`. A "raid" is the Twitch/Kick end-of-stream
//! send-off: the source stream's owner raids a target stream, redirecting their
//! current viewers there. This repo records one [`Raid`] row per raid; the actual
//! viewer redirect is client-side (driven by the `StreamEvent::Raid` broadcast).
//!
//! Owner-gating (the source-stream owner) is enforced by the HTTP layer; this repo
//! persists what it is given. Purely additive: a NEW [`RaidRepo`]; no existing repo
//! is touched. `source_stream`/`target_stream` are plain UUID columns (not
//! cascading FKs) so a raid row outlives a pruned stream, mirroring
//! [`ClipRepo`](crate::ClipRepo).

use aero_common::{ParticipantId, RaidId};
use serde::Serialize;
use sqlx::PgPool;
use ulid::Ulid;
use uuid::Uuid;

/// One recorded raid — a storage-layer projection of a `raid_history` row.
#[derive(Debug, Clone, Serialize)]
pub struct Raid {
    /// The raid's unique id.
    pub id: RaidId,
    /// The stream the raid was launched FROM.
    pub source_stream: Ulid,
    /// The stream viewers were sent TO.
    pub target_stream: Ulid,
    /// The participant (source-stream owner) who initiated the raid.
    pub raider_id: ParticipantId,
    /// Viewers carried over at the time of the raid.
    pub viewer_count: i32,
    /// When the raid was recorded (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

const COLUMNS: &str = "id, source_stream, target_stream, raider_id, viewer_count, created_at";

type Row = (Uuid, Uuid, Uuid, Uuid, i32, time::OffsetDateTime);

fn row_to_model(r: Row) -> Raid {
    let (id, source_stream, target_stream, raider_id, viewer_count, created_at) = r;
    Raid {
        id: RaidId::from_uuid(id),
        source_stream: Ulid(source_stream.as_u128()),
        target_stream: Ulid(target_stream.as_u128()),
        raider_id: ParticipantId::from_uuid(raider_id),
        viewer_count,
        created_at,
    }
}

/// Repository over the `raid_history` table.
///
/// Cheap to clone — wraps a [`PgPool`]; feature modules build one inline via
/// [`RaidRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct RaidRepo {
    pool: PgPool,
}

impl RaidRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record a raid from `source` to `target` by `raider`, carrying
    /// `viewer_count` viewers, returning its generated id. The caller has already
    /// checked the raider owns the source stream and that both streams exist.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn create(
        &self,
        source: Ulid,
        target: Ulid,
        raider: ParticipantId,
        viewer_count: i32,
    ) -> Result<RaidId, sqlx::Error> {
        let id = RaidId::new();
        sqlx::query(
            r"INSERT INTO raid_history
                  (id, source_stream, target_stream, raider_id, viewer_count)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id.to_uuid())
        .bind(Uuid::from_u128(source.0))
        .bind(Uuid::from_u128(target.0))
        .bind(raider.to_uuid())
        .bind(viewer_count.max(0))
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Fetch one raid by id, or `None` if no such row exists.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(&self, id: RaidId) -> Result<Option<Raid>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM raid_history WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// Raids launched FROM a stream, newest first (its raid log).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_stream(&self, source: Ulid) -> Result<Vec<Raid>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM raid_history
              WHERE source_stream = $1
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(Uuid::from_u128(source.0))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Raids a creator (participant) initiated, newest first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_creator(
        &self,
        raider: ParticipantId,
    ) -> Result<Vec<Raid>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM raid_history
              WHERE raider_id = $1
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(raider.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored raid_
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

    async fn raider(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(id.to_uuid())
            .bind(format!("raider-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn raid_create_get_list() {
        let p = pool();
        let repo = RaidRepo::new(p.clone());
        let source = Ulid::new();
        let target = Ulid::new();
        let owner = raider(&p).await;

        let id = repo.create(source, target, owner, 42).await.unwrap();

        let got = repo.get(id).await.unwrap().expect("raid exists");
        assert_eq!(got.id, id);
        assert_eq!(got.source_stream, source);
        assert_eq!(got.target_stream, target);
        assert_eq!(got.raider_id, owner);
        assert_eq!(got.viewer_count, 42);

        let by_stream = repo.list_for_stream(source).await.unwrap();
        assert!(by_stream.iter().any(|r| r.id == id), "source's raid log shows it");

        let by_creator = repo.list_for_creator(owner).await.unwrap();
        assert!(by_creator.iter().any(|r| r.id == id), "creator's raids show it");

        // Cleanup.
        sqlx::query("DELETE FROM raid_history WHERE raider_id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
