//! Workspace-scoped, proof-bound self-service external identity migration.
//!
//! An effective workspace member may add a newly authenticated external login
//! alias only to their own immutable participant, optionally retiring the old
//! alias. The boundary verifies a short-lived ID token for the exact target
//! issuer/subject; [`IdentityMigrationRepo`] repeats the self/member checks in
//! the same transaction as the mutation.
//!
//! External subjects are credentials-adjacent identifiers. Request types do not
//! implement `Debug`, responses never echo identity keys, and repository errors
//! are mapped to fixed public messages so neither HTTP bodies nor the global
//! error logger can expose a subject embedded in a database diagnostic.

use std::{
    str::FromStr,
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

use aero_auth::{
    validate_id_token, AuthUser, JwksKeyProvider, KeyProvider, OidcClaims, OidcConfig,
};
use aero_common::{Error as AeroError, ParticipantId, Result as AeroResult, WorkspaceId};
use aero_storage::{
    ExternalIdentityKey, IdentityMigrationOutcome, IdentityMigrationRepo, IdentityMigrationRequest,
};
use axum::{
    extract::{DefaultBodyLimit, Path, State},
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::error::ApiResult;
use crate::state::AppState;

/// Large enough for real-world OIDC/SAML identifiers while bounding JSON
/// allocation before validation.
const MAX_MIGRATION_BODY_BYTES: usize = 16 * 1024;
const MAX_ISSUER_BYTES: usize = 2 * 1024;
const MAX_SUBJECT_BYTES: usize = 4 * 1024;
const TARGET_PROOF_MAX_AGE_SECS: u64 = 5 * 60;
const TARGET_PROOF_CLOCK_SKEW_SECS: u64 = 60;

static IDENTITY_MIGRATION_JWKS: OnceLock<JwksKeyProvider> = OnceLock::new();

/// External identity migration routes, ready to merge into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/workspaces/:id/identity-migrations",
        post(migrate_identity).layer(DefaultBodyLimit::max(MAX_MIGRATION_BODY_BYTES)),
    )
}

fn repo(state: &AppState) -> IdentityMigrationRepo {
    IdentityMigrationRepo::new(state.pg.clone())
}

/// Identity aliases are credentials, so migration requires a browser/device
/// session that can be revoked independently. PAT and bot authentication carry
/// no session id and must never authorize this endpoint, even when the token
/// owner is otherwise an effective workspace administrator.
fn require_interactive_session(auth: &AuthUser) -> AeroResult<()> {
    auth.session_id
        .ok_or_else(|| AeroError::Unauthorized("interactive session required".into()))?;
    Ok(())
}

fn parse_workspace(value: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(value.trim())
        .map_err(|error| AeroError::Invalid(format!("workspace id: {error}")))
}

/// Resolve the caller through the effective-member guard so deleted,
/// deactivated, removed, and mandatory-2FA-blocked users all fail closed.
async fn assert_effective_member(
    state: &AppState,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    state
        .workspaces
        .effective_member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not an active workspace member".into()))?;
    Ok(())
}

/// Deliberately lacks `Debug`: identity keys must not enter structured logs.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalIdentityInput {
    issuer: String,
    subject: String,
}

/// Deliberately lacks `Debug`: see [`ExternalIdentityInput`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrationInput {
    from: ExternalIdentityInput,
    to: ExternalIdentityInput,
    /// Fresh ID token minted for the exact target identity. Never logged,
    /// debug-formatted, persisted, or returned.
    target_id_token: String,
    /// Required rather than defaulted so the caller must explicitly choose
    /// whether the source login remains usable.
    retire_source: bool,
}

fn validate_identity_component(
    label: &'static str,
    value: &str,
    max_bytes: usize,
) -> AeroResult<()> {
    if value.is_empty() || value != value.trim() || value.chars().any(char::is_control) {
        return Err(AeroError::Invalid(format!(
            "{label} must be non-blank, exact, and control-free"
        )));
    }
    if value.len() > max_bytes {
        return Err(AeroError::Invalid(format!(
            "{label} exceeds the {max_bytes}-byte limit"
        )));
    }
    Ok(())
}

fn validated_identity(
    label: &'static str,
    input: ExternalIdentityInput,
) -> Result<ExternalIdentityKey, AeroError> {
    validate_identity_component(
        if label == "source" {
            "source issuer"
        } else {
            "target issuer"
        },
        &input.issuer,
        MAX_ISSUER_BYTES,
    )?;
    validate_identity_component(
        if label == "source" {
            "source subject"
        } else {
            "target subject"
        },
        &input.subject,
        MAX_SUBJECT_BYTES,
    )?;
    Ok(ExternalIdentityKey::new(input.issuer, input.subject))
}

impl MigrationInput {
    fn into_parts(
        self,
        workspace_id: WorkspaceId,
        actor_id: ParticipantId,
    ) -> Result<(IdentityMigrationRequest, String), AeroError> {
        let from = validated_identity("source", self.from)?;
        let to = validated_identity("target", self.to)?;
        if from == to {
            return Err(AeroError::Invalid(
                "source and target identities must differ".into(),
            ));
        }
        Ok((
            IdentityMigrationRequest {
                workspace_id,
                actor_id,
                participant_id: actor_id,
                from,
                to,
                retire_source: self.retire_source,
            },
            self.target_id_token,
        ))
    }
}

fn target_proof_rejected() -> AeroError {
    AeroError::Unauthorized("target identity proof rejected".into())
}

fn validate_target_claims(
    claims: &OidcClaims,
    target: &ExternalIdentityKey,
    now: u64,
) -> AeroResult<()> {
    if claims.sub != target.subject {
        return Err(target_proof_rejected());
    }
    let issued_at = claims.iat.ok_or_else(target_proof_rejected)?;
    if issued_at > now.saturating_add(TARGET_PROOF_CLOCK_SKEW_SECS)
        || now
            > issued_at
                .saturating_add(TARGET_PROOF_MAX_AGE_SECS)
                .saturating_add(TARGET_PROOF_CLOCK_SKEW_SECS)
    {
        return Err(target_proof_rejected());
    }
    Ok(())
}

async fn verify_target_proof(
    token: &str,
    target: &ExternalIdentityKey,
    config: &OidcConfig,
    keys: &dyn KeyProvider,
    now: u64,
) -> AeroResult<()> {
    if target.issuer != config.issuer {
        return Err(target_proof_rejected());
    }
    let claims = validate_id_token(token, config, keys)
        .await
        .map_err(|_| target_proof_rejected())?;
    validate_target_claims(&claims, target, now)
}

fn unix_now() -> Result<u64, AeroError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| AeroError::Internal(anyhow::anyhow!("system clock is before unix epoch")))
}

/// Safe, stable response shape. It intentionally contains neither identity key.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct MigrationResponse {
    participant_id: ParticipantId,
    target_created: bool,
    source_retired: bool,
    scim_rows_updated: u64,
}

impl From<IdentityMigrationOutcome> for MigrationResponse {
    fn from(outcome: IdentityMigrationOutcome) -> Self {
        Self {
            participant_id: outcome.participant_id,
            target_created: outcome.target_created,
            source_retired: outcome.source_retired,
            scim_rows_updated: outcome.scim_rows_updated,
        }
    }
}

/// Keep identity values out of both the public error and `ApiError`'s global
/// logging. In particular, `PostgreSQL` diagnostics for a uniqueness failure may
/// include all conflicting key columns.
fn sanitize_repository_error(error: &AeroError) -> AeroError {
    match error {
        AeroError::NotFound(_) => AeroError::NotFound("identity migration participant".into()),
        AeroError::Forbidden(_) | AeroError::Unauthorized(_) => {
            AeroError::Forbidden("identity migration is not allowed for this workspace".into())
        }
        AeroError::Conflict(_) => {
            AeroError::Conflict("identity migration conflicts with current account state".into())
        }
        AeroError::Invalid(_) => AeroError::Invalid("invalid identity migration request".into()),
        AeroError::RateLimited => AeroError::RateLimited,
        AeroError::Upstream(_)
        | AeroError::Database(_)
        | AeroError::Serde(_)
        | AeroError::Internal(_) => {
            AeroError::Internal(anyhow::anyhow!("identity migration failed"))
        }
    }
}

/// `POST /api/workspaces/:id/identity-migrations` — an effective member proves
/// and adds a target external identity alias to their own participant, optionally
/// tombstoning the source alias.
async fn migrate_identity(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(workspace): Path<String>,
    Json(input): Json<MigrationInput>,
) -> ApiResult<Json<MigrationResponse>> {
    // Keep this as the first handler-body check: a PAT/bot request must fail
    // before path/body values are interpreted by this operation and before
    // target-IdP configuration or JWKS networking can be reached. Axum has
    // already performed extractor-level JSON decoding before entering here.
    require_interactive_session(&auth)?;
    let workspace = parse_workspace(&workspace)?;
    assert_effective_member(&state, workspace, auth.participant_id).await?;
    let (request, target_id_token) = input.into_parts(workspace, auth.participant_id)?;
    let oidc =
        OidcConfig::from_env().ok_or_else(|| AeroError::Invalid("oidc not configured".into()))?;
    let keys = IDENTITY_MIGRATION_JWKS.get_or_init(|| JwksKeyProvider::from_config(&oidc));
    verify_target_proof(&target_id_token, &request.to, &oidc, keys, unix_now()?).await?;
    let outcome = repo(&state)
        .migrate(request)
        .await
        .map_err(|error| sanitize_repository_error(&error))?;
    Ok(Json(outcome.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_migration_rejects_pat_or_bot_authentication() {
        let auth = AuthUser {
            participant_id: ParticipantId::new(),
            session_id: None,
            exp: None,
        };

        let error = require_interactive_session(&auth)
            .expect_err("opaque credentials cannot migrate login identities");
        assert_eq!(error.status_code(), 401);
    }

    #[test]
    fn identity_migration_accepts_an_authenticated_session() {
        let auth = AuthUser {
            participant_id: ParticipantId::new(),
            session_id: Some(aero_common::SessionId::new()),
            exp: Some(1),
        };

        require_interactive_session(&auth).expect("JWT session is eligible");
    }

    fn identity(issuer: impl Into<String>, subject: impl Into<String>) -> ExternalIdentityInput {
        ExternalIdentityInput {
            issuer: issuer.into(),
            subject: subject.into(),
        }
    }

    fn valid_input() -> MigrationInput {
        MigrationInput {
            from: identity("https://old-idp.example", "old-subject"),
            to: identity("https://new-idp.example", "new-subject"),
            target_id_token: "signed-target-proof".into(),
            retire_source: true,
        }
    }

    #[test]
    fn valid_input_binds_the_migration_to_the_authenticated_participant() {
        let workspace = WorkspaceId::new();
        let actor = ParticipantId::new();
        let (request, proof) = valid_input()
            .into_parts(workspace, actor)
            .expect("valid request");

        assert_eq!(request.workspace_id, workspace);
        assert_eq!(request.actor_id, actor);
        assert_eq!(request.participant_id, actor);
        assert_eq!(request.from.issuer, "https://old-idp.example");
        assert_eq!(request.from.subject, "old-subject");
        assert_eq!(request.to.issuer, "https://new-idp.example");
        assert_eq!(request.to.subject, "new-subject");
        assert!(request.retire_source);
        assert_eq!(proof, "signed-target-proof");
    }

    #[test]
    fn identity_components_are_exact_bounded_and_control_free() {
        for invalid in ["", " ", " padded", "padded ", "line\nbreak", "nul\0byte"] {
            assert!(validate_identity_component("subject", invalid, MAX_SUBJECT_BYTES).is_err());
        }
        assert!(
            validate_identity_component("subject", "case:SENSITIVE", MAX_SUBJECT_BYTES).is_ok()
        );
        assert!(validate_identity_component(
            "issuer",
            &"x".repeat(MAX_ISSUER_BYTES + 1),
            MAX_ISSUER_BYTES,
        )
        .is_err());
        assert!(validate_identity_component(
            "subject",
            &"x".repeat(MAX_SUBJECT_BYTES + 1),
            MAX_SUBJECT_BYTES,
        )
        .is_err());
    }

    #[test]
    fn source_and_target_must_differ() {
        let participant = ParticipantId::new();
        let same = "case-sensitive-subject";
        let input = MigrationInput {
            from: identity("https://idp.example", same),
            to: identity("https://idp.example", same),
            target_id_token: "signed-target-proof".into(),
            retire_source: false,
        };
        let error = input
            .into_parts(WorkspaceId::new(), participant)
            .err()
            .expect("same identity must fail");
        assert_eq!(error.status_code(), 400);
    }

    #[test]
    fn input_requires_target_proof_and_retirement_choice_and_rejects_unknown_fields() {
        let without_choice = serde_json::json!({
            "from": { "issuer": "https://old.example", "subject": "old" },
            "to": { "issuer": "https://new.example", "subject": "new" },
            "target_id_token": "signed-target-proof"
        });
        assert!(serde_json::from_value::<MigrationInput>(without_choice).is_err());

        let without_target_proof = serde_json::json!({
            "from": { "issuer": "https://old.example", "subject": "old" },
            "to": { "issuer": "https://new.example", "subject": "new" },
            "retire_source": false
        });
        assert!(serde_json::from_value::<MigrationInput>(without_target_proof).is_err());

        let with_typo = serde_json::json!({
            "from": { "issuer": "https://old.example", "subject": "old" },
            "to": { "issuer": "https://new.example", "subject": "new" },
            "target_id_token": "signed-target-proof",
            "retire_source": false,
            "retireSource": true
        });
        assert!(serde_json::from_value::<MigrationInput>(with_typo).is_err());

        let attempts_to_select_another_participant = serde_json::json!({
            "participant_id": ParticipantId::new(),
            "from": { "issuer": "https://old.example", "subject": "old" },
            "to": { "issuer": "https://new.example", "subject": "new" },
            "target_id_token": "signed-target-proof",
            "retire_source": false
        });
        assert!(
            serde_json::from_value::<MigrationInput>(attempts_to_select_another_participant)
                .is_err()
        );
    }

    fn claims(subject: &str, issued_at: Option<u64>) -> OidcClaims {
        OidcClaims {
            sub: subject.into(),
            email: None,
            name: None,
            email_verified: None,
            preferred_username: None,
            nonce: None,
            iat: issued_at,
        }
    }

    #[test]
    fn target_proof_claims_require_exact_subject_and_recent_iat() {
        let now = 10_000;
        let target = ExternalIdentityKey::new("https://idp.example", "case:SENSITIVE");
        assert!(validate_target_claims(&claims("case:SENSITIVE", Some(now)), &target, now).is_ok());
        assert!(validate_target_claims(
            &claims(
                "case:SENSITIVE",
                Some(now - TARGET_PROOF_MAX_AGE_SECS - TARGET_PROOF_CLOCK_SKEW_SECS)
            ),
            &target,
            now,
        )
        .is_ok());
        for rejected in [
            claims("case:sensitive", Some(now)),
            claims("case:SENSITIVE", None),
            claims(
                "case:SENSITIVE",
                Some(now - TARGET_PROOF_MAX_AGE_SECS - TARGET_PROOF_CLOCK_SKEW_SECS - 1),
            ),
            claims(
                "case:SENSITIVE",
                Some(now + TARGET_PROOF_CLOCK_SKEW_SECS + 1),
            ),
        ] {
            assert_eq!(
                validate_target_claims(&rejected, &target, now)
                    .expect_err("invalid proof claims")
                    .status_code(),
                401
            );
        }
    }

    #[tokio::test]
    async fn target_proof_requires_the_configured_issuer_without_leaking_values() {
        let config = OidcConfig {
            issuer: "https://trusted-idp.example".into(),
            audience: "aero-im".into(),
            jwks_uri: "https://trusted-idp.example/jwks".into(),
        };
        let target = ExternalIdentityKey::new("https://attacker-idp.example", "secret-subject");
        let token = "secret-target-id-token";
        let keys = JwksKeyProvider::new(&config.jwks_uri);
        let error = verify_target_proof(token, &target, &config, &keys, 10_000)
            .await
            .expect_err("the target issuer must be exact");
        let public = error.to_string();
        assert_eq!(error.status_code(), 401);
        for secret in [token, target.issuer.as_str(), target.subject.as_str()] {
            assert!(!public.contains(secret));
        }
    }

    #[test]
    fn success_response_contains_no_external_identity_material() {
        let response = MigrationResponse::from(IdentityMigrationOutcome {
            participant_id: ParticipantId::new(),
            target_created: true,
            source_retired: true,
            scim_rows_updated: 2,
        });
        let value = serde_json::to_value(response).expect("serialize response");
        let object = value.as_object().expect("response object");
        assert_eq!(object.len(), 4);
        assert!(object.contains_key("participant_id"));
        assert!(object.contains_key("target_created"));
        assert!(object.contains_key("source_retired"));
        assert!(object.contains_key("scim_rows_updated"));
        assert!(!object.contains_key("from"));
        assert!(!object.contains_key("to"));
        assert!(!object.contains_key("issuer"));
        assert!(!object.contains_key("subject"));
    }

    #[test]
    fn repository_errors_are_sanitized_before_global_logging() {
        let secret = "sensitive-external-subject";
        for error in [
            AeroError::Conflict(secret.into()),
            AeroError::Database(sqlx::Error::Protocol(secret.into())),
            AeroError::Internal(anyhow::anyhow!(secret)),
        ] {
            let safe = sanitize_repository_error(&error).to_string();
            assert!(!safe.contains(secret), "sanitized error leaked subject");
        }
    }
}
