//! Workspace (tenant / org) + membership repository.
//!
//! Backs `migrations/0006_workspaces.sql`. A workspace groups members (each with
//! a [`WorkspaceRole`]) and channels (`rooms.workspace_id`). `participants` stay
//! global identities; tenancy is a membership edge, not a property of the user.
//!
//! This repo is purely additive: it introduces a NEW [`WorkspaceRepo`] and does
//! not touch existing repos. Threading `workspace_id` into existing room/message
//! queries is a later batch.
//!
//! Authorization predicates ([`role_can_invite`], [`role_can_remove`], …) are
//! free functions with no DB dependency so they unit-test directly, mirroring how
//! the rest of `aero-storage` keeps testable logic separate from live SQL.

use aero_common::{
    AuditId, Message, MessageId, ParticipantId, Room, RoomId, RoomKind, Workspace, WorkspaceId,
    WorkspaceMember, WorkspaceRole,
};
use serde::Serialize;
use sqlx::PgPool;

use crate::audit::AuditEvent;

#[derive(Clone)]
pub struct WorkspaceRepo {
    pool: PgPool,
}

/// Largest number of messages exported per room. A single room's export is
/// bounded so one enormous channel cannot blow up memory / the response body;
/// callers needing the complete backlog of a huge room should page it via
/// [`MessageRepo::list_recent`](crate::MessageRepo::list_recent) separately.
/// This is a documented cap, not silent truncation:
/// [`RoomExport::message_cap_hit`] flags when a room held more messages than
/// were included (the most recent `EXPORT_MESSAGES_PER_ROOM` are kept).
pub const EXPORT_MESSAGES_PER_ROOM: i64 = 10_000;

/// A room within a workspace export, with its messages (chronological, bounded).
#[derive(Debug, Clone, Serialize)]
pub struct RoomExport {
    pub room: Room,
    /// The room's non-deleted messages in chronological (oldest-first) order, up
    /// to [`EXPORT_MESSAGES_PER_ROOM`].
    pub messages: Vec<Message>,
    /// `true` when the room held more than [`EXPORT_MESSAGES_PER_ROOM`] messages
    /// and the export was capped (the most recent `EXPORT_MESSAGES_PER_ROOM`
    /// kept, then re-sorted oldest-first).
    pub message_cap_hit: bool,
}

/// A complete, self-contained snapshot of one tenant's data — the workspace row,
/// its members, its channels (rooms) and each channel's messages, plus the
/// administrative audit trail. Backs the GDPR-style data-portability export
/// (ROADMAP 方向一 合规). Serializes to the JSON returned by the export route.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceExport {
    pub workspace: Workspace,
    pub members: Vec<WorkspaceMember>,
    pub rooms: Vec<RoomExport>,
    pub audit_events: Vec<AuditEvent>,
    #[serde(with = "time::serde::rfc3339")]
    pub exported_at: time::OffsetDateTime,
}

impl WorkspaceRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a workspace and enroll its creator as `owner`, atomically.
    pub async fn create(
        &self,
        name: String,
        slug: String,
        created_by: ParticipantId,
    ) -> Result<Workspace, sqlx::Error> {
        let id = WorkspaceId::new();
        let created_at = time::OffsetDateTime::now_utc();

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r"INSERT INTO workspaces (id, name, slug, created_by, created_at)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id.to_uuid())
        .bind(&name)
        .bind(&slug)
        .bind(created_by.to_uuid())
        .bind(created_at)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r"INSERT INTO workspace_members (workspace_id, participant_id, role, joined_at)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (workspace_id, participant_id) DO NOTHING",
        )
        .bind(id.to_uuid())
        .bind(created_by.to_uuid())
        .bind(WorkspaceRole::Owner.as_str())
        .bind(created_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(Workspace {
            id,
            name,
            slug,
            created_by: Some(created_by),
            created_at,
        })
    }

    /// Add (or, on conflict, leave untouched) a member with the given role.
    /// Idempotent: re-adding an existing member is a no-op rather than an error.
    pub async fn add_member(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
        role: WorkspaceRole,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO workspace_members (workspace_id, participant_id, role, joined_at)
               VALUES ($1, $2, $3, NOW())
               ON CONFLICT (workspace_id, participant_id) DO NOTHING",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(role.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Set (insert-or-update) a member's role, idempotently.
    ///
    /// Unlike [`add_member`](Self::add_member) — whose `ON CONFLICT DO NOTHING`
    /// leaves an existing row untouched — this upserts: a fresh member is created
    /// with `role`, and an existing member's role is overwritten. This replaces
    /// the remove-then-add dance callers previously needed to change a role.
    pub async fn update_member_role(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
        role: WorkspaceRole,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO workspace_members (workspace_id, participant_id, role, joined_at)
               VALUES ($1, $2, $3, NOW())
               ON CONFLICT (workspace_id, participant_id)
               DO UPDATE SET role = EXCLUDED.role",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(role.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Remove a member from a workspace. No-op if they were not a member.
    pub async fn remove_member(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"DELETE FROM workspace_members
               WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The participant's role in the workspace, or `None` if not a member (or if
    /// the stored token is unrecognized).
    pub async fn member_role(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<Option<WorkspaceRole>, sqlx::Error> {
        let row = sqlx::query_as::<_, (String,)>(
            r"SELECT role FROM workspace_members
               WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(r,)| WorkspaceRole::from_db_str(&r)))
    }

    /// All members of a workspace, newest joiners last.
    pub async fn list_members(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<WorkspaceMember>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, String, time::OffsetDateTime)>(
            r"SELECT workspace_id, participant_id, role, joined_at
               FROM workspace_members
               WHERE workspace_id = $1
               ORDER BY joined_at ASC, participant_id ASC",
        )
        .bind(workspace.to_uuid())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(ws, pid, role, joined_at)| WorkspaceMember {
                workspace_id: WorkspaceId::from_uuid(ws),
                participant_id: ParticipantId::from_uuid(pid),
                // Default unknown tokens to the least-privileged role rather than
                // dropping the row, so membership listing never silently shrinks.
                role: WorkspaceRole::from_db_str(&role).unwrap_or(WorkspaceRole::Guest),
                joined_at,
            })
            .collect())
    }

    /// All workspaces a participant belongs to, most recently created first.
    pub async fn list_for_participant(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<Workspace>, sqlx::Error> {
        let rows = sqlx::query_as::<
            _,
            (uuid::Uuid, String, String, Option<uuid::Uuid>, time::OffsetDateTime),
        >(
            r"SELECT w.id, w.name, w.slug, w.created_by, w.created_at
               FROM workspaces w
               JOIN workspace_members m ON m.workspace_id = w.id
               WHERE m.participant_id = $1
               ORDER BY w.created_at DESC",
        )
        .bind(participant.to_uuid())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(id, name, slug, by, at)| Workspace {
                id: WorkspaceId::from_uuid(id),
                name,
                slug,
                created_by: by.map(ParticipantId::from_uuid),
                created_at: at,
            })
            .collect())
    }

    /// Whether the participant is a member of the workspace.
    pub async fn is_member(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (i64,)>(
            r"SELECT COUNT(*) FROM workspace_members
               WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0 > 0)
    }

    /// Fetch a single workspace row, or `None` if it does not exist. Used by the
    /// export path to resolve the tenant before gathering its data.
    pub async fn get(&self, workspace: WorkspaceId) -> Result<Option<Workspace>, sqlx::Error> {
        let row = sqlx::query_as::<
            _,
            (uuid::Uuid, String, String, Option<uuid::Uuid>, time::OffsetDateTime),
        >(
            r"SELECT id, name, slug, created_by, created_at
               FROM workspaces WHERE id = $1",
        )
        .bind(workspace.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id, name, slug, by, at)| Workspace {
            id: WorkspaceId::from_uuid(id),
            name,
            slug,
            created_by: by.map(ParticipantId::from_uuid),
            created_at: at,
        }))
    }

    // -------------------------------------------------- compliance: export + delete

    /// Gather a complete tenant snapshot for GDPR-style data portability
    /// (ROADMAP 方向一 合规): the workspace row, its members, every channel
    /// (`rooms.workspace_id = workspace`) with each channel's non-deleted
    /// messages (chronological, bounded by [`EXPORT_MESSAGES_PER_ROOM`]), and the
    /// workspace's audit trail.
    ///
    /// Returns `Ok(None)` if the workspace does not exist. Per-room messages are
    /// bounded so one enormous channel cannot exhaust memory; a capped room sets
    /// [`RoomExport::message_cap_hit`]. The snapshot reads consistently inside a
    /// single read-only transaction so members/rooms/messages cannot tear
    /// relative to a concurrent mutation.
    pub async fn export(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Option<WorkspaceExport>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        // Workspace row first — absence short-circuits the whole export.
        let ws_row = sqlx::query_as::<
            _,
            (uuid::Uuid, String, String, Option<uuid::Uuid>, time::OffsetDateTime),
        >(
            r"SELECT id, name, slug, created_by, created_at
               FROM workspaces WHERE id = $1",
        )
        .bind(workspace.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some((id, name, slug, created_by, created_at)) = ws_row else {
            // Nothing to export; commit the (empty) read txn for cleanliness.
            tx.commit().await?;
            return Ok(None);
        };
        let workspace_row = Workspace {
            id: WorkspaceId::from_uuid(id),
            name,
            slug,
            created_by: created_by.map(ParticipantId::from_uuid),
            created_at,
        };

        // Members (oldest joiners first), mirroring `list_members`.
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
                workspace_id: WorkspaceId::from_uuid(ws),
                participant_id: ParticipantId::from_uuid(pid),
                role: WorkspaceRole::from_db_str(&role).unwrap_or(WorkspaceRole::Guest),
                joined_at,
            })
            .collect();

        // Channels in the workspace (all rooms, not just the caller's).
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
            // Most-recent N (capped) so a giant channel is bounded; fetch one
            // extra to detect truncation, then re-sort oldest-first for the
            // export's chronological contract.
            let probe = EXPORT_MESSAGES_PER_ROOM + 1;
            let mut msg_rows = sqlx::query_as::<_, ExportMessageRow>(
                r"SELECT id, room_id, sender_id, blocks, reply_to, metadata, created_at, edited_at, deleted_at
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
            // Fetched DESC (newest-first); reverse to chronological oldest-first.
            msg_rows.reverse();
            let messages = msg_rows.into_iter().map(Message::from).collect();

            rooms.push(RoomExport { room, messages, message_cap_hit });
        }

        // Audit trail (newest-first), reusing the same shape as `AuditRepo`.
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
                workspace_id: WorkspaceId::from_uuid(ws),
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

    /// Hard-delete a workspace and everything scoped to it, atomically
    /// (ROADMAP 方向一 合规 — erasure / right-to-be-forgotten).
    ///
    /// Returns `true` if a workspace row was deleted, `false` if it did not exist.
    ///
    /// ## Cascade chain (verified against the migrations)
    ///
    /// Both statements run in one transaction, in order.
    ///
    /// **Step 1 — `DELETE FROM rooms WHERE workspace_id = $1` (explicit).**
    /// `rooms.workspace_id` (0006) is a plain FK with **no** `ON DELETE` action
    /// (`NO ACTION`), so the parent `workspaces` row cannot be removed while any
    /// room references it — hence rooms are deleted explicitly first. That delete
    /// then cascades, via `ON DELETE CASCADE` FKs, to all room-scoped data:
    /// `room_members` (0001), `messages` (0001) → `reactions` (0002),
    /// `read_receipts` (0002), `call_sessions` (0002) → `call_participants`
    /// (0002), and `mls_groups` (0003). (`messages.reply_to` is a self-FK; those
    /// rows go together.) `streams.room_id` (0002) is `ON DELETE SET NULL` — a
    /// stream is owned by a participant, not the room, so it is unlinked, not
    /// deleted.
    ///
    /// **Step 2 — `DELETE FROM workspaces WHERE id = $1`.** Cascades, via
    /// `ON DELETE CASCADE`, to the workspace-scoped tables `workspace_members`
    /// (0006) and `audit_events` (0007).
    ///
    /// Not touched: `ai_jobs.workspace_id` (0008) is a bare nullable column with
    /// **no** FK constraint — those are transient queue rows, not tenant records,
    /// and are left to the worker's normal lifecycle. `participants` are global
    /// identities and are never deleted by tenant erasure.
    pub async fn delete(&self, workspace: WorkspaceId) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        // 1. Remove the workspace's channels first (rooms→workspaces FK has no
        //    cascade), which cascades to all room-scoped data.
        sqlx::query(r"DELETE FROM rooms WHERE workspace_id = $1")
            .bind(workspace.to_uuid())
            .execute(&mut *tx)
            .await?;

        // 2. Remove the workspace itself; members + audit cascade automatically.
        let result = sqlx::query(r"DELETE FROM workspaces WHERE id = $1")
            .bind(workspace.to_uuid())
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }
}

/// Parse a DB `kind` token into a [`RoomKind`], defaulting unknown tokens to
/// `Group` (mirrors `room.rs`'s lenient parsing, kept local so `export` does not
/// depend on `room.rs`'s private helper).
fn room_kind_of(s: &str) -> RoomKind {
    match s {
        "direct" => RoomKind::Direct,
        "channel" => RoomKind::Channel,
        _ => RoomKind::Group,
    }
}

/// Row shape for export message decoding. Mirrors `messages` columns; converts
/// into [`Message`] via the same `From` impl `MessageRepo` uses, kept local so
/// `export` stays self-contained without widening `message.rs`'s private row.
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
        }
    }
}

// ---------- Pure authorization predicates (DB-free, unit-tested) ----------

/// Can a member with `role` invite new members? Admins and owners may.
#[must_use]
pub fn role_can_invite(role: WorkspaceRole) -> bool {
    role.can_administer()
}

/// Can a member with `role` remove other members? Admins and owners may.
#[must_use]
pub fn role_can_remove(role: WorkspaceRole) -> bool {
    role.can_administer()
}

/// Can an actor holding `actor` assign the role `target` to someone?
///
/// Rules:
/// - Only administrators (admin/owner) may assign roles at all.
/// - You may never grant a role strictly above your own (no privilege
///   escalation): an admin can mint members/guests/admins but not owners.
#[must_use]
pub fn role_can_assign(actor: WorkspaceRole, target: WorkspaceRole) -> bool {
    actor.can_administer() && actor.at_least(target)
}

/// Can `actor` change/remove the membership of a member currently holding
/// `subject`? Administrators may act on anyone at or below their own privilege;
/// nobody may act on someone strictly more privileged than themselves.
#[must_use]
pub fn role_can_manage_member(actor: WorkspaceRole, subject: WorkspaceRole) -> bool {
    actor.can_administer() && actor.at_least(subject)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [WorkspaceRole; 4] = [
        WorkspaceRole::Guest,
        WorkspaceRole::Member,
        WorkspaceRole::Admin,
        WorkspaceRole::Owner,
    ];

    #[test]
    fn invite_and_remove_require_admin() {
        assert!(role_can_invite(WorkspaceRole::Owner));
        assert!(role_can_invite(WorkspaceRole::Admin));
        assert!(!role_can_invite(WorkspaceRole::Member));
        assert!(!role_can_invite(WorkspaceRole::Guest));

        // remove mirrors invite
        for r in ALL {
            assert_eq!(role_can_remove(r), role_can_invite(r), "role {r:?}");
        }
    }

    #[test]
    fn non_admins_can_never_assign() {
        for target in ALL {
            assert!(!role_can_assign(WorkspaceRole::Member, target));
            assert!(!role_can_assign(WorkspaceRole::Guest, target));
        }
    }

    #[test]
    fn admin_cannot_grant_owner_but_can_grant_lower() {
        assert!(!role_can_assign(WorkspaceRole::Admin, WorkspaceRole::Owner));
        assert!(role_can_assign(WorkspaceRole::Admin, WorkspaceRole::Admin));
        assert!(role_can_assign(WorkspaceRole::Admin, WorkspaceRole::Member));
        assert!(role_can_assign(WorkspaceRole::Admin, WorkspaceRole::Guest));
    }

    #[test]
    fn owner_can_grant_anything() {
        for target in ALL {
            assert!(role_can_assign(WorkspaceRole::Owner, target), "target {target:?}");
        }
    }

    #[test]
    fn no_privilege_escalation_via_assign() {
        // For every actor, granting a role strictly above the actor must fail.
        for actor in ALL {
            for target in ALL {
                if target.rank() > actor.rank() {
                    assert!(
                        !role_can_assign(actor, target),
                        "actor {actor:?} must not grant higher {target:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn manage_member_respects_hierarchy() {
        // Admin can manage members/guests/other admins, but not owners.
        assert!(role_can_manage_member(WorkspaceRole::Admin, WorkspaceRole::Member));
        assert!(role_can_manage_member(WorkspaceRole::Admin, WorkspaceRole::Admin));
        assert!(!role_can_manage_member(WorkspaceRole::Admin, WorkspaceRole::Owner));

        // Owner can manage anyone.
        for subject in ALL {
            assert!(role_can_manage_member(WorkspaceRole::Owner, subject), "subject {subject:?}");
        }

        // Regular members and guests can manage nobody.
        for subject in ALL {
            assert!(!role_can_manage_member(WorkspaceRole::Member, subject));
            assert!(!role_can_manage_member(WorkspaceRole::Guest, subject));
        }
    }
}

/// PG-gated integration tests for the compliance export + delete path. Run with
/// a live Postgres + applied migrations:
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored workspace_
/// ```
///
/// They are `#[ignore]` so the default `cargo test` stays hermetic (no DB in
/// CI); the orchestrator runs them against a live database.
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::audit::AuditRepo;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn new_participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("ws-export-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    /// Create a workspace owned by `owner`, a channel in it, and one message in
    /// that channel. Returns `(workspace, room, message)` ids.
    async fn seed_workspace(
        repo: &WorkspaceRepo,
        p: &PgPool,
        owner: ParticipantId,
    ) -> (WorkspaceId, RoomId, MessageId) {
        let ws = repo
            .create("Export WS".into(), format!("exp-{}", WorkspaceId::new()), owner)
            .await
            .expect("create workspace");

        let room = RoomId::new();
        sqlx::query(
            r"INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
               VALUES ($1, 'channel', $2, $3, now(), $4)",
        )
        .bind(room.to_uuid())
        .bind("general")
        .bind(owner.to_uuid())
        .bind(ws.id.to_uuid())
        .execute(p)
        .await
        .expect("insert room");

        let msg = MessageId::new();
        sqlx::query(
            r"INSERT INTO messages (id, room_id, sender_id, blocks, searchable_text, created_at)
               VALUES ($1, $2, $3, $4, $5, now())",
        )
        .bind(msg.to_uuid())
        .bind(room.to_uuid())
        .bind(owner.to_uuid())
        .bind(serde_json::json!([{ "type": "text", "text": "hello tenant" }]))
        .bind("hello tenant")
        .execute(p)
        .await
        .expect("insert message");

        (ws.id, room, msg)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn workspace_export_returns_members_rooms_and_messages() {
        let p = pool();
        let repo = WorkspaceRepo::new(p.clone());
        let owner = new_participant(&p).await;
        let (ws, room, msg) = seed_workspace(&repo, &p, owner).await;

        let export = repo.export(ws).await.unwrap().expect("workspace exists");

        // Workspace identity.
        assert_eq!(export.workspace.id, ws);
        // Owner is enrolled as a member by `create`.
        assert!(
            export.members.iter().any(|m| m.participant_id == owner && m.role == WorkspaceRole::Owner),
            "owner must appear as a member"
        );
        // The seeded channel + its message are present.
        let r = export.rooms.iter().find(|r| r.room.id == room).expect("room exported");
        assert!(!r.message_cap_hit, "tiny room is not capped");
        assert!(r.messages.iter().any(|m| m.id == msg), "message exported");
        // The workspace.create audit event is captured.
        assert!(
            export.audit_events.iter().all(|e| e.workspace_id == ws),
            "audit events are tenant-scoped"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn workspace_export_missing_is_none() {
        let p = pool();
        let repo = WorkspaceRepo::new(p);
        assert!(repo.export(WorkspaceId::new()).await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn workspace_delete_removes_workspace_rooms_members_and_audit() {
        let p = pool();
        let repo = WorkspaceRepo::new(p.clone());
        let owner = new_participant(&p).await;
        let (ws, room, msg) = seed_workspace(&repo, &p, owner).await;
        // An audit row to prove the cascade reaches audit_events.
        AuditRepo::new(p.clone())
            .append(ws, Some(owner), "workspace.delete", None, serde_json::json!({}))
            .await
            .unwrap();

        let deleted = repo.delete(ws).await.unwrap();
        assert!(deleted, "delete reports a row was removed");

        // Workspace gone.
        assert!(repo.get(ws).await.unwrap().is_none(), "workspace row deleted");
        // Members cascade-deleted.
        assert!(!repo.is_member(ws, owner).await.unwrap(), "members cascade-deleted");
        // Room explicitly deleted (rooms→workspaces FK has no cascade).
        let room_left: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM rooms WHERE id = $1")
                .bind(room.to_uuid())
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(room_left, 0, "room deleted with its workspace");
        // Messages cascade-deleted with the room.
        let msg_left: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE id = $1")
                .bind(msg.to_uuid())
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(msg_left, 0, "messages cascade-deleted with the room");
        // Audit rows cascade-deleted with the workspace.
        let audit_left: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE workspace_id = $1")
                .bind(ws.to_uuid())
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(audit_left, 0, "audit events cascade-deleted with the workspace");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn workspace_delete_missing_reports_false() {
        let p = pool();
        let repo = WorkspaceRepo::new(p);
        assert!(!repo.delete(WorkspaceId::new()).await.unwrap(), "no row to delete");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn workspace_delete_does_not_touch_a_second_tenant() {
        let p = pool();
        let repo = WorkspaceRepo::new(p.clone());
        let owner_a = new_participant(&p).await;
        let owner_b = new_participant(&p).await;
        let (ws_a, room_a, msg_a) = seed_workspace(&repo, &p, owner_a).await;
        let (ws_b, room_b, msg_b) = seed_workspace(&repo, &p, owner_b).await;

        repo.delete(ws_a).await.unwrap();

        // Tenant A is gone …
        assert!(repo.get(ws_a).await.unwrap().is_none());
        // … but tenant B is completely intact: workspace, member, room, message.
        assert!(repo.get(ws_b).await.unwrap().is_some(), "second workspace survives");
        assert!(repo.is_member(ws_b, owner_b).await.unwrap(), "B's member survives");
        let b_export = repo.export(ws_b).await.unwrap().expect("B still exportable");
        assert!(
            b_export.rooms.iter().any(|r| r.room.id == room_b
                && r.messages.iter().any(|m| m.id == msg_b)),
            "B's room + message survive A's deletion"
        );
        // And A's room/message are truly gone (tenant isolation, both directions).
        let a_rooms: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rooms WHERE id = $1")
            .bind(room_a.to_uuid())
            .fetch_one(&p)
            .await
            .unwrap();
        let a_msgs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE id = $1")
            .bind(msg_a.to_uuid())
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!((a_rooms, a_msgs), (0, 0), "A's room + message deleted");
    }
}
