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
const TOKEN_BYTES: usize = 32;

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

/// The injectable HTTP seam: POST a built [`Delivery`], returning the response
/// status code. The real impl is [`ReqwestSender`]; tests use [`FakeSender`].
#[async_trait::async_trait]
pub trait WebhookSender: Send + Sync {
    /// Deliver one request. Returns the HTTP status code on a completed round
    /// trip, or an error string when the request could not be made at all
    /// (DNS/connect/timeout) — the caller logs and moves on (best-effort).
    async fn deliver(&self, delivery: &Delivery) -> Result<u16, String>;
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
    async fn deliver(&self, delivery: &Delivery) -> Result<u16, String> {
        let mut req = self.client.post(&delivery.url).body(delivery.body.clone());
        for (k, v) in &delivery.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        Ok(resp.status().as_u16())
    }
}

/// Test double: records every [`Delivery`] and returns a canned status. Lets the
/// delivery path (and `build_delivery`'s signature) be asserted without a server.
#[derive(Clone)]
pub struct FakeSender {
    status: u16,
    calls: std::sync::Arc<std::sync::Mutex<Vec<Delivery>>>,
}

impl FakeSender {
    /// A sender that always reports `status` and captures calls.
    #[must_use]
    pub fn new(status: u16) -> Self {
        Self { status, calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())) }
    }

    /// Snapshot of every delivery seen so far, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<Delivery> {
        self.calls.lock().expect("fake-sender mutex not poisoned").clone()
    }
}

#[async_trait::async_trait]
impl WebhookSender for FakeSender {
    async fn deliver(&self, delivery: &Delivery) -> Result<u16, String> {
        self.calls.lock().expect("fake-sender mutex not poisoned").push(delivery.clone());
        Ok(self.status)
    }
}

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

/// An active outgoing delivery target (url + signing secret) for dispatch.
#[derive(Debug, Clone)]
pub struct OutgoingTarget {
    pub url: String,
    pub secret: String,
}

/// Decide whether an outgoing hook with `filter` (its `events` array) should fire
/// for an event of kind `event_kind`. An empty filter means "all events"; a
/// non-empty filter fires only on an exact membership match. Pure, so the
/// filter semantics are unit-tested without a DB.
#[must_use]
pub fn event_matches(filter: &[String], event_kind: &str) -> bool {
    filter.is_empty() || filter.iter().any(|e| e == event_kind)
}

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
        let rows = sqlx::query_as::<_, (String, String)>(
            r"SELECT url, secret
               FROM outgoing_webhooks
               WHERE room_id = $1
                 AND revoked_at IS NULL
                 AND (cardinality(events) = 0 OR $2 = ANY(events))",
        )
        .bind(room.to_uuid())
        .bind(event_kind)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(url, secret)| OutgoingTarget { url, secret }).collect())
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
        let status = sender.deliver(&d).await.unwrap();
        assert_eq!(status, 202);
        let calls = sender.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], d);
        // A second delivery accumulates.
        sender.deliver(&d).await.unwrap();
        assert_eq!(sender.calls().len(), 2);
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
}
