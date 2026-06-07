//! Per-channel "Files tab" index — the file/media attachments shared in a room.
//!
//! This repo owns NO table of its own: it is a read-only projection over the
//! existing `messages` table. A `File` block ([`aero_common::Block::File`])
//! serializes inside a message's `blocks` JSONB array as an object tagged
//! `"type":"file"` carrying `blob_id`, `kind`, `size`, and (optionally) `name`.
//! [`FileIndexRepo::list_for_room`] laterally unnests that array and projects one
//! [`SharedFile`] per file block, newest-message-first, with a keyset `before`
//! cursor for pagination. The GIN index on `messages(blocks)` (migration 0001)
//! covers the block-shape predicate, so no new migration is needed.
//!
//! Purely additive: a NEW [`FileIndexRepo`]; no existing repo is touched. The
//! [`SharedFile`] model lives here (and is re-exported from the crate root)
//! rather than in `aero-common`, since it is a storage-layer projection. This
//! repo does NOT enforce room access — the HTTP layer asserts room membership
//! (via `ImService::assert_room_access`) before calling it.

use std::str::FromStr;

use aero_common::{BlobId, MessageId, ParticipantId, RoomId};
use serde::Serialize;
use sqlx::PgPool;

/// The hard ceiling on a single [`FileIndexRepo::list_for_room`] page, matching
/// the message-history cap so a caller can never pull an unbounded result set.
const MAX_FILES_LIMIT: i64 = 200;

/// One file/media attachment shared in a room — a projection of a single
/// [`File`](aero_common::Block::File) block within a (non-deleted) message.
///
/// A storage-layer view, not a stored row, so a handler can hand it straight back
/// as JSON; `created_at` (the parent message's timestamp) renders as RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct SharedFile {
    /// The message the file block was shared in.
    pub message_id: MessageId,
    /// Who shared the file (the parent message's sender).
    pub sender_id: ParticipantId,
    /// The stored blob the file block references (downloadable via `/api/blobs/:id`).
    pub blob_id: BlobId,
    /// The file kind, verbatim from the block (`image` / `video` / `audio` /
    /// `document` / `other`).
    pub kind: String,
    /// The original filename, if the block carried one.
    pub name: Option<String>,
    /// The file size in bytes, if the block carried one.
    pub size: Option<i64>,
    /// When the file was shared (the parent message's creation time; RFC 3339 on
    /// the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// The columns a [`SharedFile`] is decoded from, in select order: message id,
/// sender id, blob-id text, kind text, optional name text, optional size, and the
/// parent message's `created_at`.
type Row = (
    uuid::Uuid,
    uuid::Uuid,
    String,
    String,
    Option<String>,
    Option<i64>,
    time::OffsetDateTime,
);

/// Decode one projected row into a [`SharedFile`], or `None` when the block's
/// `blob_id` text fails to parse as a [`BlobId`] (a malformed block is skipped
/// rather than failing the whole listing).
fn row_to_model(r: Row) -> Option<SharedFile> {
    let (id, sender_id, blob_id, kind, name, size, created_at) = r;
    let blob_id = BlobId::from_str(&blob_id).ok()?;
    Some(SharedFile {
        message_id: MessageId::from_uuid(id),
        sender_id: ParticipantId::from_uuid(sender_id),
        blob_id,
        kind,
        name,
        size,
        created_at,
    })
}

/// Clamp a requested page size into the supported `[1, MAX_FILES_LIMIT]` range.
/// Pure, so the cap/floor is unit-tested offline (Postgres absent in CI).
fn clamp_limit(limit: i64) -> i64 {
    limit.clamp(1, MAX_FILES_LIMIT)
}

/// Read-only repository projecting the file/media attachments shared in a room
/// out of the existing `messages` table.
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`FileIndexRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct FileIndexRepo {
    pool: PgPool,
}

impl FileIndexRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// List the files shared in `room`, newest-message-first.
    ///
    /// Laterally unnests each (non-deleted) message's `blocks` array and projects
    /// one [`SharedFile`] per `File` block (`b->>'type' = 'file'`). `before` is an
    /// exclusive keyset cursor (`m.id < before`) for paging toward older files;
    /// `None` starts from the newest. `limit` is clamped into
    /// `[1, MAX_FILES_LIMIT]`. Blocks whose `blob_id` text fails to parse are
    /// skipped. Does NOT check room access — the caller must assert membership
    /// first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_room(
        &self,
        room: RoomId,
        before: Option<MessageId>,
        limit: i64,
    ) -> Result<Vec<SharedFile>, sqlx::Error> {
        let limit = clamp_limit(limit);
        let rows = sqlx::query_as::<_, Row>(
            r"SELECT m.id,
                     m.sender_id,
                     b->>'blob_id'        AS blob_id,
                     b->>'kind'           AS kind,
                     b->>'name'           AS name,
                     (b->>'size')::bigint AS size,
                     m.created_at
                FROM messages m, jsonb_array_elements(m.blocks) AS b
               WHERE m.room_id = $1
                 AND m.deleted_at IS NULL
                 AND b->>'type' = 'file'
                 AND ($2::uuid IS NULL OR m.id < $2)
               ORDER BY m.id DESC
               LIMIT $3",
        )
        .bind(room.to_uuid())
        .bind(before.map(|m| m.to_uuid()))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().filter_map(row_to_model).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_limit_floors_and_caps() {
        assert_eq!(clamp_limit(0), 1, "zero floors to 1");
        assert_eq!(clamp_limit(-5), 1, "negative floors to 1");
        assert_eq!(clamp_limit(50), 50, "in-range passes through");
        assert_eq!(clamp_limit(MAX_FILES_LIMIT), MAX_FILES_LIMIT, "cap passes through");
        assert_eq!(clamp_limit(10_000), MAX_FILES_LIMIT, "over-cap clamps down");
    }

    #[test]
    fn row_to_model_skips_unparseable_blob_id() {
        let now = time::OffsetDateTime::now_utc();
        let mid = MessageId::new();
        let sender = ParticipantId::new();
        let blob = BlobId::new();

        // A well-formed row decodes (blob_id is a valid ULID string).
        let ok = row_to_model((
            mid.to_uuid(),
            sender.to_uuid(),
            blob.to_string(),
            "image".to_owned(),
            Some("diagram.png".to_owned()),
            Some(123),
            now,
        ))
        .expect("valid blob_id decodes");
        assert_eq!(ok.message_id, mid);
        assert_eq!(ok.sender_id, sender);
        assert_eq!(ok.blob_id, blob);
        assert_eq!(ok.kind, "image");
        assert_eq!(ok.name.as_deref(), Some("diagram.png"));
        assert_eq!(ok.size, Some(123));

        // A garbage blob_id text is skipped (None) rather than panicking.
        assert!(
            row_to_model((
                mid.to_uuid(),
                sender.to_uuid(),
                "not-a-ulid".to_owned(),
                "image".to_owned(),
                None,
                None,
                now,
            ))
            .is_none(),
            "unparseable blob_id is skipped"
        );
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored file_index
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    /// The reserved all-zero default workspace, guaranteed to exist by migration
    /// 0006's backfill — reused as the seeded room's tenant.
    const DEFAULT_WS: &str = "00000000-0000-0000-0000-000000000000";

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway participant so the test is self-contained.
    async fn mk_participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("file-index-owner-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    /// Create a throwaway channel room owned by `creator`, in the default workspace.
    async fn mk_room(p: &PgPool, creator: ParticipantId) -> RoomId {
        let id = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id) \
             VALUES ($1, 'channel', $2, $3, '00000000-0000-0000-0000-000000000000')",
        )
        .bind(id.to_uuid())
        .bind(format!("file-index-room-{id}"))
        .bind(creator.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        let _ = DEFAULT_WS; // documents the literal tenant used above.
        id
    }

    /// Insert a message with an explicit id and raw `blocks` JSON.
    async fn mk_message(p: &PgPool, id: MessageId, room: RoomId, sender: ParticipantId, blocks: &str) {
        sqlx::query("INSERT INTO messages (id, room_id, sender_id, blocks) VALUES ($1, $2, $3, $4::jsonb)")
            .bind(id.to_uuid())
            .bind(room.to_uuid())
            .bind(sender.to_uuid())
            .bind(blocks)
            .execute(p)
            .await
            .expect("insert message");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn list_for_room_returns_only_files_and_paginates() {
        let p = pool();
        let repo = FileIndexRepo::new(p.clone());
        let owner = mk_participant(&p).await;
        let room = mk_room(&p, owner).await;

        // Two file messages and one plain-text message. Sort the two file ids so
        // the keyset ordering is deterministic regardless of ULID monotonicity.
        let mut file_ids = [MessageId::new(), MessageId::new()];
        file_ids.sort();
        let (older, newer) = (file_ids[0], file_ids[1]);
        let text_id = MessageId::new();

        let blob_a = BlobId::new();
        let blob_b = BlobId::new();
        mk_message(
            &p,
            older,
            room,
            owner,
            &format!(r#"[{{"type":"file","blob_id":"{blob_a}","kind":"image","name":"a.png","size":111}}]"#),
        )
        .await;
        mk_message(
            &p,
            newer,
            room,
            owner,
            &format!(r#"[{{"type":"file","blob_id":"{blob_b}","kind":"document","size":222}}]"#),
        )
        .await;
        mk_message(
            &p,
            text_id,
            room,
            owner,
            r#"[{"type":"text","content":"just talking, no files"}]"#,
        )
        .await;

        // Newest-first, files only: exactly the two file messages, text excluded.
        let all = repo.list_for_room(room, None, 50).await.unwrap();
        assert_eq!(all.len(), 2, "exactly the two file messages are listed");
        assert_eq!(all[0].message_id, newer, "newest file first");
        assert_eq!(all[1].message_id, older, "older file second");
        assert!(
            !all.iter().any(|f| f.message_id == text_id),
            "the plain-text message is never listed"
        );
        // The newer file carried no name; the older one did.
        assert_eq!(all[0].kind, "document");
        assert_eq!(all[0].size, Some(222));
        assert_eq!(all[0].name, None);
        assert_eq!(all[0].blob_id, blob_b);
        assert_eq!(all[1].name.as_deref(), Some("a.png"));
        assert_eq!(all[1].size, Some(111));
        assert_eq!(all[1].blob_id, blob_a);

        // before-cursor paginates: `before = newer` yields only the older file.
        let page = repo.list_for_room(room, Some(newer), 50).await.unwrap();
        assert_eq!(page.len(), 1, "before-newest yields one older file");
        assert_eq!(page[0].message_id, older);
        // Paging past the oldest yields nothing.
        assert!(
            repo.list_for_room(room, Some(older), 50).await.unwrap().is_empty(),
            "before-oldest yields nothing"
        );

        // Cleanup so reruns stay self-contained (messages cascade on room delete,
        // but be explicit; the creator participant has no cascade).
        sqlx::query("DELETE FROM messages WHERE room_id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
