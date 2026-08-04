//! Password-reset token repository.
//!
//! Backs `migrations/0066_password_reset.sql`. Supports the forgot-password /
//! reset-password flow: `create()` records a hashed token with a TTL; `consume()`
//! atomically marks it used and returns the owner's `ParticipantId` so the caller
//! can update the password in one round-trip.
//!
//! Only the SHA-256 hash of the plaintext token is stored — mirrors the
//! `revoked_tokens` / `auth_sessions` / PAT pattern so the table is useless if it
//! leaks. Tokens are single-use (`used_at` stamped on first consume) and
//! expire (`expires_at` checked in the same query), so a replay or a late arrival
//! both return `None`.

use std::time::Duration;

use aero_common::ParticipantId;
use sqlx::PgPool;

/// Default token lifetime: 1 hour. Enough for the user to check their inbox; not
/// so long that a leaked reset link stays dangerous.
pub const DEFAULT_TTL: Duration = Duration::from_secs(3600);

/// Repository over the `password_reset_tokens` table.
///
/// Cheap to clone — wraps a [`PgPool`] (itself an `Arc` internally).
#[derive(Clone)]
pub struct PasswordResetRepo {
    pool: PgPool,
}

impl PasswordResetRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Store a hashed reset token for `participant`, expiring after `ttl`.
    ///
    /// The plaintext token is never stored here — the caller is responsible for
    /// generating it (e.g. via `generate_token()`) and hashing it with
    /// [`hash_reset_token`] before passing it in. The raw token is what the
    /// reset email delivers to the user.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn create(
        &self,
        participant: ParticipantId,
        token_hash: &str,
        ttl: Duration,
    ) -> Result<(), sqlx::Error> {
        let expires_at = time::OffsetDateTime::now_utc()
            + time::Duration::seconds(i64::try_from(ttl.as_secs()).unwrap_or(3600));
        sqlx::query(
            "INSERT INTO password_reset_tokens (token_hash, participant_id, expires_at)
             VALUES ($1, $2, $3)",
        )
        .bind(token_hash)
        .bind(participant.to_uuid())
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Atomically consume a reset token and return the owner's `ParticipantId`.
    ///
    /// Returns `None` when the token is unknown, already used (`used_at IS NOT
    /// NULL`), or expired (`expires_at <= now()`). On success the row's `used_at`
    /// is stamped so replays return `None`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn consume(&self, token_hash: &str) -> Result<Option<ParticipantId>, sqlx::Error> {
        let row: Option<(uuid::Uuid,)> = sqlx::query_as(
            "UPDATE password_reset_tokens
                SET used_at = now()
              WHERE token_hash = $1
                AND used_at IS NULL
                AND expires_at > now()
            RETURNING participant_id",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(uid,)| ParticipantId::from_uuid(uid)))
    }

    /// Resolve the owner of a currently usable token without consuming it.
    ///
    /// The reset handler uses this read to perform the expensive Argon2 history
    /// checks before entering the final atomic credential-rotation transaction.
    /// The transaction rechecks and locks the token, so this is not treated as an
    /// authorization decision on its own.
    pub async fn valid_owner(
        &self,
        token_hash: &str,
    ) -> Result<Option<ParticipantId>, sqlx::Error> {
        let row: Option<(uuid::Uuid,)> = sqlx::query_as(
            r"SELECT participant_id
                FROM password_reset_tokens
               WHERE token_hash = $1
                 AND used_at IS NULL
                 AND expires_at > now()",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(uid,)| ParticipantId::from_uuid(uid)))
    }

    /// Delete expired or consumed token history older than `cutoff`.
    ///
    /// Rows that may still authorize a reset (`used_at IS NULL` and
    /// `expires_at >= cutoff`) are never removed by this sweep.
    pub async fn sweep_terminal_before(
        &self,
        cutoff: time::OffsetDateTime,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r"DELETE FROM password_reset_tokens
                WHERE expires_at < $1
                   OR (used_at IS NOT NULL AND used_at < $1)",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

/// Generate a cryptographically random 32-byte reset token as a hex string.
///
/// The returned value is what gets embedded in the reset link (the "plaintext
/// token"). It must be hashed with [`hash_reset_token`] before storage.
#[must_use]
pub fn generate_token() -> String {
    use rand::RngCore as _;
    use std::fmt::Write as _;
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    let mut s = String::with_capacity(buf.len() * 2);
    for b in buf {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Hash a plaintext reset token to the value stored in `password_reset_tokens`.
///
/// SHA-256 hex, mirrors [`crate::revoked_token::hash_token`].
#[must_use]
pub fn hash_reset_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_token_is_64_hex_chars() {
        let t = generate_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_token_is_different_each_call() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b, "two successive tokens must differ");
    }

    #[test]
    fn hash_reset_token_is_deterministic() {
        let h1 = hash_reset_token("secret");
        let h2 = hash_reset_token("secret");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn hash_reset_token_differs_for_different_inputs() {
        assert_ne!(hash_reset_token("a"), hash_reset_token("b"));
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL with migrations applied"]
    async fn retention_sweeps_only_old_terminal_tokens() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let participant = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(participant.to_uuid())
            .bind(format!("password-reset-retention-{participant}"))
            .execute(&pool)
            .await
            .unwrap();
        let repo = PasswordResetRepo::new(pool.clone());
        let now = time::OffsetDateTime::now_utc();
        let cutoff = now - time::Duration::days(7);
        for (hash, expires_at, used_at) in [
            (
                format!("expired-old-{participant}"),
                now - time::Duration::days(30),
                None,
            ),
            (
                format!("used-old-{participant}"),
                now + time::Duration::hours(1),
                Some(now - time::Duration::days(30)),
            ),
            (
                format!("expired-recent-{participant}"),
                now - time::Duration::hours(1),
                None,
            ),
            (
                format!("active-{participant}"),
                now + time::Duration::hours(1),
                None,
            ),
        ] {
            sqlx::query(
                r"INSERT INTO password_reset_tokens
                      (token_hash, participant_id, expires_at, used_at)
                   VALUES ($1, $2, $3, $4)",
            )
            .bind(hash)
            .bind(participant.to_uuid())
            .bind(expires_at)
            .bind(used_at)
            .execute(&pool)
            .await
            .unwrap();
        }

        assert_eq!(repo.sweep_terminal_before(cutoff).await.unwrap(), 2);
        let remaining: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM password_reset_tokens WHERE participant_id = $1",
        )
        .bind(participant.to_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(remaining, 2);
    }
}
