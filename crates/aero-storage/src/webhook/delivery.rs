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

use super::crypto::sign_payload;

type HmacSha256 = Hmac<Sha256>;
// --------------------------------------------------------- Delivery (the seam)

/// Header name carrying the HMAC signature of a delivery.
pub const SIGNATURE_HEADER: &str = "X-Aero-Signature";
/// Header name carrying the unix timestamp the signature was computed over.
pub const TIMESTAMP_HEADER: &str = "X-Aero-Timestamp";

/// A fully-shaped outgoing HTTP request: where to POST, the headers (signature +
/// timestamp + content-type), and the JSON body. Produced by [`build_delivery`]
/// and consumed by a [`WebhookSender`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub url: String,
    /// Ordered `(name, value)` header pairs.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Delivery {
    /// Lookup a header value by case-insensitive name (small list, linear scan).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Build the signed delivery for `event_json` to `url` with `secret`, stamped at
/// `now` (unix seconds). Pure: same inputs ⇒ byte-identical request, so the
/// signature header is unit-testable without a clock or network. The signature
/// is computed over the *exact* JSON bytes that are sent, so a receiver
/// re-running [`sign_payload`] over the body it received reproduces it.
#[must_use]
pub fn build_delivery(url: &str, secret: &str, event_json: &serde_json::Value, now: i64) -> Delivery {
    // `to_vec` on a Value never fails; fall back to an empty object on the
    // theoretical error so the function stays total.
    let body = serde_json::to_vec(event_json).unwrap_or_else(|_| b"{}".to_vec());
    let signature = sign_payload(secret, now, &body);
    let headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        (SIGNATURE_HEADER.to_string(), signature),
        (TIMESTAMP_HEADER.to_string(), now.to_string()),
    ];
    Delivery { url: url.to_string(), headers, body }
}

/// What one completed round trip tells the caller: the HTTP status code plus the
/// parsed `Retry-After` cooldown (only meaningful on a 429). Produced by a
/// [`WebhookSender`] and consumed by the delivery-log bookkeeping (numeric
/// `status`) and the circuit breaker ([`outcome_of`] reads `retry_after_secs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryResponse {
    /// The HTTP status code of the completed request.
    pub status: u16,
    /// The receiver's `Retry-After` cooldown in whole seconds, when it sent a
    /// usable integer-seconds value (typically alongside a 429). `None` when the
    /// header is absent, malformed, negative, or in the HTTP-date form (the latter
    /// is best-effort only).
    pub retry_after_secs: Option<i64>,
}

impl DeliveryResponse {
    /// A response carrying just a status (no `Retry-After`) — the common case and
    /// what test doubles / non-429 responses use.
    #[must_use]
    pub fn new(status: u16) -> Self {
        Self { status, retry_after_secs: None }
    }
}

/// Parse an HTTP `Retry-After` header value into whole seconds. Only the
/// delta-seconds form is supported (`"120"` ⇒ `Some(120)`); the HTTP-date form and
/// any negative/garbage value yield `None` (best-effort — the breaker then falls
/// back to its default 429 cooldown). Pure, so it's unit-tested directly.
#[must_use]
pub fn parse_retry_after(value: &str) -> Option<i64> {
    // Delta-seconds is a non-negative integer; reject negatives and non-digits.
    match value.trim().parse::<i64>() {
        Ok(secs) if secs >= 0 => Some(secs),
        _ => None,
    }
}

/// The injectable HTTP seam: POST a built [`Delivery`], returning the response
/// status code (and any `Retry-After`). The real impl is [`ReqwestSender`]; tests
/// use [`FakeSender`].
#[async_trait::async_trait]
pub trait WebhookSender: Send + Sync {
    /// Deliver one request. Returns a [`DeliveryResponse`] on a completed round
    /// trip, or an error string when the request could not be made at all
    /// (DNS/connect/timeout) — the caller logs and moves on (best-effort).
    async fn deliver(&self, delivery: &Delivery) -> Result<DeliveryResponse, String>;
}

/// Real HTTP transport over `reqwest`.
#[derive(Clone)]
pub struct ReqwestSender {
    client: reqwest::Client,
}

impl ReqwestSender {
    /// Build a sender with a sane per-request timeout so a slow endpoint can't
    /// stall the dispatcher.
    #[must_use]
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self { client }
    }
}

impl Default for ReqwestSender {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl WebhookSender for ReqwestSender {
    async fn deliver(&self, delivery: &Delivery) -> Result<DeliveryResponse, String> {
        let mut req = self.client.post(&delivery.url).body(delivery.body.clone());
        for (k, v) in &delivery.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        // Read the receiver's Retry-After (integer-seconds form) so the breaker can
        // honor it exactly on a 429; non-numeric / HTTP-date / negative ⇒ None.
        let retry_after_secs = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after);
        Ok(DeliveryResponse { status, retry_after_secs })
    }
}

/// Test double: records every [`Delivery`] and returns a canned status (and an
/// optional canned `Retry-After`). Lets the delivery path (and `build_delivery`'s
/// signature) be asserted without a server.
#[derive(Clone)]
pub struct FakeSender {
    status: u16,
    /// Canned `Retry-After` seconds returned on every delivery (default `None`).
    retry_after_secs: Option<i64>,
    calls: std::sync::Arc<std::sync::Mutex<Vec<Delivery>>>,
}

impl FakeSender {
    /// A sender that always reports `status` (no `Retry-After`) and captures calls.
    #[must_use]
    pub fn new(status: u16) -> Self {
        Self {
            status,
            retry_after_secs: None,
            calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Builder: also report a canned `Retry-After` of `secs` seconds (e.g. paired
    /// with `new(429)` to exercise the rate-limit path). Consumes and returns self.
    #[must_use]
    pub fn with_retry_after(mut self, secs: i64) -> Self {
        self.retry_after_secs = Some(secs);
        self
    }

    /// Snapshot of every delivery seen so far, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<Delivery> {
        self.calls.lock().expect("fake-sender mutex not poisoned").clone()
    }
}

#[async_trait::async_trait]
impl WebhookSender for FakeSender {
    async fn deliver(&self, delivery: &Delivery) -> Result<DeliveryResponse, String> {
        self.calls.lock().expect("fake-sender mutex not poisoned").push(delivery.clone());
        Ok(DeliveryResponse { status: self.status, retry_after_secs: self.retry_after_secs })
    }
}
