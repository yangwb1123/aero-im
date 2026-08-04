//! Live-stream chat moderation repository (bans / timeouts).
//!
//! Backs `migrations/0026_stream_moderation.sql`. A stream owner or current
//! moderator bans (permanent) or times-out (until an expiry) a viewer from
//! posting in that stream's danmaku chat; a banned/timed-out viewer's chat posts
//! are rejected until the ban is lifted or the timeout expires.
//!
//! Purely additive: a NEW [`StreamModRepo`]; no existing repo is touched. Stream
//! ids are [`Ulid`]s stored as UUID (same as the `streams` table), bound via
//! `uuid::Uuid::from_u128(ulid.0)` to match [`StreamRepo`](crate::StreamRepo).

use aero_common::{Error as AeroError, ParticipantId};
use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use ulid::Ulid;
use uuid::Uuid;

use crate::stream_moderator::{
    lock_stream_authority_in_tx, set_live_governance_actor, RequiredStreamAuthority,
};

/// Maximum stored moderation reason length, measured in Unicode scalar values.
pub const MAX_BAN_REASON_CHARS: usize = 500;

/// One active or recorded ban/timeout on a stream's chat.
#[derive(Debug, Clone, Serialize)]
pub struct StreamBan {
    pub stream_id: Ulid,
    pub participant_id: ParticipantId,
    /// The moderator (stream owner) who issued the ban, if recorded.
    pub banned_by: Option<ParticipantId>,
    pub reason: Option<String>,
    /// `None` is a permanent ban; `Some(t)` is a timeout active while `t > now`.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub until: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Whether a ban with the given `until` is active at `now`.
///
/// `None` is a permanent ban (always active). `Some(t)` is a timeout that is
/// active only while it has not yet expired (`t > now`). Pure so it unit-tests
/// without a DB.
#[must_use]
pub fn ban_active(until: Option<OffsetDateTime>, now: OffsetDateTime) -> bool {
    match until {
        None => true,
        Some(t) => t > now,
    }
}

#[derive(Clone)]
pub struct StreamModRepo {
    pool: PgPool,
}

impl StreamModRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Ban (or re-ban) a viewer while atomically proving `actor` is the current
    /// stream owner/moderator. The canonical stream lock serializes this with a
    /// concurrent moderator revocation.
    pub async fn ban_authorized(
        &self,
        stream: Ulid,
        participant: ParticipantId,
        actor: ParticipantId,
        reason: Option<&str>,
        until: Option<OffsetDateTime>,
    ) -> Result<(), AeroError> {
        let reason = reason.map(str::trim).filter(|value| !value.is_empty());
        if reason.is_some_and(|value| value.chars().count() > MAX_BAN_REASON_CHARS) {
            return Err(AeroError::Invalid(format!(
                "ban reason exceeds {MAX_BAN_REASON_CHARS} characters"
            )));
        }

        let mut tx = self.pool.begin().await?;
        lock_stream_authority_in_tx(
            &mut tx,
            stream,
            actor,
            RequiredStreamAuthority::OwnerOrModerator,
        )
        .await?;
        let target_active = sqlx::query_scalar::<_, bool>(
            "SELECT deleted_at IS NULL FROM participants WHERE id = $1 FOR SHARE",
        )
        .bind(participant.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(false);
        if !target_active {
            return Err(AeroError::NotFound(format!("participant {participant}")));
        }
        set_live_governance_actor(&mut tx, actor).await?;

        sqlx::query(
            r"INSERT INTO stream_bans (stream_id, participant_id, banned_by, reason, until)
               VALUES ($1, $2, $3, $4, $5)
               ON CONFLICT (stream_id, participant_id)
               DO UPDATE SET banned_by  = EXCLUDED.banned_by,
                             reason      = EXCLUDED.reason,
                             until       = EXCLUDED.until,
                             created_at  = now()",
        )
        .bind(Uuid::from_u128(stream.0))
        .bind(participant.to_uuid())
        .bind(actor.to_uuid())
        .bind(reason)
        .bind(until)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Compatibility spelling for internal workflows and older callers. This
    /// delegates to the actor-aware transaction; `banned_by` is the actor whose
    /// current authority is proved, not trusted audit metadata.
    pub async fn ban(
        &self,
        stream: Ulid,
        participant: ParticipantId,
        banned_by: ParticipantId,
        reason: Option<&str>,
        until: Option<OffsetDateTime>,
    ) -> Result<(), AeroError> {
        self.ban_authorized(stream, participant, banned_by, reason, until)
            .await
    }

    /// Lift a ban after atomically proving `actor` still owns/moderates the
    /// stream. Returns `false` when no row existed.
    pub async fn unban_authorized(
        &self,
        stream: Ulid,
        participant: ParticipantId,
        actor: ParticipantId,
    ) -> Result<bool, AeroError> {
        let mut tx = self.pool.begin().await?;
        lock_stream_authority_in_tx(
            &mut tx,
            stream,
            actor,
            RequiredStreamAuthority::OwnerOrModerator,
        )
        .await?;
        set_live_governance_actor(&mut tx, actor).await?;
        let res =
            sqlx::query(r"DELETE FROM stream_bans WHERE stream_id = $1 AND participant_id = $2")
                .bind(Uuid::from_u128(stream.0))
                .bind(participant.to_uuid())
                .execute(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(res.rows_affected() > 0)
    }

    /// Whether `participant` is currently banned from `stream`'s chat at `now`:
    /// a row exists AND it is still active (`until IS NULL OR until > now`).
    pub async fn is_banned(
        &self,
        stream: Ulid,
        participant: ParticipantId,
        now: OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        let banned = sqlx::query_scalar::<_, bool>(
            r"SELECT EXISTS(
                SELECT 1 FROM stream_bans
                 WHERE stream_id = $1
                   AND participant_id = $2
                   AND (until IS NULL OR until > $3)
              )",
        )
        .bind(Uuid::from_u128(stream.0))
        .bind(participant.to_uuid())
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        Ok(banned)
    }

    /// Sweep hard-delete ban rows whose timeout (`until`) has passed. Permanent bans
    /// (`until IS NULL`) and still-active timeouts are left untouched. Returns the
    /// number of rows removed.
    ///
    /// NOTE: keys on `until` — the column `ban()` actually writes (and `is_banned`
    /// reads), NOT the `expires_at` column migration 0108 added. `ban()` never wrote
    /// `expires_at`, so the original `WHERE expires_at < NOW()` matched zero rows and
    /// expired timeouts were never garbage-collected (a dead sweep).
    pub async fn sweep_expired_bans(&self) -> Result<u64, sqlx::Error> {
        let r = sqlx::query("DELETE FROM stream_bans WHERE until IS NOT NULL AND until < NOW()")
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }

    /// All bans recorded for a stream, visible only to its current
    /// owner/moderators. Authorization and the read share one transaction.
    pub async fn list_bans_authorized(
        &self,
        stream: Ulid,
        actor: ParticipantId,
    ) -> Result<Vec<StreamBan>, AeroError> {
        let mut tx = self.pool.begin().await?;
        lock_stream_authority_in_tx(
            &mut tx,
            stream,
            actor,
            RequiredStreamAuthority::OwnerOrModerator,
        )
        .await?;
        let rows = sqlx::query_as::<
            _,
            (
                Uuid,
                Uuid,
                Option<Uuid>,
                Option<String>,
                Option<OffsetDateTime>,
                OffsetDateTime,
            ),
        >(
            r"SELECT stream_id, participant_id, banned_by, reason, until, created_at
               FROM stream_bans
               WHERE stream_id = $1
               ORDER BY created_at DESC",
        )
        .bind(Uuid::from_u128(stream.0))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(rows
            .into_iter()
            .map(
                |(stream_id, participant_id, banned_by, reason, until, created_at)| StreamBan {
                    stream_id: Ulid(stream_id.as_u128()),
                    participant_id: ParticipantId::from_uuid(participant_id),
                    banned_by: banned_by.map(ParticipantId::from_uuid),
                    reason,
                    until,
                    created_at,
                },
            )
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    #[test]
    fn ban_active_permanent_is_always_active() {
        let now = OffsetDateTime::now_utc();
        assert!(
            ban_active(None, now),
            "a permanent ban (None) is always active"
        );
        // Independent of the reference instant.
        assert!(ban_active(None, now + Duration::days(365)));
        assert!(ban_active(None, now - Duration::days(365)));
    }

    #[test]
    fn ban_active_timeout_active_only_until_expiry() {
        let now = OffsetDateTime::now_utc();
        // Future expiry => still timed out (active).
        assert!(ban_active(Some(now + Duration::seconds(1)), now));
        assert!(ban_active(Some(now + Duration::hours(1)), now));
        // Past expiry => no longer active.
        assert!(!ban_active(Some(now - Duration::seconds(1)), now));
        assert!(!ban_active(Some(now - Duration::hours(1)), now));
    }

    #[test]
    fn ban_active_timeout_boundary_is_exclusive() {
        // until == now is NOT active (strictly `until > now`): the timeout has
        // elapsed at exactly its expiry instant.
        let now = OffsetDateTime::now_utc();
        assert!(!ban_active(Some(now), now));
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored stream_mod_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::StreamModeratorRepo;
    use sqlx::postgres::PgConnectOptions;
    use time::Duration;

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
            .expect("connect tagged moderation test pool")
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
        panic!("tagged moderation transaction never reached its expected lock wait");
    }

    // Create a throwaway viewer + owner and their canonical roomless stream.
    async fn fixture(repo_pool: &PgPool) -> (Ulid, ParticipantId, ParticipantId) {
        let viewer = ParticipantId::new();
        let owner = ParticipantId::new();
        for (p, label) in [(viewer, "viewer"), (owner, "owner")] {
            sqlx::query(
                "INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)",
            )
            .bind(p.to_uuid())
            .bind(format!("stream-mod-{label}-{p}"))
            .execute(repo_pool)
            .await
            .expect("insert participant");
        }
        let stream = Ulid::new();
        sqlx::query(
            r"INSERT INTO streams
                    (id, owner_id, title, stream_key, status, protocol)
              VALUES ($1, $2, $3, $4, 'idle', 'rtmp')",
        )
        .bind(Uuid::from_u128(stream.0))
        .bind(owner.to_uuid())
        .bind(format!("stream-mod-{stream}"))
        .bind(format!("stream-mod-key-{stream}"))
        .execute(repo_pool)
        .await
        .expect("insert stream");
        (stream, viewer, owner)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn stream_mod_permanent_ban_is_banned() {
        let p = pool();
        let repo = StreamModRepo::new(p.clone());
        let (stream, viewer, owner) = fixture(&p).await;

        repo.ban_authorized(stream, viewer, owner, Some("spam"), None)
            .await
            .unwrap();
        let now = OffsetDateTime::now_utc();
        assert!(
            repo.is_banned(stream, viewer, now).await.unwrap(),
            "a permanent ban is active"
        );

        let bans = repo.list_bans_authorized(stream, owner).await.unwrap();
        assert_eq!(bans.len(), 1);
        assert_eq!(bans[0].participant_id, viewer);
        assert_eq!(bans[0].reason.as_deref(), Some("spam"));
        assert!(bans[0].until.is_none(), "permanent ban has no expiry");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn stream_mod_expired_timeout_not_banned() {
        let p = pool();
        let repo = StreamModRepo::new(p.clone());
        let (stream, viewer, owner) = fixture(&p).await;

        let now = OffsetDateTime::now_utc();
        // Timeout expired an hour ago.
        repo.ban_authorized(stream, viewer, owner, None, Some(now - Duration::hours(1)))
            .await
            .unwrap();
        assert!(
            !repo.is_banned(stream, viewer, now).await.unwrap(),
            "an expired timeout is not an active ban"
        );
        // The row still exists for the audit/list view.
        assert_eq!(
            repo.list_bans_authorized(stream, owner)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            repo.sweep_expired_bans().await.unwrap() >= 1,
            "the actorless system sweep may remove expired timeouts"
        );
        assert!(repo
            .list_bans_authorized(stream, owner)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn stream_mod_unban_clears() {
        let p = pool();
        let repo = StreamModRepo::new(p.clone());
        let (stream, viewer, owner) = fixture(&p).await;

        repo.ban_authorized(stream, viewer, owner, None, None)
            .await
            .unwrap();
        let now = OffsetDateTime::now_utc();
        assert!(repo.is_banned(stream, viewer, now).await.unwrap());

        assert!(
            repo.unban_authorized(stream, viewer, owner).await.unwrap(),
            "row removed"
        );
        assert!(
            !repo.is_banned(stream, viewer, now).await.unwrap(),
            "after unban the viewer is no longer banned"
        );
        assert!(
            !repo.unban_authorized(stream, viewer, owner).await.unwrap(),
            "unban is idempotent: nothing to remove the second time"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn stream_mod_revocation_fences_inflight_moderation() {
        let p = pool();
        let repo = StreamModRepo::new(p.clone());
        let (stream, viewer, owner) = fixture(&p).await;
        let moderator = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(moderator.to_uuid())
            .bind(format!("stream-mod-race-{moderator}"))
            .execute(&p)
            .await
            .unwrap();
        StreamModeratorRepo::new(p.clone())
            .add_authorized(stream, moderator, owner)
            .await
            .unwrap();

        let mut revocation = p.begin().await.unwrap();
        sqlx::query("SELECT id FROM streams WHERE id = $1 FOR UPDATE")
            .bind(Uuid::from_u128(stream.0))
            .execute(&mut *revocation)
            .await
            .unwrap();
        crate::stream_moderator::set_live_governance_actor(&mut revocation, owner)
            .await
            .unwrap();
        sqlx::query("DELETE FROM stream_moderators WHERE stream_id = $1 AND participant_id = $2")
            .bind(Uuid::from_u128(stream.0))
            .bind(moderator.to_uuid())
            .execute(&mut *revocation)
            .await
            .unwrap();

        let application_name = format!("stream-mod-revocation-race-{moderator}");
        let raced_repo = StreamModRepo::new(tagged_pool(&application_name).await);
        let raced = tokio::spawn(async move {
            raced_repo
                .ban_authorized(stream, viewer, moderator, Some("raced"), None)
                .await
        });
        wait_until_tagged_query_waits_on_lock(&p, &application_name).await;
        revocation.commit().await.unwrap();

        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(3), raced)
                .await
                .expect("moderation race completed")
                .expect("moderation task"),
            Err(AeroError::Forbidden(_))
        ));
        assert!(
            !repo
                .is_banned(stream, viewer, OffsetDateTime::now_utc())
                .await
                .unwrap(),
            "a revoked moderator cannot commit a ban after revocation"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn stream_mod_raw_sql_cannot_forge_authority_or_identity() {
        let p = pool();
        let repo = StreamModRepo::new(p.clone());
        let (stream, viewer, owner) = fixture(&p).await;
        let outsider = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(outsider.to_uuid())
            .bind(format!("stream-mod-raw-outsider-{outsider}"))
            .execute(&p)
            .await
            .unwrap();

        let missing_context = sqlx::query(
            r"INSERT INTO stream_bans
                    (stream_id, participant_id, banned_by, reason)
              VALUES ($1, $2, $3, 'no-context')",
        )
        .bind(Uuid::from_u128(stream.0))
        .bind(viewer.to_uuid())
        .bind(owner.to_uuid())
        .execute(&p)
        .await;
        assert!(
            missing_context.is_err(),
            "copying the canonical owner UUID cannot replace actor context"
        );

        let mut legacy = p.begin().await.unwrap();
        crate::stream_moderator::set_live_governance_actor(&mut legacy, owner)
            .await
            .unwrap();
        let first_revision: i64 = sqlx::query_scalar(
            r"INSERT INTO stream_bans
                    (stream_id, participant_id, banned_by, reason)
              VALUES ($1, $2, $3, 'legacy-a')
              RETURNING ban_revision",
        )
        .bind(Uuid::from_u128(stream.0))
        .bind(viewer.to_uuid())
        .bind(owner.to_uuid())
        .fetch_one(&mut *legacy)
        .await
        .expect("a 0219 writer remains compatible after revision rollout");
        let second_revision: i64 = sqlx::query_scalar(
            r"INSERT INTO stream_bans
                    (stream_id, participant_id, banned_by, reason)
              VALUES ($1, $2, $3, 'legacy-b')
              ON CONFLICT (stream_id, participant_id)
              DO UPDATE SET banned_by = EXCLUDED.banned_by,
                            reason = EXCLUDED.reason,
                            created_at = now()
              RETURNING ban_revision",
        )
        .bind(Uuid::from_u128(stream.0))
        .bind(viewer.to_uuid())
        .bind(owner.to_uuid())
        .fetch_one(&mut *legacy)
        .await
        .expect("old ON CONFLICT re-ban remains compatible");
        assert_ne!(
            first_revision, second_revision,
            "every old-writer re-ban receives a new incarnation revision"
        );
        legacy.commit().await.unwrap();

        let raw_unban =
            sqlx::query("DELETE FROM stream_bans WHERE stream_id = $1 AND participant_id = $2")
                .bind(Uuid::from_u128(stream.0))
                .bind(viewer.to_uuid())
                .execute(&p)
                .await;
        assert!(
            raw_unban.is_err(),
            "an active ban cannot be deleted without actor context"
        );

        let mut forged = p.begin().await.unwrap();
        crate::stream_moderator::set_live_governance_actor(&mut forged, outsider)
            .await
            .unwrap();
        let forged_authority = sqlx::query(
            r"INSERT INTO stream_bans
                    (stream_id, participant_id, banned_by, reason)
              VALUES ($1, $2, $3, 'forged')",
        )
        .bind(Uuid::from_u128(stream.0))
        .bind(viewer.to_uuid())
        .bind(outsider.to_uuid())
        .execute(&mut *forged)
        .await;
        assert!(
            forged_authority.is_err(),
            "an authenticated outsider is not owner/mod authority"
        );
        forged.rollback().await.unwrap();

        repo.ban_authorized(stream, viewer, owner, None, None)
            .await
            .unwrap();
        let tamper = sqlx::query(
            r"UPDATE stream_bans
                  SET participant_id = $1
                WHERE stream_id = $2
                  AND participant_id = $3",
        )
        .bind(outsider.to_uuid())
        .bind(Uuid::from_u128(stream.0))
        .bind(viewer.to_uuid())
        .execute(&p)
        .await;
        assert!(tamper.is_err(), "persisted ban scope is immutable");

        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(viewer.to_uuid())
            .execute(&p)
            .await
            .expect("participant FK cascade may remove its active ban");
        assert!(!repo
            .is_banned(stream, viewer, OffsetDateTime::now_utc())
            .await
            .unwrap());
    }
}
