//! Ban / timeout appeal-workflow repository.
//!
//! Backs `migrations/0091_ban_appeals.sql`. A viewer banned (or timed-out) from a
//! stream's danmaku chat ([`StreamModRepo`](crate::StreamModRepo) / 0026) may APPEAL:
//! submit a reason for the creator/mods to review. The reviewer approves — which
//! lifts the ban via the EXISTING unban path ([`StreamModRepo::unban`]) — or denies
//! (the ban stands). One row per appeal; a viewer may appeal again after a denial.
//!
//! No DB FK to `stream_bans`: an appeal is a HISTORICAL record that must OUTLIVE the ban
//! it appeals (approving lifts the ban but keeps the `approved` appeal). `stream_bans`
//! has the composite PK `(stream_id, participant_id)` and no surrogate id;
//! `stream_moderators` is a DIFFERENT table.
//!
//! Submission gate: [`BanAppealRepo::submit_appeal`] only inserts if an ACTIVE ban
//! exists for `(stream, appellant)` (a row that is permanent or whose timeout has not
//! expired) — there is nothing to appeal otherwise. That repository-level check (not a
//! DB FK) is what guarantees the ban existed at submit time. Purely additive: a NEW
//! [`BanAppealRepo`]; no existing repo is touched.

use aero_common::{BanAppealId, ParticipantId};
use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use ulid::Ulid;
use uuid::Uuid;

use crate::stream_mod::StreamModRepo;

/// Why an appeal could not be submitted, or the outcome of a review.
#[derive(Debug, thiserror::Error)]
pub enum AppealError {
    /// No active ban exists for `(stream, appellant)` — nothing to appeal.
    #[error("no active ban to appeal")]
    NotBanned,
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
    /// `pending` / `approved` / `denied`.
    pub status: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub reviewed_by: Option<ParticipantId>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub reviewed_at: Option<OffsetDateTime>,
    pub decision_reason: Option<String>,
}

const COLUMNS: &str = "id, stream_id, appellant_id, appeal_reason, status, created_at, \
                       reviewed_by, reviewed_at, decision_reason";

type Row = (
    Uuid,
    Uuid,
    Uuid,
    String,
    String,
    OffsetDateTime,
    Option<Uuid>,
    Option<OffsetDateTime>,
    Option<String>,
);

fn row_to_model(r: Row) -> BanAppeal {
    let (id, stream_id, appellant_id, appeal_reason, status, created_at, reviewed_by, reviewed_at, decision_reason) =
        r;
    BanAppeal {
        id: BanAppealId::from_uuid(id),
        stream_id: Ulid(stream_id.as_u128()),
        appellant_id: ParticipantId::from_uuid(appellant_id),
        appeal_reason,
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

    /// Submit an appeal for `(stream, appellant)`, returning its id. Only succeeds if
    /// an ACTIVE ban exists for the pair at `now` (permanent, or a timeout not yet
    /// expired); otherwise [`AppealError::NotBanned`].
    ///
    /// # Errors
    /// - [`AppealError::NotBanned`] if the viewer is not actively banned.
    /// - [`AppealError::Db`] on any storage error.
    pub async fn submit_appeal(
        &self,
        stream: Ulid,
        appellant: ParticipantId,
        reason: &str,
        now: OffsetDateTime,
    ) -> Result<BanAppealId, AppealError> {
        // Only an actively-banned viewer may appeal.
        let banned = StreamModRepo::new(self.pool.clone())
            .is_banned(stream, appellant, now)
            .await?;
        if !banned {
            return Err(AppealError::NotBanned);
        }
        let id = BanAppealId::new();
        sqlx::query(
            r"INSERT INTO ban_appeals (id, stream_id, appellant_id, appeal_reason)
               VALUES ($1, $2, $3, $4)",
        )
        .bind(id.to_uuid())
        .bind(Uuid::from_u128(stream.0))
        .bind(appellant.to_uuid())
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(id)
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

    /// A stream's PENDING appeals, oldest first (review queue).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_pending(&self, stream: Ulid) -> Result<Vec<BanAppeal>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM ban_appeals
              WHERE stream_id = $1 AND status = 'pending'
              ORDER BY created_at ASC, id ASC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(Uuid::from_u128(stream.0))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Review a PENDING appeal: set its status to `approved` / `denied`, stamping the
    /// reviewer + decision. On approval, ALSO lift the ban via the existing unban path
    /// ([`StreamModRepo::unban`]). Returns `true` if THIS call reviewed a pending
    /// appeal (idempotent: `false` if the appeal is gone or already reviewed).
    ///
    /// On approval the appeal row is reviewed FIRST, then the ban is removed; there is no
    /// FK between the two tables, so the `approved` appeal persists as a historical record
    /// after the ban is lifted. Wrapped in one transaction so the decision + unban are
    /// atomic.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the transaction.
    pub async fn review(
        &self,
        appeal: BanAppealId,
        reviewer: ParticipantId,
        approved: bool,
        reason: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        let status = if approved { "approved" } else { "denied" };
        let mut tx = self.pool.begin().await?;

        // Stamp the decision on a still-pending appeal, returning the (stream,
        // appellant) so we can unban without a second round-trip. RETURNING also tells
        // us whether THIS call did the review.
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

        let Some((stream_uuid, appellant_uuid)) = stamped else {
            // Already reviewed / unknown — nothing to do.
            return Ok(false);
        };

        if approved {
            // Lift the ban (the existing unban DELETE), inside the same tx. There is no
            // FK between `ban_appeals` and `stream_bans`, so the just-stamped 'approved'
            // appeal persists as a historical record after the ban row is removed.
            sqlx::query(
                r"DELETE FROM stream_bans WHERE stream_id = $1 AND participant_id = $2",
            )
            .bind(stream_uuid)
            .bind(appellant_uuid)
            .execute(&mut *tx)
            .await?;
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
            .bind(format!("appeal-{label}-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn ban_appeal_only_active_ban_appealable() {
        let p = pool();
        let mods = StreamModRepo::new(p.clone());
        let repo = BanAppealRepo::new(p.clone());
        let stream = Ulid::new();
        let viewer = participant(&p, "viewer").await;
        let owner = participant(&p, "owner").await;
        let now = OffsetDateTime::now_utc();

        // No ban yet => cannot appeal.
        let err = repo
            .submit_appeal(stream, viewer, "let me back", now)
            .await
            .unwrap_err();
        assert!(matches!(err, AppealError::NotBanned));

        // Ban the viewer (permanent) => appeal succeeds.
        mods.ban(stream, viewer, owner, Some("spam"), None).await.unwrap();
        let appeal = repo
            .submit_appeal(stream, viewer, "I was joking", now)
            .await
            .unwrap();
        let got = repo.get(appeal).await.unwrap().unwrap();
        assert_eq!(got.status, "pending");
        assert_eq!(got.appellant_id, viewer);
        assert!(repo.list_pending(stream).await.unwrap().iter().any(|a| a.id == appeal));
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn ban_appeal_approve_triggers_unban() {
        let p = pool();
        let mods = StreamModRepo::new(p.clone());
        let repo = BanAppealRepo::new(p.clone());
        let stream = Ulid::new();
        let viewer = participant(&p, "viewer").await;
        let owner = participant(&p, "owner").await;
        let now = OffsetDateTime::now_utc();

        mods.ban(stream, viewer, owner, None, None).await.unwrap();
        let appeal = repo.submit_appeal(stream, viewer, "sorry", now).await.unwrap();

        // Approve => the viewer is unbanned and the appeal stamped approved.
        assert!(repo.review(appeal, owner, true, Some("first offense")).await.unwrap());
        assert!(
            !mods.is_banned(stream, viewer, now).await.unwrap(),
            "approving an appeal lifts the ban"
        );
        let got = repo.get(appeal).await.unwrap().unwrap();
        assert_eq!(got.status, "approved");
        assert_eq!(got.reviewed_by, Some(owner));
        assert_eq!(got.decision_reason.as_deref(), Some("first offense"));

        // Re-reviewing is a no-op.
        assert!(!repo.review(appeal, owner, false, None).await.unwrap());
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn ban_appeal_deny_keeps_ban() {
        let p = pool();
        let mods = StreamModRepo::new(p.clone());
        let repo = BanAppealRepo::new(p.clone());
        let stream = Ulid::new();
        let viewer = participant(&p, "viewer").await;
        let owner = participant(&p, "owner").await;
        let now = OffsetDateTime::now_utc();

        mods.ban(stream, viewer, owner, Some("harassment"), None).await.unwrap();
        let appeal = repo.submit_appeal(stream, viewer, "not me", now).await.unwrap();

        // Deny => the ban stands, the appeal is denied.
        assert!(repo.review(appeal, owner, false, Some("evidence stands")).await.unwrap());
        assert!(
            mods.is_banned(stream, viewer, now).await.unwrap(),
            "denying an appeal keeps the ban"
        );
        let got = repo.get(appeal).await.unwrap().unwrap();
        assert_eq!(got.status, "denied");
    }
}
