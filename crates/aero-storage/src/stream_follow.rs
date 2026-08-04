//! Stream / creator-follow repository (per-user).
//!
//! Backs `migrations/0039_stream_follows.sql`. A participant follows a creator
//! (another participant), then unfollows or lists who they follow / who follows
//! them. Each follow is a single `(follower_id, streamer_id)` pair — the composite
//! primary key makes following idempotent and needs no surrogate id, so this repo
//! carries no model struct: it returns the bare [`ParticipantId`]s on each side of
//! the relation.
//!
//! [`StreamFollowRepo::followers`] is the notify fan-out set: when a creator goes
//! live, the orchestrator calls it to find every follower to alert. A self-follow
//! (`follower == streamer`) is the CALLER's concern — the HTTP layer rejects it;
//! this repo stores opaque participant uuids with no FK. Purely additive: a NEW
//! [`StreamFollowRepo`]; no existing repo is touched.

use aero_common::ParticipantId;
use sqlx::PgPool;

/// Repository over the `stream_follows` table (per-user creator follows).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`StreamFollowRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct StreamFollowRepo {
    pool: PgPool,
}

impl StreamFollowRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record that `follower` follows `streamer`. Idempotent: re-following an
    /// already-followed creator is a no-op (`ON CONFLICT DO NOTHING`). A
    /// self-follow (`follower == streamer`) is the caller's concern — the HTTP
    /// layer rejects it before reaching here.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn follow(
        &self,
        follower: ParticipantId,
        streamer: ParticipantId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO stream_follows (follower_id, streamer_id)
               VALUES ($1, $2)
               ON CONFLICT (follower_id, streamer_id) DO NOTHING",
        )
        .bind(follower.to_uuid())
        .bind(streamer.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Stop `follower` following `streamer`. Returns `true` iff a row was removed —
    /// so a second unfollow (or unfollowing a creator that was never followed) is a
    /// no-op returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn unfollow(
        &self,
        follower: ParticipantId,
        streamer: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result =
            sqlx::query("DELETE FROM stream_follows WHERE follower_id = $1 AND streamer_id = $2")
                .bind(follower.to_uuid())
                .bind(streamer.to_uuid())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    /// List the participants who follow `streamer` (the notify fan-out set), newest
    /// first. The orchestrator calls this on go-live to alert each follower.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn followers(
        &self,
        streamer: ParticipantId,
    ) -> Result<Vec<ParticipantId>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT follower_id
               FROM stream_follows
              WHERE streamer_id = $1
              ORDER BY created_at DESC, follower_id DESC",
        )
        .bind(streamer.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(r,)| ParticipantId::from_uuid(r))
            .collect())
    }

    /// List the creators `follower` follows, newest first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn following(
        &self,
        follower: ParticipantId,
    ) -> Result<Vec<ParticipantId>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT streamer_id
               FROM stream_follows
              WHERE follower_id = $1
              ORDER BY created_at DESC, streamer_id DESC",
        )
        .bind(follower.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(r,)| ParticipantId::from_uuid(r))
            .collect())
    }

    /// Whether `follower` currently follows `streamer`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn is_following(
        &self,
        follower: ParticipantId,
        streamer: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (i32,)>(
            r"SELECT 1 FROM stream_follows WHERE follower_id = $1 AND streamer_id = $2",
        )
        .bind(follower.to_uuid())
        .bind(streamer.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored stream_follow
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

    /// Create a throwaway participant so the test is self-contained. Follows store
    /// opaque participant uuids (no FK), but seeding real participants keeps the
    /// rows well-formed and the test independent of fixture data.
    async fn mk_participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("stream-follow-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn stream_follow_follow_lists_is_following_unfollow() {
        let p = pool();
        let repo = StreamFollowRepo::new(p.clone());
        let follower = mk_participant(&p).await;
        let streamer = mk_participant(&p).await;

        // Not following yet.
        assert!(
            !repo.is_following(follower, streamer).await.unwrap(),
            "not following before follow"
        );

        // follow → idempotent; followers/following reflect it; is_following true.
        repo.follow(follower, streamer).await.unwrap();
        repo.follow(follower, streamer).await.unwrap(); // idempotent (ON CONFLICT DO NOTHING)
        assert!(
            repo.is_following(follower, streamer).await.unwrap(),
            "following after follow"
        );
        assert!(
            repo.followers(streamer).await.unwrap().contains(&follower),
            "follower appears in the streamer's followers (notify fan-out set)"
        );
        assert!(
            repo.following(follower).await.unwrap().contains(&streamer),
            "streamer appears in the follower's following list"
        );

        // unfollow → true once, then false/empty; is_following false.
        assert!(
            repo.unfollow(follower, streamer).await.unwrap(),
            "unfollow removes the row"
        );
        assert!(
            !repo.unfollow(follower, streamer).await.unwrap(),
            "second unfollow is a no-op"
        );
        assert!(
            !repo.is_following(follower, streamer).await.unwrap(),
            "no longer following after unfollow"
        );
        assert!(
            !repo.followers(streamer).await.unwrap().contains(&follower),
            "follower gone from the streamer's followers"
        );
        assert!(
            !repo.following(follower).await.unwrap().contains(&streamer),
            "streamer gone from the follower's following list"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM stream_follows WHERE follower_id = $1 OR streamer_id = $1")
            .bind(follower.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
