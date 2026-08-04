//! Exact external-identity binding used by SCIM aggregate writes.
//!
//! Callers own the workspace aggregate lock before entering this module. The
//! helper then follows the shared workspace → identity → participant order used
//! by OIDC JIT, identity migration, and account erasure.

use aero_common::ParticipantId;
use sqlx::{Postgres, Transaction};

use super::ScimUserWriteError;

const MAX_IDENTITY_ISSUER_BYTES: usize = 2 * 1024;

/// One route-facing SCIM user mutation. The nested `external_id` option
/// distinguishes "set NULL" from "leave unchanged".
#[derive(Debug, Clone, Copy, Default)]
pub struct ScimUserUpdate<'a> {
    pub user_name: Option<&'a str>,
    pub external_id: Option<Option<&'a str>>,
    pub active: Option<bool>,
    pub display_name: Option<&'a str>,
    pub identity_issuer: Option<&'a str>,
}

pub(super) fn validated_identity_binding<'a>(
    identity_issuer: Option<&'a str>,
    external_id: Option<&'a str>,
) -> Result<Option<(&'a str, &'a str)>, ScimUserWriteError> {
    let Some(issuer) = identity_issuer else {
        return Ok(None);
    };
    if issuer.len() > MAX_IDENTITY_ISSUER_BYTES
        || !crate::sso::valid_external_identity_component(issuer)
    {
        return Err(ScimUserWriteError::InvalidIdentityIssuer);
    }
    let Some(subject) =
        external_id.filter(|value| crate::sso::valid_external_identity_component(value))
    else {
        return Err(ScimUserWriteError::IdentitySubjectRequired);
    };
    Ok(Some((issuer, subject)))
}

/// Bind an exact, validated identity to an existing participant without ever
/// creating a second participant. The per-identity advisory lock makes the
/// tombstone check, collision check, and insert one lifecycle decision.
pub(super) async fn bind_existing_participant_identity_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    issuer: &str,
    subject: &str,
    participant: ParticipantId,
) -> Result<bool, ScimUserWriteError> {
    sqlx::query("SELECT aero_lock_external_identity($1, $2)")
        .bind(issuer)
        .bind(subject)
        .execute(&mut **tx)
        .await?;

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
        return Err(ScimUserWriteError::IdentityTombstoned);
    }

    let existing = sqlx::query_scalar::<_, uuid::Uuid>(
        r"SELECT participant_id
            FROM sso_identities
           WHERE issuer = $1 AND subject = $2
           FOR UPDATE",
    )
    .bind(issuer)
    .bind(subject)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(existing) = existing {
        if existing == participant.to_uuid() {
            return Ok(false);
        }
        return Err(ScimUserWriteError::IdentityConflict);
    }

    match sqlx::query(
        r"INSERT INTO sso_identities (issuer, subject, participant_id, email)
           VALUES ($1, $2, $3, NULL)",
    )
    .bind(issuer)
    .bind(subject)
    .bind(participant.to_uuid())
    .execute(&mut **tx)
    .await
    {
        Ok(_) => Ok(true),
        Err(error) if is_tombstone_constraint(&error) => {
            Err(ScimUserWriteError::IdentityTombstoned)
        }
        Err(error) => Err(ScimUserWriteError::Storage(error)),
    }
}

fn is_tombstone_constraint(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(|database| database.constraint() == Some("sso_identity_not_tombstoned"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scim::{
        db_test_support::{new_participant, new_workspace, pool},
        ScimRepo,
    };
    use crate::workspace::WorkspaceRepo;
    use crate::SsoRepo;

    #[test]
    fn configured_identity_requires_exact_issuer_and_subject() {
        assert_eq!(
            validated_identity_binding(None, Some("subject")).unwrap(),
            None
        );
        for issuer in ["", " issuer", "issuer ", "issuer\n"] {
            assert!(matches!(
                validated_identity_binding(Some(issuer), Some("subject")),
                Err(ScimUserWriteError::InvalidIdentityIssuer)
            ));
        }
        assert!(matches!(
            validated_identity_binding(Some(&"x".repeat(MAX_IDENTITY_ISSUER_BYTES + 1)), Some("s")),
            Err(ScimUserWriteError::InvalidIdentityIssuer)
        ));
        for subject in [None, Some(""), Some(" subject"), Some("subject\n")] {
            assert!(matches!(
                validated_identity_binding(Some("https://idp.example"), subject),
                Err(ScimUserWriteError::IdentitySubjectRequired)
            ));
        }
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn scim_legacy_subject_binds_to_existing_participant_before_oidc_jit() {
        let pool = pool();
        let scim = ScimRepo::new(pool.clone());
        let sso = SsoRepo::new(pool.clone());
        let workspaces = WorkspaceRepo::new(pool.clone());
        let owner = new_participant(&pool).await;
        let workspace = new_workspace(&workspaces, owner).await;
        let marker = uuid::Uuid::new_v4();
        let issuer = format!("https://snaplink.example/{marker}");
        let subject = format!("legacy-subject-{marker}");
        let user_name = format!("legacy-{marker}@example.com");
        let legacy = scim
            .provision_user(
                workspace,
                "Legacy SCIM user",
                &user_name,
                Some(&subject),
                true,
            )
            .await
            .expect("create legacy unbound SCIM row");
        assert_eq!(sso.find_participant(&issuer, &subject).await.unwrap(), None);

        // PATCH without an externalId delta upgrades the row. A subsequent PUT
        // carrying the same stable subject is idempotent.
        scim.update_user_atomic_with_identity(
            workspace,
            legacy.participant_id,
            super::super::ScimUserUpdate {
                active: Some(true),
                identity_issuer: Some(&issuer),
                ..Default::default()
            },
        )
        .await
        .expect("PATCH binds legacy identity")
        .expect("legacy SCIM row exists");
        scim.update_user_atomic_with_identity(
            workspace,
            legacy.participant_id,
            super::super::ScimUserUpdate {
                user_name: Some(&user_name),
                external_id: Some(Some(&subject)),
                active: Some(true),
                display_name: Some("Legacy SCIM user"),
                identity_issuer: Some(&issuer),
            },
        )
        .await
        .expect("PUT keeps exact binding")
        .expect("legacy SCIM row exists");

        let logged_in = sso
            .resolve_or_provision_human(
                &issuer,
                &subject,
                "must not create a duplicate",
                Some(&user_name),
                workspace,
            )
            .await
            .expect("OIDC resolves upgraded SCIM identity");
        assert_eq!(logged_in, legacy.participant_id);
        let identity_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sso_identities WHERE issuer = $1 AND subject = $2",
        )
        .bind(&issuer)
        .bind(&subject)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(identity_count, 1);
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn scim_legacy_binding_respects_collision_and_erasure_tombstone() {
        let pool = pool();
        let scim = ScimRepo::new(pool.clone());
        let sso = SsoRepo::new(pool.clone());
        let workspaces = WorkspaceRepo::new(pool.clone());
        let owner = new_participant(&pool).await;
        let workspace = new_workspace(&workspaces, owner).await;
        let marker = uuid::Uuid::new_v4();
        let issuer = format!("https://snaplink.example/{marker}");

        let collision_subject = format!("collision-{marker}");
        let canonical = new_participant(&pool).await;
        sso.link(&issuer, &collision_subject, canonical, None)
            .await
            .expect("seed canonical login binding");
        let colliding = scim
            .provision_user(
                workspace,
                "Legacy collision",
                &format!("collision-{marker}@example.com"),
                Some(&collision_subject),
                true,
            )
            .await
            .expect("create colliding legacy row");
        let collision = scim
            .update_user_atomic_with_identity(
                workspace,
                colliding.participant_id,
                super::super::ScimUserUpdate {
                    display_name: Some("must roll back"),
                    identity_issuer: Some(&issuer),
                    ..Default::default()
                },
            )
            .await
            .expect_err("identity owned by another participant must conflict");
        assert!(matches!(collision, ScimUserWriteError::IdentityConflict));
        assert_eq!(
            sso.find_participant(&issuer, &collision_subject)
                .await
                .unwrap(),
            Some(canonical)
        );

        let tombstoned_subject = format!("tombstoned-{marker}");
        let tombstoned = scim
            .provision_user(
                workspace,
                "Legacy erased",
                &format!("erased-{marker}@example.com"),
                Some(&tombstoned_subject),
                true,
            )
            .await
            .expect("create tombstoned legacy row");
        sqlx::query(
            r"INSERT INTO sso_identity_tombstones
                  (issuer, subject, former_participant_id, reason)
               VALUES ($1, $2, $3, 'test_erasure')",
        )
        .bind(&issuer)
        .bind(&tombstoned_subject)
        .bind(ParticipantId::new().to_uuid())
        .execute(&pool)
        .await
        .unwrap();
        let erased = scim
            .update_user_atomic_with_identity(
                workspace,
                tombstoned.participant_id,
                super::super::ScimUserUpdate {
                    identity_issuer: Some(&issuer),
                    ..Default::default()
                },
            )
            .await
            .expect_err("tombstoned identity cannot be revived");
        assert!(matches!(erased, ScimUserWriteError::IdentityTombstoned));
        assert_eq!(
            sso.find_participant(&issuer, &tombstoned_subject)
                .await
                .unwrap(),
            None
        );
    }
}
