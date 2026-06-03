//! Live-stream chat moderation repository (bans / timeouts).
//!
//! Backs `migrations/0026_stream_moderation.sql`. A stream owner bans (permanent)
//! or times-out (until an expiry) a viewer from posting in that stream's danmaku
//! chat; a banned/timed-out viewer's chat posts are rejected until the ban is
//! lifted ([`unban`](StreamModRepo::unban)) or the timeout expires.
//!
//! Purely additive: a NEW [`StreamModRepo`]; no existing repo is touched. Stream
//! ids are [`Ulid`]s stored as UUID (same as the `streams` table), bound via
//! `uuid::Uuid::from_u128(ulid.0)` to match [`StreamRepo`](crate::StreamRepo).

use aero_common::ParticipantId;
use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use ulid::Ulid;
use uuid::Uuid;

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

    /// Ban (or re-ban) a viewer from a stream's chat. Upserts on
    /// `(stream_id, participant_id)`, so re-banning refreshes the reason/expiry.
    /// `until = None` is a permanent ban; `Some(t)` a timeout expiring at `t`.
    pub async fn ban(
        &self,
        stream: Ulid,
        participant: ParticipantId,
        banned_by: ParticipantId,
        reason: Option<&str>,
        until: Option<OffsetDateTime>,
    ) -> Result<(), sqlx::Error> {
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
        .bind(banned_by.to_uuid())
        .bind(reason)
        .bind(until)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Lift a ban. Returns `true` if a row was removed (idempotent: `false` when
    /// the viewer was not banned).
    pub async fn unban(
        &self,
        stream: Ulid,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let res = sqlx::query(
            r"DELETE FROM stream_bans WHERE stream_id = $1 AND participant_id = $2",
        )
        .bind(Uuid::from_u128(stream.0))
        .bind(participant.to_uuid())
        .execute(&self.pool)
        .await?;
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

    /// All bans recorded for a stream (active and expired-timeout rows), newest
    /// first. Always filtered to `stream`.
    pub async fn list_bans(&self, stream: Ulid) -> Result<Vec<StreamBan>, sqlx::Error> {
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
        .fetch_all(&self.pool)
        .await?;

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
        assert!(ban_active(None, now), "a permanent ban (None) is always active");
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
    use time::Duration;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    // Create a throwaway viewer + moderator so the test is self-contained. The
    // stream id is a fresh ULID (no `streams` FK on `stream_bans.stream_id`).
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
        (Ulid::new(), viewer, owner)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn stream_mod_permanent_ban_is_banned() {
        let p = pool();
        let repo = StreamModRepo::new(p.clone());
        let (stream, viewer, owner) = fixture(&p).await;

        repo.ban(stream, viewer, owner, Some("spam"), None)
            .await
            .unwrap();
        let now = OffsetDateTime::now_utc();
        assert!(
            repo.is_banned(stream, viewer, now).await.unwrap(),
            "a permanent ban is active"
        );

        let bans = repo.list_bans(stream).await.unwrap();
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
        repo.ban(stream, viewer, owner, None, Some(now - Duration::hours(1)))
            .await
            .unwrap();
        assert!(
            !repo.is_banned(stream, viewer, now).await.unwrap(),
            "an expired timeout is not an active ban"
        );
        // The row still exists for the audit/list view.
        assert_eq!(repo.list_bans(stream).await.unwrap().len(), 1);
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn stream_mod_unban_clears() {
        let p = pool();
        let repo = StreamModRepo::new(p.clone());
        let (stream, viewer, owner) = fixture(&p).await;

        repo.ban(stream, viewer, owner, None, None).await.unwrap();
        let now = OffsetDateTime::now_utc();
        assert!(repo.is_banned(stream, viewer, now).await.unwrap());

        assert!(repo.unban(stream, viewer).await.unwrap(), "row removed");
        assert!(
            !repo.is_banned(stream, viewer, now).await.unwrap(),
            "after unban the viewer is no longer banned"
        );
        assert!(
            !repo.unban(stream, viewer).await.unwrap(),
            "unban is idempotent: nothing to remove the second time"
        );
    }
}
