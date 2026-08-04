//! Atomic password rotation and reset lifecycle.
//!
//! A credential change is a security state transition, not a sequence of
//! best-effort writes. These helpers update the live credential, retain/prune
//! password history, consume a reset token (for reset flows), revoke every
//! active refresh session, and blacklist their token hashes in one `PostgreSQL`
//! transaction. Any failure rolls the whole transition back.

use aero_common::ParticipantId;
use sqlx::{PgPool, Postgres, Transaction};

use crate::password_history::HISTORY_DEPTH;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangePasswordResult {
    Applied { sessions_revoked: u64 },
    StaleCredentials,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetPasswordResult {
    Applied {
        participant: ParticipantId,
        sessions_revoked: u64,
    },
    InvalidToken,
    StaleCredentials,
}

#[derive(Clone)]
#[must_use]
pub struct CredentialRotationRepo {
    pool: PgPool,
}

impl CredentialRotationRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Change an authenticated participant's password atomically.
    ///
    /// `expected_current_hash` binds the final commit to the credential that the
    /// HTTP layer verified. A concurrent password change therefore returns
    /// [`ChangePasswordResult::StaleCredentials`] without changing any row.
    pub async fn change_password(
        &self,
        participant: ParticipantId,
        expected_current_hash: &str,
        new_hash: &str,
    ) -> Result<ChangePasswordResult, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let Some(current_hash) = lock_current_hash(&mut tx, participant).await? else {
            tx.rollback().await?;
            return Ok(ChangePasswordResult::StaleCredentials);
        };
        if current_hash != expected_current_hash {
            tx.rollback().await?;
            return Ok(ChangePasswordResult::StaleCredentials);
        }

        replace_password_and_record_history(&mut tx, participant, &current_hash, new_hash).await?;
        invalidate_reset_tokens(&mut tx, participant).await?;
        let sessions_revoked = revoke_all_sessions(&mut tx, participant).await?;
        tx.commit().await?;
        Ok(ChangePasswordResult::Applied { sessions_revoked })
    }

    /// Consume a valid reset token and rotate its owner's password atomically.
    ///
    /// The credential is locked first, then the presented token is revalidated
    /// and locked. Every password-changing flow takes locks in that order, so a
    /// reset racing an authenticated change (or another sibling reset token)
    /// cannot deadlock through opposing credential/token locks.
    pub async fn reset_password(
        &self,
        token_hash: &str,
        expected_current_hash: &str,
        new_hash: &str,
    ) -> Result<ResetPasswordResult, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // This first lookup only discovers the credential row to lock. The
        // token is authoritatively rechecked under `FOR UPDATE` below.
        let owner: Option<(uuid::Uuid,)> = sqlx::query_as(
            r"SELECT participant_id
                FROM password_reset_tokens
               WHERE token_hash = $1
                 AND used_at IS NULL
                 AND expires_at > now()",
        )
        .bind(token_hash)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((owner,)) = owner else {
            tx.rollback().await?;
            return Ok(ResetPasswordResult::InvalidToken);
        };
        let participant = ParticipantId::from_uuid(owner);

        let Some(current_hash) = lock_current_hash(&mut tx, participant).await? else {
            tx.rollback().await?;
            return Ok(ResetPasswordResult::StaleCredentials);
        };

        let token_is_still_valid = sqlx::query_scalar::<_, String>(
            r"SELECT token_hash
                FROM password_reset_tokens
               WHERE token_hash = $1
                 AND participant_id = $2
                 AND used_at IS NULL
                 AND expires_at > now()
               FOR UPDATE",
        )
        .bind(token_hash)
        .bind(participant.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !token_is_still_valid {
            tx.rollback().await?;
            return Ok(ResetPasswordResult::InvalidToken);
        }

        if current_hash != expected_current_hash {
            tx.rollback().await?;
            return Ok(ResetPasswordResult::StaleCredentials);
        }

        let consumed = sqlx::query(
            r"UPDATE password_reset_tokens
                  SET used_at = now()
                WHERE participant_id = $1
                  AND used_at IS NULL",
        )
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        // The selected token was locked, valid, and unused, so the
        // participant-wide invalidation must consume at least that row. Marking
        // every sibling token used closes the account-takeover window where an
        // older reset link could overwrite the freshly rotated password.
        if consumed == 0 {
            tx.rollback().await?;
            return Ok(ResetPasswordResult::InvalidToken);
        }

        replace_password_and_record_history(&mut tx, participant, &current_hash, new_hash).await?;
        let sessions_revoked = revoke_all_sessions(&mut tx, participant).await?;
        tx.commit().await?;
        Ok(ResetPasswordResult::Applied {
            participant,
            sessions_revoked,
        })
    }
}

async fn invalidate_reset_tokens(
    tx: &mut Transaction<'_, Postgres>,
    participant: ParticipantId,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        r"UPDATE password_reset_tokens
              SET used_at = now()
            WHERE participant_id = $1
              AND used_at IS NULL",
    )
    .bind(participant.to_uuid())
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected())
}

async fn lock_current_hash(
    tx: &mut Transaction<'_, Postgres>,
    participant: ParticipantId,
) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(String,)> = sqlx::query_as(
        r"SELECT password_hash
            FROM credentials
           WHERE participant_id = $1
           FOR UPDATE",
    )
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|(hash,)| hash))
}

async fn replace_password_and_record_history(
    tx: &mut Transaction<'_, Postgres>,
    participant: ParticipantId,
    current_hash: &str,
    new_hash: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE credentials SET password_hash = $1 WHERE participant_id = $2")
        .bind(new_hash)
        .bind(participant.to_uuid())
        .execute(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO password_history (participant_id, password_hash) VALUES ($1, $2)")
        .bind(participant.to_uuid())
        .bind(current_hash)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        r"DELETE FROM password_history
           WHERE participant_id = $1
             AND id NOT IN (
                 SELECT id
                   FROM password_history
                  WHERE participant_id = $1
                  ORDER BY created_at DESC, id DESC
                  LIMIT $2
             )",
    )
    .bind(participant.to_uuid())
    .bind(HISTORY_DEPTH)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn revoke_all_sessions(
    tx: &mut Transaction<'_, Postgres>,
    participant: ParticipantId,
) -> Result<u64, sqlx::Error> {
    let (count,): (i64,) = sqlx::query_as(
        r"WITH revoked AS (
               UPDATE auth_sessions
                  SET revoked_at = now()
                WHERE participant_id = $1
                  AND revoked_at IS NULL
            RETURNING token_hash
           ),
           blacklisted AS (
               INSERT INTO revoked_tokens (token_hash, participant_id)
               SELECT token_hash, $1 FROM revoked
               ON CONFLICT (token_hash) DO NOTHING
               RETURNING token_hash
           )
           SELECT COUNT(*) FROM revoked",
    )
    .bind(participant.to_uuid())
    .fetch_one(&mut **tx)
    .await?;
    Ok(u64::try_from(count).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL is required for ignored credential-rotation tests");
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_lazy(&url)
            .expect("valid DATABASE_URL")
    }

    async fn fixture(pool: &PgPool) -> ParticipantId {
        let participant = ParticipantId::new();
        sqlx::query(
            "INSERT INTO participants (id, kind, display_name)
             VALUES ($1, 'human', $2)",
        )
        .bind(participant.to_uuid())
        .bind(format!("credential-rotation-{participant}"))
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO credentials (participant_id, email, password_hash)
             VALUES ($1, $2, 'hash-old')",
        )
        .bind(participant.to_uuid())
        .bind(format!("{participant}@credential-rotation.test"))
        .execute(pool)
        .await
        .unwrap();
        participant
    }

    async fn issue(pool: &PgPool, participant: ParticipantId, token: &str) {
        sqlx::query(
            "INSERT INTO password_reset_tokens
                 (token_hash, participant_id, expires_at)
             VALUES ($1, $2, now() + interval '1 hour')",
        )
        .bind(token)
        .bind(participant.to_uuid())
        .execute(pool)
        .await
        .unwrap();
    }

    async fn usable(pool: &PgPool, token: &str) -> bool {
        sqlx::query_scalar::<_, bool>(
            r"SELECT EXISTS(
                   SELECT 1
                     FROM password_reset_tokens
                    WHERE token_hash = $1
                      AND used_at IS NULL
                      AND expires_at > now()
               )",
        )
        .bind(token)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL with migrations applied"]
    async fn every_password_rotation_invalidates_all_outstanding_reset_tokens() {
        let pool = pool();
        let participant = fixture(&pool).await;
        issue(&pool, participant, "reset-token-one").await;
        issue(&pool, participant, "reset-token-two").await;

        let repo = CredentialRotationRepo::new(pool.clone());
        assert!(matches!(
            repo.reset_password("reset-token-one", "hash-old", "hash-new")
                .await
                .unwrap(),
            ResetPasswordResult::Applied {
                participant: applied,
                ..
            } if applied == participant
        ));
        assert!(!usable(&pool, "reset-token-one").await);
        assert!(!usable(&pool, "reset-token-two").await);
        assert_eq!(
            repo.reset_password("reset-token-two", "hash-new", "hash-stolen")
                .await
                .unwrap(),
            ResetPasswordResult::InvalidToken
        );

        issue(&pool, participant, "reset-token-three").await;
        assert!(matches!(
            repo.change_password(participant, "hash-new", "hash-final")
                .await
                .unwrap(),
            ChangePasswordResult::Applied { .. }
        ));
        assert!(!usable(&pool, "reset-token-three").await);

        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(participant.to_uuid())
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL with migrations applied"]
    async fn sibling_reset_tokens_serialize_without_deadlock() {
        let pool = pool();
        let participant = fixture(&pool).await;
        let token_a = format!("reset-a-{participant}");
        let token_b = format!("reset-b-{participant}");
        issue(&pool, participant, &token_a).await;
        issue(&pool, participant, &token_b).await;

        let repo = CredentialRotationRepo::new(pool.clone());
        let (a, b) = tokio::join!(
            repo.reset_password(&token_a, "hash-old", "hash-winner-a"),
            repo.reset_password(&token_b, "hash-old", "hash-winner-b")
        );
        let a = a.expect("first reset must not deadlock/error");
        let b = b.expect("second reset must not deadlock/error");
        let applied = usize::from(matches!(a, ResetPasswordResult::Applied { .. }))
            + usize::from(matches!(b, ResetPasswordResult::Applied { .. }));
        let rejected = usize::from(matches!(a, ResetPasswordResult::InvalidToken))
            + usize::from(matches!(b, ResetPasswordResult::InvalidToken));
        assert_eq!(applied, 1, "exactly one sibling reset commits");
        assert_eq!(rejected, 1, "the losing sibling token is invalidated");
        assert!(!usable(&pool, &token_a).await);
        assert!(!usable(&pool, &token_b).await);

        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(participant.to_uuid())
            .execute(&pool)
            .await
            .unwrap();
    }
}
