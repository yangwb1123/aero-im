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

use super::delivery::DeliveryResponse;

// ----------------------------------------------------- Circuit breaker (pure)

/// After this many consecutive failures the breaker opens (stops delivering until
/// the cooldown elapses).
pub const BREAKER_FAILURE_THRESHOLD: u32 = 5;
/// First open lasts this long; each further half-open failure doubles it…
pub const BREAKER_BASE_COOLDOWN_SECS: i64 = 60;
/// …capped here so a chronically-dead endpoint is still probed ~hourly.
pub const BREAKER_MAX_COOLDOWN_SECS: i64 = 3600;
/// A 429 trips the breaker immediately (before the failure threshold) for at least
/// this long — the endpoint explicitly asked us to slow down. Honors the *intent*
/// of `Retry-After` using only the status code the sender already surfaces.
pub const BREAKER_RATE_LIMIT_COOLDOWN_SECS: i64 = 120;

/// How one delivery attempt looks to the breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// Any 2xx.
    Success,
    /// A transport error or a non-2xx that isn't 429.
    Failure,
    /// The endpoint returned 429 Too Many Requests. `retry_after_secs` is the
    /// parsed `Retry-After` header when the receiver sent a usable integer-seconds
    /// value; `None` falls back to the default 429 cooldown. The breaker keeps the
    /// endpoint open until exactly `now + retry_after_secs` (clamped).
    RateLimited { retry_after_secs: Option<i64> },
}

/// Per-endpoint circuit-breaker state. Pure value type: every transition is a
/// total function of the prior state, the outcome, and an injected `now`, so the
/// whole state machine is unit-tested without a clock, DB, or network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BreakerState {
    /// Consecutive failures since the last success (reset to 0 on any 2xx).
    pub failures: u32,
    /// Unix seconds until which the breaker is open; `None` = closed. While `now`
    /// is below this the dispatcher skips the endpoint.
    pub open_until: Option<i64>,
}

impl BreakerState {
    /// Is the breaker open at `now` (so the caller should SKIP sending)? Once
    /// `open_until` has passed the breaker is *half-open*: this returns `false`, so
    /// the caller sends exactly one probe whose outcome re-opens or closes it.
    #[must_use]
    pub fn is_open_at(&self, now: i64) -> bool {
        matches!(self.open_until, Some(t) if now < t)
    }

    /// Fold one delivery outcome into the next state. Pure.
    ///
    /// A non-success outcome NEVER shortens an existing open window — the gate keeps
    /// whichever `open_until` is later (see [`later_gate`]). This makes the fold safe
    /// to apply on a freshly-opened state: e.g. a concurrent 429 that set a long
    /// cooldown can't be walked back to a shorter generic-failure cooldown by a
    /// delivery whose read straddled the 429. (Persistence applies this fold under a
    /// row lock — see [`WebhookRepo::apply_breaker_outcome`] — so the read+fold+write
    /// is atomic per endpoint.)
    #[must_use]
    pub fn after(self, outcome: DeliveryOutcome, now: i64) -> BreakerState {
        match outcome {
            // Any success fully closes the breaker (a 2xx means the endpoint is up).
            DeliveryOutcome::Success => BreakerState {
                failures: 0,
                open_until: None,
            },
            // 429: back off immediately for the requested (or default) cooldown,
            // regardless of how few failures preceded it.
            DeliveryOutcome::RateLimited { retry_after_secs } => {
                let cooldown = retry_after_secs
                    .unwrap_or(BREAKER_RATE_LIMIT_COOLDOWN_SECS)
                    .clamp(1, BREAKER_MAX_COOLDOWN_SECS);
                BreakerState {
                    failures: self.failures.saturating_add(1),
                    open_until: later_gate(self.open_until, Some(now + cooldown)),
                }
            }
            // Generic failure: count it; once we reach the threshold, open with an
            // exponential cooldown by how far past the threshold we are (capped).
            DeliveryOutcome::Failure => {
                let failures = self.failures.saturating_add(1);
                let opened = if failures >= BREAKER_FAILURE_THRESHOLD {
                    let over = (failures - BREAKER_FAILURE_THRESHOLD).min(6);
                    let cooldown =
                        (BREAKER_BASE_COOLDOWN_SECS << over).min(BREAKER_MAX_COOLDOWN_SECS);
                    Some(now + cooldown)
                } else {
                    // Still below threshold: don't open on our own account; the
                    // delivery-log layer's per-delivery backoff handles early retries.
                    None
                };
                BreakerState {
                    failures,
                    open_until: later_gate(self.open_until, opened),
                }
            }
        }
    }
}

/// The later of two optional gate deadlines (`None` = "no gate", treated as
/// earliest). Used so a non-success fold never SHORTENS an existing open window.
#[must_use]
fn later_gate(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) => Some(x),
        (None, b) => b,
    }
}

/// Map a delivery result (the `WebhookSender::deliver` return) into the breaker's
/// view of it. `Ok(2xx)` → success, `Ok(429)` → rate-limited (carrying the
/// response's parsed `Retry-After`), anything else → failure. Pure, so the
/// classification is unit-tested directly.
#[must_use]
pub fn outcome_of(result: &Result<DeliveryResponse, String>) -> DeliveryOutcome {
    match result {
        Ok(r) if (200..300).contains(&r.status) => DeliveryOutcome::Success,
        Ok(r) if r.status == 429 => DeliveryOutcome::RateLimited {
            retry_after_secs: r.retry_after_secs,
        },
        Ok(_) | Err(_) => DeliveryOutcome::Failure,
    }
}

/// Rebuild a [`BreakerState`] from its two stored columns. A negative
/// `breaker_failures` (impossible under the schema's `DEFAULT 0`, but cheap to
/// guard) clamps to 0; the timestamp becomes unix seconds.
#[must_use]
pub(crate) fn breaker_from_row(
    failures: i32,
    open_until: Option<time::OffsetDateTime>,
) -> BreakerState {
    BreakerState {
        failures: u32::try_from(failures).unwrap_or(0),
        open_until: open_until.map(|t| t.unix_timestamp()),
    }
}

/// Decide whether an outgoing hook with `filter` (its `events` array) should fire
/// for an event of kind `event_kind`. An empty filter means "all events"; a
/// non-empty filter fires only on an exact membership match. Pure, so the
/// filter semantics are unit-tested without a DB.
#[must_use]
pub fn event_matches(filter: &[String], event_kind: &str) -> bool {
    filter.is_empty() || filter.iter().any(|e| e == event_kind)
}
