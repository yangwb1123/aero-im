//! Outbound-webhook delivery log + retry/DLQ persistence (operability plane).
//!
//! Backs `migrations/0085_webhook_delivery_log.sql`. Where [`crate::webhook`]
//! holds the *targets* (url + signing secret) and the pure signing/shaping logic,
//! this module records the *outcome* of each delivery so the dispatcher can do
//! more than one-shot best-effort:
//!
//! * Each `(webhook, event)` send is a [`webhook_delivery_log`] row whose `status`
//!   walks `pending → delivered | failed → dead`.
//! * A `failed` row carries a future `next_attempt_at` set by an **exponential
//!   backoff** schedule ([`backoff_delay`] / [`next_attempt_at`]). A retry loop
//!   [`claim_due`](WebhookDeliveryRepo::claim_due)s the due ones and re-sends.
//! * After [`MAX_ATTEMPTS`] the row is parked at `dead` — a dead-letter queue an
//!   admin can [`list_dead`](WebhookDeliveryRepo::list_dead) and
//!   [`requeue`](WebhookDeliveryRepo::requeue).
//!
//! ## Testable seam (DB-free, unit-tested)
//!
//! The backoff schedule ([`backoff_delay`]) and the cap predicate
//! ([`is_dead_at`]) are pure functions, unit-tested without a clock or database
//! (Postgres is absent in CI). The async methods are a thin SQL shell over them.
//!
//! Purely additive: a NEW [`WebhookDeliveryRepo`]; no existing repo is touched.

use aero_common::{WebhookDeliveryId, WebhookId};
use serde::Serialize;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};

// ----------------------------------------------------- Pure backoff schedule

/// Maximum number of delivery attempts before a row is parked at `dead`. The
/// first attempt is attempt 1; once `attempts` reaches this, a further failure
/// is terminal rather than retried.
pub const MAX_ATTEMPTS: i32 = 6;

/// Base delay (seconds) for the first retry. Each subsequent retry doubles it.
const BASE_DELAY_SECS: i64 = 30;

/// Cap on a single backoff delay (seconds), so the schedule plateaus instead of
/// growing unbounded — 1 hour here (`30s,60s,120s,240s,480s,960s,…` clamped).
const MAX_DELAY_SECS: i64 = 3600;

/// Exponential backoff delay (in seconds) before retry number `attempts`.
///
/// `attempts` is the count of attempts **already made**: after the 1st failed
/// attempt (`attempts == 1`) the delay is [`BASE_DELAY_SECS`]; it doubles each
/// further attempt, clamped to [`MAX_DELAY_SECS`]. A non-positive `attempts`
/// (defensive) is treated as `1`. Pure + total, so the schedule is unit-tested
/// without a clock.
#[must_use]
pub fn backoff_delay(attempts: i32) -> Duration {
    let n = attempts.max(1);
    // Shift the base left by (n-1), saturating so a large n can't overflow.
    let shift = u32::try_from(n - 1).unwrap_or(u32::MAX).min(20); // 2^20 already >> cap
    let raw = BASE_DELAY_SECS.saturating_mul(1_i64.checked_shl(shift).unwrap_or(i64::MAX));
    Duration::seconds(raw.min(MAX_DELAY_SECS))
}

/// The `next_attempt_at` for a row that has made `attempts` attempts, computed
/// from `now` plus [`backoff_delay`]. Pure (the clock is an argument), so the
/// scheduling is unit-testable.
#[must_use]
pub fn next_attempt_at(now: OffsetDateTime, attempts: i32) -> OffsetDateTime {
    now + backoff_delay(attempts)
}

/// Whether a delivery that has made `attempts` attempts is now permanently dead
/// (the retry budget is exhausted). Pure, so the cap is unit-tested.
#[must_use]
pub fn is_dead_at(attempts: i32) -> bool {
    attempts >= MAX_ATTEMPTS
}

// ----------------------------------------------------------------- Row types

/// One outbound-delivery record (a row of `webhook_delivery_log`). `Serialize`
/// so an admin handler can hand it straight back as JSON.
#[derive(Debug, Clone, Serialize)]
pub struct WebhookDelivery {
    pub id: WebhookDeliveryId,
    pub webhook_id: WebhookId,
    /// Opaque correlation id of the delivered event (e.g. the source message id).
    pub event_id: Option<String>,
    /// Lifecycle: `pending` | `delivered` | `failed` | `dead`.
    pub status: String,
    pub attempts: i32,
    pub last_status_code: Option<i32>,
    pub last_error: Option<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub next_attempt_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

type DeliveryRow = (
    uuid::Uuid,
    uuid::Uuid,
    Option<String>,
    String,
    i32,
    Option<i32>,
    Option<String>,
    Option<OffsetDateTime>,
    OffsetDateTime,
    OffsetDateTime,
);

fn row_to_delivery(r: DeliveryRow) -> WebhookDelivery {
    let (id, webhook_id, event_id, status, attempts, code, err, next, created, updated) = r;
    WebhookDelivery {
        id: WebhookDeliveryId::from_uuid(id),
        webhook_id: WebhookId::from_uuid(webhook_id),
        event_id,
        status,
        attempts,
        last_status_code: code,
        last_error: err,
        next_attempt_at: next,
        created_at: created,
        updated_at: updated,
    }
}

/// Largest page [`WebhookDeliveryRepo::list_dead`] / `list_for_webhook` return.
pub const MAX_PAGE: i64 = 200;

/// Clamp a requested page size into `1..=MAX_PAGE` (a `None`/non-positive request
/// defaults to [`MAX_PAGE`]). Pure, so unit-tested without a DB.
#[must_use]
pub fn clamp_limit(requested: Option<i64>) -> i64 {
    match requested {
        Some(n) if n >= 1 => n.min(MAX_PAGE),
        _ => MAX_PAGE,
    }
}

// ------------------------------------------------------------------ The repo

/// Repository over `webhook_delivery_log`. Cheap to clone (wraps a [`PgPool`]).
#[derive(Clone)]
#[must_use]
pub struct WebhookDeliveryRepo {
    pool: PgPool,
}

impl WebhookDeliveryRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Hard-delete terminal delivery-log rows (`delivered`/`dead`) last updated
    /// before `cutoff` (data-lifecycle retention sweep, ROADMAP5 方向四). The
    /// log had no retention, so every delivery attempt accumulated forever. Rows
    /// still in `pending`/`failed` are retryable and never swept. Returns the
    /// number of rows deleted.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn sweep_terminal_before(
        &self,
        cutoff: OffsetDateTime,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(
            r"DELETE FROM webhook_delivery_log
               WHERE status IN ('delivered','dead') AND updated_at < $1",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Record a brand-new delivery attempt in `pending` state with `attempts = 1`
    /// (the dispatcher records the row as it makes the first send). Returns the
    /// generated id so a subsequent `mark_*` can target it.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] (e.g. an unknown `webhook` FK).
    pub async fn record_attempt(
        &self,
        webhook: WebhookId,
        event_id: Option<&str>,
    ) -> Result<WebhookDeliveryId, sqlx::Error> {
        let id = WebhookDeliveryId::new();
        sqlx::query(
            r"INSERT INTO webhook_delivery_log
                  (id, webhook_id, event_id, status, attempts, created_at, updated_at)
               VALUES ($1, $2, $3, 'pending', 1, now(), now())",
        )
        .bind(id.to_uuid())
        .bind(webhook.to_uuid())
        .bind(event_id)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Mark a delivery `delivered` (a 2xx). Clears any pending retry. Idempotent
    /// over `status` — re-marking a delivered row is a harmless no-op update.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn mark_delivered(
        &self,
        id: WebhookDeliveryId,
        status_code: i32,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"UPDATE webhook_delivery_log
                  SET status = 'delivered',
                      last_status_code = $2,
                      last_error = NULL,
                      next_attempt_at = NULL,
                      updated_at = now()
                WHERE id = $1",
        )
        .bind(id.to_uuid())
        .bind(status_code)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record a failed attempt. If `attempts` (the count already made, including
    /// this one) has reached [`MAX_ATTEMPTS`] the row is parked at `dead`;
    /// otherwise it goes to `failed` with `next_attempt_at` set by the
    /// [`next_attempt_at`] exponential-backoff schedule (computed in Rust from
    /// `now` so the schedule is the unit-tested pure function, not SQL).
    ///
    /// `status_code` is the HTTP status of the failed attempt (`None` on a
    /// transport error); `err` is a short diagnostic.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn mark_failed_with_backoff(
        &self,
        id: WebhookDeliveryId,
        attempts: i32,
        status_code: Option<i32>,
        err: &str,
    ) -> Result<(), sqlx::Error> {
        let now = OffsetDateTime::now_utc();
        if is_dead_at(attempts) {
            sqlx::query(
                r"UPDATE webhook_delivery_log
                      SET status = 'dead',
                          last_status_code = $2,
                          last_error = $3,
                          next_attempt_at = NULL,
                          updated_at = now()
                    WHERE id = $1",
            )
            .bind(id.to_uuid())
            .bind(status_code)
            .bind(err)
            .execute(&self.pool)
            .await?;
        } else {
            let next = next_attempt_at(now, attempts);
            sqlx::query(
                r"UPDATE webhook_delivery_log
                      SET status = 'failed',
                          last_status_code = $2,
                          last_error = $3,
                          next_attempt_at = $4,
                          updated_at = now()
                    WHERE id = $1",
            )
            .bind(id.to_uuid())
            .bind(status_code)
            .bind(err)
            .bind(next)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// Claim up to `limit` `failed` deliveries whose `next_attempt_at <= now`,
    /// flipping each back to `pending` and bumping `attempts`, returning the
    /// claimed rows for the retry loop to re-send. Uses `FOR UPDATE SKIP LOCKED`
    /// so multiple retry workers don't double-send the same row.
    ///
    /// The returned `attempts` reflects the post-increment count, i.e. the attempt
    /// number this re-send represents — pass it back to
    /// [`mark_failed_with_backoff`](Self::mark_failed_with_backoff) if it fails.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn claim_due(
        &self,
        now: OffsetDateTime,
        limit: i64,
    ) -> Result<Vec<WebhookDelivery>, sqlx::Error> {
        let limit = limit.clamp(1, MAX_PAGE);
        let rows = sqlx::query_as::<_, DeliveryRow>(
            r"UPDATE webhook_delivery_log
                  SET status = 'pending', attempts = attempts + 1, updated_at = now()
                WHERE id IN (
                    SELECT id FROM webhook_delivery_log
                     WHERE status = 'failed'
                       AND next_attempt_at IS NOT NULL
                       AND next_attempt_at <= $1
                     ORDER BY next_attempt_at ASC
                     FOR UPDATE SKIP LOCKED
                     LIMIT $2
                )
                RETURNING id, webhook_id, event_id, status, attempts, last_status_code,
                          last_error, next_attempt_at, created_at, updated_at",
        )
        .bind(now)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_delivery).collect())
    }

    /// List the dead (DLQ) deliveries for one webhook, newest first, capped via
    /// [`clamp_limit`].
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn list_dead(
        &self,
        webhook: WebhookId,
        limit: Option<i64>,
    ) -> Result<Vec<WebhookDelivery>, sqlx::Error> {
        let limit = clamp_limit(limit);
        let rows = sqlx::query_as::<_, DeliveryRow>(
            r"SELECT id, webhook_id, event_id, status, attempts, last_status_code,
                     last_error, next_attempt_at, created_at, updated_at
               FROM webhook_delivery_log
               WHERE webhook_id = $1 AND status = 'dead'
               ORDER BY created_at DESC
               LIMIT $2",
        )
        .bind(webhook.to_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_delivery).collect())
    }

    /// List a webhook's recent deliveries (any status), newest first, capped via
    /// [`clamp_limit`]. The full per-hook delivery log for the admin view.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn list_for_webhook(
        &self,
        webhook: WebhookId,
        limit: Option<i64>,
    ) -> Result<Vec<WebhookDelivery>, sqlx::Error> {
        let limit = clamp_limit(limit);
        let rows = sqlx::query_as::<_, DeliveryRow>(
            r"SELECT id, webhook_id, event_id, status, attempts, last_status_code,
                     last_error, next_attempt_at, created_at, updated_at
               FROM webhook_delivery_log
               WHERE webhook_id = $1
               ORDER BY created_at DESC
               LIMIT $2",
        )
        .bind(webhook.to_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_delivery).collect())
    }

    /// Requeue a dead delivery for a fresh attempt: reset `attempts = 0`, clear
    /// the error/status, and mark it `failed` due **now** so the retry loop picks
    /// it up on its next tick. Returns whether a dead row was actually flipped
    /// (so the caller can 404 a non-dead/unknown id). Only acts on `dead` rows —
    /// requeuing a live (pending/failed) row is a no-op.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn requeue(&self, id: WebhookDeliveryId) -> Result<bool, sqlx::Error> {
        let res = sqlx::query(
            r"UPDATE webhook_delivery_log
                  SET status = 'failed',
                      attempts = 0,
                      last_error = NULL,
                      last_status_code = NULL,
                      next_attempt_at = now(),
                      updated_at = now()
                WHERE id = $1 AND status = 'dead'",
        )
        .bind(id.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Look up a single delivery by id (for the admin requeue path to resolve the
    /// owning webhook before authorizing). `None` when no row matches.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn get(
        &self,
        id: WebhookDeliveryId,
    ) -> Result<Option<WebhookDelivery>, sqlx::Error> {
        let row = sqlx::query_as::<_, DeliveryRow>(
            r"SELECT id, webhook_id, event_id, status, attempts, last_status_code,
                     last_error, next_attempt_at, created_at, updated_at
               FROM webhook_delivery_log
               WHERE id = $1",
        )
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_delivery))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- backoff_delay: doubling, clamped, defensive -----

    #[test]
    fn backoff_doubles_from_base() {
        // attempts=1 ⇒ base; each further attempt doubles.
        assert_eq!(backoff_delay(1), Duration::seconds(30));
        assert_eq!(backoff_delay(2), Duration::seconds(60));
        assert_eq!(backoff_delay(3), Duration::seconds(120));
        assert_eq!(backoff_delay(4), Duration::seconds(240));
        assert_eq!(backoff_delay(5), Duration::seconds(480));
        assert_eq!(backoff_delay(6), Duration::seconds(960));
    }

    #[test]
    fn backoff_is_clamped_and_monotonic() {
        // The schedule plateaus at MAX_DELAY_SECS and never decreases.
        let mut prev = Duration::ZERO;
        for n in 1..=40 {
            let d = backoff_delay(n);
            assert!(d >= prev, "non-decreasing at n={n}");
            assert!(d <= Duration::seconds(MAX_DELAY_SECS), "clamped at n={n}");
            prev = d;
        }
        // Far past the cap it sits exactly at MAX_DELAY_SECS (no overflow).
        assert_eq!(backoff_delay(i32::MAX), Duration::seconds(MAX_DELAY_SECS));
    }

    #[test]
    fn backoff_defends_non_positive_attempts() {
        // 0 / negative are treated as the first attempt (base delay), not a panic.
        assert_eq!(backoff_delay(0), Duration::seconds(30));
        assert_eq!(backoff_delay(-5), Duration::seconds(30));
    }

    #[test]
    fn next_attempt_at_adds_backoff_to_now() {
        let now = OffsetDateTime::UNIX_EPOCH;
        assert_eq!(next_attempt_at(now, 1), now + Duration::seconds(30));
        assert_eq!(next_attempt_at(now, 3), now + Duration::seconds(120));
    }

    // ----- is_dead_at: cap predicate -----

    #[test]
    fn is_dead_only_at_or_past_cap() {
        assert!(!is_dead_at(1));
        assert!(!is_dead_at(MAX_ATTEMPTS - 1));
        assert!(is_dead_at(MAX_ATTEMPTS));
        assert!(is_dead_at(MAX_ATTEMPTS + 1));
        // The last retryable attempt is MAX_ATTEMPTS - 1; the cap'th failure dies.
        assert_eq!(MAX_ATTEMPTS, 6);
    }

    // ----- clamp_limit -----

    #[test]
    fn clamp_limit_defaults_and_bounds() {
        assert_eq!(clamp_limit(None), MAX_PAGE);
        assert_eq!(clamp_limit(Some(0)), MAX_PAGE);
        assert_eq!(clamp_limit(Some(-3)), MAX_PAGE);
        assert_eq!(clamp_limit(Some(1)), 1);
        assert_eq!(clamp_limit(Some(50)), 50);
        assert_eq!(clamp_limit(Some(10_000)), MAX_PAGE);
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored webhook_delivery_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::webhook::{generate_secret, WebhookRepo};
    use aero_common::{ParticipantId, RoomId, WebhookId, WorkspaceId};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    // A throwaway workspace + room + an outgoing webhook so the FK is satisfiable.
    async fn fixture(p: &PgPool) -> WebhookId {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(actor.to_uuid())
            .bind(format!("whd-actor-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");
        let ws = WorkspaceId::new();
        sqlx::query("INSERT INTO workspaces (id, name, slug, created_by, created_at) VALUES ($1,$2,$3,$4, now())")
            .bind(ws.to_uuid())
            .bind("WHD Test WS")
            .bind(format!("whd-{ws}"))
            .bind(actor.to_uuid())
            .execute(p)
            .await
            .expect("insert workspace");
        let room = RoomId::new();
        sqlx::query("INSERT INTO rooms (id, kind, created_by, workspace_id, created_at) VALUES ($1,'group',$2,$3, now())")
            .bind(room.to_uuid())
            .bind(actor.to_uuid())
            .bind(ws.to_uuid())
            .execute(p)
            .await
            .expect("insert room");
        WebhookRepo::new(p.clone())
            .create_outgoing(room, "https://hook.test", &generate_secret(), &[], Some("d"), actor)
            .await
            .expect("create outgoing webhook")
    }

    /// A failed delivery accrues attempts across the backoff schedule and lands at
    /// `dead` once the cap is reached; a `requeue` then resets it back to `failed`
    /// with `attempts = 0` and a due `next_attempt_at`.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn webhook_delivery_fails_to_dead_then_requeue_resets() {
        let p = pool();
        let repo = WebhookDeliveryRepo::new(p.clone());
        let hook = fixture(&p).await;

        // Record the first attempt (attempts = 1, pending).
        let id = repo.record_attempt(hook, Some("evt-1")).await.unwrap();
        let row = repo.get(id).await.unwrap().expect("row exists");
        assert_eq!(row.status, "pending");
        assert_eq!(row.attempts, 1);

        // Fail attempts 1..MAX-1 — each goes to `failed` with a future retry time,
        // claiming each due row back to bump the attempt count for the next fail.
        let mut attempts = 1;
        while attempts < MAX_ATTEMPTS {
            repo.mark_failed_with_backoff(id, attempts, Some(500), "boom").await.unwrap();
            let r = repo.get(id).await.unwrap().unwrap();
            assert_eq!(r.status, "failed", "attempt {attempts} is retryable");
            assert!(r.next_attempt_at.is_some(), "retryable rows have a next time");
            // Claim it as due (use a far-future now so it's eligible) to bump attempts.
            let far = OffsetDateTime::now_utc() + Duration::days(365);
            let claimed = repo.claim_due(far, 10).await.unwrap();
            assert!(claimed.iter().any(|d| d.id == id), "due row is claimed");
            attempts += 1;
        }

        // The cap'th failure parks it at `dead`.
        repo.mark_failed_with_backoff(id, MAX_ATTEMPTS, Some(500), "final").await.unwrap();
        let dead = repo.get(id).await.unwrap().unwrap();
        assert_eq!(dead.status, "dead", "cap reached ⇒ dead");
        assert!(dead.next_attempt_at.is_none(), "dead rows carry no retry time");
        assert_eq!(dead.last_error.as_deref(), Some("final"));

        // It surfaces in the DLQ listing for the hook.
        let dlq = repo.list_dead(hook, Some(10)).await.unwrap();
        assert!(dlq.iter().any(|d| d.id == id), "dead row is in the DLQ");

        // Requeue resets attempts to 0 and makes it due now (status back to failed).
        assert!(repo.requeue(id).await.unwrap(), "a dead row is requeued");
        let requeued = repo.get(id).await.unwrap().unwrap();
        assert_eq!(requeued.status, "failed");
        assert_eq!(requeued.attempts, 0, "attempts reset");
        assert!(requeued.last_error.is_none(), "error cleared");
        assert!(requeued.next_attempt_at.is_some(), "due now");

        // Idempotent: requeuing a non-dead row flips nothing.
        assert!(!repo.requeue(id).await.unwrap(), "second requeue is a no-op");
    }

    /// A delivered attempt is terminal and never surfaces for retry.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn webhook_delivery_delivered_is_terminal() {
        let p = pool();
        let repo = WebhookDeliveryRepo::new(p.clone());
        let hook = fixture(&p).await;

        let id = repo.record_attempt(hook, None).await.unwrap();
        repo.mark_delivered(id, 200).await.unwrap();
        let row = repo.get(id).await.unwrap().unwrap();
        assert_eq!(row.status, "delivered");
        assert_eq!(row.last_status_code, Some(200));
        assert!(row.next_attempt_at.is_none());

        // It is not in the DLQ and not claimed by the retry loop.
        assert!(repo.list_dead(hook, None).await.unwrap().iter().all(|d| d.id != id));
        let far = OffsetDateTime::now_utc() + Duration::days(365);
        assert!(repo.claim_due(far, 10).await.unwrap().iter().all(|d| d.id != id));
    }
}
