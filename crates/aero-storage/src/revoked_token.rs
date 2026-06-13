//! Revoked-token repository (refresh-token revocation / logout).
//!
//! Backs `migrations/0047_revoked_tokens.sql`. Session management exposes the
//! `AuthService`'s existing refresh logic over HTTP and layers explicit
//! revocation on top: `POST /api/auth/logout` records a refresh token here, and
//! `POST /api/auth/refresh` first checks this table and rejects a revoked token
//! with `401`.
//!
//! Only the SHA-256 *hash* of a token is stored ([`hash_token`]) — the plaintext
//! never reaches storage (mirrors `scim` / `webhook` / `pat` token handling), so
//! the table is useless if it leaks. Purely additive: a NEW [`RevokedTokenRepo`];
//! no existing repo is touched. The hashing is a pure free function so it
//! unit-tests offline, keeping testable logic separate from live SQL.

use aero_common::ParticipantId;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

/// Hash a token to the value stored in `revoked_tokens.token_hash`.
///
/// SHA-256 hex. A refresh token is a long random secret (a signed JWT), so a fast
/// cryptographic digest is the right primitive: it makes the stored value useless
/// if the table leaks, while keeping the per-refresh revocation check a single
/// indexed primary-key lookup. Pure, so it is unit-tested without a DB. Mirrors
/// [`scim::hash_token`](crate::scim) / [`webhook::hash_token`](crate::webhook).
#[must_use]
pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut s = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(s, "{byte:02x}");
    }
    s
}

/// Repository over the `revoked_tokens` table (refresh-token revocation).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`RevokedTokenRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct RevokedTokenRepo {
    pool: PgPool,
}

impl RevokedTokenRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record `token_hash` as revoked, attributing it to `participant` when known.
    /// Idempotent: a second revoke of the same hash is a no-op (`ON CONFLICT DO
    /// NOTHING`), so logout is safe to retry. The caller is responsible for
    /// hashing the plaintext token via [`hash_token`] first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn revoke(
        &self,
        token_hash: &str,
        participant: Option<ParticipantId>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO revoked_tokens (token_hash, participant_id)
               VALUES ($1, $2)
               ON CONFLICT (token_hash) DO NOTHING",
        )
        .bind(token_hash)
        .bind(participant.map(|p| p.to_uuid()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Whether `token_hash` has been revoked. The caller is responsible for
    /// hashing the plaintext token via [`hash_token`] first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn is_revoked(&self, token_hash: &str) -> Result<bool, sqlx::Error> {
        let row: Option<(i32,)> =
            sqlx::query_as("SELECT 1 FROM revoked_tokens WHERE token_hash = $1")
                .bind(token_hash)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.is_some())
    }

    /// When `token_hash` was revoked, or `None` if it has never been revoked.
    /// Used by the refresh endpoint to distinguish a benign lost-response retry
    /// (revoked just now) from a token-theft replay (revoked long ago). The caller
    /// hashes the plaintext token via [`hash_token`] first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn revoked_at(
        &self,
        token_hash: &str,
    ) -> Result<Option<time::OffsetDateTime>, sqlx::Error> {
        let row: Option<(time::OffsetDateTime,)> =
            sqlx::query_as("SELECT revoked_at FROM revoked_tokens WHERE token_hash = $1")
                .bind(token_hash)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|r| r.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure hashing: a fixed SHA-256 vector, stable, sensitive to input, and a
    /// 64-char lowercase-hex digest (verified via `sha256sum`).
    #[test]
    fn hash_token_is_deterministic_hex_sha256() {
        // SHA-256("aero") — a fixed, well-known vector (verified via `sha256sum`).
        assert_eq!(
            hash_token("aero"),
            "101a07e5f182ba079644cac91eff0a8586945de26c519ad0736147a74094e275"
        );
        assert_eq!(hash_token("x"), hash_token("x"));
        assert_ne!(hash_token("x"), hash_token("y"));
        assert_eq!(hash_token("anything").len(), 64);
        assert!(hash_token("anything").bytes().all(|b| b.is_ascii_hexdigit()));
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored revoked_token
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

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn revoke_then_is_revoked_and_unknown_is_false() {
        let p = pool();
        let repo = RevokedTokenRepo::new(p.clone());
        let owner = ParticipantId::new();
        // A unique per-run hash so reruns stay self-contained.
        let token = format!("revoked-token-test-{owner}");
        let h = hash_token(&token);

        // Unknown hash is not revoked.
        assert!(
            !repo.is_revoked(&h).await.unwrap(),
            "unknown hash is not revoked"
        );

        // Revoke → now revoked.
        repo.revoke(&h, Some(owner)).await.unwrap();
        assert!(repo.is_revoked(&h).await.unwrap(), "revoked after revoke");

        // Revoke is idempotent (ON CONFLICT DO NOTHING) — a second call is a no-op.
        repo.revoke(&h, Some(owner)).await.unwrap();
        assert!(repo.is_revoked(&h).await.unwrap(), "still revoked");

        // A different, never-revoked hash stays false.
        assert!(
            !repo.is_revoked(&hash_token("never-revoked")).await.unwrap(),
            "a different hash is not revoked"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM revoked_tokens WHERE token_hash = $1")
            .bind(&h)
            .execute(&p)
            .await
            .ok();
    }

    /// `revoked_at` returns `None` for an unknown hash and a recent timestamp for a
    /// freshly-revoked one — the grace-window input the refresh endpoint uses to
    /// distinguish a benign retry from a token-theft replay.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn revoked_at_reports_none_then_a_recent_timestamp() {
        let p = pool();
        let repo = RevokedTokenRepo::new(p.clone());
        let owner = ParticipantId::new();
        let h = hash_token(&format!("revoked-at-test-{owner}"));

        assert!(repo.revoked_at(&h).await.unwrap().is_none(), "unknown hash has no revoked_at");

        repo.revoke(&h, Some(owner)).await.unwrap();
        let at = repo.revoked_at(&h).await.unwrap().expect("revoked_at present after revoke");
        let age = time::OffsetDateTime::now_utc() - at;
        assert!(
            age >= time::Duration::ZERO && age < time::Duration::minutes(5),
            "revoked_at is a recent timestamp (age = {age})",
        );

        sqlx::query("DELETE FROM revoked_tokens WHERE token_hash = $1").bind(&h).execute(&p).await.ok();
    }
}
