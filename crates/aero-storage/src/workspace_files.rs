//! Workspace-wide file browser — every file/media attachment shared across ALL
//! rooms the caller belongs to within one workspace.
//!
//! The workspace-wide complement to the per-channel
//! [`FileIndexRepo`](crate::FileIndexRepo): instead of one room, it spans every
//! room of a workspace, but only the rooms the caller is a member of. Like the
//! per-room files tab, this repo owns NO table of its own — it is a read-only
//! projection over the existing `messages` table. A `File` block
//! ([`aero_common::Block::File`]) serializes inside a message's `blocks` JSONB
//! array as an object tagged `"type":"file"` carrying `blob_id`, `kind`, `name`,
//! and `size`. [`WorkspaceFileRepo::list_for_workspace`] laterally unnests that
//! array and projects one [`WorkspaceFile`] per file block, newest-message-first.
//!
//! The same effective-access boundary as
//! [`MessageRepo::search_all_rooms_in_workspace`](crate::MessageRepo) intersects
//! room/workspace membership, active account/deactivation state, and mandatory
//! 2FA, so stale room edges cannot leak attachments. There is NO migration and
//! NO new id — it reads existing tables only.
//!
//! Purely additive: a NEW [`WorkspaceFileRepo`]; no existing repo is touched. The
//! [`WorkspaceFile`] model lives here (and is re-exported from the crate root)
//! rather than in `aero-common`, since it is a storage-layer projection, mirroring
//! [`SharedFile`](crate::SharedFile).

use std::fmt::Write as _;
use std::str::FromStr;

use aero_common::{BlobId, MessageId, ParticipantId, RoomId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;

/// The hard ceiling on a single [`WorkspaceFileRepo::list_for_workspace`] page,
/// matching the per-room files cap so a caller can never pull an unbounded set.
const MAX_FILES_LIMIT: i64 = 200;

/// One file/media attachment shared somewhere in a workspace — a projection of a
/// single [`File`](aero_common::Block::File) block within a (non-deleted) message
/// of a room the caller belongs to.
///
/// A storage-layer view, not a stored row, so a handler can hand it straight back
/// as JSON; `created_at` (the parent message's timestamp) renders as RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceFile {
    /// The message the file block was shared in.
    pub message_id: MessageId,
    /// The room the file was shared in (a room the caller belongs to).
    pub room_id: RoomId,
    /// The stored blob the file block references (downloadable via `/api/blobs/:id`).
    pub blob_id: BlobId,
    /// The original filename, verbatim from the block.
    pub filename: String,
    /// The file kind, verbatim from the block (`image` / `video` / `audio` /
    /// `document` / `other`).
    pub kind: String,
    /// Who shared the file (the parent message's sender).
    pub sender_id: ParticipantId,
    /// When the file was shared (the parent message's creation time; RFC 3339 on
    /// the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// The columns a [`WorkspaceFile`] is decoded from, in select order: message id,
/// room id, blob-id text, filename text, kind text, sender id, and the parent
/// message's `created_at`.
type Row = (
    uuid::Uuid,
    uuid::Uuid,
    String,
    Option<String>,
    String,
    uuid::Uuid,
    time::OffsetDateTime,
);

/// Decode one projected row into a [`WorkspaceFile`], or `None` when the block's
/// `blob_id` text fails to parse as a [`BlobId`] or it carries no filename (a
/// malformed block is skipped rather than failing the whole listing).
fn row_to_model(r: Row) -> Option<WorkspaceFile> {
    let (id, room_id, blob_id, filename, kind, sender_id, created_at) = r;
    let blob_id = BlobId::from_str(&blob_id).ok()?;
    let filename = filename?;
    Some(WorkspaceFile {
        message_id: MessageId::from_uuid(id),
        room_id: RoomId::from_uuid(room_id),
        blob_id,
        filename,
        kind,
        sender_id: ParticipantId::from_uuid(sender_id),
        created_at,
    })
}

/// Clamp a requested page size into the supported `[1, MAX_FILES_LIMIT]` range.
/// Pure, so the cap/floor is unit-tested offline (Postgres absent in CI).
fn clamp_limit(limit: i64) -> i64 {
    limit.clamp(1, MAX_FILES_LIMIT)
}

/// Read-only repository projecting the file/media attachments shared across a
/// workspace's rooms out of the existing `messages` table.
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`WorkspaceFileRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct WorkspaceFileRepo {
    pool: PgPool,
}

impl WorkspaceFileRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// List the files shared across `workspace`'s rooms that `caller` belongs to,
    /// newest-message-first.
    ///
    /// Laterally unnests each (non-deleted) message's `blocks` array and projects
    /// one [`WorkspaceFile`] per `File` block (`b->>'type' = 'file'`). The
    /// Effective-access joins are the authorization boundary — only files from
    /// rooms the caller may currently access are returned. An optional `kind`
    /// narrows to one file kind
    /// (exact match, e.g. `image`); an optional `q` narrows by a filename
    /// substring (`ILIKE '%q%'`). A blank/empty filter is ignored (treated as
    /// absent). `limit` is clamped into `[1, MAX_FILES_LIMIT]` and `offset` floored
    /// at `0`. Blocks whose `blob_id` text fails to parse, or that carry no
    /// filename, are skipped.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_workspace(
        &self,
        workspace: WorkspaceId,
        caller: ParticipantId,
        kind: Option<&str>,
        q: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<WorkspaceFile>, sqlx::Error> {
        let limit = clamp_limit(limit);
        let offset = offset.max(0);

        // Treat blank filters as absent so `?kind=` / `?q=` don't match nothing.
        let kind = kind.map(str::trim).filter(|k| !k.is_empty());
        let q = q.map(str::trim).filter(|s| !s.is_empty());

        // Build the statement with positional binds. `$1` is always the caller and
        // `$2` the workspace; optional filters claim the next slots, then
        // LIMIT/OFFSET.
        let mut sql = String::from(
            "SELECT m.id,
                    m.room_id,
                    b->>'blob_id' AS blob_id,
                    b->>'name'    AS filename,
                    b->>'kind'    AS kind,
                    m.sender_id,
                    m.created_at
               FROM messages m
               JOIN rooms r ON m.room_id = r.id
               JOIN workspaces w ON w.id = r.workspace_id
               JOIN room_members rm
                 ON rm.room_id = r.id AND rm.participant_id = $1
               JOIN workspace_members wm
                 ON wm.workspace_id = r.workspace_id AND wm.participant_id = $1
               JOIN participants viewer
                 ON viewer.id = $1 AND viewer.deleted_at IS NULL
               LEFT JOIN workspace_deactivations deactivated
                 ON deactivated.workspace_id = r.workspace_id
                AND deactivated.participant_id = $1
               LEFT JOIN totp_secrets totp ON totp.participant_id = $1
               CROSS JOIN LATERAL jsonb_array_elements(m.blocks) AS b
              WHERE r.workspace_id = $2
                AND deactivated.participant_id IS NULL
                AND (
                    viewer.kind <> 'human'
                    OR NOT w.require_2fa
                    OR COALESCE(totp.activated, false)
                )
                AND m.deleted_at IS NULL
                AND b->>'type' = 'file'",
        );
        // `write!` into the `String` never fails; the `let _ =` discards the
        // always-`Ok` result without an `unwrap` (clippy `format_push_string`).
        let mut idx = 3;
        if kind.is_some() {
            let _ = write!(sql, " AND b->>'kind' = ${idx}");
            idx += 1;
        }
        if q.is_some() {
            let _ = write!(sql, " AND b->>'name' ILIKE '%'||${idx}||'%'");
            idx += 1;
        }
        let _ = write!(
            sql,
            " ORDER BY m.id DESC LIMIT ${} OFFSET ${}",
            idx,
            idx + 1
        );

        let mut query = sqlx::query_as::<_, Row>(&sql)
            .bind(caller.to_uuid())
            .bind(workspace.to_uuid());
        if let Some(k) = kind {
            query = query.bind(k.to_owned());
        }
        if let Some(needle) = q {
            query = query.bind(needle.to_owned());
        }
        let rows = query.bind(limit).bind(offset).fetch_all(&self.pool).await?;
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
        assert_eq!(
            clamp_limit(MAX_FILES_LIMIT),
            MAX_FILES_LIMIT,
            "cap passes through"
        );
        assert_eq!(clamp_limit(10_000), MAX_FILES_LIMIT, "over-cap clamps down");
    }

    #[test]
    fn row_to_model_skips_unparseable_or_nameless() {
        let now = time::OffsetDateTime::now_utc();
        let mid = MessageId::new();
        let room = RoomId::new();
        let sender = ParticipantId::new();
        let blob = BlobId::new();

        // A well-formed row decodes (blob_id is a valid ULID string, name present).
        let ok = row_to_model((
            mid.to_uuid(),
            room.to_uuid(),
            blob.to_string(),
            Some("diagram.png".to_owned()),
            "image".to_owned(),
            sender.to_uuid(),
            now,
        ))
        .expect("valid row decodes");
        assert_eq!(ok.message_id, mid);
        assert_eq!(ok.room_id, room);
        assert_eq!(ok.blob_id, blob);
        assert_eq!(ok.filename, "diagram.png");
        assert_eq!(ok.kind, "image");
        assert_eq!(ok.sender_id, sender);

        // A garbage blob_id text is skipped (None) rather than panicking.
        assert!(
            row_to_model((
                mid.to_uuid(),
                room.to_uuid(),
                "not-a-ulid".to_owned(),
                Some("x.png".to_owned()),
                "image".to_owned(),
                sender.to_uuid(),
                now,
            ))
            .is_none(),
            "unparseable blob_id is skipped"
        );

        // A missing filename is skipped too.
        assert!(
            row_to_model((
                mid.to_uuid(),
                room.to_uuid(),
                blob.to_string(),
                None,
                "image".to_owned(),
                sender.to_uuid(),
                now,
            ))
            .is_none(),
            "nameless block is skipped"
        );
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored workspace_files
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::WorkspaceRepo;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn mk_workspace(p: &PgPool, owner: ParticipantId) -> WorkspaceId {
        WorkspaceRepo::new(p.clone())
            .create(
                format!("Workspace files {owner}"),
                format!("workspace-files-{owner}"),
                owner,
            )
            .await
            .expect("create workspace")
            .id
    }

    async fn enroll(p: &PgPool, workspace: WorkspaceId, participant: ParticipantId) {
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1, $2, 'member')",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .execute(p)
        .await
        .expect("enroll participant");
    }

    /// Create a throwaway participant so the test is self-contained.
    async fn mk_participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("ws-files-actor-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    /// Create a throwaway group room owned by `creator`.
    async fn mk_room(p: &PgPool, workspace: WorkspaceId, creator: ParticipantId) -> RoomId {
        let id = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id) \
             VALUES ($1, 'group', $2, $3, $4)",
        )
        .bind(id.to_uuid())
        .bind(format!("ws-files-room-{id}"))
        .bind(creator.to_uuid())
        .bind(workspace.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        id
    }

    async fn join(p: &PgPool, room: RoomId, who: ParticipantId) {
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role) VALUES ($1,$2,'member')",
        )
        .bind(room.to_uuid())
        .bind(who.to_uuid())
        .execute(p)
        .await
        .expect("insert membership");
    }

    /// Insert a message with an explicit id and raw `blocks` JSON.
    async fn mk_message(
        p: &PgPool,
        id: MessageId,
        room: RoomId,
        sender: ParticipantId,
        blocks: &str,
    ) {
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks) VALUES ($1, $2, $3, $4::jsonb)",
        )
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
    async fn list_is_membership_scoped() {
        let p = pool();
        let repo = WorkspaceFileRepo::new(p.clone());
        let member = mk_participant(&p).await;
        let ws = mk_workspace(&p, member).await;
        let nonmember = mk_participant(&p).await;
        enroll(&p, ws, nonmember).await;
        let room = mk_room(&p, ws, member).await;
        join(&p, room, member).await; // member joins; nonmember does NOT.

        let blob = BlobId::new();
        let mid = MessageId::new();
        let fname = format!("report-{mid}.pdf");
        mk_message(
            &p,
            mid,
            room,
            member,
            &format!(r#"[{{"type":"file","blob_id":"{blob}","kind":"document","name":"{fname}","size":321}}]"#),
        )
        .await;

        // The member sees the file, with its room/blob/kind/filename projected.
        let mine = repo
            .list_for_workspace(ws, member, None, None, 50, 0)
            .await
            .unwrap();
        let found = mine
            .iter()
            .find(|f| f.message_id == mid)
            .expect("member sees the file");
        assert_eq!(found.room_id, room);
        assert_eq!(found.blob_id, blob);
        assert_eq!(found.kind, "document");
        assert_eq!(found.filename, fname);
        assert_eq!(found.sender_id, member);

        // The kind filter narrows: a non-matching kind excludes it, a matching one
        // keeps it.
        assert!(
            !repo
                .list_for_workspace(ws, member, Some("image"), None, 50, 0)
                .await
                .unwrap()
                .iter()
                .any(|f| f.message_id == mid),
            "kind=image excludes the document"
        );
        assert!(
            repo.list_for_workspace(ws, member, Some("document"), None, 50, 0)
                .await
                .unwrap()
                .iter()
                .any(|f| f.message_id == mid),
            "kind=document keeps it"
        );

        // The non-member sees NOTHING from this room — the effective-access joins
        // are the boundary.
        assert!(
            !repo
                .list_for_workspace(ws, nonmember, None, None, 50, 0)
                .await
                .unwrap()
                .iter()
                .any(|f| f.message_id == mid),
            "a non-member never sees the file"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM messages WHERE room_id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM room_members WHERE room_id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(ws.to_uuid())
            .execute(&p)
            .await
            .expect("delete workspace");
        for who in [member, nonmember] {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(who.to_uuid())
                .execute(&p)
                .await
                .ok();
        }
    }
}
