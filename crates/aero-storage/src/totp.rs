//! TOTP (RFC 6238) secret repository — per-participant authenticator-app 2FA.
//!
//! Backs `migrations/0044_totp_secrets.sql`. One row per participant who has
//! enrolled an authenticator app: the shared base32 secret plus an `activated`
//! flag. Enrollment ([`TotpRepo::upsert_secret`]) stores the secret with
//! `activated = false`; the participant proves possession of the app by
//! submitting a current code, which flips activation ([`TotpRepo::activate`]).
//! Re-enrolling overwrites the secret and resets activation, so a half-finished
//! enrollment can always be restarted cleanly.
//!
//! The TOTP crypto (code derivation/verification) lives in
//! [`aero_auth::totp`](../../aero_auth/totp/index.html); this repo owns only the
//! per-participant secret + activation state. Keyed solely by `participant_id`
//! (the table's primary key), so it carries no surrogate id and no model struct —
//! it returns the bare secret / activation booleans. Purely additive: a NEW
//! [`TotpRepo`]; no existing repo is touched.

use aero_common::ParticipantId;
use sqlx::PgPool;

/// Repository over the `totp_secrets` table (per-participant 2FA state).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`TotpRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct TotpRepo {
    pool: PgPool,
}

impl TotpRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Enroll (or re-enroll) `participant` with a new shared `secret`. Idempotent
    /// by primary key: an existing row is overwritten and its activation **reset**
    /// (`activated = false`, fresh `created_at`, cleared `activated_at`), so a
    /// re-enrollment always starts unactivated.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert.
    pub async fn upsert_secret(
        &self,
        participant: ParticipantId,
        secret: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO totp_secrets (participant_id, secret)
               VALUES ($1, $2)
               ON CONFLICT (participant_id) DO UPDATE
                 SET secret = $2, activated = false, created_at = now(), activated_at = NULL",
        )
        .bind(participant.to_uuid())
        .bind(secret)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch `participant`'s stored TOTP secret, or `None` if they have not
    /// enrolled.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get_secret(
        &self,
        participant: ParticipantId,
    ) -> Result<Option<String>, sqlx::Error> {
        let row =
            sqlx::query_as::<_, (String,)>("SELECT secret FROM totp_secrets WHERE participant_id = $1")
                .bind(participant.to_uuid())
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(secret,)| secret))
    }

    /// Whether `participant` has an **activated** 2FA enrollment (the login-time
    /// gate). `false` when there is no enrollment at all or it is still pending.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn is_activated(&self, participant: ParticipantId) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (bool,)>(
            "SELECT activated FROM totp_secrets WHERE participant_id = $1",
        )
        .bind(participant.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some_and(|(activated,)| activated))
    }

    /// Mark `participant`'s pending enrollment as activated. Returns `true` iff a
    /// row was flipped — only flips a not-yet-activated row, so a second call (or
    /// a call without an enrollment) is a no-op returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn activate(&self, participant: ParticipantId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE totp_secrets SET activated = true, activated_at = now()
              WHERE participant_id = $1 AND activated = false",
        )
        .bind(participant.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Remove `participant`'s 2FA enrollment entirely. Returns `true` iff a row
    /// was deleted; a second call (or a call without an enrollment) is a no-op
    /// returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn disable(&self, participant: ParticipantId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM totp_secrets WHERE participant_id = $1")
            .bind(participant.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored totp
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

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
            .bind(format!("totp-owner-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn totp_upsert_get_activate_disable() {
        let p = pool();
        let repo = TotpRepo::new(p.clone());
        let owner = mk_participant(&p).await;

        // No enrollment initially.
        assert!(repo.get_secret(owner).await.unwrap().is_none(), "no secret yet");
        assert!(!repo.is_activated(owner).await.unwrap(), "not activated yet");
        assert!(!repo.activate(owner).await.unwrap(), "activate without enrollment is a no-op");

        // upsert → get reflects the secret; still pending (not activated).
        repo.upsert_secret(owner, "JBSWY3DPEHPK3PXP").await.unwrap();
        assert_eq!(
            repo.get_secret(owner).await.unwrap().as_deref(),
            Some("JBSWY3DPEHPK3PXP"),
            "stored secret round-trips"
        );
        assert!(!repo.is_activated(owner).await.unwrap(), "fresh enrollment is pending");

        // activate flips is_activated; a second activate is a no-op.
        assert!(repo.activate(owner).await.unwrap(), "first activate flips");
        assert!(repo.is_activated(owner).await.unwrap(), "now activated");
        assert!(!repo.activate(owner).await.unwrap(), "second activate is a no-op");

        // Re-enroll overwrites the secret and resets activation.
        repo.upsert_secret(owner, "MFRGGZDFMZTWQ2LK").await.unwrap();
        assert_eq!(
            repo.get_secret(owner).await.unwrap().as_deref(),
            Some("MFRGGZDFMZTWQ2LK"),
            "re-enroll overwrites the secret"
        );
        assert!(!repo.is_activated(owner).await.unwrap(), "re-enroll resets activation");

        // disable removes the row; a second disable is a no-op.
        assert!(repo.disable(owner).await.unwrap(), "disable removes the enrollment");
        assert!(repo.get_secret(owner).await.unwrap().is_none(), "no secret after disable");
        assert!(!repo.is_activated(owner).await.unwrap(), "not activated after disable");
        assert!(!repo.disable(owner).await.unwrap(), "second disable is a no-op");

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM totp_secrets WHERE participant_id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
