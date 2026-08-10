//! User-group / @-usergroup repository (workspace-scoped named member groups).
//!
//! Backs `migrations/0033_user_groups.sql`. A workspace member creates a named
//! group (`@designers`) of participants, then lists, resolves, or deletes it and
//! manages its membership. The handle is unique within a workspace, so an
//! `@handle` mention resolves to at most one group via [`UserGroupRepo::resolve`].
//! The mention fan-out into notifications is wired separately and reuses
//! [`UserGroupRepo::resolve`]/[`UserGroupRepo::members`] as its seam — this repo
//! owns only the group CRUD + membership, never the notification side.
//!
//! Purely additive: a NEW [`UserGroupRepo`]; no existing repo is touched. The
//! [`UserGroup`] model lives here (and is re-exported from the crate root) rather
//! than in `aero-common`, since it is a storage-layer projection — mirroring
//! [`SavedSearch`](crate::SavedSearch) and [`ChannelSection`](crate::ChannelSection).

use aero_common::{ParticipantId, UserGroupId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;

mod tx;

use tx::{
    canonical_members, insert_members, lock_group, lock_managed_group, lock_workspace_for_group,
    lock_workspace_members, lock_workspace_role, members_in_tx, scim_creator_in_tx,
    suffixed_handle,
};

const MAX_HANDLE_CHARS: usize = 32;
const MAX_HANDLE_ATTEMPTS: u32 = 1_000;
const MAX_SCIM_PAGE_SIZE: i64 = 200;

/// One user group — a workspace-scoped, named group of member participants.
///
/// A storage-layer projection of a `user_groups` row. `Serialize` so a handler
/// can hand the row straight back as JSON; `created_at` renders as RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct UserGroup {
    /// The group's unique id.
    pub id: UserGroupId,
    /// The tenant the group is scoped to (handles are unique within it).
    pub workspace_id: WorkspaceId,
    /// The URL/mention-safe handle (`designers`), normalized lowercase + trimmed.
    pub handle: String,
    /// Human-readable display name the creator gave the group.
    pub name: String,
    /// The participant who created the group (a delete/manage authority alongside
    /// workspace admins).
    pub created_by: ParticipantId,
    /// When the group was created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// One ordered mutation in an atomic SCIM Group PATCH.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScimGroupMutation {
    SetName(String),
    AddMember(ParticipantId),
    RemoveMember(ParticipantId),
}

/// Committed SCIM Group state returned without a fallible post-commit re-read.
#[derive(Debug, Clone)]
pub struct ScimGroupWrite {
    pub group: UserGroup,
    pub members: Vec<ParticipantId>,
}

/// Expected failures from the high-level, transaction-owned SCIM Group writes.
#[derive(Debug, thiserror::Error)]
pub enum UserGroupWriteError {
    #[error("group not found in workspace")]
    NotFound,
    #[error("participant {0} is not a member of workspace")]
    MemberNotInWorkspace(ParticipantId),
    #[error("caller is not authorized to manage this group")]
    NotAuthorized,
    #[error("workspace has no owner, admin, or member to attribute as group creator")]
    WorkspaceHasNoCreator,
    #[error("could not allocate a unique group handle")]
    HandleExhausted,
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

/// Normalize a raw handle into its canonical, comparable form: trimmed of
/// surrounding whitespace and lowercased. Pure (no I/O) so it can be reused on
/// both the write path (storing `create`'s handle) and the read path
/// ([`UserGroupRepo::resolve`]) and unit-tested offline. Callers are responsible
/// for character/length validation; this only canonicalizes case + whitespace.
#[must_use]
pub fn normalize_handle(handle: &str) -> String {
    handle.trim().to_lowercase()
}

/// The columns a [`UserGroup`] is built from, in select order. Shared by every
/// query so the row decoding stays in one place.
const COLUMNS: &str = "id, workspace_id, handle, name, created_by, created_at";

type Row = (
    uuid::Uuid,
    uuid::Uuid,
    String,
    String,
    uuid::Uuid,
    time::OffsetDateTime,
);

type RowWithMembers = (
    uuid::Uuid,
    uuid::Uuid,
    String,
    String,
    uuid::Uuid,
    time::OffsetDateTime,
    Vec<uuid::Uuid>,
);

fn row_to_model(r: Row) -> UserGroup {
    let (id, workspace_id, handle, name, created_by, created_at) = r;
    UserGroup {
        id: UserGroupId::from_uuid(id),
        workspace_id: WorkspaceId::from_uuid(workspace_id),
        handle,
        name,
        created_by: ParticipantId::from_uuid(created_by),
        created_at,
    }
}

/// Repository over the `user_groups` + `user_group_members` tables.
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`UserGroupRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct UserGroupRepo {
    pool: PgPool,
}

impl UserGroupRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persist a new group in `workspace`, returning its generated id. The handle
    /// is stored [`normalize_handle`]-normalized so resolution stays case-stable.
    /// The caller is responsible for workspace-membership and handle/name
    /// validation; a duplicate `(workspace, handle)` surfaces as a `sqlx` unique
    /// violation the handler maps to a `409`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert (including the unique
    /// violation on `(workspace_id, handle)`).
    #[cfg(test)]
    pub(crate) async fn create(
        &self,
        workspace: WorkspaceId,
        handle: &str,
        name: &str,
        created_by: ParticipantId,
    ) -> Result<UserGroupId, sqlx::Error> {
        let id = UserGroupId::new();
        sqlx::query(
            r"INSERT INTO user_groups (id, workspace_id, handle, name, created_by)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id.to_uuid())
        .bind(workspace.to_uuid())
        .bind(normalize_handle(handle))
        .bind(name)
        .bind(created_by.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Create an ordinary user group while holding the caller's workspace
    /// membership row through commit. Concurrent removal or role changes
    /// therefore cannot slip between the route authorization and this insert.
    pub async fn create_authorized(
        &self,
        workspace: WorkspaceId,
        handle: &str,
        name: &str,
        caller: ParticipantId,
    ) -> Result<UserGroup, UserGroupWriteError> {
        let mut tx = self.pool.begin().await?;
        lock_workspace_for_group(&mut tx, workspace).await?;
        lock_workspace_role(&mut tx, workspace, caller).await?;
        let id = UserGroupId::new();
        let row = sqlx::query_as::<_, Row>(
            r"INSERT INTO user_groups (id, workspace_id, handle, name, created_by)
               VALUES ($1, $2, $3, $4, $5)
            RETURNING id, workspace_id, handle, name, created_by, created_at",
        )
        .bind(id.to_uuid())
        .bind(workspace.to_uuid())
        .bind(normalize_handle(handle))
        .bind(name)
        .bind(caller.to_uuid())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(row_to_model(row))
    }

    /// Atomically create a SCIM-backed group and its complete initial member set.
    ///
    /// Every requested participant is locked and verified as a current member of
    /// `workspace` before the group becomes visible. Handle collisions are resolved
    /// inside the same transaction with `ON CONFLICT DO NOTHING`, so a rejected
    /// member or exhausted handle search leaves no orphan group.
    pub async fn create_scim_group(
        &self,
        workspace: WorkspaceId,
        base_handle: &str,
        name: &str,
        members: &[ParticipantId],
    ) -> Result<ScimGroupWrite, UserGroupWriteError> {
        let mut tx = self.pool.begin().await?;
        lock_workspace_for_group(&mut tx, workspace).await?;
        let created_by = scim_creator_in_tx(&mut tx, workspace).await?;
        let members = canonical_members(members);
        lock_workspace_members(&mut tx, workspace, &members).await?;

        let base_handle = {
            let normalized = normalize_handle(base_handle);
            if normalized.is_empty() {
                "group".to_owned()
            } else {
                normalized
            }
        };
        for attempt in 0..MAX_HANDLE_ATTEMPTS {
            let id = UserGroupId::new();
            let handle = suffixed_handle(&base_handle, attempt);
            let inserted = sqlx::query_as::<_, Row>(
                r"INSERT INTO user_groups
                      (id, workspace_id, handle, name, created_by)
                   VALUES ($1, $2, $3, $4, $5)
                   ON CONFLICT (workspace_id, handle) DO NOTHING
                RETURNING id, workspace_id, handle, name, created_by, created_at",
            )
            .bind(id.to_uuid())
            .bind(workspace.to_uuid())
            .bind(&handle)
            .bind(name)
            .bind(created_by.to_uuid())
            .fetch_optional(&mut *tx)
            .await?;
            let Some(row) = inserted else {
                continue;
            };
            insert_members(&mut tx, id, &members).await?;
            let group = row_to_model(row);
            tx.commit().await?;
            return Ok(ScimGroupWrite { group, members });
        }
        tx.rollback().await?;
        Err(UserGroupWriteError::HandleExhausted)
    }

    /// Add one member while atomically enforcing both group and participant
    /// tenancy. This is the write seam for the ordinary user-group API.
    pub async fn add_member_in_workspace(
        &self,
        workspace: WorkspaceId,
        group: UserGroupId,
        participant: ParticipantId,
    ) -> Result<bool, UserGroupWriteError> {
        let mut tx = self.pool.begin().await?;
        lock_workspace_for_group(&mut tx, workspace).await?;
        lock_group(&mut tx, workspace, group).await?;
        lock_workspace_members(&mut tx, workspace, &[participant]).await?;
        let inserted = sqlx::query(
            r"INSERT INTO user_group_members (group_id, participant_id)
               VALUES ($1, $2)
               ON CONFLICT (group_id, participant_id) DO NOTHING",
        )
        .bind(group.to_uuid())
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        tx.commit().await?;
        Ok(inserted)
    }

    /// Add a member through the ordinary API with the authorization decision and
    /// mutation in one transaction. The caller must remain an admin/owner or the
    /// group's creator until commit, and the target must remain in the workspace.
    pub async fn add_member_authorized(
        &self,
        workspace: WorkspaceId,
        group: UserGroupId,
        participant: ParticipantId,
        caller: ParticipantId,
    ) -> Result<bool, UserGroupWriteError> {
        let mut tx = self.pool.begin().await?;
        lock_workspace_for_group(&mut tx, workspace).await?;
        lock_managed_group(&mut tx, workspace, group, caller).await?;
        lock_workspace_members(&mut tx, workspace, &[participant]).await?;
        let inserted = sqlx::query(
            r"INSERT INTO user_group_members (group_id, participant_id)
               VALUES ($1, $2)
               ON CONFLICT (group_id, participant_id) DO NOTHING",
        )
        .bind(group.to_uuid())
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        tx.commit().await?;
        Ok(inserted)
    }

    /// Remove a member through the ordinary API with authorization and mutation
    /// in one transaction. The target need not still be a workspace member so a
    /// stale group edge remains removable after deprovisioning.
    pub async fn remove_member_authorized(
        &self,
        workspace: WorkspaceId,
        group: UserGroupId,
        participant: ParticipantId,
        caller: ParticipantId,
    ) -> Result<bool, UserGroupWriteError> {
        let mut tx = self.pool.begin().await?;
        lock_workspace_for_group(&mut tx, workspace).await?;
        lock_managed_group(&mut tx, workspace, group, caller).await?;
        let removed = sqlx::query(
            "DELETE FROM user_group_members WHERE group_id = $1 AND participant_id = $2",
        )
        .bind(group.to_uuid())
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        tx.commit().await?;
        Ok(removed)
    }

    /// Delete an ordinary user group with its authorization decision protected
    /// by the same transaction and row locks as the delete.
    pub async fn delete_authorized(
        &self,
        workspace: WorkspaceId,
        group: UserGroupId,
        caller: ParticipantId,
    ) -> Result<(), UserGroupWriteError> {
        let mut tx = self.pool.begin().await?;
        lock_workspace_for_group(&mut tx, workspace).await?;
        lock_managed_group(&mut tx, workspace, group, caller).await?;
        sqlx::query("DELETE FROM user_groups WHERE id = $1 AND workspace_id = $2")
            .bind(group.to_uuid())
            .bind(workspace.to_uuid())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Atomically replace a SCIM group's display name and exact member set.
    ///
    /// The group is selected by `(workspace,id)` and locked before any mutation.
    /// A foreign/unknown id therefore has no side effects, while a member that is
    /// absent from the token workspace rolls the entire replacement back.
    pub async fn replace_scim_group(
        &self,
        workspace: WorkspaceId,
        id: UserGroupId,
        name: &str,
        members: &[ParticipantId],
    ) -> Result<ScimGroupWrite, UserGroupWriteError> {
        let mut tx = self.pool.begin().await?;
        lock_workspace_for_group(&mut tx, workspace).await?;
        let mut group = lock_group(&mut tx, workspace, id).await?;
        let members = canonical_members(members);
        lock_workspace_members(&mut tx, workspace, &members).await?;

        sqlx::query("UPDATE user_groups SET name = $3 WHERE id = $1 AND workspace_id = $2")
            .bind(id.to_uuid())
            .bind(workspace.to_uuid())
            .bind(name)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM user_group_members WHERE group_id = $1")
            .bind(id.to_uuid())
            .execute(&mut *tx)
            .await?;
        insert_members(&mut tx, id, &members).await?;

        group.name = name.to_owned();
        tx.commit().await?;
        Ok(ScimGroupWrite { group, members })
    }

    /// Apply an ordered SCIM Group PATCH as one transaction.
    ///
    /// Participants being added are validated and locked before the first rename
    /// or membership mutation. Removes deliberately do not require a current
    /// workspace membership: deprovisioning removes that membership first, and a
    /// later `IdP` Group PATCH must still be able to clean the stale group edge.
    /// A later failure can therefore never expose a prefix of the PATCH operations.
    pub async fn patch_scim_group(
        &self,
        workspace: WorkspaceId,
        id: UserGroupId,
        mutations: &[ScimGroupMutation],
    ) -> Result<ScimGroupWrite, UserGroupWriteError> {
        let mut tx = self.pool.begin().await?;
        lock_workspace_for_group(&mut tx, workspace).await?;
        let mut group = lock_group(&mut tx, workspace, id).await?;
        let added_members = canonical_members(
            &mutations
                .iter()
                .filter_map(|mutation| match mutation {
                    ScimGroupMutation::AddMember(participant) => Some(*participant),
                    ScimGroupMutation::SetName(_) | ScimGroupMutation::RemoveMember(_) => None,
                })
                .collect::<Vec<_>>(),
        );
        lock_workspace_members(&mut tx, workspace, &added_members).await?;

        for mutation in mutations {
            match mutation {
                ScimGroupMutation::SetName(name) => {
                    sqlx::query(
                        "UPDATE user_groups SET name = $3 WHERE id = $1 AND workspace_id = $2",
                    )
                    .bind(id.to_uuid())
                    .bind(workspace.to_uuid())
                    .bind(name)
                    .execute(&mut *tx)
                    .await?;
                    group.name.clone_from(name);
                }
                ScimGroupMutation::AddMember(participant) => {
                    sqlx::query(
                        r"INSERT INTO user_group_members (group_id, participant_id)
                           VALUES ($1, $2)
                           ON CONFLICT (group_id, participant_id) DO NOTHING",
                    )
                    .bind(id.to_uuid())
                    .bind(participant.to_uuid())
                    .execute(&mut *tx)
                    .await?;
                }
                ScimGroupMutation::RemoveMember(participant) => {
                    sqlx::query(
                        "DELETE FROM user_group_members WHERE group_id = $1 AND participant_id = $2",
                    )
                    .bind(id.to_uuid())
                    .bind(participant.to_uuid())
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
        let members = members_in_tx(&mut tx, id).await?;
        tx.commit().await?;
        Ok(ScimGroupWrite { group, members })
    }

    /// Rename a group — update its human-readable `name` (the `handle`, Aero's
    /// stable mention key, is intentionally immutable). Scoped to `workspace` so a
    /// group id from another tenant is a no-op. Returns `true` iff a row changed.
    ///
    /// Used by the SCIM Group surface ([`crate::scim`] in `aero-server`) to reflect
    /// a `displayName` replace onto the backing group; the `@-usergroup` REST
    /// surface does not currently expose rename, so this is purely additive.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn rename(
        &self,
        id: UserGroupId,
        workspace: WorkspaceId,
        name: &str,
    ) -> Result<bool, sqlx::Error> {
        let result =
            sqlx::query("UPDATE user_groups SET name = $3 WHERE id = $1 AND workspace_id = $2")
                .bind(id.to_uuid())
                .bind(workspace.to_uuid())
                .bind(name)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Fetch one group by id, or `None` if no such row exists.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(&self, id: UserGroupId) -> Result<Option<UserGroup>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM user_groups WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// List every group in `workspace`, by handle ascending.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_workspace(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<UserGroup>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM user_groups
              WHERE workspace_id = $1
              ORDER BY handle ASC, id ASC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(workspace.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Count and fetch one bounded SCIM page with each group's members aggregated
    /// in the page query. This avoids full-workspace materialization and N+1
    /// membership queries for large tenants.
    pub async fn list_scim_groups_page(
        &self,
        workspace: WorkspaceId,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<ScimGroupWrite>), sqlx::Error> {
        let limit = limit.clamp(0, MAX_SCIM_PAGE_SIZE);
        let offset = offset.max(0);
        let total = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM user_groups WHERE workspace_id = $1",
        )
        .bind(workspace.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        let rows = sqlx::query_as::<_, RowWithMembers>(
            r"SELECT page.id,
                     page.workspace_id,
                     page.handle,
                     page.name,
                     page.created_by,
                     page.created_at,
                     ARRAY(
                         SELECT membership.participant_id
                           FROM user_group_members membership
                          WHERE membership.group_id = page.id
                          ORDER BY membership.added_at, membership.participant_id
                     )
                FROM (
                    SELECT id, workspace_id, handle, name, created_by, created_at
                      FROM user_groups
                     WHERE workspace_id = $1
                     ORDER BY handle, id
                     LIMIT $2 OFFSET $3
                ) page
               ORDER BY page.handle, page.id",
        )
        .bind(workspace.to_uuid())
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let groups = rows
            .into_iter()
            .map(
                |(id, workspace_id, handle, name, created_by, created_at, members)| {
                    ScimGroupWrite {
                        group: row_to_model((
                            id,
                            workspace_id,
                            handle,
                            name,
                            created_by,
                            created_at,
                        )),
                        members: members.into_iter().map(ParticipantId::from_uuid).collect(),
                    }
                },
            )
            .collect();
        Ok((total, groups))
    }

    /// Resolve a group by its (normalized) handle within `workspace`, or `None`.
    /// The lookup [`normalize_handle`]-normalizes the input so `@Designers` and
    /// `@designers` resolve identically — the seam an `@handle` mention uses.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn resolve(
        &self,
        workspace: WorkspaceId,
        handle: &str,
    ) -> Result<Option<UserGroup>, sqlx::Error> {
        let sql =
            format!("SELECT {COLUMNS} FROM user_groups WHERE workspace_id = $1 AND handle = $2");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(workspace.to_uuid())
            .bind(normalize_handle(handle))
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// Delete a group (its memberships cascade away via the FK). Scoped to
    /// `workspace` so a group id from another tenant is a no-op. Returns `true`
    /// iff a row was removed.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    #[cfg(test)]
    pub(crate) async fn delete(
        &self,
        id: UserGroupId,
        workspace: WorkspaceId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM user_groups WHERE id = $1 AND workspace_id = $2")
            .bind(id.to_uuid())
            .bind(workspace.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Delete a SCIM group under the same workspace-first lock order as every
    /// other aggregate group mutation.
    pub async fn delete_scim_group(
        &self,
        workspace: WorkspaceId,
        id: UserGroupId,
    ) -> Result<bool, UserGroupWriteError> {
        let mut tx = self.pool.begin().await?;
        lock_workspace_for_group(&mut tx, workspace).await?;
        match lock_group(&mut tx, workspace, id).await {
            Ok(_) => {}
            Err(UserGroupWriteError::NotFound) => {
                tx.rollback().await?;
                return Ok(false);
            }
            Err(error) => return Err(error),
        }
        sqlx::query("DELETE FROM user_groups WHERE id = $1 AND workspace_id = $2")
            .bind(id.to_uuid())
            .bind(workspace.to_uuid())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Add `participant` to `group`, idempotently (a duplicate is a no-op via
    /// `ON CONFLICT DO NOTHING`). The caller is responsible for authorization and
    /// for verifying the group exists.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    #[cfg(test)]
    pub(crate) async fn add_member(
        &self,
        group: UserGroupId,
        participant: ParticipantId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO user_group_members (group_id, participant_id)
               VALUES ($1, $2)
               ON CONFLICT (group_id, participant_id) DO NOTHING",
        )
        .bind(group.to_uuid())
        .bind(participant.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Remove `participant` from `group`. Returns `true` iff a membership row was
    /// actually removed (a second removal, or a non-member, returns `false`).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    #[cfg(test)]
    pub(crate) async fn remove_member(
        &self,
        group: UserGroupId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM user_group_members WHERE group_id = $1 AND participant_id = $2",
        )
        .bind(group.to_uuid())
        .bind(participant.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// List the group ids `participant` belongs to within `workspace`. Joins
    /// membership against `user_groups` so the result is tenant-scoped (a
    /// participant may belong to groups across several workspaces). The seam the
    /// information-barrier check ([`crate::BarrierRepo::barred`]) reads to map a
    /// participant to their groups.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn groups_for(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<Vec<UserGroupId>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT m.group_id
               FROM user_group_members m
               JOIN user_groups g ON g.id = m.group_id
              WHERE g.workspace_id = $1 AND m.participant_id = $2
              ORDER BY m.group_id ASC",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(g,)| UserGroupId::from_uuid(g))
            .collect())
    }

    /// List the participant ids that belong to `group`, oldest membership first.
    /// The seam an `@handle` mention fan-out reads to find recipients.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn members(&self, group: UserGroupId) -> Result<Vec<ParticipantId>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT participant_id
               FROM user_group_members
              WHERE group_id = $1
              ORDER BY added_at ASC, participant_id ASC",
        )
        .bind(group.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(p,)| ParticipantId::from_uuid(p))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_handle_lowercases_and_trims() {
        assert_eq!(normalize_handle("  Designers  "), "designers");
        assert_eq!(normalize_handle("ON-CALL"), "on-call");
        assert_eq!(normalize_handle("team_42"), "team_42");
        // Idempotent: normalizing an already-normalized handle is a fixed point.
        assert_eq!(normalize_handle("designers"), "designers");
    }

    #[test]
    fn scim_handle_candidates_stay_within_the_storage_limit() {
        let base = "a".repeat(MAX_HANDLE_CHARS + 10);
        let first = suffixed_handle(&base, 0);
        let retry = suffixed_handle(&base, 1);

        assert_eq!(first.chars().count(), MAX_HANDLE_CHARS);
        assert!(retry.chars().count() <= MAX_HANDLE_CHARS);
        assert!(retry.ends_with("-2"));
    }
}

#[cfg(test)]
#[path = "user_group/db_tests.rs"]
mod db_tests;

#[cfg(test)]
#[path = "user_group/scim_tests.rs"]
mod scim_tests;
