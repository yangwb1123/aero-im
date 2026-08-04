//! Push token registry (ROADMAP 方向二 — mobile push).
//!
//! Backs `migrations/0067_push_tokens.sql`. Stores FCM and APNs device tokens
//! per participant so the notification dispatch layer can push alerts to offline
//! mobile clients.
//!
//! ## Design
//!
//! The `(platform, token)` pair is globally unique: when a device re-registers
//! after an account switch, [`PushTokenRepo::register`] upserts the row and
//! moves the token to the new owner — no orphan rows accumulate. Tokens are
//! cascade-deleted when the owning participant is deleted (GDPR compliance).

use aero_common::ParticipantId;
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// Maximum encoded size accepted for an FCM/APNs token.
///
/// Real provider tokens are far smaller; the ceiling also stays below
/// `PostgreSQL`'s btree entry limit for the `(platform, token)` unique index.
pub const MAX_PUSH_TOKEN_BYTES: usize = 2_048;

/// Maximum number of registered devices retained for one participant.
pub const MAX_PUSH_TOKENS_PER_PARTICIPANT: i64 = 32;

/// A registration failure that callers can map without parsing database text.
#[derive(Debug, thiserror::Error)]
pub enum PushTokenRegistrationError {
    #[error("push token must be 1..={MAX_PUSH_TOKEN_BYTES} bytes")]
    InvalidToken,
    #[error("push token quota exceeded")]
    QuotaExceeded,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// One registered push device for a participant.
#[derive(Debug, Clone, Serialize)]
pub struct PushToken {
    pub id: Uuid,
    pub participant_id: ParticipantId,
    /// `"fcm"` or `"apns"`.
    pub platform: String,
    /// The raw device token string as issued by FCM/APNs.
    pub token: String,
    #[serde(with = "time::serde::rfc3339")]
    pub registered_at: time::OffsetDateTime,
}

/// Valid push platforms. Validated at the HTTP layer; stored as TEXT in the DB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushPlatform {
    Fcm,
    Apns,
}

impl PushPlatform {
    /// Parse a user-supplied string into a known platform.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fcm" => Some(Self::Fcm),
            "apns" => Some(Self::Apns),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fcm => "fcm",
            Self::Apns => "apns",
        }
    }
}

#[derive(Clone)]
pub struct PushTokenRepo {
    pool: PgPool,
}

impl PushTokenRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Register (or re-assign) a push token for `participant`.
    ///
    /// If the `(platform, token)` pair already exists for a *different*
    /// participant (device changed hands), the row is moved to `participant`
    /// and `registered_at` is updated. Idempotent for the same owner.
    pub async fn register(
        &self,
        participant: ParticipantId,
        platform: PushPlatform,
        token: &str,
    ) -> Result<PushToken, PushTokenRegistrationError> {
        if token.is_empty() || token.len() > MAX_PUSH_TOKEN_BYTES {
            return Err(PushTokenRegistrationError::InvalidToken);
        }

        let id = Uuid::new_v4();
        let now = time::OffsetDateTime::now_utc();
        let mut tx = self.pool.begin().await?;

        // Serialize the count-check + upsert for this target participant. A hash
        // collision only causes harmless extra contention. Locking the existing
        // token row below also keeps concurrent cross-account transfers ordered.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("aero:push-token:{}", participant.to_uuid()))
            .execute(&mut *tx)
            .await?;

        let current_owner = sqlx::query_scalar::<_, Uuid>(
            r"SELECT participant_id
                FROM push_tokens
               WHERE platform = $1 AND token = $2
               FOR UPDATE",
        )
        .bind(platform.as_str())
        .bind(token)
        .fetch_optional(&mut *tx)
        .await?;

        // Refreshing a token already owned by this participant consumes no new
        // quota. A transfer/new token must reserve one of the bounded slots.
        if current_owner != Some(participant.to_uuid()) {
            let count = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM push_tokens WHERE participant_id = $1",
            )
            .bind(participant.to_uuid())
            .fetch_one(&mut *tx)
            .await?;
            if count >= MAX_PUSH_TOKENS_PER_PARTICIPANT {
                tx.commit().await?;
                return Err(PushTokenRegistrationError::QuotaExceeded);
            }
        }

        let row = sqlx::query_as::<_, PushTokenRow>(
            r"INSERT INTO push_tokens (id, participant_id, platform, token, registered_at)
               VALUES ($1, $2, $3, $4, $5)
               ON CONFLICT (platform, token) DO UPDATE
                 SET participant_id = EXCLUDED.participant_id,
                     registered_at  = EXCLUDED.registered_at
               RETURNING id, participant_id, platform, token, registered_at",
        )
        .bind(id)
        .bind(participant.to_uuid())
        .bind(platform.as_str())
        .bind(token)
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(PushToken::from(row))
    }

    /// Unregister a specific token for `participant`. No-op if the token
    /// belongs to someone else or does not exist (owner-scoped delete).
    pub async fn unregister(
        &self,
        participant: ParticipantId,
        token: &str,
    ) -> Result<bool, sqlx::Error> {
        let res = sqlx::query(r"DELETE FROM push_tokens WHERE participant_id = $1 AND token = $2")
            .bind(participant.to_uuid())
            .bind(token)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// All push tokens for `participant`, newest first.
    pub async fn list_for_participant(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<PushToken>, sqlx::Error> {
        let rows = sqlx::query_as::<_, PushTokenRow>(
            r"SELECT id, participant_id, platform, token, registered_at
               FROM push_tokens
               WHERE participant_id = $1
               ORDER BY registered_at DESC
               LIMIT $2",
        )
        .bind(participant.to_uuid())
        .bind(MAX_PUSH_TOKENS_PER_PARTICIPANT)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(PushToken::from).collect())
    }

    /// Batch-fetch push tokens for multiple participants. Used by the push
    /// dispatch layer to look up all offline device tokens for a notification.
    /// Ordered by `registered_at` DESC within each participant, so the
    /// most-recently-registered device sorts first.
    pub async fn tokens_for_participants(
        &self,
        participants: &[ParticipantId],
    ) -> Result<Vec<PushToken>, sqlx::Error> {
        if participants.is_empty() {
            return Ok(Vec::new());
        }
        let uuids: Vec<Uuid> = participants.iter().map(ParticipantId::to_uuid).collect();
        let rows = sqlx::query_as::<_, PushTokenRow>(
            r"SELECT id, participant_id, platform, token, registered_at
                FROM (
                    SELECT id, participant_id, platform, token, registered_at,
                           ROW_NUMBER() OVER (
                               PARTITION BY participant_id
                               ORDER BY registered_at DESC
                           ) AS participant_rank
                      FROM push_tokens
                     WHERE participant_id = ANY($1)
                ) AS ranked
               WHERE participant_rank <= $2
               ORDER BY participant_id, registered_at DESC",
        )
        .bind(&uuids)
        .bind(MAX_PUSH_TOKENS_PER_PARTICIPANT)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(PushToken::from).collect())
    }
}

#[derive(sqlx::FromRow)]
struct PushTokenRow {
    id: Uuid,
    participant_id: Uuid,
    platform: String,
    token: String,
    registered_at: time::OffsetDateTime,
}

impl From<PushTokenRow> for PushToken {
    fn from(r: PushTokenRow) -> Self {
        Self {
            id: r.id,
            participant_id: ParticipantId::from_uuid(r.participant_id),
            platform: r.platform,
            token: r.token,
            registered_at: r.registered_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_platform_parse_round_trips() {
        assert_eq!(PushPlatform::parse("fcm"), Some(PushPlatform::Fcm));
        assert_eq!(PushPlatform::parse("apns"), Some(PushPlatform::Apns));
        assert_eq!(PushPlatform::parse("gcm"), None);
        assert_eq!(PushPlatform::parse(""), None);
        assert_eq!(PushPlatform::Fcm.as_str(), "fcm");
        assert_eq!(PushPlatform::Apns.as_str(), "apns");
    }

    #[test]
    fn token_byte_limit_is_utf8_aware() {
        assert!("a".repeat(MAX_PUSH_TOKEN_BYTES).len() <= MAX_PUSH_TOKEN_BYTES);
        assert!("界".repeat(MAX_PUSH_TOKEN_BYTES / 3 + 1).len() > MAX_PUSH_TOKEN_BYTES);
    }
}
