//! SSO identity repository — maps an external `IdP` identity onto an internal
//! participant (SSO via OIDC / SAML).
//!
//! Backs `migrations/0014_sso.sql`. An `(issuer, subject)` pair from a validated
//! OIDC ID token resolves to exactly one [`ParticipantId`]. First-login JIT
//! provisioning is owned by [`SsoRepo::resolve_or_provision_human`], which keeps
//! the participant, initial workspace membership, and identity mapping in one
//! transaction and collapses concurrent first logins onto one canonical account.
//!
//! Purely additive: a NEW [`SsoRepo`]; no existing repo is touched. The token
//! *validation* lives in `aero-auth::oidc`; this layer only persists the mapping.
//!
//! [`link`]: SsoRepo::link

use aero_common::{ParticipantId, WorkspaceId};
use sqlx::{PgPool, Postgres, Transaction};

/// Expected failures while resolving an external identity.
#[derive(Debug, thiserror::Error)]
pub enum SsoResolveError {
    /// Issuer and subject keys are exact, non-blank, control-free identifiers.
    #[error("external identity key is invalid")]
    InvalidIdentity,
    /// The identity belonged to an account that was explicitly erased.  It may
    /// only be reused through an audited administrative recovery flow.
    #[error("external identity is tombstoned")]
    Tombstoned,
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

/// Transaction-local identity resolution result shared by OIDC/SAML JIT and
/// SCIM pre-provisioning.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResolvedExternalIdentity {
    pub(crate) participant_id: ParticipantId,
    pub(crate) newly_provisioned: bool,
}

/// Persists external-identity → participant mappings.
#[derive(Clone)]
pub struct SsoRepo {
    pool: PgPool,
}

impl SsoRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Resolve a previously-linked external identity to its internal participant.
    ///
    /// Returns `None` when the `(issuer, subject)` pair has never been linked —
    /// the signal the login handler uses to decide whether to JIT-provision.
    pub async fn find_participant(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<ParticipantId>, sqlx::Error> {
        validate_external_identity(issuer, subject)
            .map_err(|message| sqlx::Error::Protocol(message.into()))?;
        let row = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT participant_id FROM sso_identities WHERE issuer = $1 AND subject = $2",
        )
        .bind(issuer)
        .bind(subject)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(pid,)| ParticipantId::from_uuid(pid)))
    }

    /// Link an external identity to a participant (idempotent upsert).
    ///
    /// On a repeat login the `(issuer, subject)` row already exists: we keep the
    /// original `participant_id` (never re-point an identity at a different
    /// participant) but refresh the cached `email`, which may change at the `IdP`.
    pub async fn link(
        &self,
        issuer: &str,
        subject: &str,
        participant: ParticipantId,
        email: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        validate_external_identity(issuer, subject)
            .map_err(|message| sqlx::Error::Protocol(message.into()))?;
        sqlx::query(
            r"INSERT INTO sso_identities (issuer, subject, participant_id, email)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (issuer, subject)
               DO UPDATE SET email = EXCLUDED.email",
        )
        .bind(issuer)
        .bind(subject)
        .bind(participant.to_uuid())
        .bind(email)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Resolve an external identity or atomically JIT-provision its canonical
    /// human participant in `workspace`.
    ///
    /// The `(issuer, subject)` primary key is the concurrency arbiter. Two
    /// first-login transactions may each tentatively insert a participant, but
    /// only one participant id can be returned by the identity upsert. A losing
    /// transaction deletes its unreferenced candidate before commit and enrolls
    /// only the canonical participant, so neither a ghost participant nor a
    /// ghost workspace membership can survive.
    ///
    /// Existing mappings are locked together with their live participant row.
    /// This both refreshes the cached `IdP` email and prevents account erasure from
    /// interleaving between identity resolution and login.
    ///
    /// # Errors
    /// A missing workspace, deleted/tombstoned mapping, or any initial
    /// membership/link failure rolls the entire operation back. Existing
    /// identities are never silently re-enrolled into a workspace from which an
    /// administrator removed them.
    pub async fn resolve_or_provision_human(
        &self,
        issuer: &str,
        subject: &str,
        display_name: &str,
        email: Option<&str>,
        workspace: WorkspaceId,
    ) -> Result<ParticipantId, SsoResolveError> {
        validate_external_identity(issuer, subject)
            .map_err(|_| SsoResolveError::InvalidIdentity)?;
        let mut tx = self.pool.begin().await?;
        // Identity migration locks governance/workspace before its external
        // identity pair. Enter the same order here before JIT can reach the
        // default-workspace membership trigger; otherwise login (identity →
        // workspace) can deadlock migration (workspace → identity). Only this
        // aggregate root is locked: login must not serialize unrelated tenants.
        lock_login_workspace_in_tx(&mut tx, workspace).await?;
        let resolved =
            resolve_external_identity_in_tx(&mut tx, issuer, subject, display_name, email).await?;
        if resolved.newly_provisioned {
            ensure_workspace_membership(&mut tx, workspace, resolved.participant_id.to_uuid())
                .await?;
            // Security-event audit: JIT provisioning creates an account — the
            // same event class as first-party registration, so it reuses the
            // `auth.register` token with `detail.provisioning = "sso_jit"` +
            // IdP provenance ("account created" stays single-token; detail
            // carries provenance for class-level filtering). Same-fate: the row
            // commits/rolls back with the account. Subsequent logins of the
            // SAME identity are NOT audited (not an account-creation event).
            crate::AuditRepo::append_in_tx(
                &mut tx,
                workspace,
                Some(resolved.participant_id),
                "auth.register",
                Some(&resolved.participant_id.to_string()),
                serde_json::json!({
                    "provisioning": "sso_jit",
                    "issuer": issuer,
                    "subject": subject,
                    "display_name": display_name,
                    "email": email,
                }),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(resolved.participant_id)
    }
}

async fn lock_login_workspace_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
) -> Result<(), sqlx::Error> {
    let exists =
        sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .unwrap_or(false);
    if exists {
        Ok(())
    } else {
        Err(sqlx::Error::RowNotFound)
    }
}

/// Resolve or create one external identity inside a caller-owned transaction.
///
/// This is the single identity arbiter used by interactive SSO and SCIM.  The
/// migration-0231 triggers take the same per-identity advisory lock as erasure,
/// so a login racing account deletion either completes before the deletion or
/// observes the committed tombstone and fails closed.
pub(crate) async fn resolve_external_identity_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    issuer: &str,
    subject: &str,
    display_name: &str,
    email: Option<&str>,
) -> Result<ResolvedExternalIdentity, SsoResolveError> {
    validate_external_identity(issuer, subject).map_err(|_| SsoResolveError::InvalidIdentity)?;
    // Match the lifecycle triggers and erasure path before touching either the
    // identity row or its participant. Besides fencing tombstones, this gives
    // repeat-login and account deletion one deterministic lock order.
    lock_external_identity_in_tx(tx, issuer, subject).await?;

    let tombstoned = sqlx::query_scalar::<_, bool>(
        r"SELECT true
            FROM sso_identity_tombstones
           WHERE issuer = $1 AND subject = $2",
    )
    .bind(issuer)
    .bind(subject)
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(false);
    if tombstoned {
        return Err(SsoResolveError::Tombstoned);
    }

    // Fast path for repeat login/provisioning. Lock both rows so erasure cannot
    // detach the identity between resolution and the caller's membership write.
    let existing: Option<(uuid::Uuid,)> = sqlx::query_as(
        r"SELECT identity.participant_id
            FROM sso_identities identity
            JOIN participants participant
              ON participant.id = identity.participant_id
             AND participant.deleted_at IS NULL
           WHERE identity.issuer = $1
             AND identity.subject = $2
           FOR UPDATE OF identity, participant",
    )
    .bind(issuer)
    .bind(subject)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((participant_id,)) = existing {
        sqlx::query(
            r"UPDATE sso_identities
                 SET email = $3
               WHERE issuer = $1 AND subject = $2",
        )
        .bind(issuer)
        .bind(subject)
        .bind(email)
        .execute(&mut **tx)
        .await?;
        return Ok(ResolvedExternalIdentity {
            participant_id: ParticipantId::from_uuid(participant_id),
            newly_provisioned: false,
        });
    }

    let candidate = ParticipantId::new();
    sqlx::query(
        r"INSERT INTO participants (id, kind, display_name)
          VALUES ($1, 'human', $2)",
    )
    .bind(candidate.to_uuid())
    .bind(display_name)
    .execute(&mut **tx)
    .await?;

    // The identity PK remains the durable arbiter for mixed-version callers;
    // the advisory lock above prevents current OIDC/SCIM requests from creating
    // unnecessary losing participant candidates.
    let canonical = match sqlx::query_scalar::<_, uuid::Uuid>(
        r"INSERT INTO sso_identities
              (issuer, subject, participant_id, email)
          VALUES ($1, $2, $3, $4)
          ON CONFLICT (issuer, subject)
          DO UPDATE SET email = EXCLUDED.email
          RETURNING participant_id",
    )
    .bind(issuer)
    .bind(subject)
    .bind(candidate.to_uuid())
    .bind(email)
    .fetch_one(&mut **tx)
    .await
    {
        Ok(participant) => participant,
        Err(error) if is_tombstone_constraint(&error) => return Err(SsoResolveError::Tombstoned),
        Err(error) => return Err(SsoResolveError::Storage(error)),
    };

    let live = sqlx::query_scalar::<_, bool>(
        r"SELECT true
            FROM participants
           WHERE id = $1 AND deleted_at IS NULL
           FOR UPDATE",
    )
    .bind(canonical)
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(false);
    if !live {
        return Err(SsoResolveError::Tombstoned);
    }

    let newly_provisioned = canonical == candidate.to_uuid();
    if !newly_provisioned {
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(candidate.to_uuid())
            .execute(&mut **tx)
            .await?;
    }
    Ok(ResolvedExternalIdentity {
        participant_id: ParticipantId::from_uuid(canonical),
        newly_provisioned,
    })
}

/// Take the per-identity lifecycle advisory lock used by the resolver and the
/// migration-0231 delete/guard triggers. Callers must first enter any broader
/// workspace/governance lock order required by their aggregate.
async fn lock_external_identity_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    issuer: &str,
    subject: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT aero_lock_external_identity($1, $2)")
        .bind(issuer)
        .bind(subject)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(crate) fn valid_external_identity_component(value: &str) -> bool {
    !value.is_empty() && value == value.trim() && !value.chars().any(char::is_control)
}

fn validate_external_identity(issuer: &str, subject: &str) -> Result<(), &'static str> {
    if valid_external_identity_component(issuer) && valid_external_identity_component(subject) {
        Ok(())
    } else {
        Err("external identity issuer and subject must be non-blank, exact, and control-free")
    }
}

fn is_tombstone_constraint(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(|database| database.constraint() == Some("sso_identity_not_tombstoned"))
}

async fn ensure_workspace_membership(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
    participant: uuid::Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r"INSERT INTO workspace_members (workspace_id, participant_id, role)
          VALUES ($1, $2, 'member')
          ON CONFLICT (workspace_id, participant_id) DO NOTHING",
    )
    .bind(workspace.to_uuid())
    .bind(participant)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored sso_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{ParticipantId, WorkspaceId};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    // Create a throwaway participant so the FK on `participant_id` is satisfied.
    async fn fixture_participant(repo_pool: &PgPool) -> ParticipantId {
        let pid = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(pid.to_uuid())
            .bind(format!("sso-user-{pid}"))
            .execute(repo_pool)
            .await
            .expect("insert participant");
        pid
    }

    fn default_workspace() -> WorkspaceId {
        WorkspaceId::from_uuid(uuid::Uuid::nil())
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn sso_link_then_find_roundtrips() {
        let p = pool();
        let repo = SsoRepo::new(p.clone());
        let pid = fixture_participant(&p).await;
        let issuer = format!("https://idp.example/{pid}");
        let subject = "external-subject-123";

        // Unseen identity → None.
        assert!(repo
            .find_participant(&issuer, subject)
            .await
            .unwrap()
            .is_none());

        // Link, then it resolves to the participant.
        repo.link(&issuer, subject, pid, Some("user@example.com"))
            .await
            .unwrap();
        assert_eq!(
            repo.find_participant(&issuer, subject).await.unwrap(),
            Some(pid),
            "linked identity resolves to its participant"
        );

        // Re-link (repeat login) is idempotent: same participant, refreshed email.
        repo.link(&issuer, subject, pid, Some("new@example.com"))
            .await
            .unwrap();
        assert_eq!(
            repo.find_participant(&issuer, subject).await.unwrap(),
            Some(pid),
            "re-link keeps the original participant"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn sso_distinct_issuers_are_separate_identities() {
        let p = pool();
        let repo = SsoRepo::new(p.clone());
        let pid = fixture_participant(&p).await;
        let subject = "shared-subject";

        repo.link("https://idp-a.example", subject, pid, None)
            .await
            .unwrap();

        // Same subject under a different issuer is a *different* identity.
        assert!(
            repo.find_participant("https://idp-b.example", subject)
                .await
                .unwrap()
                .is_none(),
            "identity is keyed on (issuer, subject), not subject alone"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn sso_concurrent_first_login_returns_one_canonical_participant() {
        let p = pool();
        let repo = SsoRepo::new(p.clone());
        let marker = ParticipantId::new().to_string();
        let issuer = format!("https://idp.concurrent.example/{marker}");
        let subject = format!("subject-{marker}");
        let first_name = format!("sso-concurrent-{marker}-first");
        let second_name = format!("sso-concurrent-{marker}-second");

        let (first, second) = tokio::join!(
            repo.resolve_or_provision_human(
                &issuer,
                &subject,
                &first_name,
                Some("first@example.com"),
                default_workspace(),
            ),
            repo.resolve_or_provision_human(
                &issuer,
                &subject,
                &second_name,
                Some("second@example.com"),
                default_workspace(),
            )
        );
        let first = first.expect("first JIT login");
        let second = second.expect("second JIT login");
        assert_eq!(first, second, "both logins resolve the canonical account");

        let participant_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM participants WHERE display_name = ANY($1)")
                .bind(&[first_name, second_name])
                .fetch_one(&p)
                .await
                .expect("count JIT candidates");
        assert_eq!(
            participant_count.0, 1,
            "the losing candidate must be removed"
        );

        let membership_count: (i64,) = sqlx::query_as(
            r"SELECT COUNT(*)
                FROM workspace_members
               WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(default_workspace().to_uuid())
        .bind(first.to_uuid())
        .fetch_one(&p)
        .await
        .expect("count canonical memberships");
        assert_eq!(membership_count.0, 1);

        // Cleanup (FK NO ACTION, 0007): audit rows reference the participants.
        sqlx::query("DELETE FROM audit_events WHERE actor_id = $1")
            .bind(first.to_uuid())
            .execute(&p)
            .await
            .expect("delete audit rows before the participant");
        sqlx::query("DELETE FROM sso_identities WHERE issuer = $1 AND subject = $2")
            .bind(&issuer)
            .bind(&subject)
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(first.to_uuid())
            .execute(&p)
            .await
            .expect("delete participant");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn sso_jit_and_self_identity_migration_share_workspace_identity_lock_order() {
        use std::time::Duration;

        use crate::{
            ExternalIdentityKey, IdentityMigrationRepo, IdentityMigrationRequest, WorkspaceRepo,
        };

        let p = pool();
        let sso = SsoRepo::new(p.clone());
        let participant = fixture_participant(&p).await;
        WorkspaceRepo::new(p.clone())
            .add_member(
                default_workspace(),
                participant,
                aero_common::WorkspaceRole::Member,
            )
            .await
            .expect("enroll migration participant");
        let marker = ParticipantId::new().to_string();
        let source = ExternalIdentityKey::new(
            format!("https://idp.lock-order.example/source/{marker}"),
            format!("source-{marker}"),
        );
        let target = ExternalIdentityKey::new(
            format!("https://idp.lock-order.example/target/{marker}"),
            format!("target-{marker}"),
        );
        sso.link(
            &source.issuer,
            &source.subject,
            participant,
            Some("lock-order@example.test"),
        )
        .await
        .expect("link migration source");

        let migration = IdentityMigrationRepo::new(p.clone());
        let migration_request = IdentityMigrationRequest {
            workspace_id: default_workspace(),
            actor_id: participant,
            participant_id: participant,
            from: source,
            to: target.clone(),
            retire_source: false,
        };
        let raced = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(
                migration.migrate(migration_request),
                sso.resolve_or_provision_human(
                    &target.issuer,
                    &target.subject,
                    "lock-order-jit",
                    None,
                    default_workspace(),
                )
            )
        })
        .await
        .expect("workspace → identity ordering must not deadlock");

        match raced {
            (Ok(outcome), Ok(resolved)) => {
                assert_eq!(outcome.participant_id, participant);
                assert_eq!(resolved, participant);
            }
            (Err(aero_common::Error::Conflict(_)), Ok(resolved)) => {
                assert_ne!(resolved, participant, "JIT won the target binding race");
            }
            (migration, jit) => {
                panic!("unexpected migration/JIT race result: migration={migration:?}, jit={jit:?}")
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn sso_jit_does_not_lock_an_unrelated_workspace() {
        use std::time::Duration;

        use crate::WorkspaceRepo;

        let p = pool();
        let workspaces = WorkspaceRepo::new(p.clone());
        let first_owner = fixture_participant(&p).await;
        let second_owner = fixture_participant(&p).await;
        let marker = ParticipantId::new().to_string();
        let first = workspaces
            .create(
                format!("Unrelated locked workspace {marker}"),
                format!("unrelated-locked-{marker}"),
                first_owner,
            )
            .await
            .expect("create unrelated workspace")
            .id;
        let second = workspaces
            .create(
                format!("Login workspace {marker}"),
                format!("login-workspace-{marker}"),
                second_owner,
            )
            .await
            .expect("create login workspace")
            .id;

        let mut held = p.begin().await.expect("begin unrelated workspace lock");
        sqlx::query("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(first.to_uuid())
            .execute(&mut *held)
            .await
            .expect("lock unrelated workspace");

        let sso = SsoRepo::new(p.clone());
        let issuer = format!("https://idp.workspace-scope.example/{marker}");
        let subject = format!("workspace-scope-{marker}");
        let resolved = tokio::time::timeout(
            Duration::from_secs(2),
            sso.resolve_or_provision_human(
                &issuer,
                &subject,
                "workspace-scoped-login",
                None,
                second,
            ),
        )
        .await
        .expect("login in another workspace must not wait for the held row")
        .expect("JIT login in the independent workspace");
        assert_ne!(resolved, first_owner);
        assert_ne!(resolved, second_owner);
        held.rollback().await.expect("release unrelated lock");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn sso_provision_rolls_back_when_workspace_is_missing() {
        let p = pool();
        let repo = SsoRepo::new(p.clone());
        let marker = ParticipantId::new().to_string();
        let issuer = format!("https://idp.rollback.example/{marker}");
        let subject = format!("subject-{marker}");
        let display_name = format!("sso-rollback-{marker}");

        let result = repo
            .resolve_or_provision_human(
                &issuer,
                &subject,
                &display_name,
                Some("rollback@example.com"),
                WorkspaceId::new(),
            )
            .await;
        assert!(result.is_err(), "missing workspace must fail provisioning");

        let identity_count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sso_identities WHERE issuer = $1 AND subject = $2",
        )
        .bind(&issuer)
        .bind(&subject)
        .fetch_one(&p)
        .await
        .expect("count rolled-back identity");
        assert_eq!(identity_count.0, 0);

        let participant_count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM participants WHERE display_name = $1")
                .bind(&display_name)
                .fetch_one(&p)
                .await
                .expect("count rolled-back participant");
        assert_eq!(
            participant_count.0, 0,
            "participant insert must roll back with membership failure"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn existing_sso_identity_is_not_silently_reenrolled() {
        let p = pool();
        let repo = SsoRepo::new(p.clone());
        let marker = ParticipantId::new().to_string();
        let issuer = format!("https://idp.removed.example/{marker}");
        let subject = format!("subject-{marker}");
        let participant = repo
            .resolve_or_provision_human(
                &issuer,
                &subject,
                &format!("sso-removed-{marker}"),
                None,
                default_workspace(),
            )
            .await
            .expect("initial JIT");
        sqlx::query(
            "DELETE FROM workspace_members WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(default_workspace().to_uuid())
        .bind(participant.to_uuid())
        .execute(&p)
        .await
        .expect("administrator removes membership");

        let resolved = repo
            .resolve_or_provision_human(
                &issuer,
                &subject,
                "ignored repeat-login name",
                None,
                default_workspace(),
            )
            .await
            .expect("repeat identity resolution");
        assert_eq!(resolved, participant);
        let membership: (bool,) = sqlx::query_as(
            r"SELECT EXISTS (
                 SELECT 1 FROM workspace_members
                  WHERE workspace_id = $1 AND participant_id = $2
               )",
        )
        .bind(default_workspace().to_uuid())
        .bind(participant.to_uuid())
        .fetch_one(&p)
        .await
        .expect("read membership");
        assert!(
            !membership.0,
            "repeat SSO login must not undo an administrator's removal"
        );

        // Cleanup (FK NO ACTION, 0007): audit rows reference the participant.
        sqlx::query("DELETE FROM audit_events WHERE actor_id = $1")
            .bind(participant.to_uuid())
            .execute(&p)
            .await
            .expect("delete audit rows before the participant");
        sqlx::query("DELETE FROM sso_identities WHERE issuer = $1 AND subject = $2")
            .bind(&issuer)
            .bind(&subject)
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(participant.to_uuid())
            .execute(&p)
            .await
            .expect("delete participant");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn sso_jit_provisioning_audits_account_creation_once() {
        let p = pool();
        let repo = SsoRepo::new(p.clone());
        let marker = ParticipantId::new().to_string();
        let issuer = format!("https://idp.jit-audit.example/{marker}");
        let subject = format!("subject-{marker}");
        let display_name = format!("sso-jit-audit-{marker}");

        // First call (new identity) → account created: exactly one
        // `auth.register` row with `provisioning = "sso_jit"` provenance in
        // the IdP workspace (nil in tests), actor = target = the new
        // participant (AC1g). Count scoped by actor + provisioning — sibling
        // tests provision their own accounts.
        let participant = repo
            .resolve_or_provision_human(
                &issuer,
                &subject,
                &display_name,
                Some("jit@example.com"),
                default_workspace(),
            )
            .await
            .expect("first JIT login");
        let row: (String, String, String, String, String, String) = sqlx::query_as(
            "SELECT workspace_id::text, actor_id::text, target, \
                    detail->>'provisioning', detail->>'issuer', detail->>'subject' \
               FROM audit_events
              WHERE action = 'auth.register' AND actor_id = $1 \
                AND detail->>'provisioning' = 'sso_jit'",
        )
        .bind(participant.to_uuid())
        .fetch_one(&p)
        .await
        .expect("exactly one sso_jit auth.register row");
        assert_eq!(
            row.0,
            default_workspace().to_uuid().to_string(),
            "workspace = the IdP workspace (nil UUID)"
        );
        assert_eq!(row.1, participant.to_uuid().to_string(), "actor = the new participant");
        assert_eq!(row.2, participant.to_string(), "target = the new participant");
        assert_eq!(row.3, "sso_jit");
        assert_eq!(row.4, issuer);
        assert_eq!(row.5, subject);

        // Second call (existing identity) → NOT an account-creation event:
        // zero new rows (count stays 1).
        let resolved = repo
            .resolve_or_provision_human(
                &issuer,
                &subject,
                "ignored repeat name",
                None,
                default_workspace(),
            )
            .await
            .expect("repeat JIT login");
        assert_eq!(resolved, participant);
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_events WHERE action = 'auth.register' AND actor_id = $1 \
              AND detail->>'provisioning' = 'sso_jit'",
        )
        .bind(participant.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(total, 1, "repeat login of an existing identity audits nothing");

        // Cleanup (FK NO ACTION, 0007): audit rows reference the participant.
        sqlx::query("DELETE FROM audit_events WHERE actor_id = $1")
            .bind(participant.to_uuid())
            .execute(&p)
            .await
            .expect("delete audit rows before the participant");
        sqlx::query("DELETE FROM sso_identities WHERE issuer = $1 AND subject = $2")
            .bind(&issuer)
            .bind(&subject)
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(participant.to_uuid())
            .execute(&p)
            .await
            .expect("delete participant");
    }
}
