//! Webhook persistence + pure signing/delivery logic (integration plane).
//!
//! Backs `migrations/0013_webhooks.sql`. Two directions:
//!
//! * **Incoming** — an external system holds a bearer *token* and POSTs to
//!   `/hooks/in/:token`; the server posts the body into a room as a dedicated
//!   bot. Only the SHA-256 *hash* of the token is stored ([`hash_token`]); the
//!   plaintext is generated once at creation ([`generate_token`]) and returned to
//!   the caller, never persisted.
//! * **Outgoing** — the server POSTs each matching [`aero_common::RoomEvent`] to
//!   an external URL, HMAC-SHA256 signed with a per-hook secret so the receiver
//!   can verify authenticity ([`sign_payload`] / [`build_delivery`]).
//!
//! ## Testable seams (DB-free, unit-tested)
//!
//! The signing ([`sign_payload`]) and request-shaping ([`build_delivery`]) are
//! pure functions, and the HTTP POST hides behind the [`WebhookSender`] trait so
//! delivery can be exercised offline via [`FakeSender`] (Postgres + the network
//! are both absent in CI). [`ReqwestSender`] is the real transport.
//!
//! Purely additive: a NEW [`WebhookRepo`]; no existing repo is touched.

#![allow(unused_imports)]
use aero_common::{ParticipantId, RoomId, WebhookId};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use super::breaker::*;
use super::crypto::*;
use super::delivery::*;
use super::types::*;

// ------------------------------------------------------------------ The repo

#[derive(Clone)]
pub struct WebhookRepo {
    pool: PgPool,
}

impl WebhookRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    // ---- Incoming ----

    /// Create an incoming webhook bound to `room` posting as `bot_id`, storing
    /// only `token_hash`. Returns the generated id.
    pub async fn create_incoming(
        &self,
        room: RoomId,
        bot_id: ParticipantId,
        token_hash: &str,
        label: Option<&str>,
        created_by: ParticipantId,
    ) -> Result<WebhookId, sqlx::Error> {
        let id = WebhookId::new();
        sqlx::query(
            r"INSERT INTO incoming_webhooks (id, room_id, bot_id, token_hash, label, created_by)
               VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(bot_id.to_uuid())
        .bind(token_hash)
        .bind(label)
        .bind(created_by.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Resolve an incoming webhook by the SHA-256 hash of its presented token.
    /// `None` when no row matches. The `revoked` flag lets the caller reject a
    /// revoked hook with the same 404 it gives an unknown token.
    pub async fn find_incoming_by_token_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<IncomingHook>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, uuid::Uuid, Option<time::OffsetDateTime>)>(
            r"SELECT id, room_id, bot_id, revoked_at
               FROM incoming_webhooks
               WHERE token_hash = $1",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id, room, bot, revoked_at)| IncomingHook {
            id: WebhookId::from_uuid(id),
            room_id: RoomId::from_uuid(room),
            bot_id: ParticipantId::from_uuid(bot),
            revoked: revoked_at.is_some(),
        }))
    }

    /// List a room's incoming webhooks (no token/hash), newest first.
    pub async fn list_incoming(
        &self,
        room: RoomId,
    ) -> Result<Vec<IncomingHookSummary>, sqlx::Error> {
        let rows = sqlx::query_as::<
            _,
            (uuid::Uuid, uuid::Uuid, uuid::Uuid, Option<String>, Option<time::OffsetDateTime>, time::OffsetDateTime),
        >(
            r"SELECT id, room_id, bot_id, label, revoked_at, created_at
               FROM incoming_webhooks
               WHERE room_id = $1
               ORDER BY id DESC",
        )
        .bind(room.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, room, bot, label, revoked_at, created_at)| IncomingHookSummary {
                id: WebhookId::from_uuid(id),
                room_id: RoomId::from_uuid(room),
                bot_id: ParticipantId::from_uuid(bot),
                label,
                revoked: revoked_at.is_some(),
                created_at,
            })
            .collect())
    }

    /// Soft-revoke an incoming webhook (idempotent). Returns whether a live row
    /// was flipped.
    pub async fn revoke_incoming(&self, id: WebhookId) -> Result<bool, sqlx::Error> {
        let res = sqlx::query(
            r"UPDATE incoming_webhooks SET revoked_at = now()
               WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(id.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    // ---- Outgoing ----

    /// Create an outgoing webhook for `room`. `events` empty ⇒ all events.
    pub async fn create_outgoing(
        &self,
        room: RoomId,
        url: &str,
        secret: &str,
        events: &[String],
        label: Option<&str>,
        created_by: ParticipantId,
    ) -> Result<WebhookId, sqlx::Error> {
        let id = WebhookId::new();
        sqlx::query(
            r"INSERT INTO outgoing_webhooks (id, room_id, url, secret, events, label, created_by)
               VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(url)
        .bind(secret)
        .bind(events)
        .bind(label)
        .bind(created_by.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List a room's outgoing webhooks (no secret), newest first.
    pub async fn list_outgoing(
        &self,
        room: RoomId,
    ) -> Result<Vec<OutgoingHookSummary>, sqlx::Error> {
        let rows = sqlx::query_as::<
            _,
            (uuid::Uuid, uuid::Uuid, String, Vec<String>, Option<String>, Option<time::OffsetDateTime>, time::OffsetDateTime),
        >(
            r"SELECT id, room_id, url, events, label, revoked_at, created_at
               FROM outgoing_webhooks
               WHERE room_id = $1
               ORDER BY id DESC",
        )
        .bind(room.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, room, url, events, label, revoked_at, created_at)| OutgoingHookSummary {
                id: WebhookId::from_uuid(id),
                room_id: RoomId::from_uuid(room),
                url,
                events,
                label,
                revoked: revoked_at.is_some(),
                created_at,
            })
            .collect())
    }

    /// Active outgoing targets for a room whose filter matches `event_kind`
    /// (empty filter ⇒ matches every kind). Revoked hooks are excluded. This is
    /// the dispatcher's hot query, so the event-filter match is pushed into SQL.
    pub async fn list_outgoing_for_room_event(
        &self,
        room: RoomId,
        event_kind: &str,
    ) -> Result<Vec<OutgoingTarget>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid, String, String, i32, Option<time::OffsetDateTime>)>(
            r"SELECT id, url, secret, breaker_failures, breaker_open_until
               FROM outgoing_webhooks
               WHERE room_id = $1
                 AND revoked_at IS NULL
                 AND (cardinality(events) = 0 OR $2 = ANY(events))",
        )
        .bind(room.to_uuid())
        .bind(event_kind)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, url, secret, failures, open_until)| OutgoingTarget {
                id: WebhookId::from_uuid(id),
                url,
                secret,
                breaker: breaker_from_row(failures, open_until),
            })
            .collect())
    }

    /// Resolve the `room_id` an outgoing webhook belongs to (so an admin route can
    /// authorize a delivery action via the room's workspace). `None` when the hook
    /// does not exist.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn outgoing_room(&self, id: WebhookId) -> Result<Option<RoomId>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT room_id FROM outgoing_webhooks WHERE id = $1",
        )
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(room,)| RoomId::from_uuid(room)))
    }

    /// Resolve the owning `room_id` **and** the dedicated bot participant of an
    /// inbound webhook (so a management route can authorize a revoke via the room's
    /// access — mirroring [`Self::outgoing_room`] — and clean the bot out of the
    /// room on revoke). `None` when the hook does not exist.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn incoming_room_and_bot(
        &self,
        id: WebhookId,
    ) -> Result<Option<(RoomId, ParticipantId)>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid)>(
            r"SELECT room_id, bot_id FROM incoming_webhooks WHERE id = $1",
        )
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(room, bot)| (RoomId::from_uuid(room), ParticipantId::from_uuid(bot))))
    }

    /// Resolve a single **active** (non-revoked) outgoing target (url + secret) by
    /// id, for the retry loop to re-send a logged delivery. `None` when the hook is
    /// unknown or revoked (a revoked hook is not redelivered).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn outgoing_target(&self, id: WebhookId) -> Result<Option<OutgoingTarget>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid, String, String, i32, Option<time::OffsetDateTime>)>(
            r"SELECT id, url, secret, breaker_failures, breaker_open_until
               FROM outgoing_webhooks
               WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id, url, secret, failures, open_until)| OutgoingTarget {
            id: WebhookId::from_uuid(id),
            url,
            secret,
            breaker: breaker_from_row(failures, open_until),
        }))
    }

    /// Persist a recomputed circuit-breaker state for one endpoint after a
    /// delivery attempt. `open_until` is `None` when the breaker is closed.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn record_breaker(
        &self,
        id: WebhookId,
        state: BreakerState,
    ) -> Result<(), sqlx::Error> {
        let open_until = match state.open_until {
            Some(secs) => Some(
                time::OffsetDateTime::from_unix_timestamp(secs)
                    .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
            ),
            None => None,
        };
        sqlx::query(
            r"UPDATE outgoing_webhooks
                 SET breaker_failures = $2, breaker_open_until = $3
               WHERE id = $1",
        )
        .bind(id.to_uuid())
        .bind(i32::try_from(state.failures).unwrap_or(i32::MAX))
        .bind(open_until)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Atomically fold one delivery `outcome` into an endpoint's circuit-breaker
    /// state and persist it, returning the new state.
    ///
    /// Unlike a read-then-[`record_breaker`](Self::record_breaker) write, this locks
    /// the row (`SELECT … FOR UPDATE`) and folds over the CURRENT persisted state, so
    /// concurrent/batched deliveries to the same endpoint serialize. That closes the
    /// read-modify-write race: failures can't be undercounted (so the breaker trips
    /// on time), a stale success can't reset a gate another delivery just opened
    /// against fresher information, and a stale generic failure can't shorten a 429's
    /// longer cooldown (the fold itself also never shortens an open window — see
    /// [`BreakerState::after`]). A `None` row (hook deleted concurrently) is a no-op.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn apply_breaker_outcome(
        &self,
        id: WebhookId,
        outcome: DeliveryOutcome,
        now: i64,
    ) -> Result<BreakerState, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query_as::<_, (i32, Option<time::OffsetDateTime>)>(
            r"SELECT breaker_failures, breaker_open_until
                 FROM outgoing_webhooks WHERE id = $1 FOR UPDATE",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some((failures, open_until)) = row else {
            tx.rollback().await?;
            return Ok(BreakerState::default());
        };
        let current = breaker_from_row(failures, open_until);
        let next = current.after(outcome, now);
        if next != current {
            let open_until_ts = match next.open_until {
                Some(secs) => Some(
                    time::OffsetDateTime::from_unix_timestamp(secs)
                        .map_err(|e| sqlx::Error::Decode(Box::new(e)))?,
                ),
                None => None,
            };
            sqlx::query(
                r"UPDATE outgoing_webhooks
                     SET breaker_failures = $2, breaker_open_until = $3
                   WHERE id = $1",
            )
            .bind(id.to_uuid())
            .bind(i32::try_from(next.failures).unwrap_or(i32::MAX))
            .bind(open_until_ts)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(next)
    }

    /// Soft-revoke an outgoing webhook (idempotent). Returns whether a live row
    /// was flipped.
    pub async fn revoke_outgoing(&self, id: WebhookId) -> Result<bool, sqlx::Error> {
        let res = sqlx::query(
            r"UPDATE outgoing_webhooks SET revoked_at = now()
               WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(id.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- sign_payload: stable, known-vector, body-sensitive -----

    #[test]
    fn sign_payload_is_stable_and_known_vector() {
        // Fixed secret/timestamp/body ⇒ exact, stable signature. Pinned to a
        // vector computed independently (HMAC-SHA256 over "1700000000.hello world"
        // keyed with "shhh", verified via `openssl dgst -sha256 -hmac shhh`).
        // HMAC-SHA256 is deterministic so this must never drift.
        let sig = sign_payload("shhh", 1_700_000_000, b"hello world");
        assert_eq!(
            sig,
            "v0=1deaa0f017994de229ea76bdc11d5b2b157da142b275a41629b27b9f26b3cdfa"
        );
        // Slack-style prefix.
        assert!(sig.starts_with("v0="));
        // 32-byte tag ⇒ 64 hex chars after the prefix.
        assert_eq!(sig.len(), 3 + 64);
        // Recomputation is identical (purity).
        assert_eq!(sig, sign_payload("shhh", 1_700_000_000, b"hello world"));
    }

    #[test]
    fn sign_payload_changes_when_body_changes() {
        let a = sign_payload("k", 1, b"a");
        let b = sign_payload("k", 1, b"b");
        assert_ne!(a, b, "a different body must change the signature");
        // Timestamp and secret are also bound into the signature.
        assert_ne!(sign_payload("k", 1, b"a"), sign_payload("k", 2, b"a"));
        assert_ne!(sign_payload("k1", 1, b"a"), sign_payload("k2", 1, b"a"));
    }

    // ----- hash_token / generate_token -----

    #[test]
    fn hash_token_is_deterministic_and_known_vector() {
        // SHA-256("aero") — a fixed, well-known vector (verified via `sha256sum`).
        assert_eq!(
            hash_token("aero"),
            "101a07e5f182ba079644cac91eff0a8586945de26c519ad0736147a74094e275"
        );
        assert_eq!(hash_token("x"), hash_token("x"));
        assert_ne!(hash_token("x"), hash_token("y"));
        // 32-byte digest ⇒ 64 hex chars.
        assert_eq!(hash_token("anything").len(), 64);
    }

    #[test]
    fn generated_tokens_are_random_and_hex() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b, "tokens must be unpredictable");
        assert_eq!(a.len(), TOKEN_BYTES * 2, "hex of 32 bytes");
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        // A token never equals its own stored form (so a DB leak ≠ a usable token).
        assert_ne!(hash_token(&a), a);
    }

    // ----- build_delivery -----

    #[test]
    fn build_delivery_signs_body_and_sets_headers() {
        let event = serde_json::json!({ "kind": "message", "room_id": "r1" });
        let now = 1_700_000_123;
        let d = build_delivery("https://example.test/hook", "sekret", &event, now);

        assert_eq!(d.url, "https://example.test/hook");
        // Content-Type is JSON.
        assert_eq!(d.header("Content-Type"), Some("application/json"));
        // Timestamp header is the stamp we passed.
        assert_eq!(d.header(TIMESTAMP_HEADER), Some(now.to_string().as_str()));
        // Signature header is present and matches re-signing the EXACT body bytes —
        // i.e. a receiver re-running sign_payload over what it received verifies.
        let sig = d.header(SIGNATURE_HEADER).expect("signature header present");
        assert_eq!(sig, sign_payload("sekret", now, &d.body));
        assert!(sig.starts_with("v0="));
        // The body is the serialized event.
        assert_eq!(d.body, serde_json::to_vec(&event).unwrap());
    }

    #[test]
    fn build_delivery_header_lookup_is_case_insensitive() {
        let d = build_delivery("u", "s", &serde_json::json!({}), 0);
        assert!(d.header("x-aero-signature").is_some());
        assert!(d.header("X-AERO-TIMESTAMP").is_some());
        assert!(d.header("content-type").is_some());
        assert!(d.header("nope").is_none());
    }

    // ----- event_matches -----

    #[test]
    fn event_matches_empty_filter_is_all() {
        assert!(event_matches(&[], "message"));
        assert!(event_matches(&[], "anything"));
    }

    #[test]
    fn event_matches_non_empty_filter_requires_membership() {
        let filter = vec!["message".to_string(), "edited".to_string()];
        assert!(event_matches(&filter, "message"));
        assert!(event_matches(&filter, "edited"));
        assert!(!event_matches(&filter, "reaction"));
        assert!(!event_matches(&filter, "typing"));
    }

    // ----- FakeSender + delivery via the trait seam -----

    #[tokio::test]
    async fn fake_sender_captures_calls_and_returns_status() {
        let sender = FakeSender::new(202);
        let d = build_delivery("https://h", "s", &serde_json::json!({ "a": 1 }), 7);
        let resp = sender.deliver(&d).await.unwrap();
        assert_eq!(resp.status, 202);
        // No Retry-After by default.
        assert_eq!(resp.retry_after_secs, None);
        let calls = sender.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], d);
        // A second delivery accumulates.
        sender.deliver(&d).await.unwrap();
        assert_eq!(sender.calls().len(), 2);
    }

    #[tokio::test]
    async fn fake_sender_can_surface_a_canned_retry_after() {
        // The builder pairs a 429 with an explicit Retry-After the breaker honors.
        let sender = FakeSender::new(429).with_retry_after(300);
        let d = build_delivery("https://h", "s", &serde_json::json!({}), 0);
        let resp = sender.deliver(&d).await.unwrap();
        assert_eq!(resp.status, 429);
        assert_eq!(resp.retry_after_secs, Some(300));
    }

    // ----- parse_retry_after -----

    #[test]
    fn parse_retry_after_accepts_delta_seconds_only() {
        // Integer delta-seconds parse; 0 is valid ("retry immediately").
        assert_eq!(parse_retry_after("120"), Some(120));
        assert_eq!(parse_retry_after("0"), Some(0));
        // Surrounding whitespace is tolerated.
        assert_eq!(parse_retry_after("  90  "), Some(90));
        // Negative, garbage, empty, and the HTTP-date form all yield None.
        assert_eq!(parse_retry_after("-5"), None);
        assert_eq!(parse_retry_after("soon"), None);
        assert_eq!(parse_retry_after(""), None);
        assert_eq!(parse_retry_after("Wed, 21 Oct 2025 07:28:00 GMT"), None);
    }

    // ----- Circuit breaker (pure state machine) -----

    #[test]
    fn outcome_classifies_status_codes() {
        assert_eq!(outcome_of(&Ok(DeliveryResponse::new(200))), DeliveryOutcome::Success);
        assert_eq!(outcome_of(&Ok(DeliveryResponse::new(204))), DeliveryOutcome::Success);
        // A 429 with no Retry-After ⇒ rate-limited with None (default cooldown).
        assert_eq!(
            outcome_of(&Ok(DeliveryResponse::new(429))),
            DeliveryOutcome::RateLimited { retry_after_secs: None }
        );
        assert_eq!(outcome_of(&Ok(DeliveryResponse::new(500))), DeliveryOutcome::Failure);
        assert_eq!(outcome_of(&Ok(DeliveryResponse::new(404))), DeliveryOutcome::Failure);
        assert_eq!(
            outcome_of(&Err("timeout".to_string())),
            DeliveryOutcome::Failure
        );
    }

    #[test]
    fn outcome_surfaces_retry_after_on_429() {
        // A 429 carrying a parsed Retry-After flows straight through to the breaker
        // outcome, so the cooldown can be honored exactly (not just the default).
        let resp = DeliveryResponse { status: 429, retry_after_secs: Some(90) };
        assert_eq!(
            outcome_of(&Ok(resp)),
            DeliveryOutcome::RateLimited { retry_after_secs: Some(90) }
        );
        // A non-429 ignores any (spurious) Retry-After it might carry.
        let resp = DeliveryResponse { status: 503, retry_after_secs: Some(90) };
        assert_eq!(outcome_of(&Ok(resp)), DeliveryOutcome::Failure);
    }

    #[test]
    fn breaker_opens_until_exactly_now_plus_retry_after() {
        // End-to-end of the plumbed value: a 429 with Retry-After: 300 makes the
        // breaker open until exactly now + 300 (reusing the breaker-test style).
        let now = 10_000;
        let resp: Result<DeliveryResponse, String> =
            Ok(DeliveryResponse { status: 429, retry_after_secs: Some(300) });
        let s = BreakerState::default().after(outcome_of(&resp), now);
        assert_eq!(s.open_until, Some(now + 300));
        assert!(s.is_open_at(now + 299), "still open just before the deadline");
        assert!(!s.is_open_at(now + 300), "half-open exactly at now + retry_after");
    }

    #[test]
    fn breaker_stays_closed_below_threshold() {
        let mut s = BreakerState::default();
        // Up to THRESHOLD-1 failures: failures count climbs but the breaker is closed.
        for n in 1..BREAKER_FAILURE_THRESHOLD {
            s = s.after(DeliveryOutcome::Failure, 1000);
            assert_eq!(s.failures, n);
            assert!(s.open_until.is_none(), "closed below threshold");
            assert!(!s.is_open_at(1000));
        }
    }

    #[test]
    fn breaker_opens_at_threshold_with_base_cooldown() {
        let mut s = BreakerState::default();
        for _ in 0..BREAKER_FAILURE_THRESHOLD {
            s = s.after(DeliveryOutcome::Failure, 1000);
        }
        assert_eq!(s.failures, BREAKER_FAILURE_THRESHOLD);
        assert_eq!(s.open_until, Some(1000 + BREAKER_BASE_COOLDOWN_SECS));
        assert!(s.is_open_at(1000), "open right after tripping");
        assert!(s.is_open_at(1000 + BREAKER_BASE_COOLDOWN_SECS - 1), "still open mid-cooldown");
        assert!(!s.is_open_at(1000 + BREAKER_BASE_COOLDOWN_SECS), "half-open once elapsed");
    }

    #[test]
    fn breaker_cooldown_grows_exponentially_and_caps() {
        // One step past threshold is exactly 2× the base.
        let two = BreakerState { failures: BREAKER_FAILURE_THRESHOLD, open_until: None }
            .after(DeliveryOutcome::Failure, 0);
        assert_eq!(two.open_until, Some(BREAKER_BASE_COOLDOWN_SECS * 2));
        // Drive well past the threshold; the cooldown saturates at the max.
        let mut s = BreakerState::default();
        for _ in 0..20 {
            s = s.after(DeliveryOutcome::Failure, 0);
        }
        assert_eq!(
            s.open_until,
            Some(BREAKER_MAX_COOLDOWN_SECS),
            "cooldown saturates at the cap"
        );
    }

    #[test]
    fn success_fully_closes_the_breaker() {
        let open = BreakerState { failures: 9, open_until: Some(5000) };
        let closed = open.after(DeliveryOutcome::Success, 4000);
        assert_eq!(closed, BreakerState { failures: 0, open_until: None });
        assert!(!closed.is_open_at(4000));
    }

    #[test]
    fn rate_limited_trips_immediately_before_threshold() {
        // A single 429 from a healthy endpoint opens the breaker at once (the server
        // explicitly asked us to slow down) — no need to reach the failure threshold.
        let s = BreakerState::default().after(
            DeliveryOutcome::RateLimited { retry_after_secs: None },
            2000,
        );
        assert_eq!(s.failures, 1);
        assert_eq!(s.open_until, Some(2000 + BREAKER_RATE_LIMIT_COOLDOWN_SECS));
        assert!(s.is_open_at(2000));
    }

    #[test]
    fn rate_limited_honors_retry_after_when_present() {
        let s = BreakerState::default().after(
            DeliveryOutcome::RateLimited { retry_after_secs: Some(900) },
            0,
        );
        assert_eq!(s.open_until, Some(900));
        // A retry-after beyond the cap is clamped.
        let capped = BreakerState::default().after(
            DeliveryOutcome::RateLimited { retry_after_secs: Some(99_999) },
            0,
        );
        assert_eq!(capped.open_until, Some(BREAKER_MAX_COOLDOWN_SECS));
    }

    #[test]
    fn half_open_probe_failure_reopens_longer() {
        // Open at threshold (cooldown = base), elapse to half-open, then fail the
        // probe → re-opens with a longer (2× base) cooldown.
        let mut s = BreakerState::default();
        for _ in 0..BREAKER_FAILURE_THRESHOLD {
            s = s.after(DeliveryOutcome::Failure, 0);
        }
        let elapsed = BREAKER_BASE_COOLDOWN_SECS; // half-open boundary
        assert!(!s.is_open_at(elapsed));
        let reopened = s.after(DeliveryOutcome::Failure, elapsed);
        assert_eq!(reopened.open_until, Some(elapsed + BREAKER_BASE_COOLDOWN_SECS * 2));
        assert!(reopened.is_open_at(elapsed));
    }

    #[test]
    fn non_success_fold_never_shortens_an_existing_gate() {
        // A 429 sets a long cooldown; a (concurrent/stale) generic failure folded on
        // top must NOT walk it back to the shorter generic cooldown — the receiver's
        // explicit back-off survives. Guards the concurrency-race fix.
        let after_429 = BreakerState::default()
            .after(DeliveryOutcome::RateLimited { retry_after_secs: Some(3000) }, 0);
        assert_eq!(after_429.open_until, Some(3000));
        let after_fail = after_429.after(DeliveryOutcome::Failure, 10);
        assert_eq!(after_fail.open_until, Some(3000), "longer 429 gate preserved, not shortened");
        assert!(after_fail.is_open_at(2999), "still open for the full 429 window");
        // Symmetric: a shorter 429 can't shorten a longer existing generic gate.
        let mut long = BreakerState::default();
        for _ in 0..(BREAKER_FAILURE_THRESHOLD + 6) {
            long = long.after(DeliveryOutcome::Failure, 0); // grows to the cap (3600)
        }
        assert_eq!(long.open_until, Some(BREAKER_MAX_COOLDOWN_SECS));
        let after_short_429 = long.after(DeliveryOutcome::RateLimited { retry_after_secs: Some(5) }, 0);
        assert_eq!(after_short_429.open_until, Some(BREAKER_MAX_COOLDOWN_SECS), "kept the longer gate");
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored webhook_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{ParticipantId, RoomId, WorkspaceId};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    // A throwaway participant + workspace + room so each test is self-contained.
    async fn fixture(p: &PgPool) -> (RoomId, ParticipantId) {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(actor.to_uuid())
            .bind(format!("wh-actor-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");
        let ws = WorkspaceId::new();
        sqlx::query("INSERT INTO workspaces (id, name, slug, created_by, created_at) VALUES ($1,$2,$3,$4, now())")
            .bind(ws.to_uuid())
            .bind("WH Test WS")
            .bind(format!("wh-{ws}"))
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
        (room, actor)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn webhook_incoming_create_find_revoke() {
        let p = pool();
        let repo = WebhookRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;

        let token = generate_token();
        let hash = hash_token(&token);
        let id = repo
            .create_incoming(room, actor, &hash, Some("CI hook"), actor)
            .await
            .unwrap();

        // Found by the hash of the presented token, active.
        let found = repo.find_incoming_by_token_hash(&hash).await.unwrap().expect("hook exists");
        assert_eq!(found.id, id);
        assert_eq!(found.room_id, room);
        assert!(!found.revoked);

        // A different token's hash resolves to nothing.
        assert!(repo.find_incoming_by_token_hash(&hash_token("wrong")).await.unwrap().is_none());

        // Listing surfaces it (without token/hash).
        let list = repo.list_incoming(room).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);

        // Revoke flips the flag; the lookup now reports revoked.
        assert!(repo.revoke_incoming(id).await.unwrap());
        let after = repo.find_incoming_by_token_hash(&hash).await.unwrap().unwrap();
        assert!(after.revoked);
        // Idempotent: a second revoke flips nothing.
        assert!(!repo.revoke_incoming(id).await.unwrap());
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn webhook_outgoing_create_list_and_filter() {
        let p = pool();
        let repo = WebhookRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;

        // One all-events hook, one filtered to {message}.
        let secret_all = generate_secret();
        repo.create_outgoing(room, "https://all.test", &secret_all, &[], Some("all"), actor)
            .await
            .unwrap();
        let secret_msg = generate_secret();
        let filtered_id = repo
            .create_outgoing(
                room,
                "https://msg.test",
                &secret_msg,
                &["message".to_string()],
                Some("msg-only"),
                actor,
            )
            .await
            .unwrap();

        // Listing (no secrets) shows both.
        let list = repo.list_outgoing(room).await.unwrap();
        assert_eq!(list.len(), 2);

        // A "message" event hits both targets.
        let on_message = repo.list_outgoing_for_room_event(room, "message").await.unwrap();
        assert_eq!(on_message.len(), 2);
        assert!(on_message.iter().any(|t| t.url == "https://all.test"));
        assert!(on_message.iter().any(|t| t.url == "https://msg.test"));

        // A "reaction" event hits only the all-events hook.
        let on_reaction = repo.list_outgoing_for_room_event(room, "reaction").await.unwrap();
        assert_eq!(on_reaction.len(), 1);
        assert_eq!(on_reaction[0].url, "https://all.test");

        // Revoking the filtered hook removes it from dispatch.
        assert!(repo.revoke_outgoing(filtered_id).await.unwrap());
        let on_message_after = repo.list_outgoing_for_room_event(room, "message").await.unwrap();
        assert_eq!(on_message_after.len(), 1);
        assert_eq!(on_message_after[0].url, "https://all.test");
    }

    /// The circuit-breaker columns (0129) round-trip: a fresh hook loads closed
    /// (0 failures, no open_until); `record_breaker` persists an open state that
    /// both load paths (`list_outgoing_for_room_event` + `outgoing_target`) read
    /// back; a subsequent close clears it.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn webhook_breaker_state_round_trips() {
        let p = pool();
        let repo = WebhookRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;

        let secret = generate_secret();
        let id = repo
            .create_outgoing(room, "https://brk.test", &secret, &[], Some("brk"), actor)
            .await
            .unwrap();

        // Fresh hook: breaker closed.
        let fresh = repo.outgoing_target(id).await.unwrap().expect("target");
        assert_eq!(fresh.breaker, BreakerState::default());
        assert!(!fresh.breaker.is_open_at(1_000_000));

        // Open it (5 failures, open until t=2_000_000) and read back via BOTH paths.
        let open = BreakerState { failures: 5, open_until: Some(2_000_000) };
        repo.record_breaker(id, open).await.unwrap();

        let via_target = repo.outgoing_target(id).await.unwrap().expect("target");
        assert_eq!(via_target.breaker, open);
        assert!(via_target.breaker.is_open_at(1_999_999));
        assert!(!via_target.breaker.is_open_at(2_000_000));

        let via_list = repo.list_outgoing_for_room_event(room, "message").await.unwrap();
        let hit = via_list.iter().find(|t| t.id == id).expect("listed");
        assert_eq!(hit.breaker, open);

        // Close it again (success) → both columns reset.
        repo.record_breaker(id, BreakerState::default()).await.unwrap();
        let closed = repo.outgoing_target(id).await.unwrap().expect("target");
        assert_eq!(closed.breaker, BreakerState::default());
    }

    /// `apply_breaker_outcome` folds over the CURRENT persisted row (not a passed-in
    /// snapshot), so repeated failures ACCUMULATE rather than each writing
    /// `snapshot+1`. This is the property that defeats the read-modify-write lost
    /// update: N folds advance the counter by N, and a success resets it.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn apply_breaker_outcome_folds_fresh_state_and_accumulates() {
        let p = pool();
        let repo = WebhookRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;
        let id = repo
            .create_outgoing(room, "https://atomic.test", &generate_secret(), &[], Some("a"), actor)
            .await
            .unwrap();

        // Fold THRESHOLD generic failures; each reads the fresh row, so the count
        // climbs 1..=THRESHOLD and the breaker opens exactly at the threshold.
        let mut last = BreakerState::default();
        for _ in 0..BREAKER_FAILURE_THRESHOLD {
            last = repo.apply_breaker_outcome(id, DeliveryOutcome::Failure, 1_000).await.unwrap();
        }
        assert_eq!(last.failures, BREAKER_FAILURE_THRESHOLD, "N folds → N failures, not 1");
        assert!(last.is_open_at(1_000), "opened at the threshold");
        // The persisted row matches what the fold returned (it actually wrote).
        let persisted = repo.outgoing_target(id).await.unwrap().expect("target").breaker;
        assert_eq!(persisted, last);

        // A 429 with a long cooldown, then a generic failure: the longer gate must
        // survive the failure fold (anti-shortening, now persisted atomically).
        let rl = repo
            .apply_breaker_outcome(id, DeliveryOutcome::RateLimited { retry_after_secs: Some(3000) }, 2_000)
            .await
            .unwrap();
        assert_eq!(rl.open_until, Some(2_000 + 3000));
        let after_fail = repo.apply_breaker_outcome(id, DeliveryOutcome::Failure, 2_010).await.unwrap();
        assert_eq!(after_fail.open_until, Some(5_000), "429 cooldown not shortened by a later failure");

        // Success closes it.
        let ok = repo.apply_breaker_outcome(id, DeliveryOutcome::Success, 3_000).await.unwrap();
        assert_eq!(ok, BreakerState::default());
        assert_eq!(repo.outgoing_target(id).await.unwrap().expect("t").breaker, BreakerState::default());
    }
}
