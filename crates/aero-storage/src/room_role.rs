//! Read-only room-role repository over `room_members.role`.
//!
//! Mutation deliberately lives on the transaction-owned
//! [`RoomRepo`](crate::RoomRepo) governance methods. Keeping this repository
//! read-only prevents callers from separating owner authorization, final-owner
//! validation, and role updates into race-prone independent statements.

use aero_common::{ParticipantId, RoomId};
use sqlx::PgPool;

/// Repository over the `role` column of the `room_members` table.
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`RoomRoleRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct RoomRoleRepo {
    pool: PgPool,
}

impl RoomRoleRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The `participant`'s role in `room`, or `None` when they are not a member.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn role_of(
        &self,
        room: RoomId,
        participant: ParticipantId,
    ) -> Result<Option<String>, sqlx::Error> {
        let row = sqlx::query_as::<_, (String,)>(
            r"SELECT role FROM room_members WHERE room_id = $1 AND participant_id = $2",
        )
        .bind(room.to_uuid())
        .bind(participant.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(role,)| role))
    }

    /// Every member of `room` paired with their role, oldest membership first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn members_with_roles(
        &self,
        room: RoomId,
    ) -> Result<Vec<(ParticipantId, String)>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid, String)>(
            r"SELECT participant_id, role
               FROM room_members
              WHERE room_id = $1
              ORDER BY joined_at, participant_id",
        )
        .bind(room.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(pid, role)| (ParticipantId::from_uuid(pid), role))
            .collect())
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored room_role
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::WorkspaceId;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Insert a throwaway human participant so the test is self-contained.
    async fn participant(p: &PgPool, label: &str) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("room-role-{label}-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    /// Seed a group room (in a throwaway workspace) created by `creator`, with no
    /// auto-enrolled members — the caller adds `room_members` rows explicitly so
    /// role assertions are deterministic.
    async fn channel(p: &PgPool, creator: ParticipantId) -> RoomId {
        let ws = WorkspaceId::new();
        let mut tx = p.begin().await.expect("begin workspace fixture");
        sqlx::query("INSERT INTO workspaces (id, name, slug, created_by, created_at) VALUES ($1,$2,$3,$4, now())")
            .bind(ws.to_uuid())
            .bind("Room Role Test WS")
            .bind(format!("room-role-{ws}"))
            .bind(creator.to_uuid())
            .execute(&mut *tx)
            .await
            .expect("insert workspace");
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1, $2, 'owner')",
        )
        .bind(ws.to_uuid())
        .bind(creator.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert workspace owner");
        tx.commit().await.expect("commit workspace fixture");

        let room = RoomId::new();
        sqlx::query(
            r"INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
               VALUES ($1, 'group', $2, $3, now(), $4)",
        )
        .bind(room.to_uuid())
        .bind(format!("room-role-chan-{room}"))
        .bind(creator.to_uuid())
        .bind(ws.to_uuid())
        .execute(p)
        .await
        .expect("insert channel");
        room
    }

    async fn enroll(p: &PgPool, room: RoomId, member: ParticipantId, role: &str) {
        sqlx::query(
            r"INSERT INTO room_members (room_id, participant_id, role, joined_at)
               VALUES ($1, $2, $3, now())",
        )
        .bind(room.to_uuid())
        .bind(member.to_uuid())
        .bind(role)
        .execute(p)
        .await
        .expect("insert room member");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn room_role_read_and_flip() {
        let p = pool();
        let repo = RoomRoleRepo::new(p.clone());
        let owner = participant(&p, "owner").await;
        let member = participant(&p, "member").await;
        let room = channel(&p, owner).await;
        enroll(&p, room, owner, "owner").await;
        enroll(&p, room, member, "member").await;

        // members_with_roles returns both with their roles.
        let members = repo.members_with_roles(room).await.unwrap();
        assert_eq!(members.len(), 2, "two members enrolled");
        assert!(
            members.iter().any(|(pid, r)| *pid == owner && r == "owner"),
            "owner present with owner role"
        );
        assert!(
            members
                .iter()
                .any(|(pid, r)| *pid == member && r == "member"),
            "member present with member role"
        );

        // role_of resolves each role; a stranger is None.
        assert_eq!(
            repo.role_of(room, owner).await.unwrap().as_deref(),
            Some("owner")
        );
        assert_eq!(
            repo.role_of(room, member).await.unwrap().as_deref(),
            Some("member")
        );
        assert_eq!(
            repo.role_of(room, ParticipantId::new()).await.unwrap(),
            None
        );

        // Cleanup so reruns stay self-contained (room_members cascades on room delete).
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
