//! Workspace default-channels repository (admin-curated auto-join set).
//!
//! Backs `migrations/0042_default_channels.sql`. An administrator marks channels
//! (rooms) in a workspace as "defaults"; every new member auto-joins them on
//! enrollment. Each default is a single `(workspace_id, room_id)` pair — the
//! composite primary key makes marking idempotent and needs no surrogate id, so
//! this repo carries no model struct: it returns the bare [`RoomId`]s that are
//! defaults for a workspace.
//!
//! The table predates a room foreign key, so every write locks and validates the
//! workspace + room aggregate in storage. Reads join back to `rooms` and expose
//! only same-workspace `channel` rows, which also contains any historical dirty
//! entries inserted by older code.

use aero_common::{ParticipantId, RoomId, WorkspaceId};
use sqlx::PgPool;

#[derive(Debug, thiserror::Error)]
pub enum DefaultChannelWriteError {
    #[error("workspace not found")]
    WorkspaceNotFound,
    #[error("room not found")]
    RoomNotFound,
    #[error("room does not belong to the workspace")]
    RoomOutsideWorkspace,
    #[error("room is not a channel")]
    NotChannel,
    #[error("caller is not authorized to manage default channels")]
    NotAuthorized,
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

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

    /// Mark `room` as a default channel while rechecking the caller's current
    /// effective Owner/Admin role and the room's exact tenant/kind under locks.
    pub async fn add_authorized(
        &self,
        workspace: WorkspaceId,
        caller: ParticipantId,
        room: RoomId,
    ) -> Result<bool, DefaultChannelWriteError> {
        let mut tx = self.pool.begin().await?;
        lock_default_workspace_and_caller(&mut tx, workspace, caller).await?;
        lock_default_channel(&mut tx, workspace, room).await?;
        let inserted = sqlx::query(
            r"INSERT INTO workspace_default_channels (workspace_id, room_id)
               VALUES ($1, $2)
               ON CONFLICT (workspace_id, room_id) DO NOTHING",
        )
        .bind(workspace.to_uuid())
        .bind(room.to_uuid())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        tx.commit().await?;
        Ok(inserted)
    }

    /// Unmark a validated channel under the same current-admin transaction.
    pub async fn remove_authorized(
        &self,
        workspace: WorkspaceId,
        caller: ParticipantId,
        room: RoomId,
    ) -> Result<bool, DefaultChannelWriteError> {
        let mut tx = self.pool.begin().await?;
        lock_default_workspace_and_caller(&mut tx, workspace, caller).await?;
        lock_default_channel(&mut tx, workspace, room).await?;
        let result = sqlx::query(
            "DELETE FROM workspace_default_channels WHERE workspace_id = $1 AND room_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(room.to_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }

    /// List the default channels for `workspace`, newest first. The orchestrator's
    /// register/enroll path calls this to auto-join a new member into each default.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list(&self, workspace: WorkspaceId) -> Result<Vec<RoomId>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT defaults.room_id
               FROM workspace_default_channels defaults
               JOIN rooms room
                 ON room.id = defaults.room_id
                AND room.workspace_id = defaults.workspace_id
                AND room.kind = 'channel'
              WHERE defaults.workspace_id = $1
              ORDER BY defaults.created_at DESC, defaults.room_id DESC",
        )
        .bind(workspace.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(r,)| RoomId::from_uuid(r)).collect())
    }

    /// Enroll one newly-added ordinary member into the workspace's current
    /// default channels.
    ///
    /// This is a system follow-up to the authorized workspace-member insert, but
    /// it still owns a fresh transaction and rechecks the target under the same
    /// workspace serialization lock used by role/guest changes. A participant
    /// who is now a single-channel guest, has the legacy `guest` role, has been
    /// removed, or has been deleted receives no additional room edges.
    ///
    /// Returns only newly inserted room ids so callers invalidate exactly the
    /// cache entries that changed.
    pub async fn auto_join_ordinary_member(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<Vec<RoomId>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let workspace_exists =
            sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
                .bind(workspace.to_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        if !workspace_exists {
            tx.commit().await?;
            return Ok(Vec::new());
        }
        let membership = sqlx::query_as::<_, (String, bool)>(
            r"SELECT role, is_guest
                FROM workspace_members
               WHERE workspace_id = $1 AND participant_id = $2
               FOR UPDATE",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let eligible = membership.is_some_and(|(role, is_guest)| role != "guest" && !is_guest);
        let active = sqlx::query_scalar::<_, bool>(
            "SELECT deleted_at IS NULL FROM participants WHERE id = $1 FOR SHARE",
        )
        .bind(participant.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(false);
        if !eligible || !active {
            tx.commit().await?;
            return Ok(Vec::new());
        }

        // Default-channel management also locks the workspace first, so this
        // sorted room snapshot is stable until commit.
        sqlx::query_scalar::<_, uuid::Uuid>(
            r"SELECT room.id
                FROM workspace_default_channels defaults
                JOIN rooms room
                  ON room.id = defaults.room_id
                 AND room.workspace_id = defaults.workspace_id
                 AND room.kind = 'channel'
               WHERE defaults.workspace_id = $1
               ORDER BY room.id
               FOR UPDATE OF room",
        )
        .bind(workspace.to_uuid())
        .fetch_all(&mut *tx)
        .await?;

        let inserted = sqlx::query_scalar::<_, uuid::Uuid>(
            r"INSERT INTO room_members (room_id, participant_id, role, joined_at)
               SELECT room.id, $2, 'member', NOW()
                 FROM workspace_default_channels defaults
                 JOIN rooms room
                   ON room.id = defaults.room_id
                  AND room.workspace_id = defaults.workspace_id
                  AND room.kind = 'channel'
                WHERE defaults.workspace_id = $1
                ORDER BY room.id
               ON CONFLICT (room_id, participant_id) DO NOTHING
               RETURNING room_id",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(inserted.into_iter().map(RoomId::from_uuid).collect())
    }
}

async fn lock_default_workspace_and_caller(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> Result<(), DefaultChannelWriteError> {
    let exists =
        sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .is_some();
    if !exists {
        return Err(DefaultChannelWriteError::WorkspaceNotFound);
    }
    let role = sqlx::query_scalar::<_, String>(
        r"SELECT role
            FROM workspace_members
           WHERE workspace_id = $1
             AND participant_id = $2
           FOR UPDATE",
    )
    .bind(workspace.to_uuid())
    .bind(caller.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(DefaultChannelWriteError::NotAuthorized)?;
    if !crate::workspace::members::effective_workspace_access_in_tx(tx, workspace, caller).await?
        || !matches!(role.as_str(), "owner" | "admin")
    {
        return Err(DefaultChannelWriteError::NotAuthorized);
    }
    Ok(())
}

async fn lock_default_channel(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
    room: RoomId,
) -> Result<(), DefaultChannelWriteError> {
    let row = sqlx::query_as::<_, (uuid::Uuid, String)>(
        "SELECT workspace_id, kind FROM rooms WHERE id = $1 FOR UPDATE",
    )
    .bind(room.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(DefaultChannelWriteError::RoomNotFound)?;
    if row.0 != workspace.to_uuid() {
        return Err(DefaultChannelWriteError::RoomOutsideWorkspace);
    }
    if row.1 != "channel" {
        return Err(DefaultChannelWriteError::NotChannel);
    }
    Ok(())
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
    use crate::{RoomRepo, WorkspaceRepo};
    use aero_common::RoomKind;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn participant(pool: &PgPool, label: &str) -> ParticipantId {
        let participant = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(participant.to_uuid())
            .bind(format!("default-channel-{label}-{participant}"))
            .execute(pool)
            .await
            .unwrap();
        participant
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn default_channel_write_and_list_are_channel_kind_contained() {
        let p = pool();
        let repo = DefaultChannelRepo::new(p.clone());
        let owner = participant(&p, "owner").await;
        let workspace = WorkspaceRepo::new(p.clone())
            .create(
                "Default channel test".into(),
                format!("default-channel-{owner}"),
                owner,
            )
            .await
            .unwrap()
            .id;
        let rooms = RoomRepo::new(p.clone());
        let channel = rooms
            .create_in_workspace(
                workspace,
                RoomKind::Channel,
                Some("valid default".into()),
                owner,
            )
            .await
            .unwrap()
            .id;
        let group = rooms
            .create_in_workspace(
                workspace,
                RoomKind::Group,
                Some("not a channel".into()),
                owner,
            )
            .await
            .unwrap()
            .id;

        // Not a default yet.
        assert!(
            !repo.list(workspace).await.unwrap().contains(&channel),
            "room is not a default before add"
        );

        // add → idempotent; list reflects it.
        assert!(repo
            .add_authorized(workspace, owner, channel)
            .await
            .unwrap());
        assert!(!repo
            .add_authorized(workspace, owner, channel)
            .await
            .unwrap());
        assert!(
            repo.list(workspace).await.unwrap().contains(&channel),
            "list shows the default channel"
        );
        assert!(matches!(
            repo.add_authorized(workspace, owner, group)
                .await
                .expect_err("group rooms cannot become defaults"),
            DefaultChannelWriteError::NotChannel
        ));

        // Even a historical dirty row inserted outside the repository is hidden.
        sqlx::query(
            r"INSERT INTO workspace_default_channels (workspace_id, room_id)
               VALUES ($1, $2)",
        )
        .bind(workspace.to_uuid())
        .bind(group.to_uuid())
        .execute(&p)
        .await
        .unwrap();
        assert!(!repo.list(workspace).await.unwrap().contains(&group));

        assert!(
            repo.remove_authorized(workspace, owner, channel)
                .await
                .unwrap(),
            "first remove succeeds"
        );
        assert!(
            !repo
                .remove_authorized(workspace, owner, channel)
                .await
                .unwrap(),
            "second remove is a no-op"
        );
        assert!(
            !repo.list(workspace).await.unwrap().contains(&channel),
            "removed default leaves the list"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn auto_join_defaults_only_enrolls_current_ordinary_members() {
        let p = pool();
        let repo = DefaultChannelRepo::new(p.clone());
        let workspaces = WorkspaceRepo::new(p.clone());
        let rooms = RoomRepo::new(p.clone());
        let owner = participant(&p, "auto-owner").await;
        let ordinary = participant(&p, "auto-ordinary").await;
        let role_guest = participant(&p, "auto-role-guest").await;
        let scoped_guest = participant(&p, "auto-scoped-guest").await;
        let workspace = workspaces
            .create(
                "Default auto-join test".into(),
                format!("default-auto-join-{owner}"),
                owner,
            )
            .await
            .unwrap()
            .id;
        let default = rooms
            .create_in_workspace(workspace, RoomKind::Channel, Some("default".into()), owner)
            .await
            .unwrap()
            .id;
        let guest_room = rooms
            .create_in_workspace(
                workspace,
                RoomKind::Channel,
                Some("guest-only".into()),
                owner,
            )
            .await
            .unwrap()
            .id;
        repo.add_authorized(workspace, owner, default)
            .await
            .unwrap();

        workspaces
            .add_member(workspace, ordinary, aero_common::WorkspaceRole::Member)
            .await
            .unwrap();
        assert_eq!(
            repo.auto_join_ordinary_member(workspace, ordinary)
                .await
                .unwrap(),
            vec![default]
        );
        assert!(repo
            .auto_join_ordinary_member(workspace, ordinary)
            .await
            .unwrap()
            .is_empty());

        // Historical generic `guest` role rows are denied even if they predate
        // the dedicated is_guest aggregate.
        workspaces
            .add_member(workspace, role_guest, aero_common::WorkspaceRole::Guest)
            .await
            .unwrap();
        assert!(repo
            .auto_join_ordinary_member(workspace, role_guest)
            .await
            .unwrap()
            .is_empty());
        assert!(!rooms.is_member(default, role_guest).await.unwrap());

        workspaces
            .add_guest_authorized(workspace, owner, scoped_guest, guest_room)
            .await
            .unwrap();
        assert!(repo
            .auto_join_ordinary_member(workspace, scoped_guest)
            .await
            .unwrap()
            .is_empty());
        assert!(!rooms.is_member(default, scoped_guest).await.unwrap());
        assert!(rooms.is_member(guest_room, scoped_guest).await.unwrap());
    }
}
