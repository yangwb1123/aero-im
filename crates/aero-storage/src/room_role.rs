//! Room-role repository — read/mutate a member's `room_members.role`.
//!
//! Backs channel role management (see one's role, see all member roles, change a
//! member's room role, transfer ownership). It owns no schema of its own: every
//! method operates over the EXISTING `room_members` table (migration 0001), whose
//! `role` column is `CHECK (role IN ('owner', 'member', 'admin'))`. Validating a
//! requested role against that allowed set, and the "who may mutate" policy, live
//! in the server layer ([`crate`] callers); this repo is the thin SQL seam.
//!
//! Purely additive: a NEW [`RoomRoleRepo`]; no existing repo
//! ([`RoomRepo`](crate::RoomRepo)) is touched. Reads learn table/column names
//! from `room.rs` but never reuse its queries.

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

    /// Set `participant`'s role in `room` to `role`. Returns `true` iff a matching
    /// membership row was updated — `false` when the participant is not a member of
    /// the room (no row matched). The caller is responsible for validating `role`
    /// against the allowed set; an unknown value would be rejected by the
    /// `room_members.role` CHECK constraint as a database error.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update (including a CHECK violation
    /// for a role outside the allowed set).
    pub async fn set_role(
        &self,
        room: RoomId,
        participant: ParticipantId,
        role: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE room_members SET role = $3 WHERE room_id = $1 AND participant_id = $2",
        )
        .bind(room.to_uuid())
        .bind(participant.to_uuid())
        .bind(role)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// How many members of `room` currently hold the `owner` role. Used to refuse
    /// demoting the last owner (which would leave the channel ownerless).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn count_owners(&self, room: RoomId) -> Result<i64, sqlx::Error> {
        let row = sqlx::query_as::<_, (i64,)>(
            r"SELECT COUNT(*) FROM room_members WHERE room_id = $1 AND role = 'owner'",
        )
        .bind(room.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
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

    /// Seed a channel room (in a throwaway workspace) created by `creator`, with no
    /// auto-enrolled members — the caller adds `room_members` rows explicitly so
    /// role assertions are deterministic.
    async fn channel(p: &PgPool, creator: ParticipantId) -> RoomId {
        let ws = WorkspaceId::new();
        sqlx::query(
            "INSERT INTO workspaces (id, name, slug, created_by, created_at) VALUES ($1,$2,$3,$4, now())",
        )
        .bind(ws.to_uuid())
        .bind("Room Role Test WS")
        .bind(format!("room-role-{ws}"))
        .bind(creator.to_uuid())
        .execute(p)
        .await
        .expect("insert workspace");

        let room = RoomId::new();
        sqlx::query(
            r"INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
               VALUES ($1, 'channel', $2, $3, now(), $4)",
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
            members.iter().any(|(pid, r)| *pid == member && r == "member"),
            "member present with member role"
        );

        // role_of resolves each role; a stranger is None.
        assert_eq!(repo.role_of(room, owner).await.unwrap().as_deref(), Some("owner"));
        assert_eq!(repo.role_of(room, member).await.unwrap().as_deref(), Some("member"));
        assert_eq!(repo.role_of(room, ParticipantId::new()).await.unwrap(), None);

        // Exactly one owner so far.
        assert_eq!(repo.count_owners(room).await.unwrap(), 1);

        // set_role flips a role (member → admin, an allowed value) and reports the
        // change; a non-member update reports no change.
        assert!(repo.set_role(room, member, "admin").await.unwrap(), "member role flipped");
        assert_eq!(repo.role_of(room, member).await.unwrap().as_deref(), Some("admin"));
        assert!(
            !repo.set_role(room, ParticipantId::new(), "member").await.unwrap(),
            "updating a non-member changes nothing"
        );

        // Cleanup so reruns stay self-contained (room_members cascades on room delete).
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
