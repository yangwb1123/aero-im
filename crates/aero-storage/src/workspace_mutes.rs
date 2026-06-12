//! Workspace-wide mute repository.
//!
//! Backs `migrations/0095_workspace_mutes.sql`. A participant who mutes a workspace
//! suppresses ALL notifications (mentions, replies, thread follows, keyword alerts)
//! from every channel in that workspace. This is a coarser control than per-channel
//! mute; it is checked FIRST in `ImService::dispatch_notifications`.
//!
//! Purely additive: a NEW [`WorkspaceMuteRepo`]; no existing repo is touched.

use aero_common::{ParticipantId, WorkspaceId};
use sqlx::PgPool;

#[derive(Clone)]
pub struct WorkspaceMuteRepo {
    pub pg: PgPool,
}

impl WorkspaceMuteRepo {
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Mute `workspace` for `participant`. Idempotent: re-muting is a no-op
    /// (keeps the original `created_at`).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn mute(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
    ) -> Result<(), aero_common::Error> {
        sqlx::query(
            r"INSERT INTO workspace_mutes (participant_id, workspace_id, created_at)
               VALUES ($1, $2, now())
               ON CONFLICT (participant_id, workspace_id) DO NOTHING",
        )
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .execute(&self.pg)
        .await
        .map_err(aero_common::Error::from)?;
        Ok(())
    }

    /// Unmute `workspace` for `participant`. Idempotent: unmuting a workspace the
    /// participant hasn't muted is a no-op.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn unmute(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
    ) -> Result<(), aero_common::Error> {
        sqlx::query(
            r"DELETE FROM workspace_mutes
               WHERE participant_id = $1 AND workspace_id = $2",
        )
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .execute(&self.pg)
        .await
        .map_err(aero_common::Error::from)?;
        Ok(())
    }

    /// Whether `participant` has muted `workspace`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn is_muted(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
    ) -> Result<bool, aero_common::Error> {
        let row = sqlx::query_as::<_, (i64,)>(
            r"SELECT COUNT(*) FROM workspace_mutes
               WHERE participant_id = $1 AND workspace_id = $2",
        )
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .fetch_one(&self.pg)
        .await
        .map_err(aero_common::Error::from)?;
        Ok(row.0 > 0)
    }

    /// All workspace ids `participant` has muted, newest mute first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn muted_workspaces_for(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<WorkspaceId>, aero_common::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT workspace_id FROM workspace_mutes
               WHERE participant_id = $1
               ORDER BY created_at DESC",
        )
        .bind(participant.to_uuid())
        .fetch_all(&self.pg)
        .await
        .map_err(aero_common::Error::from)?;
        Ok(rows.into_iter().map(|(w,)| WorkspaceId::from_uuid(w)).collect())
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{ParticipantId, WorkspaceId};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn workspace_mute_roundtrip() {
        let p = pool();
        let repo = WorkspaceMuteRepo::new(p.clone());
        let participant = ParticipantId::new();
        let workspace = WorkspaceId::new();

        assert!(!repo.is_muted(participant, workspace).await.unwrap(), "not muted initially");

        repo.mute(participant, workspace).await.unwrap();
        repo.mute(participant, workspace).await.unwrap(); // idempotent
        assert!(repo.is_muted(participant, workspace).await.unwrap(), "muted after mute");

        let muted = repo.muted_workspaces_for(participant).await.unwrap();
        assert!(muted.contains(&workspace), "muted_workspaces_for lists it");

        repo.unmute(participant, workspace).await.unwrap();
        repo.unmute(participant, workspace).await.unwrap(); // idempotent
        assert!(!repo.is_muted(participant, workspace).await.unwrap(), "unmuted after unmute");
        assert!(!repo.muted_workspaces_for(participant).await.unwrap().contains(&workspace));
    }
}
