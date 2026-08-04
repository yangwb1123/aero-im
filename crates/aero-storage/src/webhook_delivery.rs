//! Outbound-webhook delivery log + retry/DLQ persistence (operability plane).
//!
//! Backs `migrations/0085_webhook_delivery_log.sql`. Where [`crate::webhook`]
//! holds the *targets* (url + signing secret) and the pure signing/shaping logic,
//! this module records the *outcome* of each delivery so the dispatcher can do
//! more than one-shot best-effort:
//!
//! * Each `(webhook, event)` send is a [`webhook_delivery_log`] row whose `status`
//!   walks `pending → delivered | failed → dead`. A random `claim_token` owns
//!   every `pending` generation, independently of the HTTP-attempt counter.
//! * A `failed` row carries a future `next_attempt_at` set by an **exponential
//!   backoff** schedule ([`backoff_delay`] / [`next_attempt_at`]). A retry loop
//!   [`claim_due`](WebhookDeliveryRepo::claim_due)s the due ones and re-sends.
//! * After [`MAX_ATTEMPTS`] the row is parked at `dead` — a dead-letter queue an
//!   admin can list and requeue through the transaction-owned authorized APIs.
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

#[path = "webhook_delivery/admin.rs"]
mod admin;

// ----------------------------------------------------- Pure backoff schedule

/// Maximum number of delivery attempts before a row is parked at `dead`. The
/// first attempt is attempt 1; once `attempts` reaches this, a further failure
/// is terminal rather than retried.
pub const MAX_ATTEMPTS: i32 = 6;

/// A `pending` row older than this is considered abandoned and can be claimed
/// again. The real sender times out after 10 seconds; one minute leaves ample
/// settlement headroom while recovering process crashes and DB-outage zombies.
pub const PENDING_STALE_AFTER_SECS: i64 = 60;

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
    /// Exact bytes sent on the first attempt. Legacy terminal rows created
    /// before migration 0164 may be `None`; such rows are never claimable.
    #[serde(skip)]
    pub request_body: Option<Vec<u8>>,
    /// Safe unsigned headers replayed with the immutable body. Signature and
    /// timestamp headers are deliberately regenerated for every retry.
    #[serde(skip)]
    pub request_headers: Vec<(String, String)>,
    /// Unforgeable owner token for the current claim generation. It is internal
    /// control-plane state and must never be exposed by admin JSON.
    #[serde(skip)]
    pub claim_token: uuid::Uuid,
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
    Option<Vec<u8>>,
    sqlx::types::Json<Vec<(String, String)>>,
    uuid::Uuid,
    String,
    i32,
    Option<i32>,
    Option<String>,
    Option<OffsetDateTime>,
    OffsetDateTime,
    OffsetDateTime,
);

fn row_to_delivery(r: DeliveryRow) -> WebhookDelivery {
    let (
        id,
        webhook_id,
        event_id,
        request_body,
        request_headers,
        claim_token,
        status,
        attempts,
        code,
        err,
        next,
        created,
        updated,
    ) = r;
    WebhookDelivery {
        id: WebhookDeliveryId::from_uuid(id),
        webhook_id: WebhookId::from_uuid(webhook_id),
        event_id,
        request_body,
        request_headers: request_headers.0,
        claim_token,
        status,
        attempts,
        last_status_code: code,
        last_error: err,
        next_attempt_at: next,
        created_at: created,
        updated_at: updated,
    }
}

/// Ownership handle returned when a new delivery row is durably recorded.
///
/// The token, not `attempts`, identifies the worker allowed to begin an HTTP
/// call. `attempts` remains zero until [`WebhookDeliveryRepo::begin_attempt`]
/// succeeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebhookDeliveryClaim {
    pub id: WebhookDeliveryId,
    pub claim_token: uuid::Uuid,
}

/// Largest page the authorized delivery-history APIs return.
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
    pub async fn sweep_terminal_before(&self, cutoff: OffsetDateTime) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(
            r"DELETE FROM webhook_delivery_log
               WHERE status IN ('delivered','dead') AND updated_at < $1",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Durably claim a brand-new delivery in `pending` state with `attempts = 0`.
    ///
    /// This only reserves ownership; it does **not** represent an HTTP call.
    /// After endpoint/global permits are acquired, the owner must call
    /// [`Self::begin_attempt`] with the returned token immediately before POST.
    ///
    /// IDEMPOTENT on `(webhook_id, event_id)` when `event_id` is set: returns
    /// `Ok(Some(claim))` when THIS call reserved the delivery, or
    /// `Ok(None)` when a delivery for that event→endpoint was already recorded —
    /// i.e. a `JetStream` redelivery — so the caller MUST skip the duplicate POST.
    /// A NULL `event_id` is never deduped (no correlation id), so it always claims.
    /// Backed by the partial unique index from migration 0150.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] (e.g. an unknown `webhook` FK).
    pub async fn record_attempt(
        &self,
        webhook: WebhookId,
        event_id: Option<&str>,
        request_body: &[u8],
        request_headers: &[(String, String)],
    ) -> Result<Option<WebhookDeliveryClaim>, sqlx::Error> {
        let id = WebhookDeliveryId::new();
        let row: Option<(uuid::Uuid, uuid::Uuid)> = sqlx::query_as(
            r"INSERT INTO webhook_delivery_log
                  (id, webhook_id, event_id, request_body, request_headers,
                   status, attempts, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, 'pending', 0, now(), now())
               ON CONFLICT (webhook_id, event_id) WHERE event_id IS NOT NULL DO NOTHING
               RETURNING id, claim_token",
        )
        .bind(id.to_uuid())
        .bind(webhook.to_uuid())
        .bind(event_id)
        .bind(request_body)
        .bind(sqlx::types::Json(request_headers))
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(_, claim_token)| WebhookDeliveryClaim { id, claim_token }))
    }

    /// Materialize an event for the retry worker without claiming or sending it.
    ///
    /// This is used by transactional/outbox producers that must durably persist
    /// exact request bytes but never perform HTTP on their own path. The row is
    /// immediately due in `failed` state with `attempts = 0`; [`Self::claim_due`]
    /// later rotates the token and hands it to a sender. Returns `false` for an
    /// existing `(webhook_id, event_id)` dedupe key.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] (e.g. an unknown `webhook` FK).
    pub async fn enqueue(
        &self,
        webhook: WebhookId,
        event_id: Option<&str>,
        request_body: &[u8],
        request_headers: &[(String, String)],
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"INSERT INTO webhook_delivery_log
                  (id, webhook_id, event_id, request_body, request_headers,
                   status, attempts, next_attempt_at, created_at, updated_at)
               VALUES (
                   gen_random_uuid(), $1, $2, $3, $4,
                   'failed', 0, now(), now(), now()
               )
               ON CONFLICT (webhook_id, event_id)
                   WHERE event_id IS NOT NULL DO NOTHING",
        )
        .bind(webhook.to_uuid())
        .bind(event_id)
        .bind(request_body)
        .bind(sqlx::types::Json(request_headers))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Charge one real HTTP attempt to the current claim immediately before the
    /// request is started.
    ///
    /// The token is checked and `updated_at` refreshed atomically, so a worker
    /// that waited beyond the stale-claim window cannot POST after a successor
    /// has recovered the row. Returns the new attempt count, or `None` when the
    /// claim fence was lost. A row at the retry cap cannot begin another call.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn begin_attempt(
        &self,
        id: WebhookDeliveryId,
        claim_token: uuid::Uuid,
    ) -> Result<Option<i32>, sqlx::Error> {
        let row: Option<(i32,)> = sqlx::query_as(
            r"UPDATE webhook_delivery_log
                  SET attempts = attempts + 1,
                      updated_at = now()
                WHERE id = $1
                  AND status = 'pending'
                  AND claim_token = $2
                  AND attempts < $3
                RETURNING attempts",
        )
        .bind(id.to_uuid())
        .bind(claim_token)
        .bind(MAX_ATTEMPTS)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(attempts,)| attempts))
    }

    /// Mark the currently claimed attempt `delivered` (a 2xx). Clears any
    /// pending retry. A stale worker whose claim was recovered cannot settle a
    /// successor because every claim receives a fresh, random token.
    ///
    /// Returns `false` when the row is no longer the matching pending claim.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn mark_delivered(
        &self,
        id: WebhookDeliveryId,
        claim_token: uuid::Uuid,
        status_code: i32,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE webhook_delivery_log
                  SET status = 'delivered',
                      last_status_code = $3,
                      last_error = NULL,
                      next_attempt_at = NULL,
                      updated_at = now()
                WHERE id = $1
                  AND status = 'pending'
                  AND claim_token = $2",
        )
        .bind(id.to_uuid())
        .bind(claim_token)
        .bind(status_code)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Record a failed attempt. If `attempts` (the count already made, including
    /// this one) has reached [`MAX_ATTEMPTS`] the row is parked at `dead`;
    /// otherwise it goes to `failed` with `next_attempt_at` set by the
    /// [`next_attempt_at`] exponential-backoff schedule (computed in Rust from
    /// `now` so the schedule is the unit-tested pure function, not SQL).
    ///
    /// `attempts` is the count returned by [`Self::begin_attempt`].
    /// `claim_token` is the ownership fence. `status_code` is the HTTP status of
    /// the failed attempt (`None` on a transport error); `err` is a short
    /// diagnostic. Returns `false` when a newer worker already recovered or
    /// settled this claim.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn mark_failed_with_backoff(
        &self,
        id: WebhookDeliveryId,
        claim_token: uuid::Uuid,
        attempts: i32,
        status_code: Option<i32>,
        err: &str,
    ) -> Result<bool, sqlx::Error> {
        let now = OffsetDateTime::now_utc();
        let result = if is_dead_at(attempts) {
            sqlx::query(
                r"UPDATE webhook_delivery_log
                      SET status = 'dead',
                          last_status_code = $4,
                          last_error = $5,
                          next_attempt_at = NULL,
                          updated_at = now()
                    WHERE id = $1
                      AND status = 'pending'
                      AND claim_token = $2
                      AND attempts = $3",
            )
            .bind(id.to_uuid())
            .bind(claim_token)
            .bind(attempts)
            .bind(status_code)
            .bind(err)
            .execute(&self.pool)
            .await?
        } else {
            let next = next_attempt_at(now, attempts);
            sqlx::query(
                r"UPDATE webhook_delivery_log
                      SET status = 'failed',
                          last_status_code = $4,
                          last_error = $5,
                          next_attempt_at = $6,
                          updated_at = now()
                    WHERE id = $1
                      AND status = 'pending'
                      AND claim_token = $2
                      AND attempts = $3",
            )
            .bind(id.to_uuid())
            .bind(claim_token)
            .bind(attempts)
            .bind(status_code)
            .bind(err)
            .bind(next)
            .execute(&self.pool)
            .await?
        };
        Ok(result.rows_affected() > 0)
    }

    /// Park the currently claimed generation directly in the DLQ.
    ///
    /// This is reserved for non-retryable persisted-record defects (for
    /// example, a legacy row without the immutable request body). The
    /// `claim_token` has the same stale-owner semantics as the ordinary
    /// settlement methods.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn mark_dead(
        &self,
        id: WebhookDeliveryId,
        claim_token: uuid::Uuid,
        status_code: Option<i32>,
        err: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE webhook_delivery_log
                  SET status = 'dead',
                      last_status_code = $3,
                      last_error = $4,
                      next_attempt_at = NULL,
                      updated_at = now()
                WHERE id = $1
                  AND status = 'pending'
                  AND claim_token = $2",
        )
        .bind(id.to_uuid())
        .bind(claim_token)
        .bind(status_code)
        .bind(err)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Claim up to `limit` due `failed` deliveries or abandoned `pending`
    /// deliveries older than [`PENDING_STALE_AFTER_SECS`], flipping each to
    /// `pending` and rotating `claim_token`. Claiming does **not** increment
    /// `attempts`; only [`Self::begin_attempt`] does. Stale recovery closes the
    /// crash/queueing window without charging calls that were never started.
    /// Uses `FOR UPDATE SKIP LOCKED` so multiple workers don't own one row.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn claim_due(
        &self,
        now: OffsetDateTime,
        limit: i64,
    ) -> Result<Vec<WebhookDelivery>, sqlx::Error> {
        let limit = limit.clamp(1, MAX_PAGE);
        let stale_before = now - Duration::seconds(PENDING_STALE_AFTER_SECS);
        let mut tx = self.pool.begin().await?;
        // If the final permitted call began but its worker died before
        // settlement, conservatively park it rather than exceed the HTTP-call
        // budget on recovery.
        sqlx::query(
            r"UPDATE webhook_delivery_log
                  SET status = 'dead',
                      claim_token = gen_random_uuid(),
                      last_error = COALESCE(
                          last_error,
                          'final HTTP attempt was abandoned before settlement'
                      ),
                      next_attempt_at = NULL,
                      updated_at = $2
                WHERE status = 'pending'
                  AND attempts >= $1
                  AND updated_at <= $3",
        )
        .bind(MAX_ATTEMPTS)
        .bind(now)
        .bind(stale_before)
        .execute(&mut *tx)
        .await?;
        let rows = sqlx::query_as::<_, DeliveryRow>(
            r"UPDATE webhook_delivery_log
                  SET status = 'pending',
                      claim_token = gen_random_uuid(),
                      next_attempt_at = NULL,
                      updated_at = $1
                WHERE id IN (
                    SELECT id FROM webhook_delivery_log
                     WHERE (
                              (status = 'failed'
                               AND next_attempt_at IS NOT NULL
                               AND next_attempt_at <= $1)
                              OR (status = 'pending' AND updated_at <= $3)
                           )
                       AND request_body IS NOT NULL
                       AND attempts < $4
                     ORDER BY COALESCE(next_attempt_at, updated_at) ASC
                     FOR UPDATE SKIP LOCKED
                     LIMIT $2
                )
                RETURNING id, webhook_id, event_id, request_body, request_headers,
                          claim_token, status, attempts, last_status_code, last_error,
                          next_attempt_at, created_at, updated_at",
        )
        .bind(now)
        .bind(limit)
        .bind(stale_before)
        .bind(MAX_ATTEMPTS)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(row_to_delivery).collect())
    }

    /// Defer a claimed-but-not-attempted delivery until `not_before`.
    ///
    /// This is the explicit path for an open circuit breaker or a transient
    /// target lookup failure. It is fenced by the claim token and deliberately
    /// leaves `attempts` unchanged because no HTTP call started. `PostgreSQL`'s
    /// current time is also a lower bound, so a stale caller cannot create an
    /// already-overdue loop.
    ///
    /// Returns whether this call deferred the still-owned pending row.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn defer_claim(
        &self,
        id: WebhookDeliveryId,
        claim_token: uuid::Uuid,
        not_before: OffsetDateTime,
        err: &str,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE webhook_delivery_log
                  SET status = 'failed',
                      last_error = $4,
                      next_attempt_at = GREATEST($3, now()),
                      updated_at = now()
                WHERE id = $1
                  AND status = 'pending'
                  AND claim_token = $2",
        )
        .bind(id.to_uuid())
        .bind(claim_token)
        .bind(not_before)
        .bind(err)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Return a claimed-but-not-attempted delivery to the retry queue immediately.
    ///
    /// Graceful shutdown may arrive after [`Self::claim_due`] has atomically
    /// moved a batch to `pending`, but before this process has sent every row.
    /// Releasing those untouched rows prevents a permanent `pending` zombie.
    /// Claiming never charged an attempt, so the counter is left unchanged.
    ///
    /// Returns whether this call released a still-pending row.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn release_claim(
        &self,
        id: WebhookDeliveryId,
        claim_token: uuid::Uuid,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE webhook_delivery_log
                  SET status = 'failed',
                      next_attempt_at = now(),
                      updated_at = now()
                WHERE id = $1
                  AND status = 'pending'
                  AND claim_token = $2",
        )
        .bind(id.to_uuid())
        .bind(claim_token)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// List the dead (DLQ) deliveries for one webhook, newest first, capped via
    /// [`clamp_limit`].
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    #[cfg(test)]
    pub(crate) async fn list_dead(
        &self,
        webhook: WebhookId,
        limit: Option<i64>,
    ) -> Result<Vec<WebhookDelivery>, sqlx::Error> {
        let limit = clamp_limit(limit);
        let rows = sqlx::query_as::<_, DeliveryRow>(
            r"SELECT id, webhook_id, event_id, request_body, request_headers,
                     claim_token, status, attempts, last_status_code, last_error,
                     next_attempt_at, created_at, updated_at
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

    /// Look up a single delivery by id (for the admin requeue path to resolve the
    /// owning webhook before authorizing). `None` when no row matches.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    #[cfg(test)]
    pub(crate) async fn get(
        &self,
        id: WebhookDeliveryId,
    ) -> Result<Option<WebhookDelivery>, sqlx::Error> {
        let row = sqlx::query_as::<_, DeliveryRow>(
            r"SELECT id, webhook_id, event_id, request_body, request_headers,
                     claim_token, status, attempts, last_status_code, last_error,
                     next_attempt_at, created_at, updated_at
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
#[path = "webhook_delivery/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "webhook_delivery/claim_fencing_tests.rs"]
mod claim_fencing_tests;

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

    pub(super) fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    // A throwaway workspace + room + an outgoing webhook so the FK is satisfiable.
    pub(super) async fn fixture(p: &PgPool) -> WebhookId {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(actor.to_uuid())
            .bind(format!("whd-actor-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");
        let ws = WorkspaceId::new();
        let mut tx = p.begin().await.expect("begin workspace fixture");
        sqlx::query("INSERT INTO workspaces (id, name, slug, created_by, created_at) VALUES ($1,$2,$3,$4, now())")
            .bind(ws.to_uuid())
            .bind("WHD Test WS")
            .bind(format!("whd-{ws}"))
            .bind(actor.to_uuid())
            .execute(&mut *tx)
            .await
            .expect("insert workspace");
        let room = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, created_by, workspace_id, created_at) VALUES ($1,'group',$2,$3, now())",
        )
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .bind(ws.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert room");
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1, $2, 'owner')",
        )
        .bind(ws.to_uuid())
        .bind(actor.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("join workspace");
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'owner')",
        )
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("join room");
        tx.commit().await.expect("commit workspace fixture");
        WebhookRepo::new(p.clone())
            .create_outgoing(
                room,
                "https://hook.test",
                &generate_secret(),
                &[],
                Some("d"),
                actor,
            )
            .await
            .expect("create outgoing webhook")
    }

    pub(super) async fn record_claim(
        repo: &WebhookDeliveryRepo,
        hook: WebhookId,
        event_id: &str,
    ) -> WebhookDeliveryClaim {
        repo.record_attempt(hook, Some(event_id), b"{\"kind\":\"test\"}", &[])
            .await
            .unwrap()
            .expect("new delivery claim")
    }

    pub(super) async fn begin_new_claim(
        repo: &WebhookDeliveryRepo,
        claim: WebhookDeliveryClaim,
    ) -> i32 {
        repo.begin_attempt(claim.id, claim.claim_token)
            .await
            .unwrap()
            .expect("claim owns attempt")
    }

    pub(super) async fn begin_retry(repo: &WebhookDeliveryRepo, delivery: &WebhookDelivery) -> i32 {
        repo.begin_attempt(delivery.id, delivery.claim_token)
            .await
            .unwrap()
            .expect("retry claim owns attempt")
    }

    pub(super) async fn actor_for_hook(p: &PgPool, hook: WebhookId) -> ParticipantId {
        let actor = sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT created_by FROM outgoing_webhooks WHERE id = $1",
        )
        .bind(hook.to_uuid())
        .fetch_one(p)
        .await
        .expect("fixture webhook has an actor");
        ParticipantId::from_uuid(actor)
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

        // Recording reserves ownership but does not charge an HTTP call.
        let claim = record_claim(&repo, hook, "evt-1").await;
        let id = claim.id;
        let row = repo.get(id).await.unwrap().expect("row exists");
        assert_eq!(row.status, "pending");
        assert_eq!(row.attempts, 0);
        let mut claim_token = claim.claim_token;
        let mut attempts = begin_new_claim(&repo, claim).await;
        assert_eq!(attempts, 1);

        // Fail attempts 1..MAX-1 — each goes to `failed` with a future retry time,
        // then claim without charging and begin the next real HTTP call.
        while attempts < MAX_ATTEMPTS {
            assert!(repo
                .mark_failed_with_backoff(id, claim_token, attempts, Some(500), "boom")
                .await
                .unwrap());
            let r = repo.get(id).await.unwrap().unwrap();
            assert_eq!(r.status, "failed", "attempt {attempts} is retryable");
            assert!(
                r.next_attempt_at.is_some(),
                "retryable rows have a next time"
            );
            let far = OffsetDateTime::now_utc() + Duration::days(365 + i64::from(attempts));
            let retry = repo
                .claim_due(far, 10)
                .await
                .unwrap()
                .into_iter()
                .find(|delivery| delivery.id == id)
                .expect("due row is claimed");
            assert_eq!(
                retry.attempts, attempts,
                "claiming without HTTP does not charge the counter"
            );
            assert_ne!(retry.claim_token, claim_token, "every claim rotates token");
            claim_token = retry.claim_token;
            attempts = begin_retry(&repo, &retry).await;
        }

        // The cap'th failure parks it at `dead`.
        assert!(repo
            .mark_failed_with_backoff(id, claim_token, MAX_ATTEMPTS, Some(500), "final",)
            .await
            .unwrap());
        let dead = repo.get(id).await.unwrap().unwrap();
        assert_eq!(dead.status, "dead", "cap reached ⇒ dead");
        assert!(
            dead.next_attempt_at.is_none(),
            "dead rows carry no retry time"
        );
        assert_eq!(dead.last_error.as_deref(), Some("final"));

        // It surfaces in the DLQ listing for the hook.
        let dlq = repo.list_dead(hook, Some(10)).await.unwrap();
        assert!(dlq.iter().any(|d| d.id == id), "dead row is in the DLQ");

        // Requeue resets attempts to 0 and makes it due now (status back to failed).
        let dead_token = dead.claim_token;
        let actor = actor_for_hook(&p, hook).await;
        repo.requeue_authorized(id, actor)
            .await
            .expect("a dead row is requeued");
        let requeued = repo.get(id).await.unwrap().unwrap();
        assert_eq!(requeued.status, "failed");
        assert_eq!(requeued.attempts, 0, "attempts reset");
        assert_ne!(
            requeued.claim_token, dead_token,
            "admin requeue rotates the ownership generation"
        );
        assert!(requeued.last_error.is_none(), "error cleared");
        assert!(requeued.next_attempt_at.is_some(), "due now");

        // Idempotent: requeuing a non-dead row flips nothing.
        assert!(matches!(
            repo.requeue_authorized(id, actor).await,
            Err(aero_common::Error::NotFound(_))
        ));
    }

    /// A graceful-shutdown release returns an untouched retry claim to `failed`
    /// without charging an HTTP attempt that never happened.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn webhook_delivery_release_claim_restores_retry_state() {
        let p = pool();
        let repo = WebhookDeliveryRepo::new(p.clone());
        let hook = fixture(&p).await;
        let initial = record_claim(&repo, hook, "evt-release").await;
        let id = initial.id;
        let attempts = begin_new_claim(&repo, initial).await;
        assert!(repo
            .mark_failed_with_backoff(id, initial.claim_token, attempts, None, "transient",)
            .await
            .unwrap());

        let far = OffsetDateTime::now_utc() + Duration::days(365);
        let claimed = repo.claim_due(far, 10).await.unwrap();
        let claimed = claimed
            .into_iter()
            .find(|delivery| delivery.id == id)
            .expect("failed row is claimed");
        assert_eq!(claimed.status, "pending");
        assert_eq!(claimed.attempts, 1);

        assert!(repo.release_claim(id, claimed.claim_token).await.unwrap());
        let released = repo.get(id).await.unwrap().expect("released row exists");
        assert_eq!(released.status, "failed");
        assert_eq!(released.attempts, 1, "unattempted retry is not charged");
        assert!(
            released.next_attempt_at.is_some(),
            "released row is due again"
        );
        assert!(
            !repo.release_claim(id, claimed.claim_token).await.unwrap(),
            "release is idempotent"
        );
    }

    /// Breaker deferral is queue control, not an HTTP call: it keeps attempts at
    /// zero and the row cannot be reclaimed before the breaker deadline.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn webhook_delivery_breaker_defer_preserves_attempts_and_deadline() {
        let p = pool();
        let repo = WebhookDeliveryRepo::new(p.clone());
        let hook = fixture(&p).await;
        let claim = record_claim(&repo, hook, "evt-breaker-defer").await;
        // Breaker deadlines are persisted as whole unix seconds.
        let deadline =
            OffsetDateTime::from_unix_timestamp(OffsetDateTime::now_utc().unix_timestamp() + 300)
                .unwrap();

        assert!(repo
            .defer_claim(
                claim.id,
                claim.claim_token,
                deadline,
                "circuit breaker open",
            )
            .await
            .unwrap());
        let deferred = repo.get(claim.id).await.unwrap().unwrap();
        assert_eq!(deferred.status, "failed");
        assert_eq!(deferred.attempts, 0);
        assert!(
            deferred.next_attempt_at.expect("defer deadline") >= deadline,
            "SQL deadline must never precede breaker.open_until"
        );
        assert!(
            repo.claim_due(deadline - Duration::seconds(1), 10)
                .await
                .unwrap()
                .iter()
                .all(|row| row.id != claim.id),
            "breaker-deferred row is not due early"
        );
        let reclaimed = repo
            .claim_due(deadline + Duration::seconds(1), 10)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.id == claim.id)
            .expect("row is due after breaker deadline");
        assert_eq!(reclaimed.attempts, 0, "defer did not consume a retry");
    }

    /// The counter advances exactly when each HTTP call begins: recording and
    /// retry claiming remain at 0/1, then `begin_attempt` moves 0→1 and 1→2.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn webhook_delivery_attempts_count_only_begun_http_calls() {
        let p = pool();
        let repo = WebhookDeliveryRepo::new(p.clone());
        let hook = fixture(&p).await;
        let first = record_claim(&repo, hook, "evt-attempt-count").await;
        assert_eq!(repo.get(first.id).await.unwrap().unwrap().attempts, 0);

        let first_attempt = begin_new_claim(&repo, first).await;
        assert_eq!(first_attempt, 1);
        assert!(repo
            .mark_failed_with_backoff(
                first.id,
                first.claim_token,
                first_attempt,
                Some(503),
                "retry",
            )
            .await
            .unwrap());

        let second = repo
            .claim_due(OffsetDateTime::now_utc() + Duration::days(365), 10)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.id == first.id)
            .expect("failed row is claimed");
        assert_eq!(second.attempts, 1, "claim itself is not a call");
        assert_eq!(begin_retry(&repo, &second).await, 2);
        assert!(repo
            .mark_delivered(second.id, second.claim_token, 204)
            .await
            .unwrap());
    }

    /// A delivered attempt is terminal and never surfaces for retry.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn webhook_delivery_delivered_is_terminal() {
        let p = pool();
        let repo = WebhookDeliveryRepo::new(p.clone());
        let hook = fixture(&p).await;

        let claim = repo
            .record_attempt(hook, None, b"{\"kind\":\"test\"}", &[])
            .await
            .unwrap()
            .expect("null event_id always claims");
        let id = claim.id;
        assert_eq!(begin_new_claim(&repo, claim).await, 1);
        assert!(repo
            .mark_delivered(id, claim.claim_token, 200)
            .await
            .unwrap());
        let row = repo.get(id).await.unwrap().unwrap();
        assert_eq!(row.status, "delivered");
        assert_eq!(row.last_status_code, Some(200));
        assert!(row.next_attempt_at.is_none());

        // It is not in the DLQ and not claimed by the retry loop.
        assert!(repo
            .list_dead(hook, None)
            .await
            .unwrap()
            .iter()
            .all(|d| d.id != id));
        let far = OffsetDateTime::now_utc() + Duration::days(365);
        assert!(repo
            .claim_due(far, 10)
            .await
            .unwrap()
            .iter()
            .all(|d| d.id != id));
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn webhook_delivery_request_is_immutable() {
        let p = pool();
        let repo = WebhookDeliveryRepo::new(p.clone());
        let hook = fixture(&p).await;
        let claim = repo
            .record_attempt(
                hook,
                Some("evt-immutable"),
                b"{\"body\":\"original\"}",
                &[("Content-Type".to_owned(), "application/json".to_owned())],
            )
            .await
            .unwrap()
            .expect("first attempt claims");
        let id = claim.id;

        let error = sqlx::query(
            r"UPDATE webhook_delivery_log
                  SET request_body = $2
                WHERE id = $1",
        )
        .bind(id.to_uuid())
        .bind(b"{\"body\":\"changed\"}".as_slice())
        .execute(&p)
        .await
        .expect_err("request body mutation must be rejected");
        assert_eq!(
            error
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .as_deref(),
            Some("23514")
        );

        let stored = repo.get(id).await.unwrap().unwrap();
        assert_eq!(
            stored.request_body.as_deref(),
            Some(b"{\"body\":\"original\"}".as_slice())
        );
    }

    /// `JetStream` redelivery (or a client retry) of the SAME event to the SAME
    /// endpoint must NOT trigger a second external POST: `record_attempt` claims
    /// `(webhook_id, event_id)` once (Some), and a redelivery is deduped (None) so
    /// the dispatcher skips it. NULL `event_id` (no correlation id) is never deduped.
    /// Guards migration 0150's partial unique index + the ON CONFLICT claim.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn record_attempt_is_idempotent_per_event() {
        let p = pool();
        let repo = WebhookDeliveryRepo::new(p.clone());
        let hook = fixture(&p).await;

        let first = repo
            .record_attempt(
                hook,
                Some("evt-dup"),
                b"{\"kind\":\"test\"}",
                &[("Content-Type".to_owned(), "application/json".to_owned())],
            )
            .await
            .unwrap();
        assert!(first.is_some(), "first delivery of an event claims");
        let second = repo
            .record_attempt(hook, Some("evt-dup"), b"{\"kind\":\"changed\"}", &[])
            .await
            .unwrap();
        assert!(
            second.is_none(),
            "redelivery of the same event must NOT claim again (no duplicate POST)"
        );
        let stored = repo.get(first.unwrap().id).await.unwrap().unwrap();
        assert_eq!(
            stored.request_body.as_deref(),
            Some(b"{\"kind\":\"test\"}".as_slice()),
            "a dedupe conflict must never replace the immutable original body"
        );
        assert_eq!(
            stored.request_headers,
            vec![("Content-Type".to_owned(), "application/json".to_owned())]
        );
        let other = repo
            .record_attempt(hook, Some("evt-other"), b"{\"kind\":\"test\"}", &[])
            .await
            .unwrap();
        assert!(
            other.is_some(),
            "a different event for the same hook still claims"
        );
        // NULL event_id carries no correlation id → never deduped.
        let n1 = repo
            .record_attempt(hook, None, b"{\"kind\":\"test\"}", &[])
            .await
            .unwrap();
        let n2 = repo
            .record_attempt(hook, None, b"{\"kind\":\"test\"}", &[])
            .await
            .unwrap();
        assert!(
            n1.is_some() && n2.is_some(),
            "null event_id always claims (no dedup)"
        );
    }
}
