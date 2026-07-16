//! Workspace GDPR export and hard-delete (erasure).

use aero_common::{
    AuditId, Message, MessageId, ParticipantId, Room, RoomId, RoomKind, Workspace,
    WorkspaceMember, WorkspaceRole,
};

use crate::audit::AuditEvent;

use super::{WorkspaceRepo, EXPORT_MESSAGES_PER_ROOM, RoomExport, WorkspaceExport};

impl WorkspaceRepo {
    /// Export a self-contained snapshot of one tenant (GDPR portability).
    pub async fn export(
        &self,
        workspace: aero_common::WorkspaceId,
    ) -> Result<Option<WorkspaceExport>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        let ws_row = sqlx::query_as::<
            _,
            (uuid::Uuid, String, String, Option<uuid::Uuid>, time::OffsetDateTime,
             Option<String>, Option<String>, Option<String>, Option<String>),
        >(
            r"SELECT id, name, slug, created_by, created_at,
                     logo_url, color_scheme, custom_domain, description
                FROM workspaces WHERE id = $1",
        )
        .bind(workspace.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some((id, name, slug, created_by, created_at, logo_url, color_scheme, custom_domain, description)) = ws_row else {
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
            (uuid::Uuid, String, Option<String>, uuid::Uuid, time::OffsetDateTime),
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

            let message_cap_hit = i64::try_from(msg_rows.len()).unwrap_or(i64::MAX) > EXPORT_MESSAGES_PER_ROOM;
            if message_cap_hit {
                msg_rows.truncate(usize::try_from(EXPORT_MESSAGES_PER_ROOM).unwrap_or(usize::MAX));
            }
            msg_rows.reverse();
            let messages = msg_rows.into_iter().map(Message::from).collect();
            rooms.push(RoomExport { room, messages, message_cap_hit });
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
    pub async fn delete(&self, workspace: aero_common::WorkspaceId) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        // Several ROOM-scoped tables carry a `room_id` but have NO foreign key to
        // `rooms` (historical oversight), so the `DELETE FROM rooms` below does NOT
        // cascade to them — they would be ORPHANED, leaving user content alive after
        // the tenant is gone. Clear them explicitly first, scoped to this workspace's
        // rooms. (`legal_holds` deliberately excluded — compliance preservation
        // records are not auto-destroyed by a tenant delete.)
        // REGRESSION GUARD: the workspace.rs→workspace/ split gutted this list down to
        // two tables; the full set is restored below (GDPR erasure completeness — a
        // dropped table leaves orphaned user content / analytics PII keyed to a gone
        // tenant). Any room_id-keyed non-FK table added later belongs here.
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
            let sql = format!("DELETE FROM {table} WHERE room_id IN (SELECT id FROM rooms WHERE workspace_id = $1)");
            sqlx::query(&sql)
                .bind(workspace.to_uuid())
                .execute(&mut *tx)
                .await?;
        }

        // Likewise, WORKSPACE-scoped tables that carry a `workspace_id` with NO FK to
        // `workspaces` — `DELETE FROM workspaces` (below) does NOT cascade to them, so
        // they orphan too. Clear them by workspace_id. (`ai_jobs` excluded — transient
        // queue rows; `legal_holds` excluded — compliance preservation records.)
        // REGRESSION GUARD: the split kept only `keyword_alerts`; the full set is
        // restored (esp. `search_click_events` analytics PII, whose cleanup was added
        // by commit 51759c2 and silently re-dropped by the split). Any workspace_id-
        // keyed non-FK table added later belongs here (db_test guards keyword_alerts).
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
                .execute(&mut *tx)
                .await?;
        }

        sqlx::query("DELETE FROM rooms WHERE workspace_id = $1")
            .bind(workspace.to_uuid())
            .execute(&mut *tx)
            .await?;

        let result = sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(workspace.to_uuid())
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }
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
