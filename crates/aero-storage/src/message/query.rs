//! Query methods: list_recent, changes_since, list_since, messages_around,
//! list_without_embedding, by_sender, by_sender_paged, recent_workspace.
//!
//! Extracted from `message.rs` as part of REFACTOR_PLAN.md Step 2.

use aero_common::{Message, MessageId, ParticipantId, RoomId, WorkspaceId};

use super::MessageRepo;
use crate::message::orig::MessageRow;

impl MessageRepo {
    pub async fn list_recent(
        &self,
        room: RoomId,
        before: Option<MessageId>,
        limit: i64,
    ) -> Result<Vec<Message>, sqlx::Error> {
        let limit = limit.clamp(1, 200);
        let rows = if let Some(b) = before {
            sqlx::query_as::<_, MessageRow>(
                r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
                   FROM messages
                   WHERE room_id = $1 AND id < $2 AND deleted_at IS NULL AND (expires_at IS NULL OR expires_at > now())
                   ORDER BY id DESC
                   LIMIT $3"#,
            )
            .bind(room.to_uuid())
            .bind(b.to_uuid())
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, MessageRow>(
                r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
                   FROM messages
                   WHERE room_id = $1 AND deleted_at IS NULL AND (expires_at IS NULL OR expires_at > now())
                   ORDER BY id DESC
                   LIMIT $2"#,
            )
            .bind(room.to_uuid())
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };
        Ok(rows.into_iter().map(Message::from).collect())
    }

    /// Messages **edited or deleted** since the `since` instant (ROADMAP 方向一
    /// change-replay): a client offline during an edit/delete to a message it
    /// already holds converges on those mutations on reconnect. Ordered
    /// oldest-change-first. `GREATEST(edited_at, deleted_at)` is the latest mutation
    /// instant (NULL-ignoring), so the predicate ≡ `edited_at > since OR deleted_at >
    /// since` (index-backed by migration 0125). TOMBSTONES ARE INCLUDED (no
    /// `deleted_at IS NULL` filter) — a populated `deleted_at` tells the caller to
    /// remove the message, otherwise replace it.
    ///
    /// REGRESSION GUARD: the `message.rs`→`message/` split rewrote this into an
    /// id-keyed, tombstone-EXCLUDING keyset (that is the PII-scan shape — now
    /// [`scan_after`](Self::scan_after)), silently killing edit/delete reconnect
    /// convergence on `GET /api/rooms/:id/changes`. Restored to the timestamp-keyed
    /// mutation query the route's contract (and migration 0125's index) require.
    pub async fn changes_since(
        &self,
        room: RoomId,
        since: time::OffsetDateTime,
        limit: i64,
    ) -> Result<Vec<Message>, sqlx::Error> {
        let limit = limit.clamp(1, 200);
        let rows: Vec<MessageRow> = sqlx::query_as(
            r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
               FROM messages
               WHERE room_id = $1 AND GREATEST(edited_at, deleted_at) > $2
               ORDER BY GREATEST(edited_at, deleted_at) ASC
               LIMIT $3"#,
        )
        .bind(room.to_uuid())
        .bind(since)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Message::from).collect())
    }

    /// Keyset scan of LIVE (non-deleted) messages with `id > after`, ascending,
    /// optionally scoped to one room (`None` = workspace-wide / all rooms). Used by
    /// the report-only PII backfill scan to page through historical message text;
    /// `deleted_at IS NULL` skips tombstones (their text is already cleared). This is
    /// the id-keyed query the split had (mis)named `changes_since`.
    pub async fn scan_after(
        &self,
        room: Option<RoomId>,
        after: MessageId,
        limit: i64,
    ) -> Result<Vec<Message>, sqlx::Error> {
        let limit = limit.clamp(1, 200);
        let rows: Vec<MessageRow> = if let Some(r) = room {
            sqlx::query_as(
                r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
                   FROM messages
                   WHERE room_id = $1 AND id > $2 AND deleted_at IS NULL AND (expires_at IS NULL OR expires_at > now())
                   ORDER BY id
                   LIMIT $3"#,
            )
            .bind(r.to_uuid())
            .bind(after.to_uuid())
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as(
                r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
                   FROM messages
                   WHERE id > $1 AND deleted_at IS NULL AND (expires_at IS NULL OR expires_at > now())
                   ORDER BY id
                   LIMIT $2"#,
            )
            .bind(after.to_uuid())
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };
        Ok(rows.into_iter().map(Message::from).collect())
    }

    /// Return up to `limit` messages after `since` (exclusive), ascending by id,
    /// for a given room, *including* soft-deleted ones (reconnect backfill needs to
    /// tell clients about messages deleted while they were offline). This is a
    /// KEYSET page: the caller continues by passing the last returned id as the
    /// next `since`, stopping when a page comes back shorter than `limit`. `limit`
    /// is the caller's contract — callers MUST size it so a full page is a reliable
    /// "there may be more" signal (e.g. the WS backfill asks for `cap + 1` to
    /// detect a truncated replay; the REST `?since=` route passes its clamped page
    /// size). Previously this hardcoded `LIMIT 500` and ignored the caller's limit,
    /// which silently truncated any continuation past 500 with no signal.
    pub async fn list_since(
        &self,
        room: RoomId,
        since: MessageId,
        limit: i64,
    ) -> Result<Vec<Message>, sqlx::Error> {
        let rows: Vec<MessageRow> = sqlx::query_as(
            r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
               FROM messages
               WHERE room_id = $1 AND id > $2
                 AND (expires_at IS NULL OR expires_at > now())
               ORDER BY id
               LIMIT $3"#,
        )
        .bind(room.to_uuid())
        .bind(since.to_uuid())
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Message::from).collect())
    }

    /// Messages around a cursor (for "jump to message" context view).
    pub async fn messages_around(
        &self,
        room: RoomId,
        around: MessageId,
        half_window: i64,
    ) -> Result<(Vec<Message>, bool), sqlx::Error> {
        let half_window = half_window.clamp(1, 100);
        let before: Vec<MessageRow> = sqlx::query_as(
            r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
               FROM messages
               WHERE room_id = $1 AND id < $2 AND deleted_at IS NULL AND (expires_at IS NULL OR expires_at > now())
               ORDER BY id DESC
               LIMIT $3"#,
        )
        .bind(room.to_uuid())
        .bind(around.to_uuid())
        .bind(half_window)
        .fetch_all(&self.pool)
        .await?;
        // The anchor message itself (`id = around`). The message.rs→message/ split
        // fetched only the before/after windows and dropped this, so the permalink
        // "jump to message" context omitted its own target. Restored.
        let target: Option<MessageRow> = sqlx::query_as(
            r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
               FROM messages
               WHERE room_id = $1 AND id = $2 AND deleted_at IS NULL AND (expires_at IS NULL OR expires_at > now())"#,
        )
        .bind(room.to_uuid())
        .bind(around.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        let after: Vec<MessageRow> = sqlx::query_as(
            r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
               FROM messages
               WHERE room_id = $1 AND id > $2 AND deleted_at IS NULL AND (expires_at IS NULL OR expires_at > now())
               ORDER BY id
               LIMIT $3"#,
        )
        .bind(room.to_uuid())
        .bind(around.to_uuid())
        .bind(half_window)
        .fetch_all(&self.pool)
        .await?;
        // `has_more` must reflect EITHER side hitting its window cap, not just the
        // older (`before`) side: a target with few older but many newer messages
        // truncates `after`, and signalling only on `before` would tell the client
        // "nothing more" while newer messages stay unloaded. Capture both lengths
        // before the rows are consumed below.
        let has_more =
            before.len() >= half_window as usize || after.len() >= half_window as usize;
        let mut all: Vec<Message> = before.into_iter().map(Message::from).collect();
        all.reverse();
        all.extend(target.into_iter().map(Message::from));
        all.extend(after.into_iter().map(Message::from));
        Ok((all, has_more))
    }

    /// Messages missing embeddings (for backfill).
    pub async fn list_without_embedding(&self, limit: i64) -> Result<Vec<Message>, sqlx::Error> {
        let limit = limit.clamp(1, 500);
        let rows = sqlx::query_as::<_, MessageRow>(
            // `searchable_text <> ''` is REQUIRED, not optional: a text-less message
            // (e.g. an image/file block with no caption) can never get a meaningful
            // embedding, so without this filter the 300s backfill loop re-selects and
            // re-enqueues it every cycle forever — perpetual embedding-API spend. The
            // message.rs→message/ split dropped this term; restored.
            r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
               FROM messages
               WHERE embedding IS NULL AND deleted_at IS NULL AND (expires_at IS NULL OR expires_at > now()) AND searchable_text <> ''
               ORDER BY created_at ASC
               LIMIT $1"#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Message::from).collect())
    }

    /// Messages authored by `sender`, bounded, newest-first.
    pub async fn by_sender(
        &self,
        sender: ParticipantId,
        limit: i64,
    ) -> Result<Vec<Message>, sqlx::Error> {
        let limit = limit.clamp(1, crate::message::orig::EXPORT_SENDER_CAP);
        let rows = sqlx::query_as::<_, MessageRow>(
            r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
               FROM messages
               WHERE sender_id = $1 AND deleted_at IS NULL AND (expires_at IS NULL OR expires_at > now())
               ORDER BY id DESC
               LIMIT $2"#,
        )
        .bind(sender.to_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Message::from).collect())
    }

    /// Messages authored by `sender`, keyset-paginated by `before` (exclusive,
    /// pass `MessageId::from_uuid(uuid::Uuid::max())` for the first page).
    pub async fn by_sender_paged(
        &self,
        sender: ParticipantId,
        before: MessageId,
        limit: i64,
    ) -> Result<Vec<Message>, sqlx::Error> {
        let limit = limit.clamp(1, 500);
        let rows = sqlx::query_as::<_, MessageRow>(
            r#"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at, expires_at
               FROM messages
               WHERE sender_id = $1 AND id < $2
               ORDER BY id DESC
               LIMIT $3"#,
        )
        .bind(sender.to_uuid())
        .bind(before.to_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Message::from).collect())
    }

    /// The most recent messages a participant can see across a workspace —
    /// bounded by `limit`, newest first. Used by workspace summarization
    /// (`AiService::summarize_workspace`) and workspace RAG.
    pub async fn recent_workspace(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        limit: i64,
    ) -> Result<Vec<Message>, sqlx::Error> {
        let limit = limit.clamp(1, 200);
        let rows = sqlx::query_as::<_, MessageRow>(
            r#"SELECT m.id, m.room_id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                     m.created_at, m.edited_at, m.deleted_at, m.expires_at
               FROM messages m
               JOIN rooms r ON r.id = m.room_id
               JOIN room_members rm ON rm.room_id = m.room_id AND rm.participant_id = $1
               WHERE m.deleted_at IS NULL
                 AND (m.expires_at IS NULL OR m.expires_at > now())
                 AND r.workspace_id = $2
               ORDER BY m.id DESC
               LIMIT $3"#,
        )
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Message::from).collect())
    }
}
