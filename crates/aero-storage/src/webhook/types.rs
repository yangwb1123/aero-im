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

use aero_common::{ParticipantId, RoomId, WebhookId};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use super::breaker::BreakerState;

type HmacSha256 = Hmac<Sha256>;
// ------------------------------------------------------------------ Row types

/// A resolved incoming webhook (what the inbound POST path needs to act).
#[derive(Debug, Clone, Serialize)]
pub struct IncomingHook {
    pub id: WebhookId,
    pub room_id: RoomId,
    pub bot_id: ParticipantId,
    /// True once revoked — the inbound path must reject these as if unknown.
    pub revoked: bool,
}

/// A listing row for an incoming webhook (never carries the token/hash).
#[derive(Debug, Clone, Serialize)]
pub struct IncomingHookSummary {
    pub id: WebhookId,
    pub room_id: RoomId,
    pub bot_id: ParticipantId,
    pub label: Option<String>,
    pub revoked: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// A listing row for an outgoing webhook (never carries the secret).
#[derive(Debug, Clone, Serialize)]
pub struct OutgoingHookSummary {
    pub id: WebhookId,
    pub room_id: RoomId,
    pub url: String,
    pub events: Vec<String>,
    pub label: Option<String>,
    pub revoked: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// An active outgoing delivery target (hook id + url + signing secret) for
/// dispatch. The `id` lets the dispatcher key each delivery into
/// `webhook_delivery_log` (retry/DLQ — see [`crate::webhook_delivery`]).
#[derive(Debug, Clone)]
pub struct OutgoingTarget {
    pub id: WebhookId,
    pub url: String,
    pub secret: String,
    /// Circuit-breaker state for this endpoint, loaded alongside the target so the
    /// dispatcher can skip a tripped endpoint without a second query.
    pub breaker: BreakerState,
}
