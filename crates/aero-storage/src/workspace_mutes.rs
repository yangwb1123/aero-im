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

    /// Of `participants`, the subset that has muted `workspace` — resolved in
    /// ONE index-backed query (`WHERE workspace_id = $1 AND participant_id =
    /// ANY($2)`), replacing the per-recipient [`Self::is_muted`] loop in
    /// notification dispatch (ROADMAP 方向二). For a large-room `@everyone` the
    /// old loop issued one round-trip per recipient and scaled linearly with
    /// membership. The `(workspace_id, participant_id)` composite index
    /// (migration 0123) lets this query seek by workspace; the table PK leads
    /// with `participant_id` and can't serve this shape. An empty input
    /// short-circuits without touching the database.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn muted_participants(
        &self,
        workspace: WorkspaceId,
        participants: &[ParticipantId],
    ) -> Result<std::collections::HashSet<ParticipantId>, aero_common::Error> {
        if participants.is_empty() {
            return Ok(std::collections::HashSet::new());
        }
        let ids: Vec<uuid::Uuid> = participants.iter().map(|p| p.to_uuid()).collect();
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT participant_id FROM workspace_mutes
               WHERE workspace_id = $1 AND participant_id = ANY($2)",
        )
        .bind(workspace.to_uuid())
        .bind(&ids)
        .fetch_all(&self.pg)
        .await
        .map_err(aero_common::Error::from)?;
        Ok(rows
            .into_iter()
            .map(|(p,)| ParticipantId::from_uuid(p))
            .collect())
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
        Ok(rows
            .into_iter()
            .map(|(w,)| WorkspaceId::from_uuid(w))
            .collect())
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
    async fn muted_participants_empty_input_short_circuits() {
        // An empty recipient list returns empty WITHOUT touching the database —
        // the lazy pool is never connected, so this runs offline.
        let repo = WorkspaceMuteRepo::new(pool());
        let got = repo
            .muted_participants(WorkspaceId::new(), &[])
            .await
            .expect("empty input must not query");
        assert!(got.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn muted_participants_returns_only_muted_subset() {
        let p = pool();
        let repo = WorkspaceMuteRepo::new(p.clone());
        let ws = WorkspaceId::new();
        let muted = ParticipantId::new();
        let unmuted = ParticipantId::new();
        repo.mute(muted, ws).await.unwrap();

        let set = repo
            .muted_participants(ws, &[muted, unmuted])
            .await
            .unwrap();
        assert!(set.contains(&muted), "muted participant is in the set");
        assert!(!set.contains(&unmuted), "unmuted participant is excluded");
        assert_eq!(set.len(), 1);

        repo.unmute(muted, ws).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn workspace_mute_roundtrip() {
        let p = pool();
        let repo = WorkspaceMuteRepo::new(p.clone());
        let participant = ParticipantId::new();
        let workspace = WorkspaceId::new();

        assert!(
            !repo.is_muted(participant, workspace).await.unwrap(),
            "not muted initially"
        );

        repo.mute(participant, workspace).await.unwrap();
        repo.mute(participant, workspace).await.unwrap(); // idempotent
        assert!(
            repo.is_muted(participant, workspace).await.unwrap(),
            "muted after mute"
        );

        let muted = repo.muted_workspaces_for(participant).await.unwrap();
        assert!(muted.contains(&workspace), "muted_workspaces_for lists it");

        repo.unmute(participant, workspace).await.unwrap();
        repo.unmute(participant, workspace).await.unwrap(); // idempotent
        assert!(
            !repo.is_muted(participant, workspace).await.unwrap(),
            "unmuted after unmute"
        );
        assert!(!repo
            .muted_workspaces_for(participant)
            .await
            .unwrap()
            .contains(&workspace));
    }
}
