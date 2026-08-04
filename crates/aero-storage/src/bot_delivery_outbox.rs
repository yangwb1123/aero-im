//! Durable webhook delivery queue for bot event subscriptions.

use aero_common::{ParticipantId, RoomId};
use sqlx::{PgPool, Postgres, Transaction};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

const MAX_CLAIM: i64 = 256;
const MAX_LEASE_SECONDS: i64 = 86_400;
const BASE_BACKOFF_SECONDS: i64 = 30;
const MAX_BACKOFF_SECONDS: i64 = 3_600;
const MAX_ERROR_CHARS: usize = 2_048;

/// Maximum HTTP attempts before a bot delivery is parked in the DLQ.
pub const BOT_DELIVERY_MAX_ATTEMPTS: i32 = 6;

const COLUMNS: &str = "id, subscription_id, bot_id, event_id, event_type, room_id, \
                       request_body, status, attempts, available_at, claimed_at, \
                       claim_token, completed_at, last_http_status, last_error, \
                       created_at, updated_at";

/// Rows removed by one atomic bot-delivery retention sweep.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BotDeliveryRetentionSweep {
    /// Successfully delivered durable queue rows.
    pub delivered_outbox: u64,
    /// Expired dead-letter queue rows.
    pub dead_outbox: u64,
    /// Expired per-attempt observability rows.
    pub attempt_history: u64,
}

/// One leased or terminal durable bot-subscription delivery.
#[derive(Debug, Clone)]
pub struct BotDeliveryOutbox {
    pub id: Uuid,
    pub subscription_id: Uuid,
    pub bot_id: ParticipantId,
    /// Stable producer event id, unchanged from the NATS payload.
    pub event_id: Uuid,
    pub event_type: String,
    pub room_id: RoomId,
    /// Exact producer JSON bytes. Retries never reserialize the typed event.
    pub request_body: Vec<u8>,
    pub status: String,
    /// Number of HTTP attempts, including the currently leased attempt.
    pub attempts: i32,
    pub available_at: OffsetDateTime,
    pub claimed_at: Option<OffsetDateTime>,
    /// Unpredictable fencing token for the current lease. Terminal/unclaimed
    /// rows have no token; every reclaim mints a different UUID.
    pub claim_token: Option<Uuid>,
    pub completed_at: Option<OffsetDateTime>,
    pub last_http_status: Option<i32>,
    pub last_error: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct DbRow {
    id: Uuid,
    subscription_id: Uuid,
    bot_id: Uuid,
    event_id: Uuid,
    event_type: String,
    room_id: Uuid,
    request_body: Vec<u8>,
    status: String,
    attempts: i32,
    available_at: OffsetDateTime,
    claimed_at: Option<OffsetDateTime>,
    claim_token: Option<Uuid>,
    completed_at: Option<OffsetDateTime>,
    last_http_status: Option<i32>,
    last_error: Option<String>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl From<DbRow> for BotDeliveryOutbox {
    fn from(row: DbRow) -> Self {
        Self {
            id: row.id,
            subscription_id: row.subscription_id,
            bot_id: ParticipantId::from_uuid(row.bot_id),
            event_id: row.event_id,
            event_type: row.event_type,
            room_id: RoomId::from_uuid(row.room_id),
            request_body: row.request_body,
            status: row.status,
            attempts: row.attempts,
            available_at: row.available_at,
            claimed_at: row.claimed_at,
            claim_token: row.claim_token,
            completed_at: row.completed_at,
            last_http_status: row.last_http_status,
            last_error: row.last_error,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

#[derive(Clone)]
pub struct BotDeliveryOutboxRepo {
    pool: PgPool,
}

impl BotDeliveryOutboxRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Idempotently materialize one subscription/event fan-out.
    ///
    /// `request_body` is retained byte-for-byte, including the producer's
    /// top-level `event_id`. A consumer retry returns the original queue id.
    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue(
        &self,
        subscription_id: Uuid,
        bot_id: ParticipantId,
        event_id: Uuid,
        event_type: &str,
        room_id: RoomId,
        request_body: &[u8],
    ) -> Result<Uuid, sqlx::Error> {
        let id = Uuid::new_v4();
        sqlx::query_scalar(
            r"INSERT INTO bot_subscription_delivery_outbox
                  (id, subscription_id, bot_id, event_id, event_type, room_id,
                   request_body)
               VALUES ($1, $2, $3, $4, $5, $6, $7)
               ON CONFLICT (subscription_id, event_id) DO UPDATE
                   SET event_id = EXCLUDED.event_id
            RETURNING id",
        )
        .bind(id)
        .bind(subscription_id)
        .bind(bot_id.to_uuid())
        .bind(event_id)
        .bind(event_type)
        .bind(room_id.to_uuid())
        .bind(request_body)
        .fetch_one(&self.pool)
        .await
    }

    /// Insert one fan-out inside a caller-owned transaction.
    ///
    /// Bot dispatch uses this together with
    /// [`ConsumerEventReceiptRepo::complete_in_tx`](crate::ConsumerEventReceiptRepo::complete_in_tx)
    /// so every matching subscription and the consumer receipt commit atomically.
    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        subscription_id: Uuid,
        bot_id: ParticipantId,
        event_id: Uuid,
        event_type: &str,
        room_id: RoomId,
        request_body: &[u8],
    ) -> Result<Uuid, sqlx::Error> {
        let id = Uuid::new_v4();
        sqlx::query_scalar(
            r"INSERT INTO bot_subscription_delivery_outbox
                  (id, subscription_id, bot_id, event_id, event_type, room_id,
                   request_body)
               VALUES ($1, $2, $3, $4, $5, $6, $7)
               ON CONFLICT (subscription_id, event_id) DO UPDATE
                   SET event_id = EXCLUDED.event_id
            RETURNING id",
        )
        .bind(id)
        .bind(subscription_id)
        .bind(bot_id.to_uuid())
        .bind(event_id)
        .bind(event_type)
        .bind(room_id.to_uuid())
        .bind(request_body)
        .fetch_one(&mut **tx)
        .await
    }

    /// Claim due work with a crash-recoverable lease.
    pub async fn claim_due(
        &self,
        now: OffsetDateTime,
        lease: Duration,
        limit: i64,
    ) -> Result<Vec<BotDeliveryOutbox>, sqlx::Error> {
        let stale_before = lease_stale_before(now, lease);
        let limit = limit.clamp(1, MAX_CLAIM);
        let sql = format!(
            r"WITH claimable AS (
                  SELECT id AS claimed_id
                    FROM bot_subscription_delivery_outbox
                   WHERE status IN ('pending', 'failed')
                     AND available_at <= $1
                     AND (claimed_at IS NULL OR claimed_at <= $2)
                   ORDER BY available_at, created_at, id
                   FOR UPDATE SKIP LOCKED
                   LIMIT $3
              )
              UPDATE bot_subscription_delivery_outbox AS delivery
                 SET status = 'pending',
                     claimed_at = $1,
                     claim_token = gen_random_uuid(),
                     attempts = delivery.attempts + 1,
                     updated_at = $1
                FROM claimable
               WHERE delivery.id = claimable.claimed_id
           RETURNING {COLUMNS}"
        );
        let rows = sqlx::query_as::<_, DbRow>(&sql)
            .bind(now)
            .bind(stale_before)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Fence and complete a successful HTTP attempt.
    pub async fn mark_delivered(
        &self,
        id: Uuid,
        claim_token: Uuid,
        now: OffsetDateTime,
        http_status: u16,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE bot_subscription_delivery_outbox
                  SET status = 'delivered',
                      claimed_at = NULL,
                      claim_token = NULL,
                      completed_at = $3,
                      last_http_status = $4,
                      last_error = NULL,
                      updated_at = $3
                WHERE id = $1
                  AND claim_token = $2
                  AND claimed_at IS NOT NULL
                  AND status = 'pending'",
        )
        .bind(id)
        .bind(claim_token)
        .bind(now)
        .bind(i32::from(http_status))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Re-park a failed HTTP attempt, or terminally park it after the cap.
    pub async fn mark_failed(
        &self,
        id: Uuid,
        claim_token: Uuid,
        attempts: i32,
        now: OffsetDateTime,
        http_status: Option<u16>,
        error: &str,
    ) -> Result<Option<&'static str>, sqlx::Error> {
        let dead = is_dead_at(attempts);
        let status = if dead { "dead" } else { "failed" };
        let available_at = if dead {
            now
        } else {
            now + bot_delivery_backoff(attempts)
        };
        let completed_at = dead.then_some(now);
        let error = truncate_error(error);
        let status = sqlx::query_scalar::<_, String>(
            r"UPDATE bot_subscription_delivery_outbox
                  SET status = $3,
                      available_at = $4,
                      claimed_at = NULL,
                      claim_token = NULL,
                      completed_at = $5,
                      last_http_status = $6,
                      last_error = $7,
                      updated_at = $8
                WHERE id = $1
                  AND claim_token = $2
                  AND claimed_at IS NOT NULL
                  AND status = 'pending'
            RETURNING status",
        )
        .bind(id)
        .bind(claim_token)
        .bind(status)
        .bind(available_at)
        .bind(completed_at)
        .bind(http_status.map(i32::from))
        .bind(error)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        Ok(status
            .as_deref()
            .map(|status| if status == "dead" { "dead" } else { "failed" }))
    }

    /// Park a delivery without making an HTTP request (for example, access was
    /// revoked). The attempt fencing still prevents an expired worker from
    /// overwriting a newer claim.
    pub async fn mark_dead(
        &self,
        id: Uuid,
        claim_token: Uuid,
        now: OffsetDateTime,
        error: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE bot_subscription_delivery_outbox
                  SET status = 'dead',
                      claimed_at = NULL,
                      claim_token = NULL,
                      completed_at = $3,
                      last_error = $4,
                      updated_at = $3
                WHERE id = $1
                  AND claim_token = $2
                  AND claimed_at IS NOT NULL
                  AND status = 'pending'",
        )
        .bind(id)
        .bind(claim_token)
        .bind(now)
        .bind(truncate_error(error))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Release a claim that shutdown interrupted before any HTTP attempt.
    pub async fn release_claim(
        &self,
        id: Uuid,
        claim_token: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE bot_subscription_delivery_outbox
                  SET status = 'pending',
                      attempts = GREATEST(attempts - 1, 0),
                      available_at = $3,
                      claimed_at = NULL,
                      claim_token = NULL,
                      updated_at = $3
                WHERE id = $1
                  AND claim_token = $2
                  AND claimed_at IS NOT NULL
                  AND status = 'pending'",
        )
        .bind(id)
        .bind(claim_token)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn get(&self, id: Uuid) -> Result<Option<BotDeliveryOutbox>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM bot_subscription_delivery_outbox WHERE id = $1");
        let row = sqlx::query_as::<_, DbRow>(&sql)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(Into::into))
    }

    pub async fn list_dead(&self, limit: i64) -> Result<Vec<BotDeliveryOutbox>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM bot_subscription_delivery_outbox
              WHERE status = 'dead'
              ORDER BY completed_at DESC
              LIMIT $1"
        );
        let rows = sqlx::query_as::<_, DbRow>(&sql)
            .bind(limit.clamp(1, 500))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Owner-scoped manual DLQ retry.
    ///
    /// The immutable subscription/event identity and raw request body are kept;
    /// terminal/error fields are cleared and attempts reset so the next worker
    /// claim starts a fresh retry budget at attempt 1.
    #[cfg(test)]
    pub async fn requeue_dead(
        &self,
        id: Uuid,
        bot_id: ParticipantId,
        now: OffsetDateTime,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE bot_subscription_delivery_outbox
                  SET status = 'pending',
                      attempts = 0,
                      available_at = $3,
                      claimed_at = NULL,
                      claim_token = NULL,
                      completed_at = NULL,
                      last_http_status = NULL,
                      last_error = NULL,
                      updated_at = $3
                WHERE id = $1
                  AND bot_id = $2
                  AND status = 'dead'",
        )
        .bind(id)
        .bind(bot_id.to_uuid())
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Bound queue growth without discarding retryable work or DLQ evidence.
    pub async fn sweep_delivered_before(&self, cutoff: OffsetDateTime) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r"DELETE FROM bot_subscription_delivery_outbox
                WHERE status = 'delivered'
                  AND completed_at < $1",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Atomically apply queue and observability retention.
    ///
    /// Delivered queue rows use the shorter routine-audit cutoff. Dead queue
    /// rows and `bot_subscription_deliveries` attempt history use the longer
    /// DLQ operations cutoff. The outbox predicates explicitly name only
    /// terminal states, so retryable `pending` and `failed` work is never
    /// eligible regardless of age.
    pub async fn sweep_retention_before(
        &self,
        delivered_cutoff: OffsetDateTime,
        dlq_cutoff: OffsetDateTime,
    ) -> Result<BotDeliveryRetentionSweep, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let delivered_outbox = sqlx::query(
            r"DELETE FROM bot_subscription_delivery_outbox
                WHERE status = 'delivered'
                  AND completed_at < $1",
        )
        .bind(delivered_cutoff)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let dead_outbox = sqlx::query(
            r"DELETE FROM bot_subscription_delivery_outbox
                WHERE status = 'dead'
                  AND completed_at < $1",
        )
        .bind(dlq_cutoff)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let attempt_history = sqlx::query(
            r"DELETE FROM bot_subscription_deliveries
                WHERE created_at < $1",
        )
        .bind(dlq_cutoff)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(BotDeliveryRetentionSweep {
            delivered_outbox,
            dead_outbox,
            attempt_history,
        })
    }
}

#[must_use]
pub fn bot_delivery_backoff(attempts: i32) -> Duration {
    let shift = u32::try_from(attempts.saturating_sub(1).clamp(0, 62)).unwrap_or(0);
    let raw = BASE_BACKOFF_SECONDS.saturating_mul(1_i64.checked_shl(shift).unwrap_or(i64::MAX));
    Duration::seconds(raw.min(MAX_BACKOFF_SECONDS))
}

#[must_use]
pub const fn is_dead_at(attempts: i32) -> bool {
    attempts >= BOT_DELIVERY_MAX_ATTEMPTS
}

fn lease_stale_before(now: OffsetDateTime, lease: Duration) -> OffsetDateTime {
    now - Duration::seconds(lease.whole_seconds().clamp(1, MAX_LEASE_SECONDS))
}

fn truncate_error(error: &str) -> String {
    error.chars().take(MAX_ERROR_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn scoped_room(
        pool: &PgPool,
        owner: ParticipantId,
        label: &str,
    ) -> (aero_common::WorkspaceId, RoomId) {
        let workspace = crate::WorkspaceRepo::new(pool.clone())
            .create(
                format!("{label}-{owner}"),
                format!("{label}-{owner}").to_ascii_lowercase(),
                owner,
            )
            .await
            .unwrap()
            .id;
        let room = crate::RoomRepo::new(pool.clone())
            .create_in_workspace(
                workspace,
                aero_common::RoomKind::Channel,
                Some(format!("{label}-room")),
                owner,
            )
            .await
            .unwrap()
            .id;
        (workspace, room)
    }

    async fn insert_retention_outbox(
        pool: &PgPool,
        subscription: Uuid,
        bot: ParticipantId,
        room: RoomId,
        status: &'static str,
        timestamp: OffsetDateTime,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let completed_at = matches!(status, "delivered" | "dead").then_some(timestamp);
        sqlx::query(
            r"INSERT INTO bot_subscription_delivery_outbox
                  (id, subscription_id, bot_id, event_id, event_type, room_id,
                   request_body, status, attempts, available_at, completed_at,
                   created_at, updated_at)
               VALUES ($1, $2, $3, $4, 'message', $5, $6, $7, 1, $8, $9, $8, $8)",
        )
        .bind(id)
        .bind(subscription)
        .bind(bot.to_uuid())
        .bind(Uuid::new_v4())
        .bind(room.to_uuid())
        .bind(br#"{"kind":"message"}"#.as_slice())
        .bind(status)
        .bind(timestamp)
        .bind(completed_at)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    #[test]
    fn exponential_backoff_is_bounded() {
        assert_eq!(bot_delivery_backoff(1), Duration::seconds(30));
        assert_eq!(bot_delivery_backoff(2), Duration::seconds(60));
        assert_eq!(bot_delivery_backoff(3), Duration::seconds(120));
        assert_eq!(bot_delivery_backoff(i32::MAX), Duration::seconds(3_600));
    }

    #[test]
    fn attempt_cap_is_terminal() {
        assert!(!is_dead_at(BOT_DELIVERY_MAX_ATTEMPTS - 1));
        assert!(is_dead_at(BOT_DELIVERY_MAX_ATTEMPTS));
        assert!(is_dead_at(i32::MAX));
    }

    #[test]
    fn error_truncation_matches_schema_limit() {
        let error = truncate_error(&"错".repeat(MAX_ERROR_CHARS + 20));
        assert_eq!(error.chars().count(), MAX_ERROR_CHARS);
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL with migrations applied"]
    async fn durable_fanout_fencing_retry_dlq_and_retention() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let bots = crate::BotRepo::new(pool.clone());
        let receipts = crate::ConsumerEventReceiptRepo::new(pool.clone());
        let repo = BotDeliveryOutboxRepo::new(pool.clone());

        let owner = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(owner.to_uuid())
            .bind(format!("bot-outbox-owner-{owner}"))
            .execute(&pool)
            .await
            .unwrap();
        let bot = bots
            .create(owner, "durable-delivery-bot", None, None)
            .await
            .unwrap();
        let (workspace, room) = scoped_room(&pool, owner, "durable-delivery").await;
        let filters = serde_json::json!({
            "room_id": room.to_string(),
            "workspace_id": workspace.to_string(),
        });
        let subscription = bots
            .subscribe(
                bot,
                "message",
                Some(&filters),
                Some("https://example.test/hook"),
                Some("durable-test-secret-0123456789abcdef"),
            )
            .await
            .unwrap();
        let event_id = Uuid::new_v4();
        let body = serde_json::to_vec(&serde_json::json!({
            "kind": "message",
            "event_id": event_id,
            "payload": "byte-for-byte"
        }))
        .unwrap();
        let consumer = format!("bot-outbox-test-{}", Uuid::new_v4());
        let started = OffsetDateTime::now_utc();

        let receipt_claim = receipts
            .claim(&consumer, event_id, started, Duration::minutes(5))
            .await
            .unwrap();
        let crate::ConsumerEventClaim::Claimed {
            attempts: receipt_attempts,
        } = receipt_claim
        else {
            panic!("fresh receipt must be claimed");
        };
        let mut tx = pool.begin().await.unwrap();
        let queue_id = BotDeliveryOutboxRepo::enqueue_in_tx(
            &mut tx,
            subscription,
            bot,
            event_id,
            "message",
            room,
            &body,
        )
        .await
        .unwrap();
        assert!(crate::ConsumerEventReceiptRepo::complete_in_tx(
            &mut tx,
            &consumer,
            event_id,
            receipt_attempts,
            started,
        )
        .await
        .unwrap());
        tx.commit().await.unwrap();
        assert_eq!(
            receipts
                .claim(
                    &consumer,
                    event_id,
                    started + Duration::days(1),
                    Duration::minutes(5),
                )
                .await
                .unwrap(),
            crate::ConsumerEventClaim::Completed
        );

        let duplicate = repo
            .enqueue(subscription, bot, event_id, "message", room, &body)
            .await
            .unwrap();
        assert_eq!(duplicate, queue_id);
        let stored = repo.get(queue_id).await.unwrap().unwrap();
        assert_eq!(stored.request_body, body);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&stored.request_body).unwrap()["event_id"],
            event_id.to_string()
        );

        // A failed receipt completion rolls the whole fan-out transaction back.
        let rolled_back_event = Uuid::new_v4();
        let crate::ConsumerEventClaim::Claimed {
            attempts: rollback_attempt,
        } = receipts
            .claim(&consumer, rolled_back_event, started, Duration::minutes(5))
            .await
            .unwrap()
        else {
            panic!("fresh rollback receipt must be claimed");
        };
        let mut tx = pool.begin().await.unwrap();
        BotDeliveryOutboxRepo::enqueue_in_tx(
            &mut tx,
            subscription,
            bot,
            rolled_back_event,
            "message",
            room,
            br#"{"kind":"message"}"#,
        )
        .await
        .unwrap();
        assert!(!crate::ConsumerEventReceiptRepo::complete_in_tx(
            &mut tx,
            &consumer,
            rolled_back_event,
            rollback_attempt + 1,
            started,
        )
        .await
        .unwrap());
        tx.rollback().await.unwrap();
        let rolled_back_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM bot_subscription_delivery_outbox WHERE event_id = $1",
        )
        .bind(rolled_back_event)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rolled_back_count, 0);

        let claim_at = OffsetDateTime::now_utc() + Duration::seconds(1);
        let first = repo
            .claim_due(claim_at, Duration::minutes(2), 32)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.id == queue_id)
            .unwrap();
        let first_token = first.claim_token.unwrap();
        assert_eq!(first.attempts, 1);
        assert!(repo
            .claim_due(claim_at + Duration::minutes(1), Duration::minutes(2), 32,)
            .await
            .unwrap()
            .into_iter()
            .all(|row| row.id != queue_id));

        let reclaimed_at = claim_at + Duration::minutes(3);
        let reclaimed = repo
            .claim_due(reclaimed_at, Duration::minutes(2), 32)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.id == queue_id)
            .unwrap();
        let reclaimed_token = reclaimed.claim_token.unwrap();
        assert_ne!(first_token, reclaimed_token);
        assert_eq!(reclaimed.attempts, 2);
        assert!(
            !repo
                .mark_delivered(queue_id, first_token, reclaimed_at, 200)
                .await
                .unwrap(),
            "an expired worker cannot settle a newer lease"
        );
        assert!(repo
            .release_claim(queue_id, reclaimed_token, reclaimed_at)
            .await
            .unwrap());

        let mut cursor = reclaimed_at;
        let mut current = repo
            .claim_due(cursor, Duration::minutes(2), 32)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.id == queue_id)
            .unwrap();
        loop {
            let token = current.claim_token.unwrap();
            let disposition = repo
                .mark_failed(
                    current.id,
                    token,
                    current.attempts,
                    cursor,
                    Some(503),
                    "upstream unavailable",
                )
                .await
                .unwrap()
                .unwrap();
            if current.attempts >= BOT_DELIVERY_MAX_ATTEMPTS {
                assert_eq!(disposition, "dead");
                break;
            }
            assert_eq!(disposition, "failed");
            cursor = repo.get(queue_id).await.unwrap().unwrap().available_at;
            current = repo
                .claim_due(cursor, Duration::minutes(2), 32)
                .await
                .unwrap()
                .into_iter()
                .find(|row| row.id == queue_id)
                .unwrap();
        }
        let dead = repo.get(queue_id).await.unwrap().unwrap();
        assert_eq!(dead.status, "dead");
        assert_eq!(dead.attempts, BOT_DELIVERY_MAX_ATTEMPTS);
        assert!(dead.claim_token.is_none());
        assert!(repo
            .list_dead(100)
            .await
            .unwrap()
            .iter()
            .any(|row| row.id == queue_id));
        let visible_dlq = bots
            .list_deliveries_for_bot(bot, 100)
            .await
            .unwrap()
            .into_iter()
            .find(|delivery| delivery.id == queue_id)
            .expect("existing delivery history API must surface DLQ rows");
        assert_eq!(visible_dlq.status, "dead");
        assert_eq!(visible_dlq.event_id, Some(event_id));

        let success_event = Uuid::new_v4();
        let success_id = repo
            .enqueue(
                subscription,
                bot,
                success_event,
                "message",
                room,
                &serde_json::to_vec(&serde_json::json!({
                    "kind": "message",
                    "event_id": success_event
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let success = repo
            .claim_due(cursor, Duration::minutes(2), 32)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.id == success_id)
            .unwrap();
        let success_token = success.claim_token.unwrap();
        assert!(repo
            .mark_delivered(success.id, success_token, cursor, 204)
            .await
            .unwrap());
        assert!(
            !repo
                .mark_delivered(success.id, success_token, cursor, 204)
                .await
                .unwrap(),
            "success settlement is fenced and idempotent"
        );
        assert_eq!(
            repo.sweep_delivered_before(cursor + Duration::seconds(1))
                .await
                .unwrap(),
            1
        );
        assert!(repo.get(success_id).await.unwrap().is_none());
        assert_eq!(
            repo.get(queue_id).await.unwrap().unwrap().status,
            "dead",
            "retention must not delete DLQ rows"
        );

        assert!(
            !repo
                .requeue_dead(queue_id, ParticipantId::new(), cursor)
                .await
                .unwrap(),
            "another bot cannot requeue this delivery"
        );
        assert!(repo.requeue_dead(queue_id, bot, cursor).await.unwrap());
        let requeued = repo.get(queue_id).await.unwrap().unwrap();
        assert_eq!(requeued.status, "pending");
        assert_eq!(requeued.attempts, 0);
        assert!(requeued.completed_at.is_none());
        assert!(requeued.last_http_status.is_none());
        assert!(requeued.last_error.is_none());
        assert_eq!(
            requeued.request_body, body,
            "manual retry must preserve the producer event_id bytes"
        );
        assert_eq!(
            repo.sweep_delivered_before(cursor + Duration::days(365))
                .await
                .unwrap(),
            0,
            "retention must not delete requeued pending work"
        );

        bots.delete_subscription(subscription, bot).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL with migrations applied"]
    async fn retention_sweep_deletes_only_expired_terminal_rows_and_attempt_history() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let bots = crate::BotRepo::new(pool.clone());
        let repo = BotDeliveryOutboxRepo::new(pool.clone());

        let owner = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(owner.to_uuid())
            .bind(format!("bot-retention-owner-{owner}"))
            .execute(&pool)
            .await
            .unwrap();
        let bot = bots
            .create(owner, "retention-delivery-bot", None, None)
            .await
            .unwrap();
        let (workspace, room) = scoped_room(&pool, owner, "retention-delivery").await;
        let filters = serde_json::json!({
            "room_id": room.to_string(),
            "workspace_id": workspace.to_string(),
        });
        let subscription = bots
            .subscribe(
                bot,
                "message",
                Some(&filters),
                Some("https://example.test/hook"),
                Some("retention-test-secret-0123456789abcdef"),
            )
            .await
            .unwrap();

        // Synthetic 1970 timestamps isolate this destructive global sweep from
        // any ordinary rows in a developer database.
        let old = OffsetDateTime::UNIX_EPOCH + Duration::days(2);
        let cutoff = OffsetDateTime::UNIX_EPOCH + Duration::days(10);
        let recent = OffsetDateTime::UNIX_EPOCH + Duration::days(20);
        let old_delivered =
            insert_retention_outbox(&pool, subscription, bot, room, "delivered", old).await;
        let old_dead = insert_retention_outbox(&pool, subscription, bot, room, "dead", old).await;
        let recent_delivered =
            insert_retention_outbox(&pool, subscription, bot, room, "delivered", recent).await;
        let recent_dead =
            insert_retention_outbox(&pool, subscription, bot, room, "dead", recent).await;
        let old_pending =
            insert_retention_outbox(&pool, subscription, bot, room, "pending", old).await;
        let old_failed =
            insert_retention_outbox(&pool, subscription, bot, room, "failed", old).await;

        let old_attempt = bots
            .record_delivery_attempt(
                subscription,
                bot,
                "message",
                crate::DeliveryStatus::Failed,
                Some(503),
                Some("old failure"),
                1,
            )
            .await
            .unwrap();
        let recent_attempt = bots
            .record_delivery_attempt(
                subscription,
                bot,
                "message",
                crate::DeliveryStatus::Delivered,
                Some(204),
                None,
                2,
            )
            .await
            .unwrap();
        sqlx::query(
            r"UPDATE bot_subscription_deliveries
                  SET created_at = CASE WHEN id = $1 THEN $3 ELSE $4 END
                WHERE id IN ($1, $2)",
        )
        .bind(old_attempt)
        .bind(recent_attempt)
        .bind(old)
        .bind(recent)
        .execute(&pool)
        .await
        .unwrap();

        let removed = repo.sweep_retention_before(cutoff, cutoff).await.unwrap();
        assert_eq!(
            removed,
            BotDeliveryRetentionSweep {
                delivered_outbox: 1,
                dead_outbox: 1,
                attempt_history: 1,
            }
        );
        assert!(repo.get(old_delivered).await.unwrap().is_none());
        assert!(repo.get(old_dead).await.unwrap().is_none());
        assert_eq!(
            repo.get(recent_delivered).await.unwrap().unwrap().status,
            "delivered"
        );
        assert_eq!(repo.get(recent_dead).await.unwrap().unwrap().status, "dead");
        assert_eq!(
            repo.get(old_pending).await.unwrap().unwrap().status,
            "pending",
            "retention must never delete pending work"
        );
        assert_eq!(
            repo.get(old_failed).await.unwrap().unwrap().status,
            "failed",
            "retention must never delete retryable failed work"
        );
        let retained_attempts: Vec<Uuid> = sqlx::query_scalar(
            r"SELECT id
                FROM bot_subscription_deliveries
               WHERE id IN ($1, $2)",
        )
        .bind(old_attempt)
        .bind(recent_attempt)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(retained_attempts, vec![recent_attempt]);

        bots.delete_subscription(subscription, bot).await.unwrap();
    }
}
