//! Durable post-commit work attached to message mutations.

use aero_common::MessageId;
use sqlx::{PgPool, Postgres, Transaction};
use time::{Duration, OffsetDateTime};
use ulid::Ulid;
use uuid::Uuid;

use crate::ai_job::{priority_for, AiJobKind};

const MAX_CLAIM: i64 = 256;
const MAX_LEASE_SECONDS: i64 = 86_400;
const MAX_ERROR_CHARS: usize = 2_048;
const MAX_BACKOFF_SECONDS: i64 = 300;

const COLUMNS: &str = "id, message_id, mutation_version, kind, attempts, available_at, \
                       claimed_at, completed_at, last_error, created_at";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageSideEffectKind {
    Notifications,
    Embed,
    Moderate,
}

impl MessageSideEffectKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Notifications => "notifications",
            Self::Embed => "embed",
            Self::Moderate => "moderate",
        }
    }
}

impl TryFrom<&str> for MessageSideEffectKind {
    type Error = sqlx::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "notifications" => Ok(Self::Notifications),
            "embed" => Ok(Self::Embed),
            "moderate" => Ok(Self::Moderate),
            other => Err(sqlx::Error::Decode(
                format!("unknown message side-effect kind {other:?}").into(),
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MessageSideEffectJob {
    pub id: Uuid,
    pub message_id: MessageId,
    pub mutation_version: i32,
    pub kind: MessageSideEffectKind,
    pub attempts: i32,
    pub available_at: OffsetDateTime,
    pub claimed_at: Option<OffsetDateTime>,
    pub completed_at: Option<OffsetDateTime>,
    pub last_error: Option<String>,
    pub created_at: OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct DbRow {
    id: Uuid,
    message_id: Uuid,
    mutation_version: i32,
    kind: String,
    attempts: i32,
    available_at: OffsetDateTime,
    claimed_at: Option<OffsetDateTime>,
    completed_at: Option<OffsetDateTime>,
    last_error: Option<String>,
    created_at: OffsetDateTime,
}

impl TryFrom<DbRow> for MessageSideEffectJob {
    type Error = sqlx::Error;

    fn try_from(row: DbRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            message_id: MessageId::from_uuid(row.message_id),
            mutation_version: row.mutation_version,
            kind: MessageSideEffectKind::try_from(row.kind.as_str())?,
            attempts: row.attempts,
            available_at: row.available_at,
            claimed_at: row.claimed_at,
            completed_at: row.completed_at,
            last_error: row.last_error,
            created_at: row.created_at,
        })
    }
}

#[derive(Clone)]
pub struct MessageSideEffectRepo {
    pool: PgPool,
}

impl MessageSideEffectRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Append mutation-scoped jobs on the caller's business transaction.
    pub(crate) async fn insert_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        message_id: MessageId,
        mutation_version: i32,
        kinds: &[MessageSideEffectKind],
    ) -> Result<(), sqlx::Error> {
        for kind in kinds {
            sqlx::query(
                r"INSERT INTO message_side_effect_jobs
                     (id, message_id, mutation_version, kind)
                   VALUES ($1, $2, $3, $4)
                   ON CONFLICT (message_id, mutation_version, kind) DO NOTHING",
            )
            .bind(Uuid::new_v4())
            .bind(message_id.to_uuid())
            .bind(mutation_version)
            .bind(kind.as_str())
            .execute(&mut **tx)
            .await?;
        }
        Ok(())
    }

    pub async fn claim_due(
        &self,
        now: OffsetDateTime,
        lease: Duration,
        limit: i64,
    ) -> Result<Vec<MessageSideEffectJob>, sqlx::Error> {
        self.claim_matching(now, lease, limit, None).await
    }

    pub async fn claim_for_message(
        &self,
        message_id: MessageId,
        now: OffsetDateTime,
        lease: Duration,
    ) -> Result<Vec<MessageSideEffectJob>, sqlx::Error> {
        self.claim_matching(now, lease, 16, Some(message_id)).await
    }

    async fn claim_matching(
        &self,
        now: OffsetDateTime,
        lease: Duration,
        limit: i64,
        message_id: Option<MessageId>,
    ) -> Result<Vec<MessageSideEffectJob>, sqlx::Error> {
        let stale_before = lease_stale_before(now, lease);
        let limit = limit.clamp(1, MAX_CLAIM);
        let sql = format!(
            r"WITH claimable AS (
                  SELECT id AS claimed_id
                    FROM message_side_effect_jobs
                   WHERE completed_at IS NULL
                     AND available_at <= $1
                     AND (claimed_at IS NULL OR claimed_at <= $2)
                     AND ($4::uuid IS NULL OR message_id = $4)
                   ORDER BY available_at, created_at, id
                   FOR UPDATE SKIP LOCKED
                   LIMIT $3
              )
              UPDATE message_side_effect_jobs AS job
                SET claimed_at = $1,
                     attempts = job.attempts + 1
                FROM claimable
               WHERE job.id = claimable.claimed_id
           RETURNING {COLUMNS}"
        );
        let rows = sqlx::query_as::<_, DbRow>(&sql)
            .bind(now)
            .bind(stale_before)
            .bind(limit)
            .bind(message_id.map(|message_id| message_id.to_uuid()))
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// Make every unfinished, currently-unowned job immediately due. Expired
    /// claims are released; a live worker's lease is never stolen.
    pub async fn nudge_message(
        &self,
        message_id: MessageId,
        now: OffsetDateTime,
        lease: Duration,
    ) -> Result<u64, sqlx::Error> {
        let stale_before = lease_stale_before(now, lease);
        let result = sqlx::query(
            r"UPDATE message_side_effect_jobs
                  SET available_at = LEAST(available_at, $2),
                      claimed_at = NULL
                WHERE message_id = $1
                  AND completed_at IS NULL
                  AND (claimed_at IS NULL OR claimed_at <= $3)",
        )
        .bind(message_id.to_uuid())
        .bind(now)
        .bind(stale_before)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn complete(
        &self,
        id: Uuid,
        attempts: i32,
        now: OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE message_side_effect_jobs
                  SET completed_at = $3,
                      claimed_at = NULL,
                      last_error = NULL
                WHERE id = $1
                  AND attempts = $2
                  AND claimed_at IS NOT NULL
                  AND completed_at IS NULL",
        )
        .bind(id)
        .bind(attempts)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Complete a claimed job on a caller-owned transaction.
    ///
    /// Notification projections use this after writing their inbox/outbox or
    /// bundle rows so the projection and source-ledger completion commit
    /// together. `attempts` fences an expired worker lease.
    pub(crate) async fn complete_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
        attempts: i32,
        now: OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE message_side_effect_jobs
                  SET completed_at = $3,
                      claimed_at = NULL,
                      last_error = NULL
                WHERE id = $1
                  AND attempts = $2
                  AND claimed_at IS NOT NULL
                  AND completed_at IS NULL",
        )
        .bind(id)
        .bind(attempts)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Atomically enqueue one AI job and complete the source side-effect job.
    /// A crash can therefore produce neither row or both rows, never a duplicate
    /// AI charge after the source job is reclaimed.
    #[allow(clippy::too_many_arguments)]
    pub async fn complete_with_ai_job(
        &self,
        source_id: Uuid,
        attempts: i32,
        kind: AiJobKind,
        target_id: MessageId,
        workspace_id: Option<Uuid>,
        payload: serde_json::Value,
        now: OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let completed = Self::complete_in_tx(&mut tx, source_id, attempts, now).await?;
        if !completed {
            tx.rollback().await?;
            return Ok(false);
        }

        let ai_id = Ulid::new();
        sqlx::query(
            r"INSERT INTO ai_jobs
                 (id, kind, target_id, workspace_id, status, payload, priority)
               VALUES ($1, $2, $3, $4, 'queued', $5, $6)",
        )
        .bind(Uuid::from_u128(ai_id.0))
        .bind(ai_kind_str(kind))
        .bind(target_id.to_uuid())
        .bind(workspace_id)
        .bind(payload)
        .bind(priority_for(kind))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn mark_failed(
        &self,
        id: Uuid,
        attempts: i32,
        now: OffsetDateTime,
        error: &str,
    ) -> Result<bool, sqlx::Error> {
        let available_at = now + side_effect_backoff(attempts);
        let error: String = error.chars().take(MAX_ERROR_CHARS).collect();
        let result = sqlx::query(
            r"UPDATE message_side_effect_jobs
                  SET available_at = $3,
                      claimed_at = NULL,
                      last_error = $4
                WHERE id = $1
                  AND attempts = $2
                  AND claimed_at IS NOT NULL
                  AND completed_at IS NULL",
        )
        .bind(id)
        .bind(attempts)
        .bind(available_at)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Purge only completed ledger rows. Pending/failed work is never aged out.
    pub async fn sweep_completed_before(&self, cutoff: OffsetDateTime) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM message_side_effect_jobs
              WHERE completed_at IS NOT NULL AND completed_at < $1",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

fn ai_kind_str(kind: AiJobKind) -> &'static str {
    match kind {
        AiJobKind::Embed => "embed",
        AiJobKind::Summarize => "summarize",
        AiJobKind::Moderate => "moderate",
        AiJobKind::Answer => "answer",
    }
}

#[must_use]
pub fn side_effect_backoff(attempts: i32) -> Duration {
    let shift = u32::try_from(attempts.saturating_sub(1).clamp(0, 62)).unwrap_or(0);
    let raw = 1_i64.checked_shl(shift).unwrap_or(i64::MAX);
    Duration::seconds(raw.min(MAX_BACKOFF_SECONDS))
}

fn lease_stale_before(now: OffsetDateTime, lease: Duration) -> OffsetDateTime {
    now - Duration::seconds(lease.whole_seconds().clamp(1, MAX_LEASE_SECONDS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_exponential_and_bounded() {
        assert_eq!(side_effect_backoff(-1), Duration::seconds(1));
        assert_eq!(side_effect_backoff(1), Duration::seconds(1));
        assert_eq!(side_effect_backoff(2), Duration::seconds(2));
        assert_eq!(side_effect_backoff(10), Duration::seconds(300));
        assert_eq!(side_effect_backoff(i32::MAX), Duration::seconds(300));
    }

    #[test]
    fn kinds_round_trip_schema_values() {
        for kind in [
            MessageSideEffectKind::Notifications,
            MessageSideEffectKind::Embed,
            MessageSideEffectKind::Moderate,
        ] {
            assert_eq!(
                MessageSideEffectKind::try_from(kind.as_str()).unwrap(),
                kind
            );
        }
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::message::{MessageRepo, NewMessage};
    use crate::{ParticipantRepo, RoomRepo, WorkspaceRepo};
    use aero_common::{Block, RoomKind, WorkspaceId, WorkspaceRole};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_lazy(&url)
            .expect("valid DATABASE_URL")
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migrations through 0165"]
    async fn claims_retry_atomic_ai_enqueue_and_completed_retention() {
        let pool = pool();
        let unique = Uuid::new_v4();
        let participant = ParticipantRepo::new(pool.clone())
            .create_human(crate::participant::NewHuman {
                email: format!("message-effects-{unique}@example.test"),
                display_name: format!("message-effects-{unique}"),
                password_hash: "test".into(),
            })
            .await
            .unwrap();
        WorkspaceRepo::new(pool.clone())
            .add_member(
                WorkspaceId::from_uuid(uuid::Uuid::nil()),
                participant.id,
                WorkspaceRole::Member,
            )
            .await
            .unwrap();
        let room = RoomRepo::new(pool.clone())
            .create(
                RoomKind::Group,
                Some("message-effects".into()),
                participant.id,
            )
            .await
            .unwrap();
        let inserted = MessageRepo::new(pool.clone())
            .insert_outboxed(
                NewMessage {
                    room_id: room.id,
                    sender_id: participant.id,
                    blocks: vec![Block::text("durable effects")],
                    reply_to: None,
                    metadata: serde_json::Value::Null,
                    expires_at: None,
                },
                None,
                Vec::new(),
                None,
            )
            .await
            .unwrap();
        let message = inserted.message().id;
        let repo = MessageSideEffectRepo::new(pool.clone());
        let now = OffsetDateTime::now_utc();
        let lease = Duration::seconds(30);
        let claimed = repo.claim_for_message(message, now, lease).await.unwrap();
        assert_eq!(claimed.len(), 3);
        assert!(repo
            .claim_for_message(message, now + Duration::seconds(1), lease)
            .await
            .unwrap()
            .is_empty());

        let notification = claimed
            .iter()
            .find(|job| job.kind == MessageSideEffectKind::Notifications)
            .unwrap();
        assert!(repo
            .mark_failed(notification.id, notification.attempts, now, "transient")
            .await
            .unwrap());
        assert!(repo
            .claim_for_message(message, now, lease)
            .await
            .unwrap()
            .is_empty());
        let retry_at = now + side_effect_backoff(notification.attempts);
        let retried = repo
            .claim_for_message(message, retry_at, lease)
            .await
            .unwrap();
        assert_eq!(retried.len(), 1);
        assert_eq!(retried[0].kind, MessageSideEffectKind::Notifications);
        assert!(repo
            .complete(retried[0].id, retried[0].attempts, retry_at)
            .await
            .unwrap());

        let workspace = RoomRepo::new(pool.clone())
            .room_workspace(room.id)
            .await
            .unwrap()
            .map(|workspace| workspace.to_uuid());
        for job in claimed
            .iter()
            .filter(|job| job.kind != MessageSideEffectKind::Notifications)
        {
            let kind = match job.kind {
                MessageSideEffectKind::Embed => AiJobKind::Embed,
                MessageSideEffectKind::Moderate => AiJobKind::Moderate,
                MessageSideEffectKind::Notifications => unreachable!(),
            };
            assert!(repo
                .complete_with_ai_job(
                    job.id,
                    job.attempts,
                    kind,
                    message,
                    workspace,
                    serde_json::json!({"source": "test"}),
                    retry_at,
                )
                .await
                .unwrap());
            assert!(
                !repo
                    .complete_with_ai_job(
                        job.id,
                        job.attempts,
                        kind,
                        message,
                        workspace,
                        serde_json::json!({"source": "duplicate"}),
                        retry_at,
                    )
                    .await
                    .unwrap(),
                "completed source cannot enqueue a second AI job"
            );
        }
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM ai_jobs WHERE target_id = $1 AND kind IN ('embed','moderate')",
            )
            .bind(message.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap(),
            2
        );
        let completed_ids: Vec<Uuid> = claimed.iter().map(|job| job.id).collect();
        repo.sweep_completed_before(retry_at + Duration::seconds(1))
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*)::bigint
                   FROM message_side_effect_jobs
                  WHERE id = ANY($1)",
            )
            .bind(&completed_ids)
            .fetch_one(&pool)
            .await
            .unwrap(),
            0,
            "all completed jobs seeded by this test are swept"
        );

        sqlx::query("DELETE FROM ai_jobs WHERE target_id = $1")
            .bind(message.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM event_outbox WHERE message_id = $1")
            .bind(message.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(message.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM room_members WHERE room_id = $1")
            .bind(room.id.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.id.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(participant.id.to_uuid())
            .execute(&pool)
            .await
            .ok();
    }
}
