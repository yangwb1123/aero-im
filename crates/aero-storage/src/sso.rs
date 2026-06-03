//! SSO identity repository — maps an external `IdP` identity onto an internal
//! participant (SSO via OIDC).
//!
//! Backs `migrations/0014_sso.sql`. An `(issuer, subject)` pair from a validated
//! OIDC ID token resolves to exactly one [`ParticipantId`]; on first login for an
//! unseen identity the HTTP layer JIT-provisions a participant and [`link`]s it.
//!
//! Purely additive: a NEW [`SsoRepo`]; no existing repo is touched. The token
//! *validation* lives in `aero-auth::oidc`; this layer only persists the mapping.
//!
//! [`link`]: SsoRepo::link

use aero_common::ParticipantId;
use sqlx::PgPool;

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
    use aero_common::ParticipantId;

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

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn sso_link_then_find_roundtrips() {
        let p = pool();
        let repo = SsoRepo::new(p.clone());
        let pid = fixture_participant(&p).await;
        let issuer = format!("https://idp.example/{pid}");
        let subject = "external-subject-123";

        // Unseen identity → None.
        assert!(repo.find_participant(&issuer, subject).await.unwrap().is_none());

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
}
