//! Ban / timeout appeal-workflow repository.
//!
//! Backs `migrations/0091_ban_appeals.sql`. A viewer banned (or timed-out) from a
//! stream's danmaku chat ([`StreamModRepo`](crate::StreamModRepo) / 0026) may APPEAL:
//! submit a reason for the creator/mods to review. The reviewer approves — which
//! lifts the ban inside the actor-aware review transaction — or denies (the ban
//! stands). One row per appeal; a viewer may appeal again after a denial.
//!
//! No DB FK to `stream_bans`: an appeal is a HISTORICAL record that must OUTLIVE the ban
//! it appeals (approving lifts the ban but keeps the `approved` appeal). `stream_bans`
//! has the composite PK `(stream_id, participant_id)` and no surrogate id;
//! `stream_moderators` is a DIFFERENT table.
//!
//! Submission gate: [`BanAppealRepo::submit_appeal`] only inserts if an ACTIVE ban
//! exists for `(stream, appellant)` (a row that is permanent or whose timeout has not
//! expired) — there is nothing to appeal otherwise. The repository and 0225 raw-SQL
//! trigger bind the submission to the locked ban revision instead of a cascading FK,
//! so the historical appeal outlives its ban without gaining authority over a re-ban.

use aero_common::{BanAppealId, Error as AeroError, ParticipantId};
use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use ulid::Ulid;
use uuid::Uuid;

use crate::stream_moderator::{
    lock_stream_access_in_tx, lock_stream_authority_in_tx, set_live_governance_actor,
    RequiredStreamAuthority,
};

/// Maximum appeal and reviewer-decision reason lengths, measured in Unicode
/// scalar values.
pub const MAX_APPEAL_REASON_CHARS: usize = 2_000;
pub const MAX_APPEAL_DECISION_REASON_CHARS: usize = 2_000;
/// Default and maximum pending-appeal page sizes.
pub const DEFAULT_APPEAL_PAGE_SIZE: i64 = 50;
pub const MAX_APPEAL_PAGE_SIZE: i64 = 100;
/// Bound deep offsets as well as result size.
pub const MAX_APPEAL_PAGE_OFFSET: i64 = 10_000;

/// Clamp a caller-supplied pending-appeal page to its resource envelope.
#[must_use]
pub fn bounded_appeal_page(limit: i64, offset: i64) -> (i64, i64) {
    (
        limit.clamp(1, MAX_APPEAL_PAGE_SIZE),
        offset.clamp(0, MAX_APPEAL_PAGE_OFFSET),
    )
}

/// Why an appeal could not be submitted, or the outcome of a review.
#[derive(Debug, thiserror::Error)]
pub enum AppealError {
    /// No active ban exists for `(stream, appellant)` — nothing to appeal.
    #[error("no active ban to appeal")]
    NotBanned,
    /// The appeal payload violates its resource envelope.
    #[error("{0}")]
    Invalid(String),
    /// The appellant cannot access the stream's current canonical scope.
    #[error(transparent)]
    Access(#[from] AeroError),
    /// A storage error occurred.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// One ban appeal — a `ban_appeals` row.
#[derive(Debug, Clone, Serialize)]
pub struct BanAppeal {
    pub id: BanAppealId,
    pub stream_id: Ulid,
    pub appellant_id: ParticipantId,
    pub appeal_reason: String,
    /// The concrete ban incarnation this appeal can lift. `None` only occurs
    /// for legacy history that could not safely be associated during migration.
    pub ban_revision: Option<i64>,
    /// `pending` / `approved` / `denied`.
    pub status: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub reviewed_by: Option<ParticipantId>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub reviewed_at: Option<OffsetDateTime>,
    pub decision_reason: Option<String>,
}

const COLUMNS: &str =
    "id, stream_id, appellant_id, appeal_reason, ban_revision, status, created_at, \
                       reviewed_by, reviewed_at, decision_reason";

type Row = (
    Uuid,
    Uuid,
    Uuid,
    String,
    Option<i64>,
    String,
    OffsetDateTime,
    Option<Uuid>,
    Option<OffsetDateTime>,
    Option<String>,
);

fn row_to_model(r: Row) -> BanAppeal {
    let (
        id,
        stream_id,
        appellant_id,
        appeal_reason,
        ban_revision,
        status,
        created_at,
        reviewed_by,
        reviewed_at,
        decision_reason,
    ) = r;
    BanAppeal {
        id: BanAppealId::from_uuid(id),
        stream_id: Ulid(stream_id.as_u128()),
        appellant_id: ParticipantId::from_uuid(appellant_id),
        appeal_reason,
        ban_revision,
        status,
        created_at,
        reviewed_by: reviewed_by.map(ParticipantId::from_uuid),
        reviewed_at,
        decision_reason,
    }
}

/// Repository over the `ban_appeals` table.
///
/// Cheap to clone — wraps a [`PgPool`]; feature modules build one inline via
/// [`BanAppealRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct BanAppealRepo {
    pool: PgPool,
}

impl BanAppealRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Submit an appeal for `(stream, appellant)`, returning its id. Only
    /// succeeds when the appellant still has effective access to the stream
    /// scope and a ban is active according to the database clock.
    ///
    /// # Errors
    /// - [`AppealError::NotBanned`] if the viewer is not actively banned.
    /// - [`AppealError::Invalid`] if the reason is empty or too long.
    /// - [`AppealError::Access`] if current stream-scope access is absent.
    /// - [`AppealError::Db`] on any storage error.
    pub async fn submit_appeal(
        &self,
        stream: Ulid,
        appellant: ParticipantId,
        reason: &str,
    ) -> Result<BanAppealId, AppealError> {
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(AppealError::Invalid("appeal reason is empty".into()));
        }
        if reason.chars().count() > MAX_APPEAL_REASON_CHARS {
            return Err(AppealError::Invalid(format!(
                "appeal reason exceeds {MAX_APPEAL_REASON_CHARS} characters"
            )));
        }

        let stream_uuid = Uuid::from_u128(stream.0);
        let mut tx = self.pool.begin().await?;

        // Match the global workspace -> room -> membership -> stream order and
        // prove the appellant still belongs to the canonical scope. Unlinked
        // streams instead fence the appellant's active identity.
        lock_stream_access_in_tx(&mut tx, stream, appellant).await?;

        let revision = sqlx::query_scalar::<_, i64>(
            r"SELECT ban_revision
                FROM stream_bans
               WHERE stream_id = $1
                 AND participant_id = $2
                 AND (until IS NULL OR until > clock_timestamp())
               FOR UPDATE",
        )
        .bind(stream_uuid)
        .bind(appellant.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppealError::NotBanned)?;

        set_live_governance_actor(&mut tx, appellant).await?;
        let id = BanAppealId::new();
        let inserted = sqlx::query_scalar::<_, Uuid>(
            r"INSERT INTO ban_appeals
                    (id, stream_id, appellant_id, appeal_reason, ban_revision)
               VALUES ($1, $2, $3, $4, $5)
               ON CONFLICT (stream_id, appellant_id, ban_revision)
                   WHERE status = 'pending' AND ban_revision IS NOT NULL
               DO NOTHING
               RETURNING id",
        )
        .bind(id.to_uuid())
        .bind(stream_uuid)
        .bind(appellant.to_uuid())
        .bind(reason)
        .bind(revision)
        .fetch_optional(&mut *tx)
        .await?;

        let surviving = if let Some(inserted) = inserted {
            inserted
        } else {
            sqlx::query_scalar::<_, Uuid>(
                r"SELECT id
                    FROM ban_appeals
                   WHERE stream_id = $1
                     AND appellant_id = $2
                     AND ban_revision = $3
                     AND status = 'pending'",
            )
            .bind(stream_uuid)
            .bind(appellant.to_uuid())
            .bind(revision)
            .fetch_one(&mut *tx)
            .await?
        };
        tx.commit().await?;
        Ok(BanAppealId::from_uuid(surviving))
    }

    /// Fetch one appeal by id, or `None`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(&self, appeal: BanAppealId) -> Result<Option<BanAppeal>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM ban_appeals WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(appeal.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// A stream's PENDING appeals, oldest first (review queue), after proving
    /// `actor` still owns/moderates the canonical stream under the same
    /// transaction used for the read.
    ///
    /// # Errors
    /// - [`AeroError::Forbidden`] if current stream authority is absent.
    /// - Wraps storage errors in [`AeroError::Database`].
    pub async fn list_pending_authorized(
        &self,
        stream: Ulid,
        actor: ParticipantId,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<BanAppeal>, AeroError> {
        let (limit, offset) = bounded_appeal_page(limit, offset);
        let mut tx = self.pool.begin().await?;
        lock_stream_authority_in_tx(
            &mut tx,
            stream,
            actor,
            RequiredStreamAuthority::OwnerOrModerator,
        )
        .await?;
        let sql = format!(
            "SELECT {COLUMNS}
               FROM ban_appeals
              WHERE stream_id = $1 AND status = 'pending'
              ORDER BY created_at ASC, id ASC
              LIMIT $2 OFFSET $3"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(Uuid::from_u128(stream.0))
            .bind(limit)
            .bind(offset)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Review a PENDING appeal after proving `reviewer` still owns/moderates the
    /// canonical stream under its transaction lock. On approval, lift the ban
    /// in the same transaction. Returns `false` only when an existing appeal was
    /// already reviewed.
    ///
    /// On approval the appeal row is reviewed FIRST, then the ban is removed; there is no
    /// FK between the two tables, so the `approved` appeal persists as a historical record
    /// after the ban is lifted. Wrapped in one transaction so the decision + unban are
    /// atomic.
    ///
    /// # Errors
    /// - [`AeroError::NotFound`] if the appeal no longer exists.
    /// - [`AeroError::Forbidden`] if current stream authority is absent.
    /// - Wraps storage errors in [`AeroError::Database`].
    pub async fn review_authorized(
        &self,
        appeal: BanAppealId,
        reviewer: ParticipantId,
        approved: bool,
        reason: Option<&str>,
    ) -> Result<bool, AeroError> {
        let reason = reason.map(str::trim).filter(|value| !value.is_empty());
        if reason.is_some_and(|value| value.chars().count() > MAX_APPEAL_DECISION_REASON_CHARS) {
            return Err(AeroError::Invalid(format!(
                "appeal decision reason exceeds {MAX_APPEAL_DECISION_REASON_CHARS} characters"
            )));
        }
        let status = if approved { "approved" } else { "denied" };
        let mut tx = self.pool.begin().await?;

        // Resolve without locking the appeal first. Every review/revocation
        // enters canonical stream -> moderator-edge -> appeal order.
        let (stream_uuid, appellant_uuid, ban_revision) =
            sqlx::query_as::<_, (Uuid, Uuid, Option<i64>)>(
                "SELECT stream_id, appellant_id, ban_revision FROM ban_appeals WHERE id = $1",
            )
            .bind(appeal.to_uuid())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AeroError::NotFound(format!("appeal {appeal}")))?;
        lock_stream_authority_in_tx(
            &mut tx,
            Ulid(stream_uuid.as_u128()),
            reviewer,
            RequiredStreamAuthority::OwnerOrModerator,
        )
        .await?;
        set_live_governance_actor(&mut tx, reviewer).await?;

        // Conditional UPDATE is the pending-state lock/recheck. If another
        // reviewer committed first, this call leaves both appeal and ban alone.
        let stamped = sqlx::query_as::<_, (Uuid, Uuid)>(
            r"UPDATE ban_appeals
                 SET status = $2,
                     reviewed_by = $3,
                     reviewed_at = now(),
                     decision_reason = $4
               WHERE id = $1 AND status = 'pending'
               RETURNING stream_id, appellant_id",
        )
        .bind(appeal.to_uuid())
        .bind(status)
        .bind(reviewer.to_uuid())
        .bind(reason)
        .fetch_optional(&mut *tx)
        .await?;

        let Some((stamped_stream, stamped_appellant)) = stamped else {
            tx.commit().await?;
            return Ok(false);
        };
        if stamped_stream != stream_uuid || stamped_appellant != appellant_uuid {
            return Err(AeroError::Conflict(
                "appeal scope changed during authorization".into(),
            ));
        }

        if approved {
            if let Some(ban_revision) = ban_revision {
                sqlx::query(
                    r"DELETE FROM stream_bans
                       WHERE stream_id = $1
                         AND participant_id = $2
                         AND ban_revision = $3",
                )
                .bind(stream_uuid)
                .bind(appellant_uuid)
                .bind(ban_revision)
                .execute(&mut *tx)
                .await?;
            }
        }

        tx.commit().await?;
        Ok(true)
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored ban_appeal_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::{StreamModRepo, StreamModeratorRepo};
    use sqlx::postgres::PgConnectOptions;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn tagged_pool(application_name: &str) -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        let options = url
            .parse::<PgConnectOptions>()
            .expect("valid DATABASE_URL")
            .application_name(application_name);
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("connect tagged appeal test pool")
    }

    async fn wait_until_tagged_query_waits_on_lock(pool: &PgPool, application_name: &str) {
        for _ in 0..100 {
            let waiting = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                     SELECT 1
                       FROM pg_stat_activity
                      WHERE datname = current_database()
                        AND application_name = $1
                        AND wait_event_type = 'Lock'
                 )",
            )
            .bind(application_name)
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("tagged appeal review never reached its expected lock wait");
    }

    async fn participant(p: &PgPool, label: &str) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(id.to_uuid())
            .bind(format!("appeal-{label}-{id}"))
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
        .bind(format!("appeal-{label}-{id}"))
        .bind(format!("appeal-{label}-key-{id}"))
        .execute(p)
        .await
        .expect("insert appeal stream");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn ban_appeal_only_active_ban_appealable() {
        let p = pool();
        let mods = StreamModRepo::new(p.clone());
        let repo = BanAppealRepo::new(p.clone());
        let viewer = participant(&p, "viewer").await;
        let owner = participant(&p, "owner").await;
        let stream = stream(&p, owner, "active-ban").await;
        // No ban yet => cannot appeal.
        let err = repo
            .submit_appeal(stream, viewer, "let me back")
            .await
            .unwrap_err();
        assert!(matches!(err, AppealError::NotBanned));

        // Ban the viewer (permanent) => appeal succeeds.
        mods.ban(stream, viewer, owner, Some("spam"), None)
            .await
            .unwrap();
        let appeal = repo
            .submit_appeal(stream, viewer, "I was joking")
            .await
            .unwrap();
        let got = repo.get(appeal).await.unwrap().unwrap();
        assert_eq!(got.status, "pending");
        assert_eq!(got.appellant_id, viewer);
        assert!(repo
            .list_pending_authorized(stream, owner, DEFAULT_APPEAL_PAGE_SIZE, 0)
            .await
            .unwrap()
            .iter()
            .any(|a| a.id == appeal));
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn ban_appeal_approve_triggers_unban() {
        let p = pool();
        let mods = StreamModRepo::new(p.clone());
        let repo = BanAppealRepo::new(p.clone());
        let viewer = participant(&p, "viewer").await;
        let owner = participant(&p, "owner").await;
        let stream = stream(&p, owner, "approve").await;
        let now = OffsetDateTime::now_utc();

        mods.ban(stream, viewer, owner, None, None).await.unwrap();
        let appeal = repo.submit_appeal(stream, viewer, "sorry").await.unwrap();

        // Approve => the viewer is unbanned and the appeal stamped approved.
        assert!(repo
            .review_authorized(appeal, owner, true, Some("first offense"))
            .await
            .unwrap());
        assert!(
            !mods.is_banned(stream, viewer, now).await.unwrap(),
            "approving an appeal lifts the ban"
        );
        let got = repo.get(appeal).await.unwrap().unwrap();
        assert_eq!(got.status, "approved");
        assert_eq!(got.reviewed_by, Some(owner));
        assert_eq!(got.decision_reason.as_deref(), Some("first offense"));

        // Re-reviewing is a no-op.
        assert!(!repo
            .review_authorized(appeal, owner, false, None)
            .await
            .unwrap());
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn ban_appeal_deny_keeps_ban() {
        let p = pool();
        let mods = StreamModRepo::new(p.clone());
        let repo = BanAppealRepo::new(p.clone());
        let viewer = participant(&p, "viewer").await;
        let owner = participant(&p, "owner").await;
        let stream = stream(&p, owner, "deny").await;
        let now = OffsetDateTime::now_utc();

        mods.ban(stream, viewer, owner, Some("harassment"), None)
            .await
            .unwrap();
        let appeal = repo.submit_appeal(stream, viewer, "not me").await.unwrap();

        // Deny => the ban stands, the appeal is denied.
        assert!(repo
            .review_authorized(appeal, owner, false, Some("evidence stands"))
            .await
            .unwrap());
        assert!(
            mods.is_banned(stream, viewer, now).await.unwrap(),
            "denying an appeal keeps the ban"
        );
        let got = repo.get(appeal).await.unwrap().unwrap();
        assert_eq!(got.status, "denied");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn ban_appeal_revoked_moderator_cannot_commit_review() {
        let p = pool();
        let mods = StreamModRepo::new(p.clone());
        let repo = BanAppealRepo::new(p.clone());
        let viewer = participant(&p, "race-viewer").await;
        let owner = participant(&p, "race-owner").await;
        let moderator = participant(&p, "race-moderator").await;
        let stream = stream(&p, owner, "review-race").await;
        StreamModeratorRepo::new(p.clone())
            .add_authorized(stream, moderator, owner)
            .await
            .unwrap();
        mods.ban_authorized(stream, viewer, owner, None, None)
            .await
            .unwrap();
        let appeal = repo
            .submit_appeal(stream, viewer, "please review")
            .await
            .unwrap();

        let mut revocation = p.begin().await.unwrap();
        sqlx::query("SELECT id FROM streams WHERE id = $1 FOR UPDATE")
            .bind(Uuid::from_u128(stream.0))
            .execute(&mut *revocation)
            .await
            .unwrap();
        set_live_governance_actor(&mut revocation, owner)
            .await
            .unwrap();
        sqlx::query("DELETE FROM stream_moderators WHERE stream_id = $1 AND participant_id = $2")
            .bind(Uuid::from_u128(stream.0))
            .bind(moderator.to_uuid())
            .execute(&mut *revocation)
            .await
            .unwrap();

        let application_name = format!("ban-appeal-revoke-race-{moderator}");
        let raced_repo = BanAppealRepo::new(tagged_pool(&application_name).await);
        let raced = tokio::spawn(async move {
            raced_repo
                .review_authorized(appeal, moderator, true, Some("raced approval"))
                .await
        });
        wait_until_tagged_query_waits_on_lock(&p, &application_name).await;
        revocation.commit().await.unwrap();

        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(3), raced)
                .await
                .expect("appeal race completed")
                .expect("appeal review task"),
            Err(AeroError::Forbidden(_))
        ));
        assert_eq!(
            repo.get(appeal).await.unwrap().unwrap().status,
            "pending",
            "failed post-revocation review leaves the appeal pending"
        );
        assert!(
            mods.is_banned(stream, viewer, OffsetDateTime::now_utc())
                .await
                .unwrap(),
            "failed post-revocation approval leaves the ban intact"
        );
    }
}

#[cfg(test)]
#[path = "ban_appeals/revision_tests.rs"]
mod revision_tests;
