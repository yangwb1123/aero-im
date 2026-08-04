//! Per-user thread-subscription (thread follow) repository.
//!
//! Backs `migrations/0040_thread_subscriptions.sql`. A user follows a thread —
//! identified by its root message — to be notified of new replies, even when they
//! are not @-mentioned. Each subscription is a single
//! `(participant_id, root_message_id)` pair — the composite primary key makes
//! following idempotent and needs no surrogate id, so this repo carries no model
//! struct: it returns the bare [`MessageId`]s the caller follows, or the
//! [`ParticipantId`] fan-out set for a reply.
//!
//! The caller's reads/mutates are owner-scoped (`participant_id` in the `WHERE`),
//! so a caller can only ever see or remove their own subscriptions; the fan-out
//! lookup ([`ThreadSubscriptionRepo::subscribers`]) instead scans by
//! `root_message_id` and is the notify set the dispatcher unions with room
//! members when a reply lands. Purely additive: a NEW [`ThreadSubscriptionRepo`];
//! no existing repo is touched. Subscriptions store opaque `root_message_id`
//! uuids (no FK to `messages`), mirroring [`ChannelFavoriteRepo`](crate::ChannelFavoriteRepo)
//! — the HTTP layer resolves the message's room (via
//! [`ThreadSubscriptionRepo::message_room`]) and checks room access before adding.

use aero_common::{Error, MessageId, ParticipantId, RoomId};
use sqlx::{PgPool, Postgres, Transaction};

use crate::draft::lock_effective_room_access_in_tx;

/// Repository over the `thread_subscriptions` table (per-user thread follows).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`ThreadSubscriptionRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct ThreadSubscriptionRepo {
    pool: PgPool,
}

impl ThreadSubscriptionRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Follow the thread rooted at `root` for `participant`. Idempotent:
    /// re-following an already-followed thread is a no-op (`ON CONFLICT DO
    /// NOTHING`). The caller is responsible for any room-access check.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    #[cfg(test)]
    pub(crate) async fn subscribe(
        &self,
        participant: ParticipantId,
        root: MessageId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO thread_subscriptions (participant_id, root_message_id)
               VALUES ($1, $2)
               ON CONFLICT (participant_id, root_message_id) DO NOTHING",
        )
        .bind(participant.to_uuid())
        .bind(root.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Unfollow the thread rooted at `root` for `participant`. Returns `true` iff a
    /// row was removed — owner-scoped, so a caller can never remove another user's
    /// subscription, and a second unfollow (or unfollowing a thread that was never
    /// followed) is a no-op returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    #[cfg(test)]
    pub(crate) async fn unsubscribe(
        &self,
        participant: ParticipantId,
        root: MessageId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM thread_subscriptions WHERE participant_id = $1 AND root_message_id = $2",
        )
        .bind(participant.to_uuid())
        .bind(root.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// The fan-out set for a reply: every participant following the thread rooted at
    /// `root`. The dispatcher unions this with the room's members (and drops the
    /// sender) to decide who to notify of a new reply. Not owner-scoped — by design
    /// it returns *all* subscribers.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn subscribers(&self, root: MessageId) -> Result<Vec<ParticipantId>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT subscription.participant_id
                FROM thread_subscriptions AS subscription
                JOIN messages AS root
                  ON root.id = subscription.root_message_id
                 AND root.reply_to IS NULL
                 AND root.deleted_at IS NULL
               WHERE subscription.root_message_id = $1
                 AND aero_effective_room_access(
                         root.room_id,
                         subscription.participant_id,
                         NULL
                     )
               ORDER BY subscription.created_at ASC,
                        subscription.participant_id ASC",
        )
        .bind(root.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(p,)| ParticipantId::from_uuid(p))
            .collect())
    }

    /// Whether `participant` follows the thread rooted at `root`. Owner-scoped, so
    /// it only ever reflects the caller's own subscriptions.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    #[cfg(test)]
    pub(crate) async fn is_subscribed(
        &self,
        participant: ParticipantId,
        root: MessageId,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (i32,)>(
            r"SELECT 1 FROM thread_subscriptions
              WHERE participant_id = $1 AND root_message_id = $2",
        )
        .bind(participant.to_uuid())
        .bind(root.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// List the root messages of the threads `participant` follows, newest first.
    /// Owner-scoped — only the caller's own subscriptions are returned.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    #[cfg(test)]
    pub(crate) async fn followed_by(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<MessageId>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT root_message_id
               FROM thread_subscriptions
              WHERE participant_id = $1
              ORDER BY created_at DESC, root_message_id DESC",
        )
        .bind(participant.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(m,)| MessageId::from_uuid(m))
            .collect())
    }

    /// Resolve a live canonical thread root's room, or `None` if the message is
    /// missing, deleted, or itself a reply.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn message_room(&self, message: MessageId) -> Result<Option<RoomId>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT room_id
                FROM messages
               WHERE id = $1
                 AND reply_to IS NULL
                 AND deleted_at IS NULL",
        )
        .bind(message.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(r,)| RoomId::from_uuid(r)))
    }

    /// Follow a live canonical thread under the caller's current room-access
    /// fence.
    ///
    /// # Errors
    /// Returns an opaque root/access error or a database error.
    pub async fn subscribe_authorized(
        &self,
        participant: ParticipantId,
        root: MessageId,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        lock_effective_live_thread_root_in_tx(&mut tx, participant, root).await?;
        sqlx::query(
            r"INSERT INTO thread_subscriptions
                  (participant_id, root_message_id)
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

    /// Unfollow a live canonical thread under the caller's current room-access
    /// fence.
    ///
    /// # Errors
    /// Returns an opaque root/access error or a database error.
    pub async fn unsubscribe_authorized(
        &self,
        participant: ParticipantId,
        root: MessageId,
    ) -> Result<bool, Error> {
        let mut tx = self.pool.begin().await?;
        lock_effective_live_thread_root_in_tx(&mut tx, participant, root).await?;
        let result = sqlx::query(
            r"DELETE FROM thread_subscriptions
               WHERE participant_id = $1 AND root_message_id = $2",
        )
        .bind(participant.to_uuid())
        .bind(root.to_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }

    /// Check a follow under the caller's current room-access fence.
    ///
    /// # Errors
    /// Returns an opaque root/access error or a database error.
    pub async fn is_subscribed_authorized(
        &self,
        participant: ParticipantId,
        root: MessageId,
    ) -> Result<bool, Error> {
        let mut tx = self.pool.begin().await?;
        lock_effective_live_thread_root_in_tx(&mut tx, participant, root).await?;
        let followed: bool = sqlx::query_scalar(
            r"SELECT EXISTS(
                   SELECT 1
                     FROM thread_subscriptions
                    WHERE participant_id = $1 AND root_message_id = $2
               )",
        )
        .bind(participant.to_uuid())
        .bind(root.to_uuid())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(followed)
    }

    /// List only followed roots whose live rooms the caller can currently
    /// access.
    ///
    /// # Errors
    /// Propagates database errors.
    pub async fn followed_by_accessible(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<MessageId>, Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT subscription.root_message_id
                FROM thread_subscriptions AS subscription
                JOIN messages AS root
                  ON root.id = subscription.root_message_id
                 AND root.reply_to IS NULL
                 AND root.deleted_at IS NULL
               WHERE subscription.participant_id = $1
                 AND aero_effective_room_access(root.room_id, $1, NULL)
               ORDER BY subscription.created_at DESC,
                        subscription.root_message_id DESC",
        )
        .bind(participant.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(message,)| MessageId::from_uuid(message))
            .collect())
    }
}

/// Resolve and lock a live canonical root under the platform's governance lock
/// order. The client supplies only the root id; its room is always derived from
/// the message row and rechecked after the effective-access fence.
pub(crate) async fn lock_effective_live_thread_root_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    participant: ParticipantId,
    root: MessageId,
) -> Result<RoomId, Error> {
    let resolved_room = sqlx::query_scalar::<_, uuid::Uuid>(
        r"SELECT room_id
            FROM messages
           WHERE id = $1
             AND reply_to IS NULL
             AND deleted_at IS NULL",
    )
    .bind(root.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .map(RoomId::from_uuid)
    .ok_or_else(|| Error::NotFound("thread".into()))?;

    lock_effective_room_access_in_tx(tx, participant, resolved_room).await?;

    let locked_room = sqlx::query_scalar::<_, uuid::Uuid>(
        r"SELECT room_id
            FROM messages
           WHERE id = $1
             AND reply_to IS NULL
             AND deleted_at IS NULL
           FOR SHARE",
    )
    .bind(root.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .map(RoomId::from_uuid);
    if locked_room != Some(resolved_room) {
        return Err(Error::NotFound("thread".into()));
    }
    Ok(resolved_room)
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored thread_subscription
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    struct Fixture {
        owner: ParticipantId,
        other: ParticipantId,
        stranger: ParticipantId,
        room: RoomId,
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
        let room = RoomId::new();
        let root_message = MessageId::new();
        let mut tx = p.begin().await.expect("begin thread subscription fixture");

        for participant in [owner, other, stranger] {
            sqlx::query(
                "INSERT INTO participants (id, kind, display_name)
                 VALUES ($1, 'human', $2)",
            )
            .bind(participant.to_uuid())
            .bind(format!("thread-sub-{participant}"))
            .execute(&mut *tx)
            .await
            .expect("insert participant");
        }
        sqlx::query(
            "INSERT INTO workspaces (id, name, slug, created_by)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(workspace)
        .bind(format!("Thread subscription {workspace}"))
        .bind(format!("thread-subscription-{workspace}"))
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
        .bind(room.to_uuid())
        .bind(format!("Thread subscription room {room}"))
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
            .bind(room.to_uuid())
            .bind(participant.to_uuid())
            .bind(role)
            .execute(&mut *tx)
            .await
            .expect("insert room member");
        }
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks, searchable_text)
             VALUES ($1, $2, $3, '[]'::jsonb, 'thread subscription root')",
        )
        .bind(root_message.to_uuid())
        .bind(room.to_uuid())
        .bind(owner.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert live root message");
        tx.commit()
            .await
            .expect("commit thread subscription fixture");

        Fixture {
            owner,
            other,
            stranger,
            room,
            root: root_message,
        }
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn thread_subscription_subscribe_list_unsubscribe_owner_scoped() {
        let p = pool();
        let repo = ThreadSubscriptionRepo::new(p.clone());
        let Fixture {
            owner,
            other,
            stranger,
            root,
            ..
        } = fixture(&p).await;

        // Not subscribed yet.
        assert!(
            !repo.is_subscribed(owner, root).await.unwrap(),
            "thread is not followed before subscribe"
        );

        // subscribe → idempotent; followed_by + subscribers reflect it.
        repo.subscribe(owner, root).await.unwrap();
        repo.subscribe(owner, root).await.unwrap(); // idempotent (ON CONFLICT DO NOTHING)
        repo.subscribe(other, root).await.unwrap();
        assert!(repo.is_subscribed(owner, root).await.unwrap(), "followed");
        assert!(
            repo.followed_by(owner).await.unwrap().contains(&root),
            "followed_by shows the thread"
        );
        let subs = repo.subscribers(root).await.unwrap();
        assert!(
            subs.contains(&owner) && subs.contains(&other),
            "fan-out set has both"
        );

        // Owner-scoping: a stranger neither sees nor can remove the subscription.
        assert!(
            !repo.is_subscribed(stranger, root).await.unwrap(),
            "stranger does not see another user's subscription"
        );
        assert!(
            !repo.followed_by(stranger).await.unwrap().contains(&root),
            "subscription absent from a stranger's followed_by"
        );
        assert!(
            !repo.unsubscribe(stranger, root).await.unwrap(),
            "stranger cannot remove another user's subscription"
        );

        // unsubscribe → true once, then false; followed_by + subscribers drop it.
        assert!(
            repo.unsubscribe(owner, root).await.unwrap(),
            "owner unfollows"
        );
        assert!(
            !repo.unsubscribe(owner, root).await.unwrap(),
            "second unfollow is a no-op"
        );
        assert!(
            !repo.is_subscribed(owner, root).await.unwrap(),
            "no longer followed"
        );
        assert!(
            !repo.followed_by(owner).await.unwrap().contains(&root),
            "removed subscription leaves followed_by"
        );
        assert!(
            !repo.subscribers(root).await.unwrap().contains(&owner),
            "owner gone from the fan-out set"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM thread_subscriptions WHERE root_message_id = $1")
            .bind(root.to_uuid())
            .execute(&p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn thread_subscription_message_room_resolves() {
        let p = pool();
        let repo = ThreadSubscriptionRepo::new(p.clone());
        let Fixture {
            room,
            root: message,
            ..
        } = fixture(&p).await;

        assert_eq!(
            repo.message_room(message).await.unwrap(),
            Some(room),
            "message_room resolves the message's room"
        );
        assert!(
            repo.message_room(MessageId::new()).await.unwrap().is_none(),
            "an unknown message resolves to None"
        );

        // Cleanup so reruns stay self-contained (message FKs the room).
        sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(message.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
