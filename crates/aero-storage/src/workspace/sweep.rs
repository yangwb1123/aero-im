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
        let mut tx = self.pool.begin().await?;
        let rows: Vec<(uuid::Uuid, uuid::Uuid, serde_json::Value)> = sqlx::query_as(
            r"WITH candidates AS MATERIALIZED (
                   SELECT m.id, m.room_id, m.blocks
                     FROM messages m
                     JOIN rooms r ON r.id = m.room_id
                     JOIN workspaces w ON w.id = r.workspace_id
                    WHERE m.deleted_at IS NULL
                      AND COALESCE(r.retention_days, w.retention_days) IS NOT NULL
                      AND COALESCE(r.retention_days, w.retention_days) > 0
                      AND ($2::uuid IS NULL OR w.id = $2)
                      AND m.created_at < $1 - make_interval(days => COALESCE(r.retention_days, w.retention_days))
                      -- Legal-hold exemption (GDPR Art.17(3)(e) / eDiscovery).
                      AND NOT EXISTS (
                            SELECT 1 FROM legal_holds lh
                             WHERE lh.workspace_id = w.id
                               AND lh.active
                               AND (lh.room_id IS NULL OR lh.room_id = r.id)
                      )
                    FOR UPDATE OF m
               ),
               updated AS (
                   UPDATE messages m
                      SET deleted_at = $1,
                          blocks = '[]'::jsonb,
                          searchable_text = '',
                          embedding = NULL,
                          version = version + 1
                     FROM candidates c
                    WHERE m.id = c.id
                RETURNING m.id
               )
               SELECT c.id, c.room_id, c.blocks
                 FROM candidates c
                 JOIN updated u ON u.id = c.id",
        )
        .bind(now)
        .bind(only.map(|w| w.to_uuid()))
        .fetch_all(&mut *tx)
        .await?;
        let message_ids: Vec<uuid::Uuid> = rows.iter().map(|(id, _, _)| *id).collect();
        let blob_ids = rows
            .iter()
            .flat_map(|(_, _, blocks)| crate::message::attached_blob_ids(blocks))
            .collect::<Vec<_>>();
        crate::message::MessageRepo::cleanup_visible_associations_in_tx(&mut tx, &message_ids)
            .await?;
        crate::message::MessageRepo::enqueue_unreferenced_blobs_in_tx(&mut tx, &blob_ids).await?;
        for (message_id, room_id, _) in &rows {
            crate::message::MessageRepo::append_deleted_event_in_tx(
                &mut tx,
                MessageId::from_uuid(*message_id),
                RoomId::from_uuid(*room_id),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .map(|(mid, rid, _)| (MessageId::from_uuid(mid), RoomId::from_uuid(rid)))
            .collect())
    }
}
