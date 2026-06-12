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

use aero_common::{Error as AeroError, ParticipantId, RaidId};
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

/// Aggregate analytics for raids initiated by a single raider.
///
/// `Serialize` so the handler can return it directly as JSON.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct RaidAnalytics {
    pub raids_sent: i64,
    pub total_viewers_carried: i64,
    pub avg_viewers: i64,
    pub peak_viewers: i64,
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

/// Minimum seconds between raids for the same raider (the cooldown window).
pub const RAID_COOLDOWN_SECS: i64 = 60;

impl RaidRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record a raid from `source` to `target` by `raider`, carrying
    /// `viewer_count` viewers, returning its generated id. The caller has already
    /// checked the raider owns the source stream and that both streams exist.
    ///
    /// Enforces a [`RAID_COOLDOWN_SECS`] cooldown: if the raider has sent a raid
    /// within the last minute, returns
    /// [`AeroError::Forbidden`]`("raid cooldown active")`.
    ///
    /// # Errors
    /// - [`AeroError::Forbidden`] if the raider is within the cooldown window.
    /// - Wraps any [`sqlx::Error`] via [`AeroError::Database`].
    pub async fn create(
        &self,
        source: Ulid,
        target: Ulid,
        raider: ParticipantId,
        viewer_count: i32,
    ) -> Result<RaidId, AeroError> {
        // Cooldown check: reject if the raider sent a raid within the last minute.
        let last: Option<time::OffsetDateTime> = sqlx::query_scalar(
            r"SELECT created_at FROM raid_history WHERE raider_id = $1 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(raider.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        if let Some(last_at) = last {
            let now = time::OffsetDateTime::now_utc();
            let elapsed = (now - last_at).whole_seconds();
            if elapsed < RAID_COOLDOWN_SECS {
                return Err(AeroError::Forbidden("raid cooldown active".into()));
            }
        }

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

    /// Aggregate analytics for raids the given raider has sent: total count,
    /// total + average + peak viewers carried.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn analytics(&self, raider: ParticipantId) -> Result<RaidAnalytics, sqlx::Error> {
        sqlx::query_as::<_, RaidAnalytics>(
            r"SELECT
                COUNT(*)::bigint                           AS raids_sent,
                COALESCE(SUM(viewer_count), 0)::bigint    AS total_viewers_carried,
                COALESCE(AVG(viewer_count), 0)::bigint    AS avg_viewers,
                COALESCE(MAX(viewer_count), 0)::bigint    AS peak_viewers
               FROM raid_history
              WHERE raider_id = $1",
        )
        .bind(raider.to_uuid())
        .fetch_one(&self.pool)
        .await
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
