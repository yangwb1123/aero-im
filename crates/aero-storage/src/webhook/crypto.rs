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

type HmacSha256 = Hmac<Sha256>;
// ---------------------------------------------------------------- Pure crypto

/// Number of random bytes behind a generated incoming-webhook token (256 bits).
pub(crate) const TOKEN_BYTES: usize = 32;

/// Generate a fresh, URL-safe-ish random token (lowercase hex of 32 random
/// bytes). Returned to the caller exactly once at creation; only its
/// [`hash_token`] is stored. Pure aside from RNG.
#[must_use]
pub fn generate_token() -> String {
    let mut buf = [0u8; TOKEN_BYTES];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// Generate a fresh per-hook HMAC secret (same shape/strength as a token).
#[must_use]
pub fn generate_secret() -> String {
    generate_token()
}

/// SHA-256 hex of a token. The incoming-webhook lookup keys on this so the
/// plaintext token never has to be stored. Deterministic + pure.
#[must_use]
pub fn hash_token(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    hex::encode(h.finalize())
}

/// HMAC-SHA256 over `"{timestamp}.{body}"`, returned in Slack-compatible
/// `v0={hex}` form. The timestamp is folded into the signed string so a captured
/// payload cannot be replayed under a different time (the receiver also checks
/// the `X-Aero-Timestamp` header freshness). Pure + deterministic for a fixed
/// `(secret, timestamp, body)`.
#[must_use]
pub fn sign_payload(secret: &str, timestamp: i64, body: &[u8]) -> String {
    // HMAC accepts a key of any length, so this never fails.
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    let tag = mac.finalize().into_bytes();
    format!("v0={}", hex::encode(tag))
}
