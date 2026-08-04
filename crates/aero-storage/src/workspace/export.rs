//! Workspace GDPR export and hard-delete (erasure).

use aero_common::{
    AuditId, Error, Message, MessageId, ParticipantId, Room, RoomId, RoomKind, Workspace,
    WorkspaceId, WorkspaceMember, WorkspaceRole,
};

use crate::audit::AuditEvent;

use super::{RoomExport, WorkspaceExport, WorkspaceRepo, EXPORT_MESSAGES_PER_ROOM};

impl WorkspaceRepo {
    /// Export a self-contained snapshot of one tenant (GDPR portability).
    pub async fn export(
        &self,
        workspace: aero_common::WorkspaceId,
    ) -> Result<Option<WorkspaceExport>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        let ws_row = sqlx::query_as::<
            _,
            (
                uuid::Uuid,
                String,
                String,
                Option<uuid::Uuid>,
                time::OffsetDateTime,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
            ),
        >(
            r"SELECT id, name, slug, created_by, created_at,
                     logo_url, color_scheme, custom_domain, description
                FROM workspaces WHERE id = $1",
        )
        .bind(workspace.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some((
            id,
            name,
            slug,
            created_by,
            created_at,
            logo_url,
            color_scheme,
            custom_domain,
            description,
        )) = ws_row
        else {
            tx.commit().await?;
            return Ok(None);
        };
        let workspace_row = Workspace {
            id: aero_common::WorkspaceId::from_uuid(id),
            name,
            slug,
            created_by: created_by.map(ParticipantId::from_uuid),
            created_at,
            logo_url,
            color_scheme,
            custom_domain,
            description,
        };

        let member_rows =
            sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, String, time::OffsetDateTime)>(
                r"SELECT workspace_id, participant_id, role, joined_at
                   FROM workspace_members
                   WHERE workspace_id = $1
                   ORDER BY joined_at ASC, participant_id ASC",
            )
            .bind(workspace.to_uuid())
            .fetch_all(&mut *tx)
            .await?;
        let members = member_rows
            .into_iter()
            .map(|(ws, pid, role, joined_at)| WorkspaceMember {
                workspace_id: aero_common::WorkspaceId::from_uuid(ws),
                participant_id: ParticipantId::from_uuid(pid),
                role: WorkspaceRole::from_db_str(&role).unwrap_or(WorkspaceRole::Guest),
                joined_at,
            })
            .collect();

        let room_rows = sqlx::query_as::<
            _,
            (
                uuid::Uuid,
                String,
                Option<String>,
                uuid::Uuid,
                time::OffsetDateTime,
            ),
        >(
            r"SELECT id, kind, name, created_by, created_at
               FROM rooms
               WHERE workspace_id = $1
               ORDER BY created_at ASC, id ASC",
        )
        .bind(workspace.to_uuid())
        .fetch_all(&mut *tx)
        .await?;

        let mut rooms = Vec::with_capacity(room_rows.len());
        for (rid, kind, rname, by, at) in room_rows {
            let room = Room {
                id: RoomId::from_uuid(rid),
                kind: room_kind_of(&kind),
                name: rname,
                created_by: ParticipantId::from_uuid(by),
                created_at: at,
            };
            let probe = EXPORT_MESSAGES_PER_ROOM + 1;
            let mut msg_rows = sqlx::query_as::<_, ExportMessageRow>(
                r"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at, version
                   FROM messages
                   WHERE room_id = $1 AND deleted_at IS NULL
                   ORDER BY id DESC
                   LIMIT $2",
            )
            .bind(rid)
            .bind(probe)
            .fetch_all(&mut *tx)
            .await?;

            let message_cap_hit =
                i64::try_from(msg_rows.len()).unwrap_or(i64::MAX) > EXPORT_MESSAGES_PER_ROOM;
            if message_cap_hit {
                msg_rows.truncate(usize::try_from(EXPORT_MESSAGES_PER_ROOM).unwrap_or(usize::MAX));
            }
            msg_rows.reverse();
            let messages = msg_rows.into_iter().map(Message::from).collect();
            rooms.push(RoomExport {
                room,
                messages,
                message_cap_hit,
            });
        }

        let audit_rows = sqlx::query_as::<
            _,
            (
                uuid::Uuid,
                uuid::Uuid,
                Option<uuid::Uuid>,
                String,
                Option<String>,
                serde_json::Value,
                time::OffsetDateTime,
            ),
        >(
            r"SELECT id, workspace_id, actor_id, action, target, detail, created_at
               FROM audit_events
               WHERE workspace_id = $1
               ORDER BY id DESC",
        )
        .bind(workspace.to_uuid())
        .fetch_all(&mut *tx)
        .await?;
        let audit_events = audit_rows
            .into_iter()
            .map(|(aid, ws, actor, action, target, detail, ca)| AuditEvent {
                id: AuditId::from_uuid(aid),
                workspace_id: aero_common::WorkspaceId::from_uuid(ws),
                actor_id: actor.map(ParticipantId::from_uuid),
                action,
                target,
                detail,
                created_at: ca,
            })
            .collect();

        tx.commit().await?;

        Ok(Some(WorkspaceExport {
            workspace: workspace_row,
            members,
            rooms,
            audit_events,
            exported_at: time::OffsetDateTime::now_utc(),
        }))
    }

    /// Hard-delete a workspace and everything scoped to it, atomically.
    pub async fn delete(&self, workspace: WorkspaceId) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        // Establish workspace DML intent before taking any child/room row lock.
        // Migration 0198's low-frequency participant/TOTP fence first takes a
        // conflicting SHARE table lock and then workspace -> channel rows.  An
        // explicit ROW EXCLUSIVE table lock makes the two paths serialize at
        // the table boundary instead of deadlocking as child -> workspace.
        sqlx::query("LOCK TABLE workspaces IN ROW EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await?;
        crate::ownership::lock_membership_governance(&mut tx).await?;
        let exists =
            sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
                .bind(workspace.to_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        if !exists {
            tx.commit().await?;
            return Ok(false);
        }
        let deleted = delete_workspace_rows_in_tx(&mut tx, workspace).await?;
        tx.commit().await?;
        Ok(deleted)
    }

    /// Hard-delete a workspace only while `actor` remains an effective owner in
    /// the same transaction as the destructive write.
    pub async fn delete_authorized(
        &self,
        workspace: WorkspaceId,
        actor: ParticipantId,
    ) -> Result<bool, Error> {
        let mut tx = self.pool.begin().await?;
        // Match the low-level erasure lock boundary before taking any aggregate
        // row. This serializes with participant/TOTP global governance fences.
        sqlx::query("LOCK TABLE workspaces IN ROW EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await?;
        crate::ownership::lock_membership_governance(&mut tx).await?;
        let role = super::authz::assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        if role != WorkspaceRole::Owner {
            return Err(Error::Forbidden(
                "workspace deletion requires an effective owner".into(),
            ));
        }
        let deleted = delete_workspace_rows_in_tx(&mut tx, workspace).await?;
        tx.commit().await?;
        Ok(deleted)
    }
}

async fn delete_workspace_rows_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
) -> Result<bool, sqlx::Error> {
    // Several ROOM-scoped tables carry a `room_id` but have NO foreign key to
    // `rooms`, so the workspace cascade cannot erase them.
    for table in [
        "message_drafts",
        "ooo_auto_replies",
        "recurring_messages",
        "tasks",
        "channel_bookmarks",
        "channel_canvases",
        "channel_favorites",
        "channel_notification_prefs",
        "channel_section_items",
        "channel_join_requests",
        "digest_subscriptions",
        "workspace_default_channels",
        "scheduled_streams",
        "stream_recordings",
    ] {
        let sql = format!(
            "DELETE FROM {table} WHERE room_id IN \
             (SELECT id FROM rooms WHERE workspace_id = $1)"
        );
        sqlx::query(&sql)
            .bind(workspace.to_uuid())
            .execute(&mut **tx)
            .await?;
    }

    // Likewise, explicitly clear workspace-scoped historical tables without a
    // foreign key. Legal holds remain deliberately excluded.
    for table in [
        "approvals",
        "channel_sections",
        "info_barriers",
        "keyword_alerts",
        "message_reports",
        "saved_searches",
        "search_click_events",
        "user_groups",
        "workspace_announcements",
        "workspace_deactivations",
        "workspace_mutes",
        "scheduled_streams",
        "digest_subscriptions",
        "workspace_default_channels",
    ] {
        let sql = format!("DELETE FROM {table} WHERE workspace_id = $1");
        sqlx::query(&sql)
            .bind(workspace.to_uuid())
            .execute(&mut **tx)
            .await?;
    }

    // Capture the blob set before erasing the installation-owned retention
    // state. The workspace row is already locked FOR UPDATE, which prevents a
    // same-workspace machine commit from adding a new ledger entry behind this
    // snapshot.
    let integration_blob_ids = sqlx::query_scalar::<_, uuid::Uuid>(
        r"SELECT DISTINCT ledger.blob_id
            FROM integration_blob_ledger ledger
            JOIN integration_installations installation
              ON installation.id = ledger.installation_id
           WHERE installation.workspace_id = $1
           ORDER BY ledger.blob_id",
    )
    .bind(workspace.to_uuid())
    .fetch_all(&mut **tx)
    .await?;

    // Match the integration retention sweeper's complete lock order: machine
    // requests -> receipts -> ledger/blob -> GC queue. Deleting these children
    // explicitly also means the later installation cascade has nothing left to
    // lock in reverse order.
    sqlx::query(
        r"DELETE FROM integration_machine_requests request
            USING integration_installations installation
           WHERE request.installation_id = installation.id
             AND installation.workspace_id = $1",
    )
    .bind(workspace.to_uuid())
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        r"DELETE FROM integration_notification_receipts receipt
            USING integration_installations installation
           WHERE receipt.installation_id = installation.id
             AND installation.workspace_id = $1",
    )
    .bind(workspace.to_uuid())
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        r"DELETE FROM integration_blob_receipts receipt
            USING integration_installations installation
           WHERE receipt.installation_id = installation.id
             AND installation.workspace_id = $1",
    )
    .bind(workspace.to_uuid())
    .execute(&mut **tx)
    .await?;
    let _locked_integration_blobs = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid)>(
        r"SELECT ledger.installation_id, ledger.blob_id
            FROM integration_blob_ledger ledger
            JOIN integration_installations installation
              ON installation.id = ledger.installation_id
            JOIN blobs blob ON blob.id = ledger.blob_id
           WHERE installation.workspace_id = $1
           ORDER BY ledger.created_at, ledger.installation_id, ledger.blob_id
           FOR UPDATE OF ledger, blob",
    )
    .bind(workspace.to_uuid())
    .fetch_all(&mut **tx)
    .await?;
    sqlx::query(
        r"DELETE FROM integration_blob_ledger ledger
            USING integration_installations installation
           WHERE ledger.installation_id = installation.id
             AND installation.workspace_id = $1",
    )
    .bind(workspace.to_uuid())
    .execute(&mut **tx)
    .await?;

    // Queue installation-owned blobs as one workspace-level set before deleting
    // installation rows. A row-level BEFORE DELETE trigger cannot safely infer
    // this for a multi-row DELETE: every installation can still see another
    // installation in this workspace and all of them could skip the enqueue.
    // With this workspace's ledger removed, the remaining-reference checks keep
    // blobs retained elsewhere out of this tenant's cleanup. Never downgrade a
    // stronger pre-existing erasure request.
    sqlx::query(
        r"INSERT INTO blob_gc_queue (blob_id, force_delete)
          SELECT candidate.blob_id, FALSE
            FROM unnest($1::uuid[]) AS candidate(blob_id)
           WHERE NOT EXISTS (
                     SELECT 1 FROM integration_blob_ledger other
                      WHERE other.blob_id = candidate.blob_id
                 )
             AND NOT EXISTS (
                     SELECT 1 FROM integration_blob_receipts receipt
                      WHERE receipt.blob_id = candidate.blob_id
                 )
          ON CONFLICT (blob_id) DO UPDATE
              SET force_delete = blob_gc_queue.force_delete OR EXCLUDED.force_delete",
    )
    .bind(&integration_blob_ids)
    .execute(&mut **tx)
    .await?;

    // Installations are normally removed by the workspace FK cascade, but
    // deleting them explicitly first also releases their bot RESTRICT edge and
    // cascades durable notification receipts before the bot/workspace rows are
    // considered. This keeps hard deletion valid after an integration has
    // published messages.
    sqlx::query("DELETE FROM integration_installations WHERE workspace_id = $1")
        .bind(workspace.to_uuid())
        .execute(&mut **tx)
        .await?;

    sqlx::query("DELETE FROM rooms WHERE workspace_id = $1")
        .bind(workspace.to_uuid())
        .execute(&mut **tx)
        .await?;

    Ok(sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(workspace.to_uuid())
        .execute(&mut **tx)
        .await?
        .rows_affected()
        > 0)
}

fn room_kind_of(s: &str) -> RoomKind {
    match s {
        "direct" => RoomKind::Direct,
        "channel" => RoomKind::Channel,
        _ => RoomKind::Group,
    }
}

#[derive(sqlx::FromRow)]
struct ExportMessageRow {
    id: uuid::Uuid,
    room_id: uuid::Uuid,
    sender_id: uuid::Uuid,
    blocks: serde_json::Value,
    reply_to: Option<uuid::Uuid>,
    metadata: serde_json::Value,
    created_at: time::OffsetDateTime,
    edited_at: Option<time::OffsetDateTime>,
    deleted_at: Option<time::OffsetDateTime>,
    expires_at: Option<time::OffsetDateTime>,
    version: i32,
}

impl From<ExportMessageRow> for Message {
    fn from(r: ExportMessageRow) -> Self {
        let blocks: Vec<aero_common::Block> = serde_json::from_value(r.blocks).unwrap_or_default();
        Self {
            id: MessageId::from_uuid(r.id),
            room_id: RoomId::from_uuid(r.room_id),
            sender_id: ParticipantId::from_uuid(r.sender_id),
            blocks,
            reply_to: r.reply_to.map(MessageId::from_uuid),
            metadata: r.metadata,
            created_at: r.created_at,
            edited_at: r.edited_at,
            deleted_at: r.deleted_at,
            expires_at: r.expires_at,
            version: r.version,
        }
    }
}
