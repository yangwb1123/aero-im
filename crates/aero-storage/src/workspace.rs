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

use aero_common::{ParticipantId, Workspace, WorkspaceId, WorkspaceMember, WorkspaceRole};
use sqlx::PgPool;

#[derive(Clone)]
pub struct WorkspaceRepo {
    pool: PgPool,
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
