//! Workspace user-deactivation repository (admin revokes a member's access).
//!
//! Backs `migrations/0045_workspace_deactivations.sql` plus the governance
//! constraints in `migrations/0189_deactivation_governance.sql`. An admin/owner
//! deactivates a member within a workspace; a deactivated member is then denied
//! access to that workspace's rooms. The access-enforcement check itself lives in
//! the service layer (`ImService::assert_room_access`), which calls
//! [`DeactivationRepo::is_deactivated`].
//!
//! Each deactivation is a single `(workspace_id, participant_id)` pair — the
//! composite primary key makes deactivating idempotent and needs no surrogate id.
//! Reactivating removes the row. Request paths use the transaction-owned
//! authorized methods so the current roles, effective caller state, mutation,
//! and audit event share one commit. The [`DeactivatedMember`] model lives here
//! (and is re-exported from the crate root), since it is a storage-layer
//! projection.

use aero_common::{ParticipantId, WorkspaceId, WorkspaceRole};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};

use crate::{role_can_manage_member, AuditRepo};

/// One deactivated member — a `(workspace, participant)` pair plus who/when.
///
/// A storage-layer projection of a `workspace_deactivations` row. `Serialize` so
/// a handler can hand the row straight back as JSON; `deactivated_at` renders as
/// RFC 3339. `deactivated_by` is `None` when the actor was not recorded.
#[derive(Debug, Clone, Serialize)]
pub struct DeactivatedMember {
    /// The tenant the deactivation is scoped to.
    pub workspace_id: WorkspaceId,
    /// The member that was deactivated within the workspace.
    pub participant_id: ParticipantId,
    /// When the member was deactivated (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub deactivated_at: time::OffsetDateTime,
    /// The admin/owner who performed the deactivation, if recorded.
    pub deactivated_by: Option<ParticipantId>,
}

/// Transaction-owned failures for workspace deactivation governance.
#[derive(Debug, thiserror::Error)]
pub enum DeactivationWriteError {
    /// The workspace no longer exists.
    #[error("workspace not found")]
    WorkspaceNotFound,
    /// The target is not a current workspace member.
    #[error("workspace member not found")]
    MemberNotFound,
    /// The caller is not an effective administrator or cannot manage the
    /// target's current role.
    #[error("caller is not authorized to manage this member")]
    NotAuthorized,
    /// Administrators cannot revoke their own access through this endpoint.
    #[error("cannot deactivate yourself")]
    SelfDeactivation,
    /// Ownership must be transferred/demoted explicitly before deactivation.
    #[error("workspace owners cannot be deactivated")]
    OwnerProtected,
    /// A channel would have no remaining effective owner.
    #[error("channel ownership must be transferred before deactivation")]
    ChannelOwnerProtected,
    /// Database failure.
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

/// The columns a [`DeactivatedMember`] is built from, in select order. Shared by
/// every query so the row decoding stays in one place.
const COLUMNS: &str = "workspace_id, participant_id, deactivated_at, deactivated_by";

#[derive(Debug, Clone, Copy, sqlx::FromRow)]
struct Row {
    workspace_id: uuid::Uuid,
    participant_id: uuid::Uuid,
    deactivated_at: time::OffsetDateTime,
    deactivated_by: Option<uuid::Uuid>,
}

fn row_to_model(r: Row) -> DeactivatedMember {
    DeactivatedMember {
        workspace_id: WorkspaceId::from_uuid(r.workspace_id),
        participant_id: ParticipantId::from_uuid(r.participant_id),
        deactivated_at: r.deactivated_at,
        deactivated_by: r.deactivated_by.map(ParticipantId::from_uuid),
    }
}

/// Repository over the `workspace_deactivations` table (admin member-deactivation).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`DeactivationRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct DeactivationRepo {
    pool: PgPool,
}

impl DeactivationRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Deactivate a member under the same workspace lock that protects the
    /// caller/target role decision. The caller must still be an effective
    /// administrator (active account, not workspace-deactivated, and enrolled
    /// in mandatory 2FA when configured), may only manage a role at or below
    /// their own, and can never deactivate an owner or themselves.
    ///
    /// The state change and audit event commit together. Repeating an already
    /// completed deactivation is an idempotent `Ok(false)`.
    pub async fn deactivate_authorized(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
        by: ParticipantId,
    ) -> Result<bool, DeactivationWriteError> {
        if participant == by {
            return Err(DeactivationWriteError::SelfDeactivation);
        }
        let mut tx = self.pool.begin().await?;
        crate::ownership::lock_membership_governance(&mut tx).await?;
        let (caller_role, target_role) =
            lock_governance_roles(&mut tx, workspace, by, participant).await?;
        if target_role == WorkspaceRole::Owner {
            return Err(DeactivationWriteError::OwnerProtected);
        }
        if !role_can_manage_member(caller_role, target_role) {
            return Err(DeactivationWriteError::NotAuthorized);
        }

        let inserted = sqlx::query(
            r"INSERT INTO workspace_deactivations
                  (workspace_id, participant_id, deactivated_by)
               VALUES ($1, $2, $3)
               ON CONFLICT (workspace_id, participant_id) DO NOTHING",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(by.to_uuid())
        .execute(&mut *tx)
        .await
        .map_err(map_deactivation_storage_error)?
        .rows_affected()
            == 1;
        if inserted {
            let target = participant.to_string();
            AuditRepo::append_in_tx(
                &mut tx,
                workspace,
                Some(by),
                "member.deactivate",
                Some(&target),
                serde_json::json!({ "role": target_role }),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(inserted)
    }

    /// Reactivate a member with a transactionally current caller/target role
    /// decision. This also repairs legacy owner deactivations, but only an owner
    /// can manage another owner's row. Repeating a settled reactivation is an
    /// idempotent `Ok(false)`.
    pub async fn reactivate_authorized(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
        by: ParticipantId,
    ) -> Result<bool, DeactivationWriteError> {
        let mut tx = self.pool.begin().await?;
        crate::ownership::lock_membership_governance(&mut tx).await?;
        let (caller_role, target_role) =
            lock_governance_roles(&mut tx, workspace, by, participant).await?;
        if !role_can_manage_member(caller_role, target_role) {
            return Err(DeactivationWriteError::NotAuthorized);
        }
        let removed = sqlx::query(
            "DELETE FROM workspace_deactivations
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if removed {
            let target = participant.to_string();
            AuditRepo::append_in_tx(
                &mut tx,
                workspace,
                Some(by),
                "member.reactivate",
                Some(&target),
                serde_json::json!({ "role": target_role }),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(removed)
    }

    /// Deactivate `participant` in `workspace`, recording `by` as the actor.
    /// Idempotent: re-deactivating an already-deactivated member is a no-op
    /// (`ON CONFLICT DO NOTHING`), leaving the original actor/timestamp intact.
    /// This is a low-level compatibility helper. Request paths must use
    /// [`Self::deactivate_authorized`] so authorization and audit cannot race the
    /// mutation.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn deactivate(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
        by: ParticipantId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO workspace_deactivations (workspace_id, participant_id, deactivated_by)
               VALUES ($1, $2, $3)
               ON CONFLICT (workspace_id, participant_id) DO NOTHING",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(by.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Reactivate `participant` in `workspace`, restoring their access. This is
    /// a low-level compatibility helper; request paths must use
    /// [`Self::reactivate_authorized`]. Returns
    /// `true` iff a row was removed — reactivating a member who was never
    /// deactivated (or a second reactivate) is a no-op returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn reactivate(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM workspace_deactivations WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Whether `participant` is currently deactivated in `workspace`. This is the
    /// integrator seam: the service layer calls it inside `assert_room_access` to
    /// forbid a deactivated member from that workspace's room data.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn is_deactivated(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (i32,)>(
            r"SELECT 1 FROM workspace_deactivations
               WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// List the deactivated members in `workspace`, newest first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<DeactivatedMember>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM workspace_deactivations
              WHERE workspace_id = $1
              ORDER BY deactivated_at DESC, participant_id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(workspace.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }
}

fn map_deactivation_storage_error(error: sqlx::Error) -> DeactivationWriteError {
    if crate::is_channel_effective_owner_violation(&error) {
        DeactivationWriteError::ChannelOwnerProtected
    } else {
        DeactivationWriteError::Storage(error)
    }
}

/// Lock the tenant and the two membership rows in canonical order, then return
/// a current effective caller role plus the target's raw governance role.
async fn lock_governance_roles(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    caller: ParticipantId,
    target: ParticipantId,
) -> Result<(WorkspaceRole, WorkspaceRole), DeactivationWriteError> {
    let require_2fa = sqlx::query_scalar::<_, bool>(
        "SELECT require_2fa FROM workspaces WHERE id = $1 FOR UPDATE",
    )
    .bind(workspace.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(DeactivationWriteError::WorkspaceNotFound)?;

    let mut ids = vec![caller.to_uuid(), target.to_uuid()];
    ids.sort_unstable();
    ids.dedup();
    let roles = sqlx::query_as::<_, (uuid::Uuid, String)>(
        r"SELECT participant_id, role
            FROM workspace_members
           WHERE workspace_id = $1
             AND participant_id = ANY($2)
           ORDER BY participant_id
           FOR UPDATE",
    )
    .bind(workspace.to_uuid())
    .bind(&ids)
    .fetch_all(&mut **tx)
    .await?;
    let role_for = |participant: ParticipantId| {
        roles
            .iter()
            .find(|(id, _)| *id == participant.to_uuid())
            .and_then(|(_, role)| WorkspaceRole::from_db_str(role))
    };
    let caller_role = role_for(caller).ok_or(DeactivationWriteError::NotAuthorized)?;
    let target_role = role_for(target).ok_or(DeactivationWriteError::MemberNotFound)?;

    let caller_effective = sqlx::query_scalar::<_, bool>(
        r"SELECT EXISTS (
               SELECT 1
                 FROM participants participant
                WHERE participant.id = $2
                  AND participant.deleted_at IS NULL
                  AND NOT EXISTS (
                      SELECT 1
                        FROM workspace_deactivations deactivated
                       WHERE deactivated.workspace_id = $1
                         AND deactivated.participant_id = $2
                  )
                  AND (
                      participant.kind <> 'human'
                      OR NOT $3
                      OR EXISTS (
                          SELECT 1
                            FROM totp_secrets totp
                           WHERE totp.participant_id = $2
                             AND totp.activated
                      )
                  )
           )",
    )
    .bind(workspace.to_uuid())
    .bind(caller.to_uuid())
    .bind(require_2fa)
    .fetch_one(&mut **tx)
    .await?;
    if !caller_effective || !caller_role.can_administer() {
        return Err(DeactivationWriteError::NotAuthorized);
    }
    Ok((caller_role, target_role))
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored deactivation
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::{AuditRepo, RoomMemberRole, RoomRepo, WorkspaceRepo};
    use aero_common::RoomKind;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway participant so the test is self-contained.
    async fn mk_participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("deactivation-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    async fn enroll(
        pool: &PgPool,
        workspace: WorkspaceId,
        participant: ParticipantId,
        role: WorkspaceRole,
    ) {
        WorkspaceRepo::new(pool.clone())
            .add_member(workspace, participant, role)
            .await
            .expect("enroll workspace member");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn deactivate_is_deactivated_list_reactivate() {
        let p = pool();
        let repo = DeactivationRepo::new(p.clone());
        let member = mk_participant(&p).await;
        let admin = mk_participant(&p).await;
        let workspaces = WorkspaceRepo::new(p.clone());
        let ws = workspaces
            .create(
                format!("deactivation-basic-{admin}"),
                format!("deactivation-basic-{admin}"),
                admin,
            )
            .await
            .expect("create isolated workspace")
            .id;
        enroll(&p, ws, member, WorkspaceRole::Member).await;

        // Not deactivated yet.
        assert!(
            !repo.is_deactivated(ws, member).await.unwrap(),
            "member is active before deactivate"
        );

        // deactivate → idempotent; is_deactivated true; list shows it with the actor.
        repo.deactivate(ws, member, admin).await.unwrap();
        repo.deactivate(ws, member, admin).await.unwrap(); // idempotent
        assert!(
            repo.is_deactivated(ws, member).await.unwrap(),
            "member is deactivated"
        );
        let listed = repo.list(ws).await.unwrap();
        let found = listed
            .iter()
            .find(|d| d.participant_id == member)
            .expect("list shows the deactivated member");
        assert_eq!(found.deactivated_by, Some(admin));

        // reactivate → true once; is_deactivated false; absent from the list.
        assert!(
            repo.reactivate(ws, member).await.unwrap(),
            "reactivate removes the row"
        );
        assert!(
            !repo.reactivate(ws, member).await.unwrap(),
            "second reactivate is a no-op"
        );
        assert!(
            !repo.is_deactivated(ws, member).await.unwrap(),
            "member is active again after reactivate"
        );
        assert!(
            !repo
                .list(ws)
                .await
                .unwrap()
                .iter()
                .any(|d| d.participant_id == member),
            "reactivated member leaves the list"
        );

        // Cleanup so reruns stay self-contained.
        assert!(workspaces.delete(ws).await.expect("delete workspace"));
        sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
            .bind([member.to_uuid(), admin.to_uuid()])
            .execute(&p)
            .await
            .expect("delete participants");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn authorized_writes_enforce_effective_hierarchy_owner_guard_and_audit() {
        let pool = pool();
        let workspaces = WorkspaceRepo::new(pool.clone());
        let repo = DeactivationRepo::new(pool.clone());
        let owner = mk_participant(&pool).await;
        let admin = mk_participant(&pool).await;
        let second_admin = mk_participant(&pool).await;
        let member = mk_participant(&pool).await;
        let workspace = workspaces
            .create(
                format!("deactivation-governance-{owner}"),
                format!("deactivation-governance-{owner}"),
                owner,
            )
            .await
            .expect("create workspace")
            .id;
        enroll(&pool, workspace, admin, WorkspaceRole::Admin).await;
        enroll(&pool, workspace, second_admin, WorkspaceRole::Admin).await;
        enroll(&pool, workspace, member, WorkspaceRole::Member).await;

        assert!(matches!(
            repo.deactivate_authorized(workspace, owner, admin)
                .await
                .expect_err("an owner cannot be deactivated"),
            DeactivationWriteError::OwnerProtected
        ));
        assert!(matches!(
            repo.deactivate_authorized(workspace, admin, member)
                .await
                .expect_err("a member cannot deactivate an administrator"),
            DeactivationWriteError::NotAuthorized
        ));

        assert!(repo
            .deactivate_authorized(workspace, member, admin)
            .await
            .expect("admin deactivates member"));
        assert!(!repo
            .deactivate_authorized(workspace, member, admin)
            .await
            .expect("deactivation is idempotent"));

        assert!(repo
            .deactivate_authorized(workspace, second_admin, owner)
            .await
            .expect("owner deactivates administrator"));
        assert!(matches!(
            repo.reactivate_authorized(workspace, member, second_admin)
                .await
                .expect_err("a deactivated administrator has no stale authority"),
            DeactivationWriteError::NotAuthorized
        ));
        assert!(repo
            .reactivate_authorized(workspace, second_admin, owner)
            .await
            .expect("owner reactivates administrator"));

        let events = AuditRepo::new(pool.clone())
            .list_for_workspace(workspace, None, Some(20))
            .await
            .expect("list audit events");
        let member_target = member.to_string();
        let second_admin_target = second_admin.to_string();
        assert!(events.iter().any(|event| {
            event.action == "member.deactivate"
                && event.actor_id == Some(admin)
                && event.target.as_deref() == Some(member_target.as_str())
        }));
        assert!(events.iter().any(|event| {
            event.action == "member.reactivate"
                && event.actor_id == Some(owner)
                && event.target.as_deref() == Some(second_admin_target.as_str())
        }));

        let owner_insert = sqlx::query(
            r"INSERT INTO workspace_deactivations
                  (workspace_id, participant_id, deactivated_by)
               VALUES ($1, $2, $3)",
        )
        .bind(workspace.to_uuid())
        .bind(owner.to_uuid())
        .bind(admin.to_uuid())
        .execute(&pool)
        .await
        .expect_err("database rejects owner deactivation");
        assert_eq!(
            owner_insert
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::code),
            Some(std::borrow::Cow::Borrowed("23514"))
        );

        let owner_promotion = sqlx::query(
            r"UPDATE workspace_members
                  SET role = 'owner'
                WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(member.to_uuid())
        .execute(&pool)
        .await
        .expect_err("database rejects promotion of a deactivated member");
        assert_eq!(
            owner_promotion
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::code),
            Some(std::borrow::Cow::Borrowed("23514"))
        );

        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(workspace.to_uuid())
            .execute(&pool)
            .await
            .expect("cleanup workspace");
        sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
            .bind([
                owner.to_uuid(),
                admin.to_uuid(),
                second_admin.to_uuid(),
                member.to_uuid(),
            ])
            .execute(&pool)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn sole_effective_channel_owner_cannot_be_deactivated_by_any_write_path() {
        let pool = pool();
        let workspaces = WorkspaceRepo::new(pool.clone());
        let rooms = RoomRepo::new(pool.clone());
        let repo = DeactivationRepo::new(pool.clone());
        let workspace_owner = mk_participant(&pool).await;
        let channel_owner = mk_participant(&pool).await;
        let successor = mk_participant(&pool).await;
        let workspace = workspaces
            .create(
                format!("channel-deactivation-{workspace_owner}"),
                format!("channel-deactivation-{workspace_owner}"),
                workspace_owner,
            )
            .await
            .unwrap()
            .id;
        for participant in [channel_owner, successor] {
            enroll(&pool, workspace, participant, WorkspaceRole::Member).await;
        }
        let room = rooms
            .create_in_workspace(
                workspace,
                RoomKind::Channel,
                Some("deactivation owner guard".into()),
                channel_owner,
            )
            .await
            .unwrap()
            .id;
        rooms.add_member(room, successor).await.unwrap();

        assert!(matches!(
            repo.deactivate_authorized(workspace, channel_owner, workspace_owner)
                .await
                .expect_err("authorized path protects channel governance"),
            DeactivationWriteError::ChannelOwnerProtected
        ));
        let direct = repo
            .deactivate(workspace, channel_owner, workspace_owner)
            .await
            .expect_err("low-level path is protected by the database");
        assert!(crate::is_channel_effective_owner_violation(&direct));

        rooms
            .change_channel_member_role_authorized(
                room,
                channel_owner,
                successor,
                RoomMemberRole::Owner,
            )
            .await
            .unwrap();
        assert!(repo
            .deactivate_authorized(workspace, channel_owner, workspace_owner)
            .await
            .expect("explicitly adding an effective successor unblocks deactivation"));
    }
}
