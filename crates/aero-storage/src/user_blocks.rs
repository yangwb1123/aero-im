//! User-level block / ignore repository.
//!
//! Backs `migrations/0106_user_blocks.sql`. When user A blocks user B:
//!   1. B's messages are filtered so A does NOT receive notifications from B
//!      (`ImService::dispatch_notifications` calls [`BlockRepo::blockers_of`] to
//!      drop A from the target set when B sends a message).
//!   2. A cannot open a DM with B — the HTTP `open_dm` handler checks both
//!      directions via [`BlockRepo::is_blocked`] before creating/returning the room.
//!
//! Purely additive: a NEW [`BlockRepo`]; no existing repo is touched.

use aero_common::{Error, ParticipantId};
use sqlx::{PgPool, Postgres, Transaction};

/// Serialize every state transition and gated conversation start for an
/// unordered participant pair.
///
/// Callers that also lock a workspace must acquire that workspace first. The
/// block/unblock paths own only this advisory lock, so the order cannot cycle.
pub(crate) async fn lock_user_block_pair(
    tx: &mut Transaction<'_, Postgres>,
    first: ParticipantId,
    second: ParticipantId,
) -> Result<(), sqlx::Error> {
    let (low, high) = if first.to_uuid() <= second.to_uuid() {
        (first, second)
    } else {
        (second, first)
    };
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("aero:user-block:{low}:{high}"))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Repository for user-level blocking.
#[derive(Clone)]
pub struct BlockRepo {
    pub pg: PgPool,
}

impl BlockRepo {
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Block `target` from the perspective of `blocker`. Idempotent: re-blocking
    /// is a no-op (the existing row is kept).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn block(&self, blocker: ParticipantId, target: ParticipantId) -> Result<(), Error> {
        let mut tx = self.pg.begin().await.map_err(Error::from)?;
        lock_user_block_pair(&mut tx, blocker, target)
            .await
            .map_err(Error::from)?;
        sqlx::query("INSERT INTO user_blocks (blocker_id, blocked_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(blocker.to_uuid())
            .bind(target.to_uuid())
            .execute(&mut *tx)
            .await
            .map_err(Error::from)?;
        tx.commit().await.map_err(Error::from)?;
        Ok(())
    }

    /// Unblock `target` for `blocker`. Idempotent: unblocking a participant who
    /// was never blocked is a no-op.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn unblock(
        &self,
        blocker: ParticipantId,
        target: ParticipantId,
    ) -> Result<(), Error> {
        let mut tx = self.pg.begin().await.map_err(Error::from)?;
        lock_user_block_pair(&mut tx, blocker, target)
            .await
            .map_err(Error::from)?;
        sqlx::query("DELETE FROM user_blocks WHERE blocker_id = $1 AND blocked_id = $2")
            .bind(blocker.to_uuid())
            .bind(target.to_uuid())
            .execute(&mut *tx)
            .await
            .map_err(Error::from)?;
        tx.commit().await.map_err(Error::from)?;
        Ok(())
    }

    /// Whether `blocker` has blocked `target`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn is_blocked(
        &self,
        blocker: ParticipantId,
        target: ParticipantId,
    ) -> Result<bool, Error> {
        let (exists,): (bool,) = sqlx::query_as(
            "SELECT EXISTS(SELECT 1 FROM user_blocks WHERE blocker_id = $1 AND blocked_id = $2)",
        )
        .bind(blocker.to_uuid())
        .bind(target.to_uuid())
        .fetch_one(&self.pg)
        .await
        .map_err(Error::from)?;
        Ok(exists)
    }

    /// All participants that `blocker` has blocked, newest block first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn blocks_for(&self, blocker: ParticipantId) -> Result<Vec<ParticipantId>, Error> {
        let rows: Vec<(uuid::Uuid,)> = sqlx::query_as(
            "SELECT blocked_id FROM user_blocks WHERE blocker_id = $1 ORDER BY created_at DESC",
        )
        .bind(blocker.to_uuid())
        .fetch_all(&self.pg)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .into_iter()
            .map(|(id,)| ParticipantId::from_uuid(id))
            .collect())
    }

    /// All participants who have blocked `blocked` — used by
    /// `ImService::dispatch_notifications` to remove `blocked`'s message
    /// recipients who have blocked them.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn blockers_of(&self, blocked: ParticipantId) -> Result<Vec<ParticipantId>, Error> {
        let rows: Vec<(uuid::Uuid,)> =
            sqlx::query_as("SELECT blocker_id FROM user_blocks WHERE blocked_id = $1")
                .bind(blocked.to_uuid())
                .fetch_all(&self.pg)
                .await
                .map_err(Error::from)?;
        Ok(rows
            .into_iter()
            .map(|(id,)| ParticipantId::from_uuid(id))
            .collect())
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::ParticipantId;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Seed a minimal `participants` row so FK constraints are satisfied.
    async fn seed_participant(pg: &PgPool, id: ParticipantId) {
        sqlx::query(
            "INSERT INTO participants (id, kind, display_name) \
             VALUES ($1, 'human', $2) ON CONFLICT DO NOTHING",
        )
        .bind(id.to_uuid())
        .bind(format!("User {}", id.to_uuid()))
        .execute(pg)
        .await
        .expect("seed participant");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn block_unblock_is_blocked_roundtrip() {
        let pg = pool();
        let repo = BlockRepo::new(pg.clone());

        let alice = ParticipantId::new();
        let bob = ParticipantId::new();
        seed_participant(&pg, alice).await;
        seed_participant(&pg, bob).await;

        // Initially not blocked in either direction.
        assert!(!repo.is_blocked(alice, bob).await.unwrap());
        assert!(!repo.is_blocked(bob, alice).await.unwrap());

        // Alice blocks Bob.
        repo.block(alice, bob).await.unwrap();
        // Idempotent.
        repo.block(alice, bob).await.unwrap();
        assert!(
            repo.is_blocked(alice, bob).await.unwrap(),
            "alice blocked bob"
        );
        assert!(!repo.is_blocked(bob, alice).await.unwrap(), "not symmetric");

        // blocks_for and blockers_of.
        let blocked_by_alice = repo.blocks_for(alice).await.unwrap();
        assert!(blocked_by_alice.contains(&bob));

        let blockers_of_bob = repo.blockers_of(bob).await.unwrap();
        assert!(blockers_of_bob.contains(&alice));

        // Unblock.
        repo.unblock(alice, bob).await.unwrap();
        // Idempotent.
        repo.unblock(alice, bob).await.unwrap();
        assert!(!repo.is_blocked(alice, bob).await.unwrap(), "unblocked");
        assert!(!repo.blocks_for(alice).await.unwrap().contains(&bob));
    }
}
