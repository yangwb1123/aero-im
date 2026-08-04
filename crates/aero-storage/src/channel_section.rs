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
//! returns an opaque not-found outcome when the owner-scoped section lookup
//! fails; `remove_channel` remains an idempotent owner-scoped no-op.

use aero_common::{ChannelSectionId, ParticipantId, RoomId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;

/// Expected failures while assigning a room to a personal channel section.
#[derive(Debug, thiserror::Error)]
pub enum ChannelSectionAddError {
    #[error("channel section not found")]
    SectionNotFound,
    #[error("room not found")]
    RoomNotFound,
    #[error("room does not belong to the section workspace")]
    RoomOutsideWorkspace,
    #[error("section items must reference channel rooms")]
    NotChannel,
    #[error("participant no longer has effective room access")]
    NotAuthorized,
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

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
        let result =
            sqlx::query(r"DELETE FROM channel_sections WHERE id = $1 AND participant_id = $2")
                .bind(id.to_uuid())
                .bind(participant.to_uuid())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Assign a channel to one of the participant's own sections.
    ///
    /// Section ownership, section/room tenant containment, channel kind, and the
    /// participant's effective room access are all rechecked under the same
    /// transaction locks as the insert. The canonical effective-access helper
    /// locks the workspace/access edges and room aggregate before this method
    /// locks the section. A concurrent room/workspace revocation therefore
    /// either waits for this insert or commits first and makes it fail with
    /// [`ChannelSectionAddError::NotAuthorized`].
    ///
    /// Re-adding an already-assigned channel remains an idempotent no-op
    /// (`Ok(false)`).
    ///
    /// # Errors
    /// Returns a typed scope/access error for expected refusals and propagates
    /// database failures through [`ChannelSectionAddError::Storage`].
    pub async fn add_channel(
        &self,
        id: ChannelSectionId,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<bool, ChannelSectionAddError> {
        let mut tx = self.pool.begin().await?;

        // Resolve the immutable authorization route without a row lock. The
        // section is locked only after workspace/room authorization edges so
        // every writer follows the global governance order.
        let section_workspace = sqlx::query_scalar::<_, uuid::Uuid>(
            r"SELECT workspace_id
                FROM channel_sections
               WHERE id = $1 AND participant_id = $2",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(ChannelSectionAddError::SectionNotFound)?;
        let section_workspace = WorkspaceId::from_uuid(section_workspace);

        let room_workspace =
            sqlx::query_scalar::<_, uuid::Uuid>("SELECT workspace_id FROM rooms WHERE id = $1")
                .bind(room.to_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(ChannelSectionAddError::RoomNotFound)?;
        if room_workspace != section_workspace.to_uuid() {
            return Err(ChannelSectionAddError::RoomOutsideWorkspace);
        }

        // This canonical database function locks/rechecks workspace membership,
        // account liveness, deactivation, human-only mandatory 2FA, the room,
        // and the participant's room-membership edge.
        let effective_access: bool =
            sqlx::query_scalar("SELECT aero_effective_room_access($1, $2, $3)")
                .bind(room.to_uuid())
                .bind(participant.to_uuid())
                .bind(section_workspace.to_uuid())
                .fetch_one(&mut *tx)
                .await?;
        if !effective_access {
            return Err(ChannelSectionAddError::NotAuthorized);
        }

        // Re-read the room under the SHARE lock acquired by the helper. This
        // proves both tenant and type at the write boundary.
        let locked_room = sqlx::query_as::<_, (uuid::Uuid, String)>(
            "SELECT workspace_id, kind FROM rooms WHERE id = $1 FOR SHARE",
        )
        .bind(room.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(ChannelSectionAddError::RoomNotFound)?;
        if locked_room.0 != section_workspace.to_uuid() {
            return Err(ChannelSectionAddError::RoomOutsideWorkspace);
        }
        if locked_room.1 != "channel" {
            return Err(ChannelSectionAddError::NotChannel);
        }

        // The unlocked section lookup only selected the lock route. Lock and
        // revalidate owner + workspace immediately before inserting.
        let locked_section = sqlx::query_scalar::<_, uuid::Uuid>(
            r"SELECT workspace_id
                FROM channel_sections
               WHERE id = $1 AND participant_id = $2
               FOR UPDATE",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(ChannelSectionAddError::SectionNotFound)?;
        if locked_section != section_workspace.to_uuid() {
            return Err(ChannelSectionAddError::SectionNotFound);
        }

        let result = sqlx::query(
            r"INSERT INTO channel_section_items (section_id, room_id, position)
               VALUES (
                 $1, $2,
                 COALESCE(
                   (SELECT MAX(position) + 1 FROM channel_section_items
                     WHERE section_id = $1),
                   0
                 )
               )
               ON CONFLICT (section_id, room_id) DO NOTHING",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
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
    use std::time::Duration;

    use aero_common::{RoomKind, WorkspaceRole};

    use super::*;
    use crate::{RoomRepo, WorkspaceRepo};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(8)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn actor(p: &PgPool, label: &str) -> ParticipantId {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(actor.to_uuid())
            .bind(format!("section-{label}-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");
        actor
    }

    async fn workspace(p: &PgPool, owner: ParticipantId, label: &str) -> WorkspaceId {
        WorkspaceRepo::new(p.clone())
            .create(
                format!("Channel section {label}"),
                format!("channel-section-{label}-{owner}"),
                owner,
            )
            .await
            .expect("create workspace")
            .id
    }

    async fn room(
        p: &PgPool,
        workspace: WorkspaceId,
        kind: RoomKind,
        owner: ParticipantId,
        label: &str,
    ) -> RoomId {
        RoomRepo::new(p.clone())
            .create_in_workspace_authorized(
                workspace,
                kind,
                Some(format!("section-{label}-{owner}")),
                owner,
            )
            .await
            .expect("create room")
            .id
    }

    async fn item_count(p: &PgPool, section: ChannelSectionId, room: RoomId) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM channel_section_items
              WHERE section_id = $1 AND room_id = $2",
        )
        .bind(section.to_uuid())
        .bind(room.to_uuid())
        .fetch_one(p)
        .await
        .expect("count section item")
    }

    async fn assert_raw_insert_rejected(p: &PgPool, section: ChannelSectionId, room: RoomId) {
        let error = sqlx::query(
            "INSERT INTO channel_section_items (section_id, room_id, position)
             VALUES ($1, $2, 999)",
        )
        .bind(section.to_uuid())
        .bind(room.to_uuid())
        .execute(p)
        .await
        .expect_err("database containment trigger must reject raw insert");
        assert_eq!(
            error
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::constraint),
            Some("channel_section_items_room_containment")
        );
        assert_eq!(item_count(p, section, room).await, 0);
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn channel_section_lifecycle_and_owner_scoping() {
        let p = pool();
        let repo = ChannelSectionRepo::new(p.clone());
        let owner = actor(&p, "lifecycle-owner").await;
        let other = actor(&p, "lifecycle-other").await;
        let ws = workspace(&p, owner, "lifecycle").await;
        let room_a = room(&p, ws, RoomKind::Channel, owner, "a").await;
        let room_b = room(&p, ws, RoomKind::Channel, owner, "b").await;

        // Create → list shows it (and starts empty).
        let id = repo.create(owner, ws, "Favorites").await.unwrap();
        let listed = repo.list_for(owner, ws).await.unwrap();
        assert_eq!(listed.len(), 1, "one section after create");
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].name, "Favorites");
        assert!(listed[0].room_ids.is_empty(), "new section has no channels");

        // Add two channels → list shows them in insertion order.
        assert!(
            repo.add_channel(id, owner, room_a).await.unwrap(),
            "first add creates"
        );
        assert!(
            repo.add_channel(id, owner, room_b).await.unwrap(),
            "second add creates"
        );
        assert!(
            !repo.add_channel(id, owner, room_a).await.unwrap(),
            "re-adding a channel is a no-op"
        );
        let listed = repo.list_for(owner, ws).await.unwrap();
        assert_eq!(
            listed[0].room_ids,
            vec![room_a, room_b],
            "channels listed in order"
        );

        // Remove one → only the other remains.
        assert!(
            repo.remove_channel(id, owner, room_a).await.unwrap(),
            "removed room_a"
        );
        assert!(
            !repo.remove_channel(id, owner, room_a).await.unwrap(),
            "second remove is a no-op"
        );
        let listed = repo.list_for(owner, ws).await.unwrap();
        assert_eq!(listed[0].room_ids, vec![room_b], "only room_b remains");

        // Rename (owner-scoped).
        assert!(
            repo.rename(id, owner, "Pinned").await.unwrap(),
            "owner renames"
        );
        let listed = repo.list_for(owner, ws).await.unwrap();
        assert_eq!(listed[0].name, "Pinned", "name updated");

        // Owner-scoping: a different participant can't see, rename, mutate, or
        // delete the owner's section.
        assert!(
            repo.list_for(other, ws).await.unwrap().is_empty(),
            "sections are per-user"
        );
        assert!(
            !repo.rename(id, other, "Hijacked").await.unwrap(),
            "stranger cannot rename"
        );
        assert!(
            matches!(
                repo.add_channel(id, other, room_b).await,
                Err(ChannelSectionAddError::SectionNotFound)
            ),
            "a stranger observes the same result as a missing section"
        );
        assert!(
            !repo.remove_channel(id, other, room_b).await.unwrap(),
            "stranger cannot remove a channel"
        );
        assert!(
            !repo.delete(id, other).await.unwrap(),
            "stranger cannot delete"
        );
        // The owner's section is untouched by all the stranger's attempts.
        let listed = repo.list_for(owner, ws).await.unwrap();
        assert_eq!(listed[0].name, "Pinned", "rename attempt did not apply");
        assert_eq!(listed[0].room_ids, vec![room_b], "channel set unchanged");

        // Delete (owner-scoped) cascades items.
        assert!(repo.delete(id, owner).await.unwrap(), "owner deletes");
        assert!(
            !repo.delete(id, owner).await.unwrap(),
            "second delete is a no-op"
        );
        assert!(
            repo.list_for(owner, ws).await.unwrap().is_empty(),
            "no sections after delete"
        );
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

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn add_rejects_cross_tenant_non_channel_and_revoked_access_without_writes() {
        let p = pool();
        let repo = ChannelSectionRepo::new(p.clone());
        let owner = actor(&p, "containment-owner").await;
        let peer = actor(&p, "containment-peer").await;
        let ws = workspace(&p, owner, "containment-a").await;
        let other_ws = workspace(&p, peer, "containment-b").await;
        WorkspaceRepo::new(p.clone())
            .add_member(ws, peer, WorkspaceRole::Member)
            .await
            .expect("enroll peer in first workspace");

        let section = repo.create(owner, ws, "Contained").await.unwrap();
        let valid = room(&p, ws, RoomKind::Channel, owner, "valid").await;
        let group = room(&p, ws, RoomKind::Group, owner, "group").await;
        let inaccessible = room(&p, ws, RoomKind::Channel, peer, "private").await;
        let cross_tenant = room(&p, other_ws, RoomKind::Channel, peer, "cross-tenant").await;

        assert!(matches!(
            repo.add_channel(section, owner, cross_tenant).await,
            Err(ChannelSectionAddError::RoomOutsideWorkspace)
        ));
        assert!(matches!(
            repo.add_channel(section, owner, group).await,
            Err(ChannelSectionAddError::NotChannel)
        ));
        assert!(matches!(
            repo.add_channel(section, owner, inaccessible).await,
            Err(ChannelSectionAddError::NotAuthorized)
        ));
        for rejected in [cross_tenant, group, inaccessible] {
            assert_eq!(item_count(&p, section, rejected).await, 0);
        }

        // Direct SQL cannot bypass the same tenant/type/access boundary.
        assert_raw_insert_rejected(&p, section, cross_tenant).await;
        assert_raw_insert_rejected(&p, section, group).await;
        assert_raw_insert_rejected(&p, section, inaccessible).await;

        // A personal mapping never blocks a room hard-delete; the room FK
        // deliberately cascades this sidebar-only metadata.
        assert!(repo.add_channel(section, owner, valid).await.unwrap());
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(valid.to_uuid())
            .execute(&p)
            .await
            .expect("room hard-delete must not be blocked by section items");
        assert_eq!(item_count(&p, section, valid).await, 0);
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn add_waits_for_room_access_revocation_and_writes_nothing() {
        let p = pool();
        let governor = actor(&p, "race-governor").await;
        let section_owner = actor(&p, "race-section-owner").await;
        let ws = workspace(&p, governor, "race").await;
        WorkspaceRepo::new(p.clone())
            .add_member(ws, section_owner, WorkspaceRole::Member)
            .await
            .expect("enroll section owner");
        let channel = room(&p, ws, RoomKind::Channel, governor, "race").await;
        RoomRepo::new(p.clone())
            .add_member_authorized(channel, governor, section_owner)
            .await
            .expect("grant initial room access");
        let section = ChannelSectionRepo::new(p.clone())
            .create(section_owner, ws, "Race")
            .await
            .unwrap();

        let mut revocation = p.begin().await.unwrap();
        crate::ownership::lock_membership_governance(&mut revocation)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(ws.to_uuid())
            .execute(&mut *revocation)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM rooms WHERE id = $1 FOR UPDATE")
            .bind(channel.to_uuid())
            .execute(&mut *revocation)
            .await
            .unwrap();
        sqlx::query(
            "DELETE FROM room_members
              WHERE room_id = $1 AND participant_id = $2",
        )
        .bind(channel.to_uuid())
        .bind(section_owner.to_uuid())
        .execute(&mut *revocation)
        .await
        .unwrap();

        let raced_repo = ChannelSectionRepo::new(p.clone());
        let mut raced = tokio::spawn(async move {
            raced_repo
                .add_channel(section, section_owner, channel)
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut raced)
                .await
                .is_err(),
            "section add must wait behind room-access revocation"
        );
        revocation.commit().await.unwrap();

        assert!(matches!(
            raced.await.unwrap(),
            Err(ChannelSectionAddError::NotAuthorized)
        ));
        assert_eq!(item_count(&p, section, channel).await, 0);
    }
}
