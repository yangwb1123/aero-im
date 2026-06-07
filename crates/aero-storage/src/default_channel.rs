//! Workspace default-channels repository (admin-curated auto-join set).
//!
//! Backs `migrations/0042_default_channels.sql`. An administrator marks channels
//! (rooms) in a workspace as "defaults"; every new member auto-joins them on
//! enrollment. Each default is a single `(workspace_id, room_id)` pair — the
//! composite primary key makes marking idempotent and needs no surrogate id, so
//! this repo carries no model struct: it returns the bare [`RoomId`]s that are
//! defaults for a workspace.
//!
//! Purely additive: a NEW [`DefaultChannelRepo`]; no existing repo is touched.
//! Defaults store opaque `room_id` uuids (no FK to `rooms`), mirroring
//! [`ChannelFavoriteRepo`](crate::ChannelFavoriteRepo) — the HTTP layer is
//! responsible for verifying the room belongs to the workspace before adding.
//! The register/enroll path reads this set via [`DefaultChannelRepo::list`] to
//! auto-join a new member into the workspace's default channels.

use aero_common::{RoomId, WorkspaceId};
use sqlx::PgPool;

/// Repository over the `workspace_default_channels` table (admin-curated
/// auto-join set).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`DefaultChannelRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct DefaultChannelRepo {
    pool: PgPool,
}

impl DefaultChannelRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Mark `room` as a default channel for `workspace`. Idempotent: re-marking an
    /// already-default room is a no-op (`ON CONFLICT DO NOTHING`). The caller is
    /// responsible for verifying the room belongs to the workspace.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn add(&self, workspace: WorkspaceId, room: RoomId) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO workspace_default_channels (workspace_id, room_id)
               VALUES ($1, $2)
               ON CONFLICT (workspace_id, room_id) DO NOTHING",
        )
        .bind(workspace.to_uuid())
        .bind(room.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Unmark `room` as a default for `workspace`. Returns `true` iff a row was
    /// removed — a second remove (or unmarking a room that was never a default) is
    /// a no-op returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn remove(&self, workspace: WorkspaceId, room: RoomId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM workspace_default_channels WHERE workspace_id = $1 AND room_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(room.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// List the default channels for `workspace`, newest first. The orchestrator's
    /// register/enroll path calls this to auto-join a new member into each default.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list(&self, workspace: WorkspaceId) -> Result<Vec<RoomId>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT room_id
               FROM workspace_default_channels
              WHERE workspace_id = $1
              ORDER BY created_at DESC, room_id DESC",
        )
        .bind(workspace.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(r,)| RoomId::from_uuid(r)).collect())
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored default_channel
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

    /// The reserved all-zero default workspace, guaranteed to exist by migration
    /// 0006's backfill — reused so the default-channel rows are well-scoped.
    const DEFAULT_WS: &str = "00000000-0000-0000-0000-000000000000";

    fn default_ws() -> WorkspaceId {
        WorkspaceId::from_uuid(uuid::Uuid::parse_str(DEFAULT_WS).expect("valid uuid"))
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn default_channel_add_list_remove() {
        let p = pool();
        let repo = DefaultChannelRepo::new(p.clone());
        let ws = default_ws();
        // Defaults store opaque `room_id` uuids (no FK to `rooms`), so a fresh
        // `RoomId` can be used without inserting a room.
        let room = RoomId::new();

        // Not a default yet.
        assert!(
            !repo.list(ws).await.unwrap().contains(&room),
            "room is not a default before add"
        );

        // add → idempotent; list reflects it.
        repo.add(ws, room).await.unwrap();
        repo.add(ws, room).await.unwrap(); // idempotent (ON CONFLICT DO NOTHING)
        assert!(
            repo.list(ws).await.unwrap().contains(&room),
            "list shows the default channel"
        );

        // remove → true once; second remove is a no-op returning false; list empties.
        assert!(repo.remove(ws, room).await.unwrap(), "first remove succeeds");
        assert!(
            !repo.remove(ws, room).await.unwrap(),
            "second remove is a no-op"
        );
        assert!(
            !repo.list(ws).await.unwrap().contains(&room),
            "removed default leaves the list"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM workspace_default_channels WHERE room_id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
