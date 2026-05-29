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
        sqlx::query(
            r#"INSERT INTO rooms (id, kind, name, created_by, created_at)
               VALUES ($1, $2, $3, $4, $5)"#,
        )
        .bind(id.to_uuid())
        .bind(kind_s)
        .bind(&name)
        .bind(created_by.to_uuid())
        .bind(created_at)
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
}
