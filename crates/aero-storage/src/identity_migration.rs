//! Audited self-service migration of an external `IdP` identity.
//!
//! The internal [`ParticipantId`] is deliberately immutable: this operation
//! only adds/retires login aliases. It never merges participants or moves any
//! participant-owned messages, files, memberships, or other content.

use aero_common::{Error, ParticipantId, WorkspaceId};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, Postgres, Transaction};

use crate::AuditRepo;

pub const IDENTITY_MIGRATED_REASON: &str = "identity_migrated";
pub const IDENTITY_MIGRATED_AUDIT_ACTION: &str = "identity.migrated";

/// Exact, case-sensitive external identity key asserted by an `IdP`.
#[derive(Clone, PartialEq, Eq)]
pub struct ExternalIdentityKey {
    pub issuer: String,
    pub subject: String,
}

impl ExternalIdentityKey {
    #[must_use]
    pub fn new(issuer: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            subject: subject.into(),
        }
    }
}

/// Workspace-scoped authorization and exact source/target identity pair.
#[derive(Clone, PartialEq, Eq)]
pub struct IdentityMigrationRequest {
    pub workspace_id: WorkspaceId,
    pub actor_id: ParticipantId,
    pub participant_id: ParticipantId,
    pub from: ExternalIdentityKey,
    pub to: ExternalIdentityKey,
    /// Delete and tombstone `from` after `to` is secured for the participant.
    pub retire_source: bool,
}

/// State changes committed by one migration request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityMigrationOutcome {
    pub participant_id: ParticipantId,
    pub target_created: bool,
    pub source_retired: bool,
    pub scim_rows_updated: u64,
}

/// Transactional external-identity migration repository.
#[derive(Clone)]
pub struct IdentityMigrationRepo {
    pool: PgPool,
}

#[derive(FromRow)]
struct LockedIdentity {
    issuer: String,
    subject: String,
    participant_id: uuid::Uuid,
    email: Option<String>,
}

#[derive(FromRow)]
struct LockedIdentityTombstone {
    issuer: String,
    subject: String,
    former_participant_id: uuid::Uuid,
    reason: String,
}

impl IdentityMigrationRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Add `to` as an alias for `participant_id`, optionally retiring `from`.
    ///
    /// Lock order is load-bearing: the concrete workspace aggregate root, the
    /// caller's membership, both identity lifecycle advisory locks in database
    /// collation order, identity rows, then the subject participant. The
    /// self-service and effective-membership checks therefore share one
    /// transaction-owned proof with every state change. Unrelated workspaces
    /// are deliberately not locked.
    ///
    /// A target already bound to the same participant is an idempotent success.
    /// A completed retire request can also be replayed safely when its exact
    /// source tombstone and target binding prove the prior commit; that read-only
    /// replay does not append another audit row. A target bound to another
    /// participant is always a conflict, and this method intentionally has no
    /// account/content merge behavior.
    pub async fn migrate(
        &self,
        request: IdentityMigrationRequest,
    ) -> Result<IdentityMigrationOutcome, Error> {
        validate_request(&request)?;

        let mut tx = self.pool.begin().await?;
        if request.actor_id != request.participant_id {
            return Err(Error::Forbidden(
                "external identity migration is self-service only".into(),
            ));
        }
        let require_2fa =
            lock_workspace_member_boundary_in_tx(&mut tx, request.workspace_id, request.actor_id)
                .await?;

        lock_identity_pair_in_tx(&mut tx, &request.from, &request.to).await?;
        let identities = lock_identity_rows_in_tx(&mut tx, &request.from, &request.to).await?;
        let tombstones =
            lock_identity_tombstones_in_tx(&mut tx, &request.from, &request.to).await?;

        // This validation deliberately precedes the completed-replay return:
        // retries never bypass current account-liveness, deactivation,
        // mandatory-2FA, or membership policy merely because a prior request
        // committed.
        assert_effective_live_human_in_tx(
            &mut tx,
            request.workspace_id,
            request.participant_id,
            require_2fa,
        )
        .await?;

        let source = identities
            .iter()
            .find(|identity| identity_matches(identity, &request.from));
        let source_tombstone = tombstones
            .iter()
            .find(|tombstone| tombstone_matches(tombstone, &request.from));
        let target_tombstoned = tombstones
            .iter()
            .any(|tombstone| tombstone_matches(tombstone, &request.to));
        if target_tombstoned {
            return Err(Error::Conflict("target identity is tombstoned".into()));
        }

        let target_owner = identities
            .iter()
            .find(|identity| identity_matches(identity, &request.to))
            .map(|identity| identity.participant_id);
        if target_owner.is_some_and(|owner| owner != request.participant_id.to_uuid()) {
            return Err(Error::Conflict(
                "target identity is linked to another participant".into(),
            ));
        }

        let source_email = if let Some(source) = source {
            if source.participant_id != request.participant_id.to_uuid()
                || source_tombstone.is_some()
            {
                return Err(Error::Conflict(
                    "source identity is not linked to participant".into(),
                ));
            }
            source.email.clone()
        } else {
            let completed_retire = request.retire_source
                && source_tombstone.is_some_and(|tombstone| {
                    tombstone.reason == IDENTITY_MIGRATED_REASON
                        && tombstone.former_participant_id == request.participant_id.to_uuid()
                });
            if !completed_retire {
                return Err(Error::Conflict(
                    "source identity is not linked to participant".into(),
                ));
            }
            if target_owner != Some(request.participant_id.to_uuid()) {
                return Err(Error::Conflict(
                    "completed migration target is not linked to participant".into(),
                ));
            }

            // The exact retired-source tombstone plus the exact live target is
            // the durable completion marker. The first transaction updated SCIM
            // and wrote the audit atomically, so a network retry is read-only.
            tx.commit().await?;
            return Ok(IdentityMigrationOutcome {
                participant_id: request.participant_id,
                target_created: false,
                source_retired: true,
                scim_rows_updated: 0,
            });
        };

        let target_created = if target_owner.is_none() {
            sqlx::query(
                r"INSERT INTO sso_identities (issuer, subject, participant_id, email)
                   VALUES ($1, $2, $3, $4)",
            )
            .bind(&request.to.issuer)
            .bind(&request.to.subject)
            .bind(request.participant_id.to_uuid())
            .bind(source_email.as_deref())
            .execute(&mut *tx)
            .await?;
            true
        } else {
            false
        };

        let scim_rows_updated = if request.retire_source {
            retire_source_in_tx(&mut tx, &request).await?
        } else {
            0
        };

        let audit_target = request.participant_id.to_string();
        AuditRepo::append_in_tx(
            &mut tx,
            request.workspace_id,
            Some(request.actor_id),
            IDENTITY_MIGRATED_AUDIT_ACTION,
            Some(&audit_target),
            serde_json::json!({
                "target_created": target_created,
                "source_retired": request.retire_source,
                "scim_rows_updated": scim_rows_updated,
            }),
        )
        .await?;

        tx.commit().await?;
        Ok(IdentityMigrationOutcome {
            participant_id: request.participant_id,
            target_created,
            source_retired: request.retire_source,
            scim_rows_updated,
        })
    }
}

fn validate_request(request: &IdentityMigrationRequest) -> Result<(), Error> {
    let valid = |identity: &ExternalIdentityKey| {
        crate::sso::valid_external_identity_component(&identity.issuer)
            && crate::sso::valid_external_identity_component(&identity.subject)
    };
    if !valid(&request.from) || !valid(&request.to) {
        return Err(Error::Invalid(
            "external identity keys must be non-blank, exact, and control-free".into(),
        ));
    }
    if request.from == request.to {
        return Err(Error::Invalid(
            "source and target identities must differ".into(),
        ));
    }
    Ok(())
}

async fn lock_identity_pair_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    from: &ExternalIdentityKey,
    to: &ExternalIdentityKey,
) -> Result<(), sqlx::Error> {
    // Ask PostgreSQL to order the keys so this matches the database collation
    // used by participant erasure's multi-identity lifecycle lock acquisition.
    let ordered = sqlx::query_as::<_, (String, String)>(
        r"SELECT identity.issuer, identity.subject
            FROM (VALUES ($1::text, $2::text), ($3::text, $4::text))
                 AS identity(issuer, subject)
           ORDER BY identity.issuer, identity.subject",
    )
    .bind(&from.issuer)
    .bind(&from.subject)
    .bind(&to.issuer)
    .bind(&to.subject)
    .fetch_all(&mut **tx)
    .await?;
    for (issuer, subject) in ordered {
        sqlx::query("SELECT aero_lock_external_identity($1, $2)")
            .bind(issuer)
            .bind(subject)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn lock_identity_rows_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    from: &ExternalIdentityKey,
    to: &ExternalIdentityKey,
) -> Result<Vec<LockedIdentity>, sqlx::Error> {
    sqlx::query_as::<_, LockedIdentity>(
        r"SELECT issuer, subject, participant_id, email
            FROM sso_identities
           WHERE (issuer = $1 AND subject = $2)
              OR (issuer = $3 AND subject = $4)
           ORDER BY issuer, subject
           FOR UPDATE",
    )
    .bind(&from.issuer)
    .bind(&from.subject)
    .bind(&to.issuer)
    .bind(&to.subject)
    .fetch_all(&mut **tx)
    .await
}

async fn lock_identity_tombstones_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    from: &ExternalIdentityKey,
    to: &ExternalIdentityKey,
) -> Result<Vec<LockedIdentityTombstone>, sqlx::Error> {
    sqlx::query_as::<_, LockedIdentityTombstone>(
        r"SELECT issuer, subject, former_participant_id, reason
            FROM sso_identity_tombstones
           WHERE (issuer = $1 AND subject = $2)
              OR (issuer = $3 AND subject = $4)
           ORDER BY issuer, subject
           FOR UPDATE",
    )
    .bind(&from.issuer)
    .bind(&from.subject)
    .bind(&to.issuer)
    .bind(&to.subject)
    .fetch_all(&mut **tx)
    .await
}

fn identity_matches(identity: &LockedIdentity, key: &ExternalIdentityKey) -> bool {
    identity.issuer == key.issuer && identity.subject == key.subject
}

fn tombstone_matches(tombstone: &LockedIdentityTombstone, key: &ExternalIdentityKey) -> bool {
    tombstone.issuer == key.issuer && tombstone.subject == key.subject
}

/// Lock the aggregate root and the caller's current membership before taking
/// any external-identity lock. OIDC JIT follows the same workspace → identity
/// order, including for the default-workspace bootstrap trigger.
async fn lock_workspace_member_boundary_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Result<bool, Error> {
    let require_2fa = sqlx::query_scalar::<_, bool>(
        "SELECT require_2fa FROM workspaces WHERE id = $1 FOR UPDATE",
    )
    .bind(workspace.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| Error::NotFound("workspace".into()))?;
    let member = sqlx::query_scalar::<_, bool>(
        r"SELECT true
            FROM workspace_members
           WHERE workspace_id = $1 AND participant_id = $2
           FOR UPDATE",
    )
    .bind(workspace.to_uuid())
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(false);
    if !member {
        return Err(Error::Forbidden("caller is not a workspace member".into()));
    }
    Ok(require_2fa)
}

async fn assert_effective_live_human_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    participant: ParticipantId,
    require_2fa: bool,
) -> Result<(), Error> {
    let live_human = sqlx::query_scalar::<_, bool>(
        r"SELECT true
            FROM participants
           WHERE id = $1 AND kind = 'human' AND deleted_at IS NULL
           FOR UPDATE",
    )
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(false);
    if !live_human {
        return Err(Error::NotFound("live human participant".into()));
    }

    let deactivated = sqlx::query_scalar::<_, bool>(
        r"SELECT true
            FROM workspace_deactivations
           WHERE workspace_id = $1 AND participant_id = $2
           FOR SHARE",
    )
    .bind(workspace.to_uuid())
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(false);
    if deactivated {
        return Err(Error::Forbidden(
            "caller is not an effective workspace member".into(),
        ));
    }
    if require_2fa {
        let has_2fa = sqlx::query_scalar::<_, bool>(
            r"SELECT activated
                FROM totp_secrets
               WHERE participant_id = $1
               FOR SHARE",
        )
        .bind(participant.to_uuid())
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or(false);
        if !has_2fa {
            return Err(Error::Forbidden(
                "caller is not an effective workspace member".into(),
            ));
        }
    }
    Ok(())
}

async fn retire_source_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    request: &IdentityMigrationRequest,
) -> Result<u64, Error> {
    let deleted = sqlx::query(
        r"DELETE FROM sso_identities
           WHERE issuer = $1 AND subject = $2 AND participant_id = $3",
    )
    .bind(&request.from.issuer)
    .bind(&request.from.subject)
    .bind(request.participant_id.to_uuid())
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if deleted != 1 {
        return Err(Error::Conflict(
            "source identity is not linked to participant".into(),
        ));
    }

    sqlx::query(
        r"INSERT INTO sso_identity_tombstones
              (issuer, subject, former_participant_id, reason)
           VALUES ($1, $2, $3, $4)",
    )
    .bind(&request.from.issuer)
    .bind(&request.from.subject)
    .bind(request.participant_id.to_uuid())
    .bind(IDENTITY_MIGRATED_REASON)
    .execute(&mut **tx)
    .await?;

    let scim_rows_updated = sqlx::query(
        r"UPDATE scim_users
              SET external_id = $4, updated_at = NOW()
            WHERE workspace_id = $1
              AND participant_id = $2
              AND external_id = $3",
    )
    .bind(request.workspace_id.to_uuid())
    .bind(request.participant_id.to_uuid())
    .bind(&request.from.subject)
    .bind(&request.to.subject)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(scim_rows_updated)
}

#[cfg(test)]
mod tests;
