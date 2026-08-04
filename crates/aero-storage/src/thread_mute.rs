//! Per-user thread-MUTE repository (the inverse of thread-follow).
//!
//! Backs `migrations/0088_thread_mutes.sql`. A user mutes a thread — identified
//! by its root message — to STOP receiving reply notifications for it, even when
//! they would otherwise be in the reply fan-out (a thread subscriber via
//! `thread_subscriptions`, or the root author). Each mute is a single
//! `(participant_id, root_message_id)` pair — the composite primary key makes
//! muting idempotent and needs no surrogate id, so this repo carries no model
//! struct.
//!
//! This is the exact inverse of [`ThreadSubscriptionRepo`](crate::ThreadSubscriptionRepo):
//! a follow ADDS a recipient to a reply's notification fan-out, a mute SUBTRACTS
//! one. The dispatcher (`ImService::dispatch_notifications`) unions the room
//! members + thread subscribers for a reply, then removes the
//! [`muted_by`](ThreadMuteRepo::muted_by) set — muting suppresses only the
//! NOTIFICATION, never the room broadcast of the reply. Root message ids are
//! stored as opaque uuids (no FK to `messages`), mirroring
//! [`ThreadSubscriptionRepo`](crate::ThreadSubscriptionRepo). Purely additive: a
//! NEW [`ThreadMuteRepo`]; no existing repo is touched.

use std::collections::HashSet;

use aero_common::{Error, MessageId, ParticipantId};
use sqlx::PgPool;

use crate::thread_subscription::lock_effective_live_thread_root_in_tx;

/// Repository over the `thread_mutes` table (per-user thread mutes).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`ThreadMuteRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct ThreadMuteRepo {
    pool: PgPool,
}

impl ThreadMuteRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Mute the thread rooted at `root` for `participant`. Idempotent:
    /// re-muting an already-muted thread is a no-op (`ON CONFLICT DO NOTHING`).
    /// The caller is responsible for any room-access check.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    #[cfg(test)]
    pub(crate) async fn mute(
        &self,
        participant: ParticipantId,
        root: MessageId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO thread_mutes (participant_id, root_message_id)
               VALUES ($1, $2)
               ON CONFLICT (participant_id, root_message_id) DO NOTHING",
        )
        .bind(participant.to_uuid())
        .bind(root.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Unmute the thread rooted at `root` for `participant`. Returns `true` iff a
    /// row was removed — owner-scoped, so a caller can never remove another
    /// user's mute, and a second unmute (or unmuting a thread that was never
    /// muted) is a no-op returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    #[cfg(test)]
    pub(crate) async fn unmute(
        &self,
        participant: ParticipantId,
        root: MessageId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM thread_mutes WHERE participant_id = $1 AND root_message_id = $2",
        )
        .bind(participant.to_uuid())
        .bind(root.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// The set of participants who have MUTED the thread rooted at `root`, as a
    /// [`HashSet`] for O(1) membership tests. The dispatcher SUBTRACTS this from
    /// the reply fan-out (room members ∪ thread subscribers) so a muted thread
    /// stops notifying — never owner-scoped, by design it returns *all* muters.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn muted_by(&self, root: MessageId) -> Result<HashSet<ParticipantId>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT mute.participant_id
                FROM thread_mutes AS mute
                JOIN messages AS root
                  ON root.id = mute.root_message_id
                 AND root.reply_to IS NULL
                 AND root.deleted_at IS NULL
               WHERE mute.root_message_id = $1
                 AND aero_effective_room_access(
                         root.room_id,
                         mute.participant_id,
                         NULL
                     )",
        )
        .bind(root.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(p,)| ParticipantId::from_uuid(p))
            .collect())
    }

    /// Whether `participant` has muted the thread rooted at `root`. Owner-scoped,
    /// so it only ever reflects the caller's own mutes.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    #[cfg(test)]
    pub(crate) async fn is_muted(
        &self,
        participant: ParticipantId,
        root: MessageId,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (i32,)>(
            r"SELECT 1 FROM thread_mutes
              WHERE participant_id = $1 AND root_message_id = $2",
        )
        .bind(participant.to_uuid())
        .bind(root.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// Mute a live canonical thread under the caller's current room-access
    /// fence.
    ///
    /// # Errors
    /// Returns an opaque root/access error or a database error.
    pub async fn mute_authorized(
        &self,
        participant: ParticipantId,
        root: MessageId,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        lock_effective_live_thread_root_in_tx(&mut tx, participant, root).await?;
        sqlx::query(
            r"INSERT INTO thread_mutes (participant_id, root_message_id)
               VALUES ($1, $2)
               ON CONFLICT (participant_id, root_message_id) DO NOTHING",
        )
        .bind(participant.to_uuid())
        .bind(root.to_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Unmute a live canonical thread under the caller's current room-access
    /// fence.
    ///
    /// # Errors
    /// Returns an opaque root/access error or a database error.
    pub async fn unmute_authorized(
        &self,
        participant: ParticipantId,
        root: MessageId,
    ) -> Result<bool, Error> {
        let mut tx = self.pool.begin().await?;
        lock_effective_live_thread_root_in_tx(&mut tx, participant, root).await?;
        let result = sqlx::query(
            r"DELETE FROM thread_mutes
               WHERE participant_id = $1 AND root_message_id = $2",
        )
        .bind(participant.to_uuid())
        .bind(root.to_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }

    /// Check a mute under the caller's current room-access fence.
    ///
    /// # Errors
    /// Returns an opaque root/access error or a database error.
    pub async fn is_muted_authorized(
        &self,
        participant: ParticipantId,
        root: MessageId,
    ) -> Result<bool, Error> {
        let mut tx = self.pool.begin().await?;
        lock_effective_live_thread_root_in_tx(&mut tx, participant, root).await?;
        let muted: bool = sqlx::query_scalar(
            r"SELECT EXISTS(
                   SELECT 1
                     FROM thread_mutes
                    WHERE participant_id = $1 AND root_message_id = $2
               )",
        )
        .bind(participant.to_uuid())
        .bind(root.to_uuid())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(muted)
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored thread_mute
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    struct Fixture {
        owner: ParticipantId,
        other: ParticipantId,
        stranger: ParticipantId,
        root: MessageId,
    }

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn fixture(p: &PgPool) -> Fixture {
        let owner = ParticipantId::new();
        let other = ParticipantId::new();
        let stranger = ParticipantId::new();
        let workspace = uuid::Uuid::new_v4();
        let room = uuid::Uuid::new_v4();
        let root_message = MessageId::new();
        let mut tx = p.begin().await.expect("begin thread mute fixture");

        for participant in [owner, other, stranger] {
            sqlx::query(
                "INSERT INTO participants (id, kind, display_name)
                 VALUES ($1, 'human', $2)",
            )
            .bind(participant.to_uuid())
            .bind(format!("thread-mute-{participant}"))
            .execute(&mut *tx)
            .await
            .expect("insert participant");
        }
        sqlx::query(
            "INSERT INTO workspaces (id, name, slug, created_by)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(workspace)
        .bind(format!("Thread mute {workspace}"))
        .bind(format!("thread-mute-{workspace}"))
        .bind(owner.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert workspace");
        for (participant, role) in [(owner, "owner"), (other, "member")] {
            sqlx::query(
                "INSERT INTO workspace_members (workspace_id, participant_id, role)
                 VALUES ($1, $2, $3)",
            )
            .bind(workspace)
            .bind(participant.to_uuid())
            .bind(role)
            .execute(&mut *tx)
            .await
            .expect("insert workspace member");
        }
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id)
             VALUES ($1, 'group', $2, $3, $4)",
        )
        .bind(room)
        .bind(format!("Thread mute room {room}"))
        .bind(owner.to_uuid())
        .bind(workspace)
        .execute(&mut *tx)
        .await
        .expect("insert room");
        for (participant, role) in [(owner, "owner"), (other, "member")] {
            sqlx::query(
                "INSERT INTO room_members (room_id, participant_id, role)
                 VALUES ($1, $2, $3)",
            )
            .bind(room)
            .bind(participant.to_uuid())
            .bind(role)
            .execute(&mut *tx)
            .await
            .expect("insert room member");
        }
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks, searchable_text)
             VALUES ($1, $2, $3, '[]'::jsonb, 'thread mute root')",
        )
        .bind(root_message.to_uuid())
        .bind(room)
        .bind(owner.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert live root message");
        tx.commit().await.expect("commit thread mute fixture");

        Fixture {
            owner,
            other,
            stranger,
            root: root_message,
        }
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn thread_mute_mute_is_muted_muted_by_unmute_owner_scoped() {
        let p = pool();
        let repo = ThreadMuteRepo::new(p.clone());
        let Fixture {
            owner,
            other,
            stranger,
            root,
        } = fixture(&p).await;

        // Not muted yet.
        assert!(
            !repo.is_muted(owner, root).await.unwrap(),
            "thread is not muted before mute"
        );
        assert!(
            repo.muted_by(root).await.unwrap().is_empty(),
            "no muters before any mute"
        );

        // mute → idempotent; is_muted + muted_by reflect it.
        repo.mute(owner, root).await.unwrap();
        repo.mute(owner, root).await.unwrap(); // idempotent (ON CONFLICT DO NOTHING)
        repo.mute(other, root).await.unwrap();
        assert!(repo.is_muted(owner, root).await.unwrap(), "muted");
        let muters = repo.muted_by(root).await.unwrap();
        assert!(
            muters.contains(&owner) && muters.contains(&other),
            "both muters in the set"
        );

        // Owner-scoping: a stranger neither shows as muted nor can remove a mute.
        assert!(
            !repo.is_muted(stranger, root).await.unwrap(),
            "stranger does not see another user's mute"
        );
        assert!(
            !repo.unmute(stranger, root).await.unwrap(),
            "stranger cannot remove another user's mute"
        );

        // unmute → true once, then false; muted_by + is_muted drop it.
        assert!(repo.unmute(owner, root).await.unwrap(), "owner unmutes");
        assert!(
            !repo.unmute(owner, root).await.unwrap(),
            "second unmute is a no-op"
        );
        assert!(
            !repo.is_muted(owner, root).await.unwrap(),
            "no longer muted"
        );
        assert!(
            !repo.muted_by(root).await.unwrap().contains(&owner),
            "owner gone from the muter set"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM thread_mutes WHERE root_message_id = $1")
            .bind(root.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
