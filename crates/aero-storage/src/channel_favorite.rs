//! Per-user channel-favorites (starred channels) repository.
//!
//! Backs `migrations/0034_channel_favorites.sql`. A user stars a channel (room)
//! as a favorite, then lists or unstars it. Each favorite is a single
//! `(participant_id, room_id)` pair — the composite primary key makes favoriting
//! idempotent and needs no surrogate id, so this repo carries no model struct: it
//! returns the bare [`RoomId`]s the caller has favorited.
//!
//! Every read/mutate method is owner-scoped (`participant_id` in the `WHERE`), so
//! a caller can only ever see or remove their own favorites. Purely additive: a
//! NEW [`ChannelFavoriteRepo`]; no existing repo is touched. Favorites store
//! opaque `room_id` uuids (no FK to `rooms`), mirroring
//! [`ChannelSectionRepo`](crate::ChannelSectionRepo) — the HTTP layer is
//! responsible for any room-access check before adding.

use aero_common::{ParticipantId, RoomId};
use sqlx::PgPool;

/// Repository over the `channel_favorites` table (per-user starred channels).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`ChannelFavoriteRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct ChannelFavoriteRepo {
    pool: PgPool,
}

impl ChannelFavoriteRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Star `room` as a favorite for `participant`. Idempotent: re-favoriting an
    /// already-starred room is a no-op (`ON CONFLICT DO NOTHING`). The caller is
    /// responsible for any room-access check.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn add(&self, participant: ParticipantId, room: RoomId) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO channel_favorites (participant_id, room_id)
               VALUES ($1, $2)
               ON CONFLICT (participant_id, room_id) DO NOTHING",
        )
        .bind(participant.to_uuid())
        .bind(room.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Unstar `room` for `participant`. Returns `true` iff a row was removed —
    /// owner-scoped, so a caller can never remove another user's favorite, and a
    /// second remove (or unstarring a room that was never favorited) is a no-op
    /// returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn remove(
        &self,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<bool, sqlx::Error> {
        let result =
            sqlx::query("DELETE FROM channel_favorites WHERE participant_id = $1 AND room_id = $2")
                .bind(participant.to_uuid())
                .bind(room.to_uuid())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    /// List `participant`'s favorited rooms, newest first. Owner-scoped — only the
    /// caller's own favorites are returned.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list(&self, participant: ParticipantId) -> Result<Vec<RoomId>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT room_id
               FROM channel_favorites
              WHERE participant_id = $1
              ORDER BY created_at DESC, room_id DESC",
        )
        .bind(participant.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(r,)| RoomId::from_uuid(r)).collect())
    }

    /// Whether `participant` has favorited `room`. Owner-scoped, so it only ever
    /// reflects the caller's own favorites.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn is_favorite(
        &self,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (i32,)>(
            r"SELECT 1 FROM channel_favorites WHERE participant_id = $1 AND room_id = $2",
        )
        .bind(participant.to_uuid())
        .bind(room.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored channel_favorite
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

    /// Create a throwaway owner participant so the test is self-contained.
    /// Favorites store opaque `room_id` uuids (no FK to `rooms`), so a fresh
    /// [`RoomId`] can be used without inserting a room.
    async fn mk_participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("channel-favorite-owner-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn channel_favorite_add_list_is_favorite_remove_owner_scoped() {
        let p = pool();
        let repo = ChannelFavoriteRepo::new(p.clone());
        let owner = mk_participant(&p).await;
        let stranger = mk_participant(&p).await;
        let room = RoomId::new();

        // Not favorited yet.
        assert!(
            !repo.is_favorite(owner, room).await.unwrap(),
            "room is not favorited before add"
        );

        // add → idempotent; list shows it; is_favorite is true.
        repo.add(owner, room).await.unwrap();
        repo.add(owner, room).await.unwrap(); // idempotent (ON CONFLICT DO NOTHING)
        let listed = repo.list(owner).await.unwrap();
        assert!(listed.contains(&room), "list shows the favorite");
        assert!(repo.is_favorite(owner, room).await.unwrap(), "favorited");

        // Owner-scoping: a stranger neither sees nor can remove the favorite.
        assert!(
            !repo.is_favorite(stranger, room).await.unwrap(),
            "stranger does not see another user's favorite"
        );
        assert!(
            !repo.list(stranger).await.unwrap().contains(&room),
            "favorite absent from a stranger's list"
        );
        assert!(
            !repo.remove(stranger, room).await.unwrap(),
            "stranger cannot remove another user's favorite"
        );

        // remove → false/empty; second remove is a no-op.
        assert!(repo.remove(owner, room).await.unwrap(), "owner unstars");
        assert!(
            !repo.remove(owner, room).await.unwrap(),
            "second remove is a no-op"
        );
        assert!(
            !repo.is_favorite(owner, room).await.unwrap(),
            "no longer favorited"
        );
        assert!(
            !repo.list(owner).await.unwrap().contains(&room),
            "removed favorite leaves the list"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM channel_favorites WHERE participant_id = $1 OR participant_id = $2")
            .bind(owner.to_uuid())
            .bind(stranger.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
