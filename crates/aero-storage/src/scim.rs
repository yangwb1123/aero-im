//! SCIM 2.0 provisioning repository (RFC 7643/7644 — Users + Groups).
//!
//! Backs `migrations/0015_scim.sql`. Two concerns, one repo:
//!
//! 1. **Tokens** (`scim_tokens`): per-workspace bearer tokens an external `IdP`
//!    (Okta / Azure AD) presents on every SCIM request. Only the SHA-256 *hash*
//!    of a token is stored — the plaintext is shown once at mint time and never
//!    persisted (mirrors password-hash handling). Resolving an incoming token to
//!    a workspace is the SCIM auth check.
//!
//! 2. **Users** ([`scim_users`]): a workspace-scoped SCIM identity layered over a
//!    GLOBAL [`participant`](crate::ParticipantRepo). A SCIM "User" is a
//!    `participant` + a `scim_users` row carrying `userName` / `externalId` /
//!    `active`. PATCH `active=false` installs a reversible workspace access fence
//!    while retaining membership/room topology; DELETE performs final tenant
//!    deprovisioning and removes those edges. Neither flow deletes the global
//!    participant because it may belong to other tenants. Aggregate mutations
//!    are transactional.
//!
//! Token hashing is a pure free function ([`hash_token`]) so it unit-tests
//! offline, mirroring how the rest of `aero-storage` keeps testable logic
//! separate from live SQL.

use aero_common::{ParticipantId, WorkspaceId};
use sqlx::PgPool;

use crate::{
    audit::AuditRepo,
    sso::{resolve_external_identity_in_tx, SsoResolveError},
};

mod identity;
pub use identity::ScimUserUpdate;
use identity::{bind_existing_participant_identity_in_tx, validated_identity_binding};
mod tokens;
pub use tokens::{
    generate_token, hash_token, ScimTokenRecord, ScimTokenWriteError, MAX_SCIM_TOKENS_PER_WORKSPACE,
};

/// One provisioned SCIM user row (the workspace-scoped identity over a global
/// participant). The handler maps this into the RFC 7643 `User` resource.
#[derive(Debug, Clone)]
pub struct ScimUserRow {
    pub workspace_id: WorkspaceId,
    pub participant_id: ParticipantId,
    pub user_name: String,
    pub external_id: Option<String>,
    pub active: bool,
    pub created_at: time::OffsetDateTime,
    pub updated_at: time::OffsetDateTime,
}

/// Expected aggregate-write failures for a SCIM User.
#[derive(Debug, thiserror::Error)]
pub enum ScimUserWriteError {
    /// Deprovisioning the current workspace owner would strand owner-only
    /// governance operations. Ownership must be transferred first.
    #[error("workspace ownership must be transferred before deprovisioning this user")]
    OwnerDeprovision,
    /// A workspace-scoped removal would strand a channel owned by this user.
    #[error("channel ownership must be transferred before deprovisioning this user")]
    ChannelOwnerDeprovision,
    /// The stable `IdP` identity was explicitly erased and cannot be recreated by
    /// an ordinary SCIM retry.
    #[error("external identity is tombstoned")]
    IdentityTombstoned,
    /// Identity-aware SCIM requires the `IdP`'s stable subject in `externalId`.
    #[error("externalId is required when a SCIM identity issuer is configured")]
    IdentitySubjectRequired,
    /// Issuers are protocol identifiers and must be stored exactly as emitted;
    /// silently trimming a slash would create a second identity namespace.
    #[error("SCIM identity issuer is invalid")]
    InvalidIdentityIssuer,
    /// The stable subject of an identity-bound SCIM user cannot be changed by a
    /// routine profile PATCH. Rebinding requires a dedicated audited workflow.
    #[error("externalId is immutable for an identity-bound SCIM user")]
    IdentitySubjectImmutable,
    /// The requested issuer/subject is already owned by a different immutable
    /// participant. Routine SCIM updates never merge or re-point accounts.
    #[error("external identity is linked to another participant")]
    IdentityConflict,
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

/// Largest page a `list_users` call will return, regardless of requested count.
/// Mirrors the audit/history caps elsewhere in the crate.
const MAX_PAGE: i64 = 200;

/// Clamp a requested SCIM `count` into `1..=MAX_PAGE` (defaulting `None`/non-positive
/// to `MAX_PAGE`). Pure so it unit-tests without a DB.
#[must_use]
fn clamp_count(requested: Option<i64>) -> i64 {
    match requested {
        Some(n) if n >= 1 => n.min(MAX_PAGE),
        _ => MAX_PAGE,
    }
}

/// Normalize a SCIM `startIndex` (1-based, RFC 7644 §3.4.2) into a 0-based SQL
/// `OFFSET`. Absent or `< 1` is treated as the first page (offset 0). Pure.
#[must_use]
fn start_offset(start_index: Option<i64>) -> i64 {
    match start_index {
        Some(n) if n >= 1 => n - 1,
        _ => 0,
    }
}

#[derive(Clone)]
pub struct ScimRepo {
    pool: PgPool,
}

impl ScimRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    // ------------------------------------------------------------ users

    /// Provision the complete SCIM user aggregate atomically.
    ///
    /// The participant identity, optional active workspace membership, and
    /// workspace-scoped SCIM mapping either all commit or all roll back. This is
    /// the route-facing create path: in particular, a concurrent `userName`
    /// collision cannot leave behind a credential-less participant or workspace
    /// member.
    pub async fn provision_user(
        &self,
        workspace: WorkspaceId,
        display_name: &str,
        user_name: &str,
        external_id: Option<&str>,
        active: bool,
    ) -> Result<ScimUserRow, sqlx::Error> {
        match self
            .provision_user_with_identity(
                workspace,
                display_name,
                user_name,
                external_id,
                active,
                None,
            )
            .await
        {
            Ok(row) => Ok(row),
            Err(ScimUserWriteError::Storage(error)) => Err(error),
            Err(error) => Err(sqlx::Error::Protocol(error.to_string())),
        }
    }

    /// Provision a SCIM user and, when `identity_issuer` plus `external_id` are
    /// present, atomically bind/reuse the same canonical participant used by
    /// OIDC. This prevents SCIM pre-provisioning followed by first login from
    /// producing two Aero accounts for one Snaplink subject.
    pub async fn provision_user_with_identity(
        &self,
        workspace: WorkspaceId,
        display_name: &str,
        user_name: &str,
        external_id: Option<&str>,
        active: bool,
        identity_issuer: Option<&str>,
    ) -> Result<ScimUserRow, ScimUserWriteError> {
        let now = time::OffsetDateTime::now_utc();
        let identity_binding = validated_identity_binding(identity_issuer, external_id)?;

        let mut tx = self.pool.begin().await?;
        // Account erasure enters the cross-workspace governance fence before
        // acquiring external-identity and participant locks. Keep the same
        // order here: workspace → identity → participant. Taking the identity
        // advisory lock first would deadlock with a concurrent GDPR erasure
        // that already owns this workspace row.
        if !lock_scim_workspace(&mut tx, workspace).await? {
            return Err(ScimUserWriteError::Storage(sqlx::Error::RowNotFound));
        }
        let participant = if let Some((issuer, subject)) = identity_binding {
            match resolve_external_identity_in_tx(&mut tx, issuer, subject, display_name, None)
                .await
            {
                Ok(resolved) => resolved.participant_id,
                Err(SsoResolveError::InvalidIdentity) => {
                    return Err(ScimUserWriteError::IdentitySubjectRequired)
                }
                Err(SsoResolveError::Tombstoned) => {
                    return Err(ScimUserWriteError::IdentityTombstoned)
                }
                Err(SsoResolveError::Storage(error)) => {
                    return Err(ScimUserWriteError::Storage(error))
                }
            }
        } else {
            let participant = ParticipantId::new();
            sqlx::query(
                r"INSERT INTO participants
                     (id, kind, display_name, avatar_url, created_by, created_at)
                   VALUES ($1, 'human', $2, NULL, NULL, $3)",
            )
            .bind(participant.to_uuid())
            .bind(display_name)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            participant
        };

        // SCIM is authoritative for the current display label even when the
        // identity was first seen through OIDC.
        sqlx::query("UPDATE participants SET display_name = $2 WHERE id = $1")
            .bind(participant.to_uuid())
            .bind(display_name)
            .execute(&mut *tx)
            .await?;

        // Keep the topology for reversible suspension. Effective access is
        // denied by workspace_deactivations when `active=false`.
        sqlx::query(
            r"INSERT INTO workspace_members
                 (workspace_id, participant_id, role, joined_at)
               VALUES ($1, $2, 'member', $3)
               ON CONFLICT (workspace_id, participant_id) DO NOTHING",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(now)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r"INSERT INTO scim_users
                 (workspace_id, participant_id, user_name, external_id, active, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, $6)",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(user_name)
        .bind(external_id)
        .bind(active)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        if active {
            // SCIM is authoritative for this workspace-scoped lifecycle. An
            // identity may already be a manually deactivated member before it
            // is first materialized as a SCIM resource; reporting `active=true`
            // while retaining that access fence would create contradictory
            // control-plane and authorization state.
            sqlx::query(
                r"DELETE FROM workspace_deactivations
                   WHERE workspace_id = $1 AND participant_id = $2",
            )
            .bind(workspace.to_uuid())
            .bind(participant.to_uuid())
            .execute(&mut *tx)
            .await?;
        } else {
            // The default workspace promotes its first effective member to
            // owner inside the membership INSERT trigger. Existing external
            // identities may also already own the target workspace. Detect
            // either case explicitly so callers receive the stable domain
            // conflict and every earlier aggregate write rolls back.
            assert_not_workspace_owner(&mut tx, workspace, participant).await?;
            sqlx::query(
                r"INSERT INTO workspace_deactivations
                      (workspace_id, participant_id, deactivated_by)
                   VALUES ($1, $2, NULL)
                   ON CONFLICT (workspace_id, participant_id) DO NOTHING",
            )
            .bind(workspace.to_uuid())
            .bind(participant.to_uuid())
            .execute(&mut *tx)
            .await
            .map_err(map_user_storage_error)?;
        }

        let target = participant.to_string();
        AuditRepo::append_in_tx(
            &mut tx,
            workspace,
            None,
            "scim.user.provision",
            Some(&target),
            serde_json::json!({ "active": active, "identity_linked": identity_binding.is_some() }),
        )
        .await?;

        tx.commit().await?;
        Ok(ScimUserRow {
            workspace_id: workspace,
            participant_id: participant,
            user_name: user_name.to_owned(),
            external_id: external_id.map(str::to_owned),
            active,
            created_at: now,
            updated_at: now,
        })
    }

    /// Fetch one SCIM user (by participant) within a workspace, or `None`.
    pub async fn get_user(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<Option<ScimUserRow>, sqlx::Error> {
        let row = sqlx::query_as::<_, ScimUserSqlRow>(
            r"SELECT workspace_id, participant_id, user_name, external_id, active, created_at, updated_at
               FROM scim_users
               WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(ScimUserRow::from))
    }

    /// Resolve a `userName` to its participant within a workspace (the common
    /// `userName eq "x"` provisioning lookup), or `None`.
    pub async fn find_by_user_name(
        &self,
        workspace: WorkspaceId,
        user_name: &str,
    ) -> Result<Option<ScimUserRow>, sqlx::Error> {
        let row = sqlx::query_as::<_, ScimUserSqlRow>(
            r"SELECT workspace_id, participant_id, user_name, external_id, active, created_at, updated_at
               FROM scim_users
               WHERE workspace_id = $1 AND user_name = $2",
        )
        .bind(workspace.to_uuid())
        .bind(user_name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(ScimUserRow::from))
    }

    /// List a workspace's SCIM users, optionally filtered by exact `userName`
    /// (the `userName eq "x"` filter), paginated by SCIM `startIndex`/`count`.
    /// Returns `(rows, total_results)` where `total` is the unpaginated count for
    /// the same filter (RFC 7644 `totalResults`).
    pub async fn list_users(
        &self,
        workspace: WorkspaceId,
        filter_user_name: Option<&str>,
        start_index: Option<i64>,
        count: Option<i64>,
    ) -> Result<(Vec<ScimUserRow>, i64), sqlx::Error> {
        let limit = clamp_count(count);
        let offset = start_offset(start_index);

        // Total for the same filter (NULL filter ⇒ count all).
        let (total,): (i64,) = sqlx::query_as(
            r"SELECT COUNT(*) FROM scim_users
               WHERE workspace_id = $1
                 AND ($2::text IS NULL OR user_name = $2)",
        )
        .bind(workspace.to_uuid())
        .bind(filter_user_name)
        .fetch_one(&self.pool)
        .await?;

        let rows = sqlx::query_as::<_, ScimUserSqlRow>(
            r"SELECT workspace_id, participant_id, user_name, external_id, active, created_at, updated_at
               FROM scim_users
               WHERE workspace_id = $1
                 AND ($2::text IS NULL OR user_name = $2)
               ORDER BY created_at ASC, participant_id ASC
               LIMIT $3 OFFSET $4",
        )
        .bind(workspace.to_uuid())
        .bind(filter_user_name)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        Ok((rows.into_iter().map(ScimUserRow::from).collect(), total))
    }

    /// Toggle a SCIM user's `active` flag (the deprovision/reactivate path).
    /// Returns the updated row, or `None` if no such SCIM user exists.
    pub async fn set_active(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
        active: bool,
    ) -> Result<Option<ScimUserRow>, ScimUserWriteError> {
        self.update_user_atomic(workspace, participant, None, None, Some(active), None)
            .await
    }

    /// Atomically patch without interpreting a legacy `external_id` as a login
    /// identity. Identity-aware HTTP paths should call
    /// [`Self::update_user_atomic_with_identity`] with their configured issuer.
    pub async fn update_user_atomic(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
        user_name: Option<&str>,
        external_id: Option<Option<&str>>,
        active: Option<bool>,
        display_name: Option<&str>,
    ) -> Result<Option<ScimUserRow>, ScimUserWriteError> {
        self.update_user_atomic_with_identity(
            workspace,
            participant,
            ScimUserUpdate {
                user_name,
                external_id,
                active,
                display_name,
                identity_issuer: None,
            },
        )
        .await
    }

    /// Atomically patch a SCIM user's mapping, global display name, active
    /// workspace access, and configured external-login binding.
    ///
    /// The workspace row serializes SCIM aggregate mutations. Its stable
    /// pre-read lets identity-aware updates acquire the external-identity lock
    /// before the participant row (workspace → identity → participant), matching
    /// OIDC JIT, identity migration, and account erasure. Routine PUT/PATCH may
    /// bind a legacy row's existing `external_id`, but may never use that upgrade
    /// to replace the subject, merge participants, or revive a tombstone.
    pub async fn update_user_atomic_with_identity(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
        update: ScimUserUpdate<'_>,
    ) -> Result<Option<ScimUserRow>, ScimUserWriteError> {
        let ScimUserUpdate {
            user_name,
            external_id,
            active,
            display_name,
            identity_issuer,
        } = update;
        let mut tx = self.pool.begin().await?;
        if active == Some(false) {
            crate::ownership::lock_membership_governance(&mut tx).await?;
        }
        if !lock_scim_workspace(&mut tx, workspace).await? {
            tx.rollback().await?;
            return Ok(None);
        }
        // Do not lock the SCIM row ahead of its identity: identity migration
        // takes identity locks before updating SCIM rows. The workspace lock is
        // the aggregate serialization fence that keeps this pre-read stable.
        let previous = sqlx::query_as::<_, (bool, Option<String>)>(
            r"SELECT active, external_id
                FROM scim_users
               WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some((previous_active, previous_external_id)) = previous else {
            tx.rollback().await?;
            return Ok(None);
        };

        let identity_linked = if let Some(issuer) = identity_issuer {
            let (issuer, subject) =
                validated_identity_binding(Some(issuer), previous_external_id.as_deref())?
                    .expect("configured issuer produces a binding");
            // Even an unbound legacy subject is immutable at this boundary.
            // Allowing PUT/PATCH to choose a different value would silently add
            // a second login alias instead of performing an audited migration.
            if external_id.is_some() && external_id.flatten() != previous_external_id.as_deref() {
                return Err(ScimUserWriteError::IdentitySubjectImmutable);
            }
            bind_existing_participant_identity_in_tx(&mut tx, issuer, subject, participant).await?
        } else {
            false
        };

        let participant_active = sqlx::query_scalar::<_, bool>(
            r"SELECT deleted_at IS NULL
                FROM participants
               WHERE id = $1
               FOR UPDATE",
        )
        .bind(participant.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(false);
        if !participant_active {
            tx.rollback().await?;
            return Ok(None);
        }

        if identity_issuer.is_none() {
            let identity_bound = sqlx::query_scalar::<_, bool>(
                r"SELECT EXISTS (
                       SELECT 1
                         FROM sso_identities
                        WHERE participant_id = $1 AND subject = $2
                   )",
            )
            .bind(participant.to_uuid())
            .bind(previous_external_id.as_deref())
            .fetch_one(&mut *tx)
            .await?;
            if identity_bound
                && external_id.is_some()
                && external_id.flatten() != previous_external_id.as_deref()
            {
                return Err(ScimUserWriteError::IdentitySubjectImmutable);
            }
        }

        if active == Some(false) {
            assert_not_workspace_owner(&mut tx, workspace, participant).await?;
        }

        let row = sqlx::query_as::<_, ScimUserSqlRow>(
            r"UPDATE scim_users SET
                 user_name   = COALESCE($3, user_name),
                 external_id = CASE WHEN $4::boolean THEN $5 ELSE external_id END,
                 active      = COALESCE($6, active),
                 updated_at  = now()
               WHERE workspace_id = $1 AND participant_id = $2
            RETURNING workspace_id, participant_id, user_name, external_id, active, created_at, updated_at",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(user_name)
        .bind(external_id.is_some())
        .bind(external_id.flatten())
        .bind(active)
        .fetch_one(&mut *tx)
        .await?;

        if let Some(display_name) = display_name {
            sqlx::query("UPDATE participants SET display_name = $2 WHERE id = $1")
                .bind(participant.to_uuid())
                .bind(display_name)
                .execute(&mut *tx)
                .await?;
        }

        if let Some(active) = active {
            if active {
                // Repair legacy inactive SCIM rows that predate reversible
                // suspension, then remove the effective-access fence.
                sqlx::query(
                    r"INSERT INTO workspace_members
                         (workspace_id, participant_id, role, joined_at)
                       VALUES ($1, $2, 'member', now())
                       ON CONFLICT (workspace_id, participant_id) DO NOTHING",
                )
                .bind(workspace.to_uuid())
                .bind(participant.to_uuid())
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    r"DELETE FROM workspace_deactivations
                       WHERE workspace_id = $1 AND participant_id = $2",
                )
                .bind(workspace.to_uuid())
                .bind(participant.to_uuid())
                .execute(&mut *tx)
                .await?;
            } else {
                // Suspension is reversible: preserve all membership and private
                // room topology, but install the canonical effective-access
                // fence used by every room/workspace authorization path.
                sqlx::query(
                    r"INSERT INTO workspace_members
                         (workspace_id, participant_id, role, joined_at)
                       VALUES ($1, $2, 'member', now())
                       ON CONFLICT (workspace_id, participant_id) DO NOTHING",
                )
                .bind(workspace.to_uuid())
                .bind(participant.to_uuid())
                .execute(&mut *tx)
                .await?;
                sqlx::query(
                    r"INSERT INTO workspace_deactivations
                          (workspace_id, participant_id, deactivated_by)
                       VALUES ($1, $2, NULL)
                       ON CONFLICT (workspace_id, participant_id) DO NOTHING",
                )
                .bind(workspace.to_uuid())
                .bind(participant.to_uuid())
                .execute(&mut *tx)
                .await
                .map_err(map_user_storage_error)?;
            }
        }

        let target = participant.to_string();
        let lifecycle_changed = active.is_some_and(|value| value != previous_active);
        AuditRepo::append_in_tx(
            &mut tx,
            workspace,
            None,
            if lifecycle_changed {
                if active == Some(true) {
                    "scim.user.reactivate"
                } else {
                    "scim.user.suspend"
                }
            } else {
                "scim.user.update"
            },
            Some(&target),
            serde_json::json!({
                "active": row.active,
                "user_name_changed": user_name.is_some(),
                "external_id_changed": external_id.is_some(),
                "display_name_changed": display_name.is_some(),
                "identity_linked": identity_linked,
            }),
        )
        .await?;

        tx.commit().await?;
        Ok(Some(ScimUserRow::from(row)))
    }

    /// Delete a workspace-scoped SCIM resource and revoke all of its workspace
    /// and room membership edges atomically. The global participant is retained.
    /// An id with no SCIM mapping in this workspace returns `false` and cannot
    /// revoke an ordinary member's access.
    pub async fn delete_user(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<bool, ScimUserWriteError> {
        let mut tx = self.pool.begin().await?;
        crate::ownership::lock_membership_governance(&mut tx).await?;
        if !lock_scim_workspace(&mut tx, workspace).await? {
            tx.rollback().await?;
            return Ok(false);
        }

        // RFC 7644 §3.6: DELETE removes the resource — a subsequent GET must 404.
        // So the SCIM mapping row is deleted (distinct from PATCH `active=false`,
        // which keeps the row and is still retrievable). The GLOBAL participant
        // identity is retained — only this workspace's SCIM provisioning + access
        // is revoked.
        let exists = sqlx::query_scalar::<_, bool>(
            r"SELECT true
                FROM scim_users
               WHERE workspace_id = $1 AND participant_id = $2
               FOR UPDATE",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !exists {
            tx.rollback().await?;
            return Ok(false);
        }
        assert_not_workspace_owner(&mut tx, workspace, participant).await?;
        sqlx::query(r"DELETE FROM scim_users WHERE workspace_id = $1 AND participant_id = $2")
            .bind(workspace.to_uuid())
            .bind(participant.to_uuid())
            .execute(&mut *tx)
            .await?;

        // Match WorkspaceRepo::remove_member semantics: deprovisioning must not
        // leave dormant private-room edges that spring back to life if the IdP
        // later provisions the identity again.
        sqlx::query(
            r"DELETE FROM room_members
               WHERE participant_id = $2
                 AND room_id IN (SELECT id FROM rooms WHERE workspace_id = $1)",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await
        .map_err(map_user_storage_error)?;

        // Remove the workspace membership (the actual access revocation). No-op
        // if they were not a member; the participant identity is untouched.
        sqlx::query(
            r"DELETE FROM workspace_members
               WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await
        .map_err(map_user_storage_error)?;

        let target = participant.to_string();
        AuditRepo::append_in_tx(
            &mut tx,
            workspace,
            None,
            "scim.user.deprovision",
            Some(&target),
            serde_json::json!({}),
        )
        .await?;

        tx.commit().await?;
        Ok(true)
    }
}

fn map_user_storage_error(error: sqlx::Error) -> ScimUserWriteError {
    if crate::is_channel_effective_owner_violation(&error) {
        ScimUserWriteError::ChannelOwnerDeprovision
    } else {
        ScimUserWriteError::Storage(error)
    }
}

async fn lock_scim_workspace(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
) -> Result<bool, sqlx::Error> {
    Ok(
        sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .is_some(),
    )
}

async fn assert_not_workspace_owner(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Result<(), ScimUserWriteError> {
    let role = sqlx::query_scalar::<_, String>(
        r"SELECT role
            FROM workspace_members
           WHERE workspace_id = $1 AND participant_id = $2
           FOR UPDATE",
    )
    .bind(workspace.to_uuid())
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    if role.as_deref() == Some("owner") {
        return Err(ScimUserWriteError::OwnerDeprovision);
    }
    Ok(())
}

/// sqlx row shape for `scim_users` decoding.
#[derive(sqlx::FromRow)]
struct ScimUserSqlRow {
    workspace_id: uuid::Uuid,
    participant_id: uuid::Uuid,
    user_name: String,
    external_id: Option<String>,
    active: bool,
    created_at: time::OffsetDateTime,
    updated_at: time::OffsetDateTime,
}

impl From<ScimUserSqlRow> for ScimUserRow {
    fn from(r: ScimUserSqlRow) -> Self {
        Self {
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            participant_id: ParticipantId::from_uuid(r.participant_id),
            user_name: r.user_name,
            external_id: r.external_id,
            active: r.active,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[cfg(test)]
mod tests;

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored scim_
/// ```
///
/// They are `#[ignore]` so the default `cargo test` stays hermetic (no DB in CI).
#[cfg(test)]
#[path = "scim/channel_owner_tests.rs"]
mod channel_owner_tests;

#[cfg(test)]
mod db_test_support;

#[cfg(test)]
mod db_tests;
