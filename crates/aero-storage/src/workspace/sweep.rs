//! Workspace retention sweep — soft-delete messages past their window.

use aero_common::{MessageId, RoomId, WorkspaceId};

use super::WorkspaceRepo;

impl WorkspaceRepo {
    /// Soft-delete messages past their workspace's retention window.
    pub async fn sweep_expired_messages(
        &self,
        now: time::OffsetDateTime,
        only: Option<WorkspaceId>,
    ) -> Result<Vec<(MessageId, RoomId)>, sqlx::Error> {
        let rows: Vec<(uuid::Uuid, uuid::Uuid)> = sqlx::query_as(
            r"UPDATE messages m
                  SET deleted_at = $1,
                      blocks = '[]'::jsonb,
                      searchable_text = '',
                      embedding = NULL
                FROM rooms r
                JOIN workspaces w ON w.id = r.workspace_id
               WHERE m.room_id = r.id
                 AND m.deleted_at IS NULL
                 AND COALESCE(r.retention_days, w.retention_days) IS NOT NULL
                 AND COALESCE(r.retention_days, w.retention_days) > 0
                 AND ($2::uuid IS NULL OR w.id = $2)
                 AND m.created_at < $1 - make_interval(days => COALESCE(r.retention_days, w.retention_days))
                 -- Legal-hold exemption (GDPR Art.17(3)(e) / eDiscovery): an active
                 -- hold over the message's workspace (room_id NULL) or its specific
                 -- room must keep the message past its retention window. This
                 -- `NOT EXISTS` was dropped when workspace.rs was split into
                 -- workspace/sweep.rs; restored — db_test `sweep_preserves_legally_held_room`.
                 AND NOT EXISTS (
                       SELECT 1 FROM legal_holds lh
                        WHERE lh.workspace_id = w.id
                          AND lh.active
                          AND (lh.room_id IS NULL OR lh.room_id = r.id)
                 )
            RETURNING m.id, m.room_id",
        )
        .bind(now)
        .bind(only.map(|w| w.to_uuid()))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(mid, rid)| (MessageId::from_uuid(mid), RoomId::from_uuid(rid)))
            .collect())
    }
}
