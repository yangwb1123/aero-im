//! Password reuse history (ROADMAP 方向五 — enterprise hardening).
//!
//! Backs `migrations/0074_password_history.sql`. Stores recently-replaced argon2
//! hashes per participant so the change-password / reset-password flows can
//! reject reuse of a recent password. The auth layer verifies a candidate
//! plaintext against each stored hash; this repo only persists + prunes them.

use aero_common::ParticipantId;
use sqlx::PgPool;

/// How many prior passwords to retain (and check against) per participant.
pub const HISTORY_DEPTH: i64 = 5;

#[derive(Clone)]
pub struct PasswordHistoryRepo {
    pool: PgPool,
}

impl PasswordHistoryRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record a (replaced) password hash for `participant`, then prune to the
    /// most recent [`HISTORY_DEPTH`] entries so the table can't grow unbounded.
    pub async fn record(&self, participant: ParticipantId, password_hash: &str) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO password_history (participant_id, password_hash) VALUES ($1, $2)")
            .bind(participant.to_uuid())
            .bind(password_hash)
            .execute(&mut *tx)
            .await?;
        // Keep only the newest HISTORY_DEPTH rows for this participant.
        sqlx::query(
            r"DELETE FROM password_history
               WHERE participant_id = $1
                 AND id NOT IN (
                     SELECT id FROM password_history
                      WHERE participant_id = $1
                      ORDER BY created_at DESC
                      LIMIT $2
                 )",
        )
        .bind(participant.to_uuid())
        .bind(HISTORY_DEPTH)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The most recent stored password hashes for `participant`, newest first
    /// (at most [`HISTORY_DEPTH`]). The caller argon2-verifies a candidate
    /// against each to detect reuse.
    pub async fn recent(&self, participant: ParticipantId) -> Result<Vec<String>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (String,)>(
            r"SELECT password_hash FROM password_history
               WHERE participant_id = $1
               ORDER BY created_at DESC
               LIMIT $2",
        )
        .bind(participant.to_uuid())
        .bind(HISTORY_DEPTH)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(h,)| h).collect())
    }
}
