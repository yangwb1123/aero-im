//! Room + membership.

use aero_common::{ParticipantId, Room, RoomId, RoomKind, WorkspaceId};
use sqlx::PgPool;

/// Map a `RoomKind` to its lowercase DB token.
fn room_kind_str(kind: RoomKind) -> &'static str {
    match kind {
        RoomKind::Direct => "direct",
        RoomKind::Group => "group",
        RoomKind::Channel => "channel",
    }
}

/// Parse a DB `kind` token back into a `RoomKind`, defaulting unknown tokens to
/// `Group` (mirrors the lenient parsing already used by `rooms_for`).
fn room_kind_from_str(s: &str) -> RoomKind {
    match s {
        "direct" => RoomKind::Direct,
        "channel" => RoomKind::Channel,
        _ => RoomKind::Group,
    }
}

#[derive(Clone)]
pub struct RoomRepo {
    pool: PgPool,
}

impl RoomRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn create(
        &self,
        kind: RoomKind,
        name: Option<String>,
        created_by: ParticipantId,
    ) -> Result<Room, sqlx::Error> {
        let id = RoomId::new();
        let created_at = time::OffsetDateTime::now_utc();
        let kind_s = match kind {
            RoomKind::Direct => "direct",
            RoomKind::Group => "group",
            RoomKind::Channel => "channel",
        };

        let mut tx = self.pool.begin().await?;
        // Legacy/untenanted room creation lands in the all-zero "default"
        // workspace — the same tenant the 0006 migration backfilled pre-tenancy
        // rooms into. `rooms.workspace_id` is NOT NULL with no DB default, so this
        // bind is required; tenant-aware callers use `create_in_workspace`.
        sqlx::query(
            r#"INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
               VALUES ($1, $2, $3, $4, $5, $6)"#,
        )
        .bind(id.to_uuid())
        .bind(kind_s)
        .bind(&name)
        .bind(created_by.to_uuid())
        .bind(created_at)
        .bind(uuid::Uuid::nil())
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r#"INSERT INTO room_members (room_id, participant_id, role, joined_at)
               VALUES ($1, $2, 'owner', $3)"#,
        )
        .bind(id.to_uuid())
        .bind(created_by.to_uuid())
        .bind(created_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(Room { id, kind, name, created_by, created_at })
    }

    pub async fn add_member(
        &self,
        room: RoomId,
        member: ParticipantId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"INSERT INTO room_members (room_id, participant_id, role, joined_at)
               VALUES ($1, $2, 'member', NOW())
               ON CONFLICT DO NOTHING"#,
        )
        .bind(room.to_uuid())
        .bind(member.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn is_member(
        &self,
        room: RoomId,
        member: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (i64,)>(
            r#"SELECT COUNT(*) FROM room_members WHERE room_id=$1 AND participant_id=$2"#,
        )
        .bind(room.to_uuid())
        .bind(member.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0 > 0)
    }

    pub async fn members(&self, room: RoomId) -> Result<Vec<ParticipantId>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r#"SELECT participant_id FROM room_members WHERE room_id=$1"#,
        )
        .bind(room.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(u,)| ParticipantId::from_uuid(u)).collect())
    }

    pub async fn rooms_for(&self, participant: ParticipantId) -> Result<Vec<Room>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid, String, Option<String>, uuid::Uuid, time::OffsetDateTime)>(
            r#"SELECT r.id, r.kind, r.name, r.created_by, r.created_at
               FROM rooms r
               JOIN room_members m ON m.room_id = r.id
               WHERE m.participant_id = $1
               ORDER BY r.created_at DESC"#,
        )
        .bind(participant.to_uuid())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(id, kind, name, by, at)| Room {
                id: RoomId::from_uuid(id),
                kind: room_kind_from_str(&kind),
                name,
                created_by: ParticipantId::from_uuid(by),
                created_at: at,
            })
            .collect())
    }

    // ------------------------------------------------ workspace-scoped (additive)

    /// Create a room that belongs to `workspace`, enrolling the creator as
    /// `owner`, atomically. This is the tenancy-aware counterpart to
    /// [`create`](Self::create): it populates `rooms.workspace_id` (made
    /// `NOT NULL` by `migrations/0006_workspaces.sql`) so the row satisfies the
    /// tenant invariant. Existing `create` is left untouched for the in-flight
    /// server migration.
    pub async fn create_in_workspace(
        &self,
        workspace: WorkspaceId,
        kind: RoomKind,
        name: Option<String>,
        created_by: ParticipantId,
    ) -> Result<Room, sqlx::Error> {
        let id = RoomId::new();
        let created_at = time::OffsetDateTime::now_utc();
        let kind_s = room_kind_str(kind);

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r"INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
               VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id.to_uuid())
        .bind(kind_s)
        .bind(&name)
        .bind(created_by.to_uuid())
        .bind(created_at)
        .bind(workspace.to_uuid())
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r"INSERT INTO room_members (room_id, participant_id, role, joined_at)
               VALUES ($1, $2, 'owner', $3)",
        )
        .bind(id.to_uuid())
        .bind(created_by.to_uuid())
        .bind(created_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(Room { id, kind, name, created_by, created_at })
    }

    /// Rooms the participant belongs to, restricted to a single `workspace`.
    /// Tenancy-scoped counterpart to [`rooms_for`](Self::rooms_for).
    pub async fn rooms_for_in_workspace(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
    ) -> Result<Vec<Room>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid, String, Option<String>, uuid::Uuid, time::OffsetDateTime)>(
            r"SELECT r.id, r.kind, r.name, r.created_by, r.created_at
               FROM rooms r
               JOIN room_members m ON m.room_id = r.id
               WHERE m.participant_id = $1 AND r.workspace_id = $2
               ORDER BY r.created_at DESC",
        )
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(id, kind, name, by, at)| Room {
                id: RoomId::from_uuid(id),
                kind: room_kind_from_str(&kind),
                name,
                created_by: ParticipantId::from_uuid(by),
                created_at: at,
            })
            .collect())
    }

    /// The workspace a room belongs to, or `None` if the room does not exist.
    /// Used by services to resolve a room's tenant before access checks.
    pub async fn room_workspace(
        &self,
        room: RoomId,
    ) -> Result<Option<WorkspaceId>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT workspace_id FROM rooms WHERE id = $1",
        )
        .bind(room.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(ws,)| WorkspaceId::from_uuid(ws)))
    }

    /// The room's [`RoomKind`] (`direct` / `group` / `channel`). Used to label
    /// message-throughput metrics by room type (ROADMAP 方向五). A single PK
    /// lookup; `None` when the room is absent.
    pub async fn room_kind(&self, room: RoomId) -> Result<Option<RoomKind>, sqlx::Error> {
        let row = sqlx::query_as::<_, (String,)>("SELECT kind FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(k,)| room_kind_from_str(&k)))
    }

    // ------------------------------------------------ channels (migration 0012)

    /// Set a room's visibility (`is_private`). A public (non-private) channel is
    /// discoverable + joinable by any workspace member via
    /// [`list_public_channels`](Self::list_public_channels).
    pub async fn set_visibility(&self, room: RoomId, is_private: bool) -> Result<(), sqlx::Error> {
        sqlx::query(r"UPDATE rooms SET is_private = $2 WHERE id = $1")
            .bind(room.to_uuid())
            .bind(is_private)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Archive or un-archive a room. Archived channels drop out of the public
    /// browse listing but membership and history are preserved.
    pub async fn set_archived(&self, room: RoomId, archived: bool) -> Result<(), sqlx::Error> {
        sqlx::query(r"UPDATE rooms SET is_archived = $2 WHERE id = $1")
            .bind(room.to_uuid())
            .bind(archived)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Archive a room, setting `is_archived = TRUE` and recording `archived_at = NOW()`
    /// (migration 0114). Idempotent: re-archiving an already-archived room is harmless.
    pub async fn archive(&self, room: RoomId) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE rooms SET is_archived = TRUE, archived_at = NOW() WHERE id = $1")
            .bind(room.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Un-archive a room, clearing both `is_archived` and `archived_at`
    /// (migration 0114). Idempotent: un-archiving an active room is harmless.
    pub async fn unarchive(&self, room: RoomId) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE rooms SET is_archived = FALSE, archived_at = NULL WHERE id = $1")
            .bind(room.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Set (or clear, with `None`) a room's short topic line.
    pub async fn set_topic(&self, room: RoomId, topic: Option<&str>) -> Result<(), sqlx::Error> {
        sqlx::query(r"UPDATE rooms SET topic = $2 WHERE id = $1")
            .bind(room.to_uuid())
            .bind(topic)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Set (or clear, with `None`) a room's longer description.
    pub async fn set_description(
        &self,
        room: RoomId,
        description: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(r"UPDATE rooms SET description = $2 WHERE id = $1")
            .bind(room.to_uuid())
            .bind(description)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Whether the room is private, or `None` if the room does not exist.
    pub async fn is_private(&self, room: RoomId) -> Result<Option<bool>, sqlx::Error> {
        let row = sqlx::query_as::<_, (bool,)>(r"SELECT is_private FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(p,)| p))
    }

    /// Whether the room is archived, or `None` if the room does not exist.
    pub async fn is_archived(&self, room: RoomId) -> Result<Option<bool>, sqlx::Error> {
        let row = sqlx::query_as::<_, (bool,)>(r"SELECT is_archived FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(a,)| a))
    }

    // ----------------------------------------------- post policy (migration 0030)

    /// The participant who created a room, or `None` if the room does not exist.
    /// Used by the post-policy guard to recognize the room creator (who may
    /// always post in an `admins`-only channel).
    pub async fn created_by(&self, room: RoomId) -> Result<Option<ParticipantId>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid,)>(r"SELECT created_by FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|(by,)| ParticipantId::from_uuid(by)))
    }

    /// Set a room's post policy (migration 0030). Valid values are `everyone`
    /// (any room member may post — the default, unchanged behavior) and `admins`
    /// (only the room creator or a workspace Admin/Owner may post; everyone else
    /// can read but not post). Validating the policy string is the caller's
    /// responsibility — an unrecognized value is treated as `everyone` at the
    /// enforcement site, never as a lockout.
    pub async fn set_post_policy(&self, room: RoomId, policy: &str) -> Result<(), sqlx::Error> {
        sqlx::query(r"UPDATE rooms SET post_policy = $2 WHERE id = $1")
            .bind(room.to_uuid())
            .bind(policy)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// A room's post policy. The `post_policy` column is `NOT NULL DEFAULT
    /// 'everyone'` (migration 0030), so a plain SELECT always yields a value for
    /// an existing row. A missing row (or, defensively, a NULL) falls back to
    /// `everyone` — the open default — so a glitch never silently locks posting.
    pub async fn post_policy(&self, room: RoomId) -> Result<String, sqlx::Error> {
        let row =
            sqlx::query_as::<_, (Option<String>,)>(r"SELECT post_policy FROM rooms WHERE id = $1")
                .bind(room.to_uuid())
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(p,)| p).unwrap_or_else(|| "everyone".to_owned()))
    }

    /// Public, non-archived channels in a workspace — the discovery listing a
    /// workspace member browses to find joinable channels. Newest first. Uses the
    /// partial index `rooms_public_channels_idx` (migration 0012).
    ///
    /// An optional `q` name-filter performs a case-insensitive substring match
    /// (`ILIKE '%q%'`) on the channel name; `None` returns all channels.
    pub async fn list_public_channels(
        &self,
        workspace: WorkspaceId,
        q: Option<&str>,
    ) -> Result<Vec<Room>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid, String, Option<String>, uuid::Uuid, time::OffsetDateTime)>(
            r"SELECT id, kind, name, created_by, created_at
               FROM rooms
               WHERE workspace_id = $1 AND is_private = false AND is_archived = false
                 AND ($2::text IS NULL OR name ILIKE '%' || $2 || '%')
               ORDER BY created_at DESC",
        )
        .bind(workspace.to_uuid())
        .bind(q)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(id, kind, name, by, at)| Room {
                id: RoomId::from_uuid(id),
                kind: room_kind_from_str(&kind),
                name,
                created_by: ParticipantId::from_uuid(by),
                created_at: at,
            })
            .collect())
    }

    // ----------------------------------------- reaction spam limit (0118)

    /// Fetch the per-room cap on distinct emoji a single user may add to any
    /// one message (`max_reactions_per_user`, migration 0118). Returns `Ok(None)`
    /// when the column is `NULL` (no limit) or the room does not exist.
    pub async fn get_max_reactions_per_user(
        &self,
        room: RoomId,
    ) -> Result<Option<i32>, sqlx::Error> {
        let row = sqlx::query_as::<_, (Option<i32>,)>(
            "SELECT max_reactions_per_user FROM rooms WHERE id = $1",
        )
        .bind(room.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(v,)| v))
    }

    /// Set (or clear with `None`) the per-room reaction cap (migration 0118).
    pub async fn set_max_reactions_per_user(
        &self,
        room: RoomId,
        limit: Option<i32>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE rooms SET max_reactions_per_user = $1 WHERE id = $2",
        )
        .bind(limit)
        .bind(room.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ------------------------------------------ per-channel retention (0058)

    /// Set (or, with `days = None`, clear back to "inherit the workspace
    /// default") a room's message-retention override, in whole days (migration
    /// 0058). A `Some(n)` makes this room's window take precedence over its
    /// workspace's `retention_days` in the periodic sweep
    /// ([`WorkspaceRepo::sweep_expired_messages`](crate::WorkspaceRepo::sweep_expired_messages));
    /// `None` (the default) means the room inherits the workspace default.
    /// Validating the value (`1..=3650`) is the caller's responsibility, mirroring
    /// the workspace policy path; this method writes whatever it is given.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn set_retention_days(
        &self,
        room: RoomId,
        days: Option<i32>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(r"UPDATE rooms SET retention_days = $2 WHERE id = $1")
            .bind(room.to_uuid())
            .bind(days)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// A room's retention override in whole days, or `None` when the room inherits
    /// the workspace default (or does not exist). The outer `Result`/inner
    /// `Option` collapse "no such room" and "no override set" to the same `None`:
    /// both mean "fall back to the workspace policy" for the effective window.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn retention_days(&self, room: RoomId) -> Result<Option<i32>, sqlx::Error> {
        let row = sqlx::query_as::<_, (Option<i32>,)>(
            r"SELECT retention_days FROM rooms WHERE id = $1",
        )
        .bind(room.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(d,)| d))
    }

    // ------------------------------------ recommendations (read-only, additive)

    /// Public, non-archived channels in `workspace` the `participant` is NOT
    /// already a member of, each tagged with a recent-activity count — the
    /// candidate set for "channels to join" recommendations.
    ///
    /// The mirror of [`list_public_channels`](Self::list_public_channels) (same
    /// `is_private = false AND is_archived = false` discovery boundary) minus the
    /// channels the caller already belongs to (the `NOT EXISTS` membership
    /// anti-join). The second column is the number of non-deleted messages posted
    /// to the channel in the last `recent_days` days — a deterministic activity
    /// signal the ranker uses as the no-embeddings degrade ordering. Newest
    /// channel first as a stable secondary order. `recent_days` is floored at `1`.
    ///
    /// Read-only: no mutation, no migration — it reads `rooms`, `room_members`
    /// and `messages` that already exist.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_workspace_channels_not_member(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
        recent_days: i64,
    ) -> Result<Vec<(Room, i64)>, sqlx::Error> {
        let recent_days = recent_days.max(1);
        let rows = sqlx::query_as::<
            _,
            (uuid::Uuid, String, Option<String>, uuid::Uuid, time::OffsetDateTime, i64),
        >(
            r"SELECT r.id, r.kind, r.name, r.created_by, r.created_at,
                     COALESCE(act.cnt, 0) AS activity
               FROM rooms r
               LEFT JOIN LATERAL (
                   SELECT COUNT(*) AS cnt
                     FROM messages m
                    WHERE m.room_id = r.id
                      AND m.deleted_at IS NULL
                      AND m.created_at >= NOW() - make_interval(days => $3::int)
               ) act ON true
              WHERE r.workspace_id = $1
                AND r.is_private = false
                AND r.is_archived = false
                AND NOT EXISTS (
                    SELECT 1 FROM room_members rm
                     WHERE rm.room_id = r.id AND rm.participant_id = $2
                )
              ORDER BY r.created_at DESC, r.id DESC",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(recent_days)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(id, kind, name, by, at, activity)| {
                (
                    Room {
                        id: RoomId::from_uuid(id),
                        kind: room_kind_from_str(&kind),
                        name,
                        created_by: ParticipantId::from_uuid(by),
                        created_at: at,
                    },
                    activity,
                )
            })
            .collect())
    }

    /// Workspace members the `caller` shares at least one room with, each with the
    /// count of rooms shared — the affinity signal for "people to follow"
    /// recommendations.
    ///
    /// For every OTHER member of `workspace`, counts how many rooms (of any kind)
    /// in `workspace` both the caller and that member belong to. Only members with
    /// at least one shared room are returned (an `INNER JOIN` on the caller's
    /// memberships), so a candidate the caller has never co-occupied a room with is
    /// omitted — the recommender prefers people you already brush against. The
    /// caller is excluded. Highest shared-count first, ties broken by participant
    /// id so the order is deterministic.
    ///
    /// Read-only: no mutation, no migration — `room_members` + `rooms` only.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn shared_room_counts_in_workspace(
        &self,
        workspace: WorkspaceId,
        caller: ParticipantId,
    ) -> Result<Vec<(ParticipantId, i64)>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid, i64)>(
            r"SELECT other.participant_id, COUNT(*) AS shared
               FROM room_members mine
               JOIN rooms r ON r.id = mine.room_id AND r.workspace_id = $2
               JOIN room_members other ON other.room_id = mine.room_id
              WHERE mine.participant_id = $1
                AND other.participant_id <> $1
              GROUP BY other.participant_id
              ORDER BY shared DESC, other.participant_id ASC",
        )
        .bind(caller.to_uuid())
        .bind(workspace.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(pid, shared)| (ParticipantId::from_uuid(pid), shared))
            .collect())
    }

    // ------------------------------------------ slowmode (migration 0107)

    /// Set a room's slowmode interval in seconds (`0` disables slowmode).
    pub async fn set_slowmode(&self, room: RoomId, seconds: i32) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE rooms SET slowmode_seconds = $1 WHERE id = $2")
            .bind(seconds)
            .bind(room.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The room's current slowmode interval in seconds (`0` = disabled), or `0`
    /// if the room does not exist (fail-open so a glitch never silently blocks
    /// posting).
    pub async fn get_slowmode(&self, room: RoomId) -> Result<i32, sqlx::Error> {
        let row = sqlx::query_as::<_, (i32,)>(
            "SELECT slowmode_seconds FROM rooms WHERE id = $1",
        )
        .bind(room.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(s,)| s).unwrap_or(0))
    }

    /// Remove a participant from a room (used by channel leave). Idempotent: a
    /// no-op when they were not a member.
    pub async fn remove_member(
        &self,
        room: RoomId,
        participant: ParticipantId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(r"DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
            .bind(room.to_uuid())
            .bind(participant.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

/// PG-gated integration tests for channel management (migration 0012). Run with a
/// live Postgres + applied migrations:
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored channel_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// A throwaway workspace + creator participant so each test is self-contained
    /// (mirrors `audit::db_tests::fixture`).
    async fn fixture(p: &PgPool) -> (WorkspaceId, ParticipantId) {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(actor.to_uuid())
            .bind(format!("chan-actor-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");
        let ws = WorkspaceId::new();
        sqlx::query("INSERT INTO workspaces (id, name, slug, created_by, created_at) VALUES ($1,$2,$3,$4, now())")
            .bind(ws.to_uuid())
            .bind("Channel Test WS")
            .bind(format!("chan-{ws}"))
            .bind(actor.to_uuid())
            .execute(p)
            .await
            .expect("insert workspace");
        (ws, actor)
    }

    /// Insert a channel room directly in `workspace` (no auto-enrolled owner) with
    /// an explicit visibility, so listing/visibility assertions are deterministic.
    async fn insert_channel(
        p: &PgPool,
        workspace: WorkspaceId,
        creator: ParticipantId,
        is_private: bool,
    ) -> RoomId {
        let id = RoomId::new();
        sqlx::query(
            r"INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id, is_private)
               VALUES ($1, 'channel', $2, $3, now(), $4, $5)",
        )
        .bind(id.to_uuid())
        .bind(format!("chan-{id}"))
        .bind(creator.to_uuid())
        .bind(workspace.to_uuid())
        .bind(is_private)
        .execute(p)
        .await
        .expect("insert channel");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn channel_visibility_and_archive_roundtrip() {
        let p = pool();
        let repo = RoomRepo::new(p.clone());
        let (ws, actor) = fixture(&p).await;
        let room = insert_channel(&p, ws, actor, true).await;

        assert_eq!(repo.is_private(room).await.unwrap(), Some(true));
        assert_eq!(repo.is_archived(room).await.unwrap(), Some(false));

        repo.set_visibility(room, false).await.unwrap();
        assert_eq!(repo.is_private(room).await.unwrap(), Some(false));

        repo.set_archived(room, true).await.unwrap();
        assert_eq!(repo.is_archived(room).await.unwrap(), Some(true));

        // Unknown room ⇒ None, never an error.
        assert_eq!(repo.is_private(RoomId::new()).await.unwrap(), None);
        assert_eq!(repo.is_archived(RoomId::new()).await.unwrap(), None);
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn channel_topic_and_description_roundtrip() {
        let p = pool();
        let repo = RoomRepo::new(p.clone());
        let (ws, actor) = fixture(&p).await;
        let room = insert_channel(&p, ws, actor, false).await;

        repo.set_topic(room, Some("daily standup")).await.unwrap();
        repo.set_description(room, Some("the team channel")).await.unwrap();
        let (topic, desc) = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            r"SELECT topic, description FROM rooms WHERE id = $1",
        )
        .bind(room.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(topic.as_deref(), Some("daily standup"));
        assert_eq!(desc.as_deref(), Some("the team channel"));

        // Clearing sets them back to NULL.
        repo.set_topic(room, None).await.unwrap();
        let (topic, _) = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            r"SELECT topic, description FROM rooms WHERE id = $1",
        )
        .bind(room.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(topic, None);
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn channel_list_public_excludes_private_and_archived() {
        let p = pool();
        let repo = RoomRepo::new(p.clone());
        let (ws, actor) = fixture(&p).await;

        let public = insert_channel(&p, ws, actor, false).await;
        let private = insert_channel(&p, ws, actor, true).await;
        let archived = insert_channel(&p, ws, actor, false).await;
        repo.set_archived(archived, true).await.unwrap();

        let listed = repo.list_public_channels(ws, None).await.unwrap();
        let ids: Vec<RoomId> = listed.iter().map(|r| r.id).collect();
        assert!(ids.contains(&public), "public channel is discoverable");
        assert!(!ids.contains(&private), "private channel is hidden");
        assert!(!ids.contains(&archived), "archived channel is hidden");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn channel_join_then_leave_roundtrip() {
        let p = pool();
        let repo = RoomRepo::new(p.clone());
        let (ws, actor) = fixture(&p).await;
        let room = insert_channel(&p, ws, actor, false).await;

        let joiner = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(joiner.to_uuid())
            .bind(format!("joiner-{joiner}"))
            .execute(&p)
            .await
            .unwrap();

        assert!(!repo.is_member(room, joiner).await.unwrap());
        repo.add_member(room, joiner).await.unwrap();
        assert!(repo.is_member(room, joiner).await.unwrap());
        // Leave is idempotent: a second remove is a harmless no-op.
        repo.remove_member(room, joiner).await.unwrap();
        assert!(!repo.is_member(room, joiner).await.unwrap());
        repo.remove_member(room, joiner).await.unwrap();
        assert!(!repo.is_member(room, joiner).await.unwrap());
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn channels_not_member_excludes_joined_private_and_archived() {
        let p = pool();
        let repo = RoomRepo::new(p.clone());
        let (ws, actor) = fixture(&p).await;

        // A public channel the caller is NOT in (the expected candidate),
        // a public channel the caller HAS joined (excluded by the anti-join),
        // a private channel (excluded by discovery boundary),
        // an archived channel (excluded by discovery boundary).
        let candidate = insert_channel(&p, ws, actor, false).await;
        let joined = insert_channel(&p, ws, actor, false).await;
        let private = insert_channel(&p, ws, actor, true).await;
        let archived = insert_channel(&p, ws, actor, false).await;
        repo.set_archived(archived, true).await.unwrap();

        let caller = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(caller.to_uuid())
            .bind(format!("caller-{caller}"))
            .execute(&p)
            .await
            .unwrap();
        repo.add_member(joined, caller).await.unwrap();

        let listed = repo
            .list_workspace_channels_not_member(ws, caller, 30)
            .await
            .unwrap();
        let ids: Vec<RoomId> = listed.iter().map(|(r, _)| r.id).collect();
        assert!(ids.contains(&candidate), "non-member public channel is a candidate");
        assert!(!ids.contains(&joined), "channel the caller is in is excluded");
        assert!(!ids.contains(&private), "private channel is excluded");
        assert!(!ids.contains(&archived), "archived channel is excluded");
        // Each candidate carries a non-negative activity count.
        for (_, activity) in &listed {
            assert!(*activity >= 0);
        }
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn shared_room_counts_excludes_caller_and_counts_overlap() {
        let p = pool();
        let repo = RoomRepo::new(p.clone());
        let (ws, actor) = fixture(&p).await;

        let caller = ParticipantId::new();
        let buddy = ParticipantId::new();
        for (id, name) in [(caller, "caller"), (buddy, "buddy")] {
            sqlx::query(
                "INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)",
            )
            .bind(id.to_uuid())
            .bind(format!("{name}-{id}"))
            .execute(&p)
            .await
            .unwrap();
        }

        // Two channels both the caller and buddy belong to → shared count 2.
        let c1 = insert_channel(&p, ws, actor, false).await;
        let c2 = insert_channel(&p, ws, actor, false).await;
        for room in [c1, c2] {
            repo.add_member(room, caller).await.unwrap();
            repo.add_member(room, buddy).await.unwrap();
        }

        let counts = repo.shared_room_counts_in_workspace(ws, caller).await.unwrap();
        // The caller never appears as a candidate for following themselves.
        assert!(!counts.iter().any(|(p, _)| *p == caller), "caller excluded");
        let buddy_count = counts.iter().find(|(p, _)| *p == buddy).map(|(_, n)| *n);
        assert_eq!(buddy_count, Some(2), "buddy shares both channels");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn post_policy_roundtrip_defaults_to_everyone() {
        let p = pool();
        let repo = RoomRepo::new(p.clone());
        let (ws, actor) = fixture(&p).await;
        let room = insert_channel(&p, ws, actor, false).await;

        // A fresh row carries the NOT NULL DEFAULT 'everyone' (migration 0030).
        assert_eq!(repo.post_policy(room).await.unwrap(), "everyone");
        // The creator is recoverable for the post-policy guard.
        assert_eq!(repo.created_by(room).await.unwrap(), Some(actor));

        repo.set_post_policy(room, "admins").await.unwrap();
        assert_eq!(repo.post_policy(room).await.unwrap(), "admins");

        repo.set_post_policy(room, "everyone").await.unwrap();
        assert_eq!(repo.post_policy(room).await.unwrap(), "everyone");

        // Unknown room ⇒ open default, never an error (fail-open on read).
        assert_eq!(repo.post_policy(RoomId::new()).await.unwrap(), "everyone");
        assert_eq!(repo.created_by(RoomId::new()).await.unwrap(), None);
    }
}
