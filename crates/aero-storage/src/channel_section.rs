//! Per-user channel-sidebar section repository.
//!
//! Backs `migrations/0031_channel_sections.sql`. Slack/Teams "sections": a user
//! organizes their channel sidebar into named, ordered sections and assigns
//! channels (rooms) to them. A section is PRIVATE to the owning participant and
//! scoped to one workspace — pure organizational metadata over existing rooms,
//! touching no message/room data.
//!
//! Purely additive: a NEW [`ChannelSectionRepo`]; no existing repo is touched.
//! The [`ChannelSection`] model lives here (and is re-exported from the crate
//! root) rather than in `aero-common`, since it is a storage-layer projection —
//! mirroring [`Draft`](crate::Draft) and [`ScheduledStream`](crate::ScheduledStream).
//!
//! EVERY mutation is scoped to the owning `participant`, so one user can never
//! rename, delete, or assign channels to another user's sections. `add_channel`
//! and `remove_channel` first resolve the section owner-scoped, then mutate its
//! items, so a stranger targeting a section id they don't own is a no-op.

use aero_common::{ChannelSectionId, ParticipantId, RoomId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;

/// One per-user channel-sidebar section: a named, ordered folder grouping the
/// owner's channels in a single workspace. `room_ids` are the channels assigned
/// to it, in their stored order (so the client renders the sidebar directly).
///
/// `Serialize` so a handler can hand the row straight back as JSON; `created_at`
/// renders as RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelSection {
    /// The section's unique id.
    pub id: ChannelSectionId,
    /// The participant who owns the section (sections are private).
    pub participant_id: ParticipantId,
    /// The tenant the section belongs to.
    pub workspace_id: WorkspaceId,
    /// The section's display name.
    pub name: String,
    /// The section's sort position within the owner's sidebar (ascending).
    pub position: i32,
    /// When the section was created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    /// The channels (rooms) assigned to this section, in stored order.
    pub room_ids: Vec<RoomId>,
}

/// Per-user channel-section store. Cheap to clone (wraps an `Arc<PgPool>`), so
/// feature modules construct one inline rather than threading it through state.
#[derive(Clone)]
pub struct ChannelSectionRepo {
    pool: PgPool,
}

impl ChannelSectionRepo {
    /// Build a channel-section repository over the shared connection pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a new (empty) section for a participant in a workspace, appended
    /// after the participant's existing sections in that workspace. Returns the
    /// generated id.
    ///
    /// # Errors
    /// Propagates any `sqlx` error.
    pub async fn create(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        name: &str,
    ) -> Result<ChannelSectionId, sqlx::Error> {
        let id = ChannelSectionId::new();
        sqlx::query(
            r"INSERT INTO channel_sections (id, participant_id, workspace_id, name, position)
               VALUES (
                 $1, $2, $3, $4,
                 COALESCE(
                   (SELECT MAX(position) + 1 FROM channel_sections
                     WHERE participant_id = $2 AND workspace_id = $3),
                   0
                 )
               )",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .bind(name)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List a participant's sections in a workspace, ordered by `position`, each
    /// carrying its assigned `room_ids` (also in stored order). Always scoped to
    /// `participant`, so a user only ever sees their own sections.
    ///
    /// # Errors
    /// Propagates any `sqlx` error.
    pub async fn list_for(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
    ) -> Result<Vec<ChannelSection>, sqlx::Error> {
        let sections = sqlx::query_as::<_, SectionRow>(
            r"SELECT id, participant_id, workspace_id, name, position, created_at
               FROM channel_sections
              WHERE participant_id = $1 AND workspace_id = $2
              ORDER BY position ASC, created_at ASC, id ASC",
        )
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(sections.len());
        for s in sections {
            let items = sqlx::query_as::<_, (uuid::Uuid,)>(
                r"SELECT room_id FROM channel_section_items
                   WHERE section_id = $1
                   ORDER BY position ASC, room_id ASC",
            )
            .bind(s.id)
            .fetch_all(&self.pool)
            .await?;
            out.push(ChannelSection {
                id: ChannelSectionId::from_uuid(s.id),
                participant_id: ParticipantId::from_uuid(s.participant_id),
                workspace_id: WorkspaceId::from_uuid(s.workspace_id),
                name: s.name,
                position: s.position,
                created_at: s.created_at,
                room_ids: items.into_iter().map(|(r,)| RoomId::from_uuid(r)).collect(),
            });
        }
        Ok(out)
    }

    /// Rename one of the participant's own sections. Returns `true` iff a row was
    /// updated. Owner-scoped, so a user can never rename another's section.
    ///
    /// # Errors
    /// Propagates any `sqlx` error.
    pub async fn rename(
        &self,
        id: ChannelSectionId,
        participant: ParticipantId,
        name: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE channel_sections SET name = $3
               WHERE id = $1 AND participant_id = $2",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(name)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Delete one of the participant's own sections. Its `channel_section_items`
    /// are dropped by the `ON DELETE CASCADE` foreign key. Returns `true` iff a
    /// row was removed. Owner-scoped.
    ///
    /// # Errors
    /// Propagates any `sqlx` error.
    pub async fn delete(
        &self,
        id: ChannelSectionId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"DELETE FROM channel_sections WHERE id = $1 AND participant_id = $2",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Assign a channel (room) to one of the participant's own sections, appended
    /// after the section's existing channels. Idempotent: re-adding an
    /// already-assigned channel is a no-op (`ON CONFLICT DO NOTHING`). Owner-scoped
    /// — the insert resolves the section only when the caller owns it, so it
    /// affects nothing when targeting a section the caller doesn't own. Returns
    /// `true` iff a new assignment was created.
    ///
    /// # Errors
    /// Propagates any `sqlx` error.
    pub async fn add_channel(
        &self,
        id: ChannelSectionId,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"INSERT INTO channel_section_items (section_id, room_id, position)
               SELECT s.id, $3,
                      COALESCE(
                        (SELECT MAX(position) + 1 FROM channel_section_items
                          WHERE section_id = s.id),
                        0
                      )
                 FROM channel_sections s
                WHERE s.id = $1 AND s.participant_id = $2
               ON CONFLICT (section_id, room_id) DO NOTHING",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(room.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Remove a channel (room) from one of the participant's own sections. Returns
    /// `true` iff an assignment was removed. Owner-scoped — the delete only matches
    /// items of a section the caller owns.
    ///
    /// # Errors
    /// Propagates any `sqlx` error.
    pub async fn remove_channel(
        &self,
        id: ChannelSectionId,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"DELETE FROM channel_section_items i
               USING channel_sections s
               WHERE i.section_id = s.id
                 AND s.id = $1
                 AND s.participant_id = $2
                 AND i.room_id = $3",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(room.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

/// A `channel_sections` row, decoded before its items are loaded.
#[derive(sqlx::FromRow)]
struct SectionRow {
    id: uuid::Uuid,
    participant_id: uuid::Uuid,
    workspace_id: uuid::Uuid,
    name: String,
    position: i32,
    created_at: time::OffsetDateTime,
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored channel_section
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

    /// Create a throwaway participant so the test is self-contained. Sections only
    /// store opaque uuids for `workspace_id` / `room_id` (no FK to those tables),
    /// so we can use fresh ids for them without inserting rooms/workspaces.
    async fn actor(p: &PgPool) -> ParticipantId {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(actor.to_uuid())
            .bind(format!("section-actor-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");
        actor
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn channel_section_lifecycle_and_owner_scoping() {
        let p = pool();
        let repo = ChannelSectionRepo::new(p.clone());
        let owner = actor(&p).await;
        let other = actor(&p).await;
        let ws = WorkspaceId::new();

        // Create → list shows it (and starts empty).
        let id = repo.create(owner, ws, "Favorites").await.unwrap();
        let listed = repo.list_for(owner, ws).await.unwrap();
        assert_eq!(listed.len(), 1, "one section after create");
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].name, "Favorites");
        assert!(listed[0].room_ids.is_empty(), "new section has no channels");

        // Add two channels → list shows them in insertion order.
        let room_a = RoomId::new();
        let room_b = RoomId::new();
        assert!(repo.add_channel(id, owner, room_a).await.unwrap(), "first add creates");
        assert!(repo.add_channel(id, owner, room_b).await.unwrap(), "second add creates");
        assert!(
            !repo.add_channel(id, owner, room_a).await.unwrap(),
            "re-adding a channel is a no-op"
        );
        let listed = repo.list_for(owner, ws).await.unwrap();
        assert_eq!(listed[0].room_ids, vec![room_a, room_b], "channels listed in order");

        // Remove one → only the other remains.
        assert!(repo.remove_channel(id, owner, room_a).await.unwrap(), "removed room_a");
        assert!(
            !repo.remove_channel(id, owner, room_a).await.unwrap(),
            "second remove is a no-op"
        );
        let listed = repo.list_for(owner, ws).await.unwrap();
        assert_eq!(listed[0].room_ids, vec![room_b], "only room_b remains");

        // Rename (owner-scoped).
        assert!(repo.rename(id, owner, "Pinned").await.unwrap(), "owner renames");
        let listed = repo.list_for(owner, ws).await.unwrap();
        assert_eq!(listed[0].name, "Pinned", "name updated");

        // Owner-scoping: a different participant can't see, rename, mutate, or
        // delete the owner's section.
        assert!(repo.list_for(other, ws).await.unwrap().is_empty(), "sections are per-user");
        assert!(!repo.rename(id, other, "Hijacked").await.unwrap(), "stranger cannot rename");
        assert!(
            !repo.add_channel(id, other, RoomId::new()).await.unwrap(),
            "stranger cannot add a channel"
        );
        assert!(
            !repo.remove_channel(id, other, room_b).await.unwrap(),
            "stranger cannot remove a channel"
        );
        assert!(!repo.delete(id, other).await.unwrap(), "stranger cannot delete");
        // The owner's section is untouched by all the stranger's attempts.
        let listed = repo.list_for(owner, ws).await.unwrap();
        assert_eq!(listed[0].name, "Pinned", "rename attempt did not apply");
        assert_eq!(listed[0].room_ids, vec![room_b], "channel set unchanged");

        // Delete (owner-scoped) cascades items.
        assert!(repo.delete(id, owner).await.unwrap(), "owner deletes");
        assert!(!repo.delete(id, owner).await.unwrap(), "second delete is a no-op");
        assert!(repo.list_for(owner, ws).await.unwrap().is_empty(), "no sections after delete");
        // The cascade dropped the section's items.
        let remaining = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM channel_section_items WHERE section_id = $1",
        )
        .bind(id.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(remaining.0, 0, "cascade dropped the section's items");
    }
}
