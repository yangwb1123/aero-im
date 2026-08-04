//! Raid repository (creator sends viewers from one stream to another at end).
//!
//! Backs `migrations/0080_raids.sql`. A "raid" is the Twitch/Kick end-of-stream
//! send-off: the source stream's owner raids a target stream, redirecting their
//! current viewers there. This repo records one [`Raid`] row per raid; the actual
//! viewer redirect is client-side (driven by the `StreamEvent::Raid` broadcast).
//!
//! Raid creation proves current source ownership inside the same transaction
//! that locks source and target streams in UUID order and linearizes the
//! per-raider cooldown. `source_stream`/`target_stream` remain plain UUID
//! columns so historical raids outlive pruned streams.

use aero_common::{Error as AeroError, ParticipantId, RaidId};
use serde::Serialize;
use sqlx::PgPool;
use ulid::Ulid;
use uuid::Uuid;

use crate::stream_moderator::set_live_governance_actor;

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
    /// Optional custom message the raider sent to the target channel (e.g. a shoutout).
    pub message: Option<String>,
    /// When the raid was recorded (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

const COLUMNS: &str =
    "id, source_stream, target_stream, raider_id, viewer_count, message, created_at";

type Row = (
    Uuid,
    Uuid,
    Uuid,
    Uuid,
    i32,
    Option<String>,
    time::OffsetDateTime,
);

fn row_to_model(r: Row) -> Raid {
    let (id, source_stream, target_stream, raider_id, viewer_count, message, created_at) = r;
    Raid {
        id: RaidId::from_uuid(id),
        source_stream: Ulid(source_stream.as_u128()),
        target_stream: Ulid(target_stream.as_u128()),
        raider_id: ParticipantId::from_uuid(raider_id),
        viewer_count,
        message,
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
/// Maximum custom raid-message length, measured in Unicode scalar values.
pub const MAX_RAID_MESSAGE_CHARS: usize = 500;
/// Default and maximum raid-history page sizes.
pub const DEFAULT_RAID_HISTORY_LIMIT: i64 = 50;
pub const MAX_RAID_HISTORY_LIMIT: i64 = 100;
/// Bound deep offsets as well as result size.
pub const MAX_RAID_HISTORY_OFFSET: i64 = 10_000;

#[must_use]
pub fn bounded_history_page(limit: i64, offset: i64) -> (i64, i64) {
    (
        limit.clamp(1, MAX_RAID_HISTORY_LIMIT),
        offset.clamp(0, MAX_RAID_HISTORY_OFFSET),
    )
}

impl RaidRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record a raid from `source` to `target` after proving `raider` is still
    /// the source owner. The source and target are locked in UUID order, and the
    /// raider identity row linearizes the cooldown even across multiple source
    /// streams owned by the same participant.
    ///
    /// Enforces a [`RAID_COOLDOWN_SECS`] cooldown: if the raider has sent a raid
    /// within the last minute, returns
    /// [`AeroError::Forbidden`]`("raid cooldown active")`.
    ///
    /// # Errors
    /// - [`AeroError::Forbidden`] if the raider is within the cooldown window.
    /// - Wraps any [`sqlx::Error`] via [`AeroError::Database`].
    pub async fn create_authorized(
        &self,
        source: Ulid,
        target: Ulid,
        raider: ParticipantId,
        viewer_count: i32,
        message: Option<&str>,
    ) -> Result<RaidId, AeroError> {
        if source == target {
            return Err(AeroError::Invalid("cannot raid the same stream".into()));
        }
        let message = message.map(str::trim).filter(|value| !value.is_empty());
        if message.is_some_and(|value| value.chars().count() > MAX_RAID_MESSAGE_CHARS) {
            return Err(AeroError::Invalid(format!(
                "raid message exceeds {MAX_RAID_MESSAGE_CHARS} characters"
            )));
        }

        let source_id = Uuid::from_u128(source.0);
        let target_id = Uuid::from_u128(target.0);
        let mut tx = self.pool.begin().await?;

        // Serialize this actor before effective-access helpers take a SHARE lock
        // on the participant row. Without this fence, two room-linked raids by
        // the same actor could both hold SHARE and deadlock while upgrading to
        // UPDATE for the cooldown fence.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text, 219))")
            .bind(raider.to_uuid())
            .execute(&mut *tx)
            .await?;

        // Resolve the optional room before stream locks so a room-linked source
        // can enter the global workspace -> room -> membership order first.
        let resolved_source = sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
            "SELECT owner_id, room_id FROM streams WHERE id = $1",
        )
        .bind(source_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| AeroError::NotFound(format!("stream {source}")))?;

        if let Some(room_id) = resolved_source.1 {
            let effective: bool =
                sqlx::query_scalar("SELECT aero_effective_room_access($1, $2, NULL)")
                    .bind(room_id)
                    .bind(raider.to_uuid())
                    .fetch_one(&mut *tx)
                    .await?;
            if !effective {
                return Err(AeroError::Forbidden(
                    "source owner lacks effective room access".into(),
                ));
            }
        }

        // UPDATE, rather than SHARE, is intentional: all raids by this actor
        // serialize here, including raids from different source streams.
        let active_raider = sqlx::query_scalar::<_, bool>(
            "SELECT deleted_at IS NULL FROM participants WHERE id = $1 FOR UPDATE",
        )
        .bind(raider.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        if active_raider != Some(true) {
            return Err(AeroError::Forbidden(
                "source owner identity is inactive".into(),
            ));
        }

        let requested = [source_id, target_id];
        let locked = sqlx::query_as::<_, (Uuid, Uuid, Option<Uuid>)>(
            r"SELECT id, owner_id, room_id
                FROM streams
               WHERE id = ANY($1)
               ORDER BY id
               FOR UPDATE",
        )
        .bind(requested)
        .fetch_all(&mut *tx)
        .await?;
        if locked.len() != 2 {
            let missing = if locked.iter().any(|row| row.0 == source_id) {
                target
            } else {
                source
            };
            return Err(AeroError::NotFound(format!("stream {missing}")));
        }
        let canonical_source = locked
            .iter()
            .find(|row| row.0 == source_id)
            .expect("two requested streams include source");
        if (canonical_source.1, canonical_source.2) != resolved_source {
            return Err(AeroError::Conflict(
                "source stream authority changed during authorization".into(),
            ));
        }
        if canonical_source.1 != raider.to_uuid() {
            return Err(AeroError::Forbidden(
                "only the source stream owner may raid".into(),
            ));
        }

        let cooling_down: bool = sqlx::query_scalar(
            r"SELECT EXISTS (
                  SELECT 1
                   FROM raid_history
                   WHERE raider_id = $1
                     AND created_at
                         > clock_timestamp()
                           - make_interval(secs => $2::double precision)
              )",
        )
        .bind(raider.to_uuid())
        .bind(RAID_COOLDOWN_SECS)
        .fetch_one(&mut *tx)
        .await?;
        if cooling_down {
            return Err(AeroError::Forbidden("raid cooldown active".into()));
        }

        set_live_governance_actor(&mut tx, raider).await?;
        let id = RaidId::new();
        sqlx::query(
            r"INSERT INTO raid_history
                  (id, source_stream, target_stream, raider_id, viewer_count, message, created_at)
               VALUES ($1, $2, $3, $4, $5, $6, clock_timestamp())",
        )
        .bind(id.to_uuid())
        .bind(source_id)
        .bind(target_id)
        .bind(raider.to_uuid())
        .bind(viewer_count.max(0))
        .bind(message)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
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
    pub async fn list_for_stream(
        &self,
        source: Ulid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Raid>, sqlx::Error> {
        let (limit, offset) = bounded_history_page(limit, offset);
        let sql = format!(
            "SELECT {COLUMNS}
               FROM raid_history
              WHERE source_stream = $1
              ORDER BY created_at DESC, id DESC
              LIMIT $2 OFFSET $3"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(Uuid::from_u128(source.0))
            .bind(limit)
            .bind(offset)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Raids a creator (participant) initiated, newest first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_creator(&self, raider: ParticipantId) -> Result<Vec<Raid>, sqlx::Error> {
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

    async fn participant(p: &PgPool, label: &str) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(id.to_uuid())
            .bind(format!("raid-{label}-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    async fn stream(p: &PgPool, owner: ParticipantId, label: &str) -> Ulid {
        let id = Ulid::new();
        sqlx::query(
            r"INSERT INTO streams
                    (id, owner_id, title, stream_key, status, protocol)
              VALUES ($1, $2, $3, $4, 'idle', 'rtmp')",
        )
        .bind(Uuid::from_u128(id.0))
        .bind(owner.to_uuid())
        .bind(format!("raid-{label}-{id}"))
        .bind(format!("raid-{label}-key-{id}"))
        .execute(p)
        .await
        .expect("insert stream");
        id
    }

    #[test]
    fn raid_history_page_is_bounded() {
        assert_eq!(bounded_history_page(0, -5), (1, 0));
        assert_eq!(
            bounded_history_page(i64::MAX, i64::MAX),
            (MAX_RAID_HISTORY_LIMIT, MAX_RAID_HISTORY_OFFSET)
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn raid_create_get_list() {
        let p = pool();
        let repo = RaidRepo::new(p.clone());
        let owner = participant(&p, "owner").await;
        let source = stream(&p, owner, "source").await;
        let target_owner = participant(&p, "target-owner").await;
        let target = stream(&p, target_owner, "target").await;

        let id = repo
            .create_authorized(source, target, owner, 42, Some("Great stream!"))
            .await
            .unwrap();

        let got = repo.get(id).await.unwrap().expect("raid exists");
        assert_eq!(got.id, id);
        assert_eq!(got.source_stream, source);
        assert_eq!(got.target_stream, target);
        assert_eq!(got.raider_id, owner);
        assert_eq!(got.viewer_count, 42);
        assert_eq!(got.message.as_deref(), Some("Great stream!"));

        let by_stream = repo
            .list_for_stream(source, DEFAULT_RAID_HISTORY_LIMIT, 0)
            .await
            .unwrap();
        assert!(
            by_stream.iter().any(|r| r.id == id),
            "source's raid log shows it"
        );

        let by_creator = repo.list_for_creator(owner).await.unwrap();
        assert!(
            by_creator.iter().any(|r| r.id == id),
            "creator's raids show it"
        );

        // Raid history is intentionally retained as append-only audit data.
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn raid_create_is_owner_scoped_and_cooldown_is_linearized() {
        let p = pool();
        let owner = participant(&p, "linear-owner").await;
        let outsider = participant(&p, "linear-outsider").await;
        let target_owner = participant(&p, "linear-target-owner").await;
        let source_a = stream(&p, owner, "linear-source-a").await;
        let source_b = stream(&p, owner, "linear-source-b").await;
        let target = stream(&p, target_owner, "linear-target").await;
        let repo = RaidRepo::new(p.clone());

        assert!(matches!(
            repo.create_authorized(source_a, target, outsider, 1, None)
                .await,
            Err(AeroError::Forbidden(_))
        ));
        assert!(matches!(
            repo.create_authorized(source_a, source_a, owner, 1, None)
                .await,
            Err(AeroError::Invalid(_))
        ));

        let first = {
            let repo = repo.clone();
            tokio::spawn(async move {
                repo.create_authorized(source_a, target, owner, 10, None)
                    .await
            })
        };
        let second = {
            let repo = repo.clone();
            tokio::spawn(async move {
                repo.create_authorized(source_b, target, owner, 20, None)
                    .await
            })
        };
        let results = [first.await.unwrap(), second.await.unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(AeroError::Forbidden(_))))
                .count(),
            1,
            "the participant lock makes concurrent per-raider cooldown checks linearizable"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn raid_opposite_directions_share_uuid_stream_lock_order() {
        let p = pool();
        let owner_a = participant(&p, "opposite-owner-a").await;
        let owner_b = participant(&p, "opposite-owner-b").await;
        let stream_a = stream(&p, owner_a, "opposite-a").await;
        let stream_b = stream(&p, owner_b, "opposite-b").await;
        let repo = RaidRepo::new(p);

        let a_to_b = {
            let repo = repo.clone();
            tokio::spawn(async move {
                repo.create_authorized(stream_a, stream_b, owner_a, 1, None)
                    .await
            })
        };
        let b_to_a = tokio::spawn(async move {
            repo.create_authorized(stream_b, stream_a, owner_b, 1, None)
                .await
        });
        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            tokio::join!(a_to_b, b_to_a)
        })
        .await
        .expect("opposite-direction raids must not deadlock");
        assert!(first.unwrap().is_ok());
        assert!(second.unwrap().is_ok());
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn raid_raw_sql_cannot_forge_actor_scope_or_history() {
        let p = pool();
        let repo = RaidRepo::new(p.clone());
        let owner = participant(&p, "raw-owner").await;
        let outsider = participant(&p, "raw-outsider").await;
        let target_owner = participant(&p, "raw-target-owner").await;
        let source = stream(&p, owner, "raw-source").await;
        let target = stream(&p, target_owner, "raw-target").await;

        let missing_context = sqlx::query(
            r"INSERT INTO raid_history
                    (id, source_stream, target_stream, raider_id, viewer_count)
              VALUES ($1, $2, $3, $4, 1)",
        )
        .bind(RaidId::new().to_uuid())
        .bind(Uuid::from_u128(source.0))
        .bind(Uuid::from_u128(target.0))
        .bind(owner.to_uuid())
        .execute(&p)
        .await;
        assert!(
            missing_context.is_err(),
            "copying the canonical owner UUID cannot replace actor context"
        );

        let legacy_id = RaidId::new();
        let mut legacy = p.begin().await.unwrap();
        set_live_governance_actor(&mut legacy, owner).await.unwrap();
        let canonical_created_at: time::OffsetDateTime = sqlx::query_scalar(
            r"INSERT INTO raid_history
                    (id, source_stream, target_stream, raider_id, viewer_count, created_at)
              VALUES ($1, $2, $3, $4, 1, '2000-01-01T00:00:00Z')
              RETURNING created_at",
        )
        .bind(legacy_id.to_uuid())
        .bind(Uuid::from_u128(source.0))
        .bind(Uuid::from_u128(target.0))
        .bind(owner.to_uuid())
        .fetch_one(&mut *legacy)
        .await
        .expect("a 0219 raid writer remains compatible");
        legacy.commit().await.unwrap();
        assert!(
            canonical_created_at > time::OffsetDateTime::now_utc() - time::Duration::seconds(10),
            "the DB replaces a caller-controlled cooldown timestamp"
        );

        let mut forged = p.begin().await.unwrap();
        set_live_governance_actor(&mut forged, outsider)
            .await
            .unwrap();
        let forged_owner = sqlx::query(
            r"INSERT INTO raid_history
                    (id, source_stream, target_stream, raider_id, viewer_count)
              VALUES ($1, $2, $3, $4, 1)",
        )
        .bind(RaidId::new().to_uuid())
        .bind(Uuid::from_u128(source.0))
        .bind(Uuid::from_u128(target.0))
        .bind(outsider.to_uuid())
        .execute(&mut *forged)
        .await;
        assert!(
            forged_owner.is_err(),
            "an authenticated outsider cannot forge source ownership"
        );
        forged.rollback().await.unwrap();

        let mut backdated = p.begin().await.unwrap();
        set_live_governance_actor(&mut backdated, owner)
            .await
            .unwrap();
        let backdated_bypass = sqlx::query(
            r"INSERT INTO raid_history
                    (id, source_stream, target_stream, raider_id, viewer_count, created_at)
              VALUES ($1, $2, $3, $4, 1, '1990-01-01T00:00:00Z')",
        )
        .bind(RaidId::new().to_uuid())
        .bind(Uuid::from_u128(source.0))
        .bind(Uuid::from_u128(target.0))
        .bind(owner.to_uuid())
        .execute(&mut *backdated)
        .await;
        assert!(
            backdated_bypass.is_err(),
            "a backdated raw insert cannot evade the serialized cooldown"
        );
        backdated.rollback().await.unwrap();
        let repo_owner = participant(&p, "raw-repo-owner").await;
        let repo_target_owner = participant(&p, "raw-repo-target-owner").await;
        let repo_source = stream(&p, repo_owner, "raw-repo-source").await;
        let repo_target = stream(&p, repo_target_owner, "raw-repo-target").await;
        let raid = repo
            .create_authorized(repo_source, repo_target, repo_owner, 7, Some("immutable"))
            .await
            .unwrap();
        let tamper = sqlx::query("UPDATE raid_history SET message = 'changed' WHERE id = $1")
            .bind(raid.to_uuid())
            .execute(&p)
            .await;
        assert!(tamper.is_err(), "committed raid history is immutable");
        let raw_delete = sqlx::query("DELETE FROM raid_history WHERE id = $1")
            .bind(raid.to_uuid())
            .execute(&p)
            .await;
        assert!(raw_delete.is_err(), "committed raid history is append-only");

        let mut cooldown = p.begin().await.unwrap();
        set_live_governance_actor(&mut cooldown, repo_owner)
            .await
            .unwrap();
        let bypass = sqlx::query(
            r"INSERT INTO raid_history
                    (id, source_stream, target_stream, raider_id, viewer_count)
              VALUES ($1, $2, $3, $4, 1)",
        )
        .bind(RaidId::new().to_uuid())
        .bind(Uuid::from_u128(repo_source.0))
        .bind(Uuid::from_u128(repo_target.0))
        .bind(repo_owner.to_uuid())
        .execute(&mut *cooldown)
        .await;
        assert!(
            bypass.is_err(),
            "raw SQL cannot bypass the serialized 60-second cooldown"
        );
        cooldown.rollback().await.unwrap();
    }
}
