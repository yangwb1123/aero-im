//! Workspace invitation / invite-link repository.
//!
//! Backs `migrations/0017_invitations.sql`. A workspace admin/owner mints an
//! [`Invitation`] — either an email invite or an open shareable link — and a
//! logged-in invitee redeems it (by its token) to become a `workspace_member`
//! with the invite's [`WorkspaceRole`]. An invite may be one-shot or multi-use,
//! bounded by an optional `max_uses` and/or `expires_at`.
//!
//! Only the SHA-256 *hash* of the token is stored ([`hash_token`]); the plaintext
//! is generated once at creation ([`generate_token`]) and returned to the caller,
//! never persisted — mirroring password / provisioning-token handling.
//!
//! Purely additive: a NEW [`InvitationRepo`]; no existing repo is touched. The
//! redeemability predicate ([`invitation_is_redeemable`]) is a pure free function
//! (no DB, no clock) so it unit-tests directly, mirroring how the rest of
//! `aero-storage` keeps testable logic separate from live SQL (Postgres is absent
//! in CI).

use aero_common::{Error, InvitationId, ParticipantId, WorkspaceId, WorkspaceRole};
use rand::RngCore;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;

/// Result of atomically accepting an invitation.
///
/// A durable `(invitation_id, participant_id)` redemption makes retries
/// idempotent even after a single-use invitation becomes exhausted. A repeated
/// accept never recreates a membership that an administrator subsequently
/// removed; callers can distinguish that case through [`membership_active`].
///
/// [`membership_active`]: Self::membership_active
#[derive(Debug, Clone)]
pub struct InvitationAcceptance {
    pub invitation: Invitation,
    /// Effective workspace role at commit time when the membership is active;
    /// otherwise the role captured by the original redemption.
    pub role: WorkspaceRole,
    /// `true` when this participant had already redeemed this exact invitation.
    pub already_accepted: bool,
    /// Whether the original acceptance inserted a new workspace membership.
    /// Existing members are never overwritten and do not consume invite capacity.
    pub membership_created: bool,
    /// Whether the participant still has a workspace membership at commit time.
    pub membership_active: bool,
}

/// Expected invitation-acceptance failures.
#[derive(Debug, thiserror::Error)]
pub enum InvitationAcceptError {
    #[error("invitation not found")]
    NotFound,
    #[error("participant is not active")]
    ParticipantUnavailable,
    #[error("invitation is expired, revoked, or exhausted")]
    NotRedeemable,
    #[error("invitation or membership contains an invalid role")]
    InvalidRole,
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

// ---------------------------------------------------------------- Pure crypto

/// Number of random bytes behind a generated invite token (256 bits).
const TOKEN_BYTES: usize = 32;

/// Generate a fresh, high-entropy invite token (256-bit, lowercase hex).
///
/// Returned to the creator exactly once (embedded in the shareable
/// `invite_url`); only its [`hash_token`] is stored. Pure aside from the RNG.
#[must_use]
pub fn generate_token() -> String {
    let mut buf = [0u8; TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}

/// SHA-256 hex of an invite token. The active-invite lookup keys on this so the
/// plaintext token never has to be stored. Deterministic + pure, so it is
/// unit-tested without a DB.
#[must_use]
pub fn hash_token(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    hex::encode(h.finalize())
}

// ----------------------------------------------------------------- Data model

/// One workspace invitation. Listing intentionally omits the token (only its
/// hash is stored anyway); the plaintext is shown once at creation time.
#[derive(Debug, Clone, Serialize)]
pub struct Invitation {
    pub id: InvitationId,
    pub workspace_id: WorkspaceId,
    /// `None` = open shareable link; otherwise the address the invite targets
    /// (informational — accept does not hard-block on a mismatch this iteration).
    pub email: Option<String>,
    /// The role granted to the invitee on accept.
    pub role: WorkspaceRole,
    /// The admin/owner who created the invite, if still known.
    pub created_by: Option<ParticipantId>,
    /// `None` = unlimited uses; otherwise the invite is exhausted at this count.
    pub max_uses: Option<i32>,
    pub use_count: i32,
    /// `None` = never expires.
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// `None` = active; set on revoke.
    #[serde(with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<OffsetDateTime>,
}

/// Raw `invitations` row shape (column order matches every SELECT below).
type InvitationRow = (
    uuid::Uuid,             // id
    uuid::Uuid,             // workspace_id
    Option<String>,         // email
    String,                 // role
    Option<uuid::Uuid>,     // created_by
    Option<i32>,            // max_uses
    i32,                    // use_count
    Option<OffsetDateTime>, // expires_at
    OffsetDateTime,         // created_at
    Option<OffsetDateTime>, // revoked_at
);

fn row_into_invitation(r: InvitationRow) -> Invitation {
    let (id, ws, email, role, created_by, max_uses, use_count, expires_at, created_at, revoked_at) =
        r;
    Invitation {
        id: InvitationId::from_uuid(id),
        workspace_id: WorkspaceId::from_uuid(ws),
        email,
        // Default an unrecognized token to the least-privileged role rather than
        // dropping the row, so listing never silently shrinks.
        role: WorkspaceRole::from_db_str(&role).unwrap_or(WorkspaceRole::Guest),
        created_by: created_by.map(ParticipantId::from_uuid),
        max_uses,
        use_count,
        expires_at,
        created_at,
        revoked_at,
    }
}

/// The columns selected by every invitation read, in a stable order.
const SELECT_COLS: &str = "id, workspace_id, email, role, created_by, max_uses, \
                           use_count, expires_at, created_at, revoked_at";

// -------------------------------------------------------------------- The repo

#[derive(Clone)]
pub struct InvitationRepo {
    pool: PgPool,
}

impl InvitationRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create an invitation while `created_by` remains an effective workspace
    /// administrator who may grant `role`.
    ///
    /// The authorization decision, invitation row, and `invitation.create`
    /// audit event commit in one transaction.
    // Each parameter maps 1:1 to a distinct, non-defaultable invitation column;
    // bundling them into a struct would only add an indirection with no clarity
    // win at the single call site.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_authorized(
        &self,
        workspace: WorkspaceId,
        token_hash: &str,
        email: Option<&str>,
        role: WorkspaceRole,
        created_by: ParticipantId,
        max_uses: Option<i32>,
        expires_at: Option<OffsetDateTime>,
    ) -> Result<InvitationId, Error> {
        if token_hash.len() != 64
            || !token_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(Error::Invalid("invalid invitation token hash".into()));
        }
        if email.is_some_and(|email| email.len() > 320) {
            return Err(Error::Invalid("invitation email too long".into()));
        }
        if max_uses.is_some_and(|max_uses| max_uses < 1) {
            return Err(Error::Invalid("max_uses must be at least 1".into()));
        }
        if expires_at.is_some_and(|expires_at| expires_at <= OffsetDateTime::now_utc()) {
            return Err(Error::Invalid(
                "invitation expiry must be in the future".into(),
            ));
        }

        let mut tx = self.pool.begin().await?;
        let creator_role =
            crate::workspace::authz::assert_effective_admin_in_tx(&mut tx, workspace, created_by)
                .await?;
        if !crate::role_can_invite(creator_role) || !crate::role_can_assign(creator_role, role) {
            return Err(Error::Forbidden(
                "caller cannot grant the requested invitation role".into(),
            ));
        }

        let id = InvitationId::new();
        sqlx::query(
            r"INSERT INTO invitations
                (id, workspace_id, token_hash, email, role, created_by,
                 max_uses, use_count, expires_at, created_at)
              VALUES ($1, $2, $3, $4, $5, $6, $7, 0, $8, now())",
        )
        .bind(id.to_uuid())
        .bind(workspace.to_uuid())
        .bind(token_hash)
        .bind(email)
        .bind(role.as_str())
        .bind(created_by.to_uuid())
        .bind(max_uses)
        .bind(expires_at)
        .execute(&mut *tx)
        .await?;
        crate::audit::AuditRepo::append_in_tx(
            &mut tx,
            workspace,
            Some(created_by),
            "invitation.create",
            Some(&id.to_string()),
            serde_json::json!({
                "role": role,
                "email": email,
                "max_uses": max_uses,
                "expires_at": expires_at.map(OffsetDateTime::unix_timestamp),
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Resolve an *active* invitation by its token hash, as of `now`: not revoked,
    /// not expired, and not exhausted (`use_count < max_uses` when a cap is set).
    ///
    /// The redeemability filter is applied in Rust (via [`invitation_is_redeemable`])
    /// rather than in SQL so the rule has a single, unit-tested home; the query
    /// only resolves the hash → row.
    pub async fn find_active_by_token_hash(
        &self,
        token_hash: &str,
        now: OffsetDateTime,
    ) -> Result<Option<Invitation>, sqlx::Error> {
        let row = sqlx::query_as::<_, InvitationRow>(&format!(
            "SELECT {SELECT_COLS} FROM invitations WHERE token_hash = $1",
        ))
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_into_invitation).filter(|inv| {
            invitation_is_redeemable(
                inv.revoked_at.is_some(),
                inv.expires_at,
                inv.use_count,
                inv.max_uses,
                now,
            )
        }))
    }

    /// All invitations for a workspace, newest first. Tokens are never selected
    /// (only their hash is stored), so this is safe for an admin listing.
    pub async fn list_for_workspace_authorized(
        &self,
        workspace: WorkspaceId,
        actor: ParticipantId,
    ) -> Result<Vec<Invitation>, Error> {
        let mut tx = self.pool.begin().await?;
        crate::workspace::authz::assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        let rows = sqlx::query_as::<_, InvitationRow>(&format!(
            "SELECT {SELECT_COLS} FROM invitations \
             WHERE workspace_id = $1 ORDER BY created_at DESC, id DESC",
        ))
        .bind(workspace.to_uuid())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(row_into_invitation).collect())
    }

    /// Atomically record one redemption of an active invite, returning `true` if a
    /// row was updated. The `WHERE` re-checks redeemability so two concurrent
    /// accepts cannot push `use_count` past `max_uses` (the loser updates nothing
    /// and gets `false`): expired/revoked/exhausted invites increment nothing.
    pub async fn increment_use(&self, id: InvitationId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE invitations
                 SET use_count = use_count + 1
               WHERE id = $1
                 AND revoked_at IS NULL
                 AND (expires_at IS NULL OR expires_at > now())
                 AND (max_uses IS NULL OR use_count < max_uses)",
        )
        .bind(id.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Accept an invitation and establish its workspace membership atomically.
    ///
    /// The invitation row is locked before capacity is checked. For a first
    /// redemption, the membership insert, durable redemption row, and use-count
    /// increment commit together. Consequently a capacity loser or any storage
    /// failure leaves no membership behind and consumes no use.
    ///
    /// Repeating the same `(invitation, participant)` redemption returns
    /// `already_accepted = true` without consuming another use, even when the
    /// invitation is now exhausted or revoked. If an administrator removed the
    /// membership after the first accept, a retry reports
    /// `membership_active = false` and deliberately does not resurrect it.
    ///
    /// A participant who already belongs to the workspace is recorded as having
    /// accepted, but their existing role is never overwritten and invite capacity
    /// is not consumed because no admission occurred.
    pub async fn accept_by_token_hash(
        &self,
        token_hash: &str,
        participant: ParticipantId,
    ) -> Result<InvitationAcceptance, InvitationAcceptError> {
        let mut tx = self.pool.begin().await?;
        // Membership writes share the global low-frequency governance fence so
        // invitation admission cannot deadlock owner/deactivation/SCIM paths
        // that subsequently acquire the same workspace row.
        crate::ownership::lock_membership_governance(&mut tx).await?;

        // Resolve immutable identity/scope without a row lock, then follow the
        // membership-governance lock order: workspace → participant → invitation
        // → workspace membership. The final locked invitation read revalidates
        // the token and scope before any write.
        let (invitation_id, workspace_id) = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid)>(
            "SELECT id, workspace_id FROM invitations WHERE token_hash = $1",
        )
        .bind(token_hash)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(InvitationAcceptError::NotFound)?;
        let workspace_exists =
            sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
                .bind(workspace_id)
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        if !workspace_exists {
            return Err(InvitationAcceptError::NotFound);
        }
        let participant_active = sqlx::query_scalar::<_, bool>(
            r"SELECT deleted_at IS NULL
                FROM participants
               WHERE id = $1
               FOR KEY SHARE",
        )
        .bind(participant.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(false);
        if !participant_active {
            return Err(InvitationAcceptError::ParticipantUnavailable);
        }

        let row = sqlx::query_as::<_, InvitationRow>(&format!(
            "SELECT {SELECT_COLS} FROM invitations \
             WHERE id = $1 AND workspace_id = $2 AND token_hash = $3 \
             FOR UPDATE",
        ))
        .bind(invitation_id)
        .bind(workspace_id)
        .bind(token_hash)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(InvitationAcceptError::NotFound)?;

        let invitation_role =
            WorkspaceRole::from_db_str(&row.3).ok_or(InvitationAcceptError::InvalidRole)?;
        let invitation = row_into_invitation(row);

        // A durable redemption is checked before current redeemability: this is
        // the idempotency path for a retry after the first request consumed the
        // final slot. The membership row is locked when present so the reported
        // role/activity is stable through commit.
        let prior = sqlx::query_as::<_, (String, bool)>(
            r"SELECT accepted_role, membership_created
                FROM invitation_redemptions
               WHERE invitation_id = $1
                 AND participant_id = $2
                 AND workspace_id = $3",
        )
        .bind(invitation.id.to_uuid())
        .bind(participant.to_uuid())
        .bind(invitation.workspace_id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        if let Some((accepted_role, membership_created)) = prior {
            let accepted_role = WorkspaceRole::from_db_str(&accepted_role)
                .ok_or(InvitationAcceptError::InvalidRole)?;
            let current_role =
                locked_membership_role(&mut tx, invitation.workspace_id, participant).await?;
            tx.commit().await?;
            return Ok(InvitationAcceptance {
                invitation,
                role: current_role.unwrap_or(accepted_role),
                already_accepted: true,
                membership_created,
                membership_active: current_role.is_some(),
            });
        }

        // `clock_timestamp()` is evaluated after acquiring the row lock. Unlike
        // transaction-stable `now()`, it cannot admit an invite that expired
        // while this transaction waited behind another accepter/revoker.
        let now = sqlx::query_scalar::<_, OffsetDateTime>("SELECT clock_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        if !invitation_is_redeemable(
            invitation.revoked_at.is_some(),
            invitation.expires_at,
            invitation.use_count,
            invitation.max_uses,
            now,
        ) {
            return Err(InvitationAcceptError::NotRedeemable);
        }

        // Lock any existing membership so accepting an invite can never overwrite
        // a current role. Existing members are a no-op admission and therefore do
        // not consume capacity.
        if let Some(existing_role) =
            locked_membership_role(&mut tx, invitation.workspace_id, participant).await?
        {
            insert_redemption(&mut tx, &invitation, participant, existing_role, false).await?;
            append_accept_audit(&mut tx, &invitation, participant, existing_role, false).await?;
            tx.commit().await?;
            return Ok(InvitationAcceptance {
                invitation,
                role: existing_role,
                already_accepted: false,
                membership_created: false,
                membership_active: true,
            });
        }

        // A direct add/SCIM operation may race the preceding read. `DO NOTHING`
        // preserves the role written by that path; only a row inserted here is
        // allowed to consume invitation capacity.
        let membership_created = sqlx::query(
            r"INSERT INTO workspace_members
                  (workspace_id, participant_id, role, joined_at)
              VALUES ($1, $2, $3, clock_timestamp())
              ON CONFLICT (workspace_id, participant_id) DO NOTHING",
        )
        .bind(invitation.workspace_id.to_uuid())
        .bind(participant.to_uuid())
        .bind(invitation_role.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;

        let accepted_role = if membership_created {
            invitation_role
        } else {
            locked_membership_role(&mut tx, invitation.workspace_id, participant)
                .await?
                .ok_or(InvitationAcceptError::InvalidRole)?
        };

        insert_redemption(
            &mut tx,
            &invitation,
            participant,
            accepted_role,
            membership_created,
        )
        .await?;

        if membership_created {
            let incremented = sqlx::query(
                r"UPDATE invitations
                      SET use_count = use_count + 1
                    WHERE id = $1
                      AND revoked_at IS NULL
                      AND (expires_at IS NULL OR expires_at > clock_timestamp())
                      AND (max_uses IS NULL OR use_count < max_uses)",
            )
            .bind(invitation.id.to_uuid())
            .execute(&mut *tx)
            .await?
            .rows_affected()
                == 1;
            if !incremented {
                // Dropping the transaction rolls back both the membership and
                // redemption insert. This is defensive: the invitation row lock
                // should make the state stable apart from wall-clock expiry.
                return Err(InvitationAcceptError::NotRedeemable);
            }
        }

        append_accept_audit(
            &mut tx,
            &invitation,
            participant,
            accepted_role,
            membership_created,
        )
        .await?;
        tx.commit().await?;
        Ok(InvitationAcceptance {
            invitation,
            role: accepted_role,
            already_accepted: false,
            membership_created,
            membership_active: true,
        })
    }

    /// Revoke an invitation while `actor` remains authorized in the invitation's
    /// own workspace. The transition and audit event commit together.
    ///
    /// A caller without effective access to the owning workspace receives the
    /// same [`Error::NotFound`] as an unknown id. A current member who lacks
    /// administrative privilege receives [`Error::Forbidden`].
    pub async fn revoke_authorized(
        &self,
        id: InvitationId,
        actor: ParticipantId,
    ) -> Result<bool, Error> {
        let mut tx = self.pool.begin().await?;
        let workspace = sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT workspace_id FROM invitations WHERE id = $1",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .map(WorkspaceId::from_uuid)
        .ok_or_else(|| Error::NotFound("invitation".into()))?;

        let workspace_exists =
            sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
                .bind(workspace.to_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        if !workspace_exists
            || !crate::workspace::members::effective_workspace_access_in_tx(
                &mut tx, workspace, actor,
            )
            .await?
        {
            return Err(Error::NotFound("invitation".into()));
        }
        let role = sqlx::query_scalar::<_, String>(
            "SELECT role
               FROM workspace_members
              WHERE workspace_id = $1 AND participant_id = $2
              FOR UPDATE",
        )
        .bind(workspace.to_uuid())
        .bind(actor.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .and_then(|role| WorkspaceRole::from_db_str(&role))
        .ok_or_else(|| Error::NotFound("invitation".into()))?;
        if !role.can_administer() {
            return Err(Error::Forbidden(
                "workspace admin required to revoke invitation".into(),
            ));
        }

        let invitation_exists = sqlx::query_scalar::<_, bool>(
            "SELECT true
               FROM invitations
              WHERE id = $1 AND workspace_id = $2
              FOR UPDATE",
        )
        .bind(id.to_uuid())
        .bind(workspace.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !invitation_exists {
            return Err(Error::NotFound("invitation".into()));
        }
        let revoked = sqlx::query(
            "UPDATE invitations
                SET revoked_at = clock_timestamp()
              WHERE id = $1
                AND workspace_id = $2
                AND revoked_at IS NULL",
        )
        .bind(id.to_uuid())
        .bind(workspace.to_uuid())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        crate::audit::AuditRepo::append_in_tx(
            &mut tx,
            workspace,
            Some(actor),
            "invitation.revoke",
            Some(&id.to_string()),
            serde_json::json!({ "already_revoked": !revoked }),
        )
        .await?;
        tx.commit().await?;
        Ok(revoked)
    }

    // Low-level fixtures preserve coverage of already-expired/exhausted legacy
    // rows without exposing authorization-free mutators in production builds.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    async fn create(
        &self,
        workspace: WorkspaceId,
        token_hash: &str,
        email: Option<&str>,
        role: WorkspaceRole,
        created_by: Option<ParticipantId>,
        max_uses: Option<i32>,
        expires_at: Option<OffsetDateTime>,
    ) -> Result<InvitationId, sqlx::Error> {
        let id = InvitationId::new();
        sqlx::query(
            r"INSERT INTO invitations
                (id, workspace_id, token_hash, email, role, created_by,
                 max_uses, use_count, expires_at, created_at)
              VALUES ($1, $2, $3, $4, $5, $6, $7, 0, $8, now())",
        )
        .bind(id.to_uuid())
        .bind(workspace.to_uuid())
        .bind(token_hash)
        .bind(email)
        .bind(role.as_str())
        .bind(created_by.map(|participant| participant.to_uuid()))
        .bind(max_uses)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    #[cfg(test)]
    async fn revoke(&self, id: InvitationId) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query(
            "UPDATE invitations SET revoked_at = NOW()
              WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(id.to_uuid())
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    #[cfg(test)]
    async fn list_for_workspace(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<Invitation>, sqlx::Error> {
        let rows = sqlx::query_as::<_, InvitationRow>(&format!(
            "SELECT {SELECT_COLS}
               FROM invitations
              WHERE workspace_id = $1
              ORDER BY created_at DESC, id DESC"
        ))
        .bind(workspace.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_into_invitation).collect())
    }
}

async fn locked_membership_role(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Result<Option<WorkspaceRole>, InvitationAcceptError> {
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
    role.map(|value| WorkspaceRole::from_db_str(&value).ok_or(InvitationAcceptError::InvalidRole))
        .transpose()
}

async fn insert_redemption(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    invitation: &Invitation,
    participant: ParticipantId,
    role: WorkspaceRole,
    membership_created: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r"INSERT INTO invitation_redemptions
              (invitation_id, participant_id, workspace_id, accepted_role,
               membership_created, redeemed_at)
          VALUES ($1, $2, $3, $4, $5, clock_timestamp())",
    )
    .bind(invitation.id.to_uuid())
    .bind(participant.to_uuid())
    .bind(invitation.workspace_id.to_uuid())
    .bind(role.as_str())
    .bind(membership_created)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn append_accept_audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    invitation: &Invitation,
    participant: ParticipantId,
    role: WorkspaceRole,
    membership_created: bool,
) -> Result<(), sqlx::Error> {
    crate::audit::AuditRepo::append_in_tx(
        tx,
        invitation.workspace_id,
        Some(participant),
        "invitation.accept",
        Some(&invitation.id.to_string()),
        serde_json::json!({
            "role": role,
            "membership_created": membership_created,
            "invite_email": invitation.email.as_deref(),
        }),
    )
    .await
    .map(|_| ())
}

// ---------- Pure redeemability predicate (DB-free, unit-tested) ----------

/// Is an invitation redeemable, given its current state as of `now`?
///
/// An invite is redeemable when it is **not** revoked, **not** past its
/// `expires_at` (when set), and **not** exhausted (`use_count < max_uses` when a
/// cap is set; an unset `max_uses` means unlimited). Pure — no DB, no wall clock
/// — so each clause is exercised offline.
#[must_use]
pub fn invitation_is_redeemable(
    revoked: bool,
    expires_at: Option<OffsetDateTime>,
    use_count: i32,
    max_uses: Option<i32>,
    now: OffsetDateTime,
) -> bool {
    if revoked {
        return false;
    }
    if let Some(exp) = expires_at {
        if now >= exp {
            return false;
        }
    }
    if let Some(cap) = max_uses {
        if use_count >= cap {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(secs).expect("valid timestamp")
    }

    #[test]
    fn hash_is_deterministic_and_differs_per_token() {
        assert_eq!(hash_token("abc"), hash_token("abc"));
        assert_ne!(hash_token("abc"), hash_token("abd"));
        // SHA-256 hex is 64 chars.
        assert_eq!(hash_token("anything").len(), 64);
    }

    #[test]
    fn generated_tokens_are_unique_hex() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b, "two tokens must not collide");
        // 32 random bytes => 64 hex chars, all hex digits.
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn redeemable_when_fresh_unlimited_and_active() {
        // Not revoked, no expiry, no cap → always redeemable.
        assert!(invitation_is_redeemable(false, None, 0, None, t(1_000)));
        assert!(invitation_is_redeemable(false, None, 9_999, None, t(1_000)));
    }

    #[test]
    fn not_redeemable_when_revoked() {
        // Revoked dominates even an otherwise-perfectly-valid invite.
        assert!(!invitation_is_redeemable(true, None, 0, None, t(1_000)));
        assert!(!invitation_is_redeemable(
            true,
            Some(t(2_000)),
            0,
            Some(10),
            t(1_000)
        ));
    }

    #[test]
    fn not_redeemable_when_expired_boundary_inclusive() {
        let exp = t(2_000);
        // Strictly before expiry → ok.
        assert!(invitation_is_redeemable(
            false,
            Some(exp),
            0,
            None,
            t(1_999)
        ));
        // Exactly at expiry → NOT redeemable (`now >= exp`).
        assert!(!invitation_is_redeemable(false, Some(exp), 0, None, exp));
        // After expiry → not redeemable.
        assert!(!invitation_is_redeemable(
            false,
            Some(exp),
            0,
            None,
            t(2_001)
        ));
    }

    #[test]
    fn not_redeemable_when_exhausted() {
        // max_uses = 3: counts 0..=2 are fine, 3 (and beyond) are exhausted.
        assert!(invitation_is_redeemable(false, None, 2, Some(3), t(1_000)));
        assert!(!invitation_is_redeemable(false, None, 3, Some(3), t(1_000)));
        assert!(!invitation_is_redeemable(false, None, 4, Some(3), t(1_000)));
        // A single-use invite: usable at 0, exhausted at 1.
        assert!(invitation_is_redeemable(false, None, 0, Some(1), t(1_000)));
        assert!(!invitation_is_redeemable(false, None, 1, Some(1), t(1_000)));
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored invitation_
/// ```
///
/// They are `#[ignore]` so the default `cargo test` stays hermetic (no DB in CI);
/// the orchestrator runs them against a live database.
#[cfg(test)]
#[path = "invitation/db_tests.rs"]
mod db_tests;
