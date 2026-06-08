//! Mobile push gateway for Aero IM (ROADMAP 方向二).
//!
//! A self-contained library that turns an in-app notification into a delivered
//! FCM (Android) or APNs (iOS) push. It is deliberately split into three layers:
//!
//! - [`PushPayload`] — the provider-neutral notification the IM layer produces.
//! - Pure builders — [`fcm_message_json`] / [`apns_payload_json`] translate a
//!   `PushPayload` into each provider's exact wire JSON. These are unit-tested
//!   against the documented contract and carry no I/O.
//! - [`PushGateway`] implementations — [`FcmGateway`], [`ApnsGateway`], and the
//!   test double [`FakeGateway`] — that POST the built JSON to the upstream.
//!
//! ## Token-provider seam
//!
//! Both real gateways take a *credential provider* closure rather than baking in
//! credential acquisition. FCM HTTP v1 needs a short-lived `OAuth2` bearer minted
//! from a service-account key; APNs needs an ES256 JWT signed with the team's
//! `.p8` key. Both rotate (FCM ~1h, APNs ≤1h), so the gateway asks for a fresh
//! token on every `send`. The closure shape is
//! `Arc<dyn Fn() -> BoxFuture<'static, Result<String, PushError>> + Send + Sync>`:
//! the integrator plugs the actual `OAuth2` / JWT machinery in there. The network
//! round-trip itself requires real credentials and is not exercised in-sandbox —
//! the verified boundary is "gateways compile, builders + [`FakeGateway`] are
//! unit-tested".

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use serde_json::{json, Value};

/// FCM HTTP v1 send endpoint template; `{project}` is the GCP project id.
const FCM_SEND_URL: &str = "https://fcm.googleapis.com/v1/projects/{project}/messages:send";

/// APNs production push endpoint template; `{token}` is the device token.
const APNS_SEND_URL: &str = "https://api.push.apple.com/3/device/{token}";

/// A provider-neutral push notification.
///
/// The IM layer fills this in once; each gateway maps it onto its own wire shape.
/// `room_id` / `message_id` ride along as custom data so the client app can deep
/// link into the conversation when the user taps the notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushPayload {
    pub title: String,
    pub body: String,
    pub room_id: Option<String>,
    pub message_id: Option<String>,
    /// iOS app-icon badge count. Ignored by FCM (Android badges are client-driven).
    pub badge: Option<u32>,
}

/// Failure modes shared by every [`PushGateway`].
///
/// Kept coarse on purpose: callers dispatch best-effort and only need to know
/// whether a token is dead (`Rejected` — drop it) versus a retryable hiccup
/// (`Auth` / `Transport`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushError {
    /// Credential minting or upstream auth rejection (expired/invalid bearer or JWT).
    Auth(String),
    /// Network, TLS, or timeout error reaching the upstream.
    Transport(String),
    /// Upstream accepted the request but refused this push (e.g. unregistered token).
    Rejected(String),
}

impl std::fmt::Display for PushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auth(m) => write!(f, "auth: {m}"),
            Self::Transport(m) => write!(f, "transport: {m}"),
            Self::Rejected(m) => write!(f, "rejected: {m}"),
        }
    }
}

impl std::error::Error for PushError {}

/// A credential provider: yields a fresh bearer/JWT for each send.
///
/// See the crate docs for why this is a seam rather than a stored secret.
pub type TokenProvider =
    Arc<dyn Fn() -> BoxFuture<'static, Result<String, PushError>> + Send + Sync>;

/// Abstraction over a single mobile push provider.
///
/// Implementors POST a built payload to their upstream for one device token. The
/// IM dispatch path holds an `Arc<dyn PushGateway>` per platform and fans a
/// notification out to every registered token.
#[async_trait::async_trait]
pub trait PushGateway: Send + Sync {
    /// Deliver `payload` to the device identified by `token`.
    async fn send(&self, token: &str, payload: &PushPayload) -> Result<(), PushError>;
}

// ----------------------------------------------------------------- builders

/// Build the FCM HTTP v1 request body for `token` + `payload`.
///
/// Shape:
/// ```json
/// {"message":{"token":"..","notification":{"title":"..","body":".."},
///  "data":{"room_id":"..","message_id":".."}}}
/// ```
/// `data` keys whose source field is `None` are omitted. The `data` object itself
/// is always present (FCM accepts an empty object); values are strings, as FCM's
/// data payload is a string-to-string map.
#[must_use]
pub fn fcm_message_json(token: &str, payload: &PushPayload) -> Value {
    let mut data = serde_json::Map::new();
    if let Some(room_id) = &payload.room_id {
        data.insert("room_id".into(), Value::String(room_id.clone()));
    }
    if let Some(message_id) = &payload.message_id {
        data.insert("message_id".into(), Value::String(message_id.clone()));
    }
    json!({
        "message": {
            "token": token,
            "notification": {
                "title": payload.title,
                "body": payload.body,
            },
            "data": Value::Object(data),
        }
    })
}

/// Build the APNs JSON payload for `payload`.
///
/// Shape:
/// ```json
/// {"aps":{"alert":{"title":"..","body":".."},"badge":N,"sound":"default"},
///  "room_id":"..","message_id":".."}
/// ```
/// `badge` is omitted when `None`; the `room_id` / `message_id` custom keys live
/// at the top level (APNs reserves the `aps` object and surfaces everything else
/// to the app) and are omitted when `None`.
#[must_use]
pub fn apns_payload_json(payload: &PushPayload) -> Value {
    let mut aps = serde_json::Map::new();
    aps.insert(
        "alert".into(),
        json!({ "title": payload.title, "body": payload.body }),
    );
    if let Some(badge) = payload.badge {
        aps.insert("badge".into(), json!(badge));
    }
    aps.insert("sound".into(), Value::String("default".into()));

    let mut root = serde_json::Map::new();
    root.insert("aps".into(), Value::Object(aps));
    if let Some(room_id) = &payload.room_id {
        root.insert("room_id".into(), Value::String(room_id.clone()));
    }
    if let Some(message_id) = &payload.message_id {
        root.insert("message_id".into(), Value::String(message_id.clone()));
    }
    Value::Object(root)
}

// --------------------------------------------------------------- FCM gateway

/// FCM HTTP v1 gateway.
///
/// Holds a pooled `reqwest::Client`, the GCP `project` id, and an `OAuth2`
/// [`TokenProvider`]. Each [`PushGateway::send`] mints a fresh bearer, POSTs
/// [`fcm_message_json`] to `.../projects/{project}/messages:send`, and maps the
/// HTTP status onto a [`PushError`].
#[derive(Clone)]
pub struct FcmGateway {
    http: reqwest::Client,
    project: String,
    token_provider: TokenProvider,
}

impl FcmGateway {
    /// Construct a gateway for GCP `project`, minting bearers via `token_provider`.
    #[must_use]
    pub fn new(project: impl Into<String>, token_provider: TokenProvider) -> Self {
        Self {
            http: reqwest::Client::new(),
            project: project.into(),
            token_provider,
        }
    }

    /// Construct with a caller-supplied `reqwest::Client` (shared pool, custom timeouts).
    #[must_use]
    pub fn with_client(
        http: reqwest::Client,
        project: impl Into<String>,
        token_provider: TokenProvider,
    ) -> Self {
        Self { http, project: project.into(), token_provider }
    }
}

#[async_trait::async_trait]
impl PushGateway for FcmGateway {
    async fn send(&self, token: &str, payload: &PushPayload) -> Result<(), PushError> {
        let bearer = (self.token_provider)().await?;
        let url = FCM_SEND_URL.replace("{project}", &self.project);
        let body = fcm_message_json(token, payload);

        let resp = self
            .http
            .post(&url)
            .bearer_auth(bearer)
            .json(&body)
            .send()
            .await
            .map_err(|e| PushError::Transport(e.to_string()))?;

        classify_response(resp).await
    }
}

// -------------------------------------------------------------- APNs gateway

/// APNs HTTP/2 gateway.
///
/// Holds a pooled `reqwest::Client` (reqwest negotiates HTTP/2 over TLS ALPN
/// automatically), the app's `topic` (bundle id), and a JWT [`TokenProvider`].
/// Each [`PushGateway::send`] mints a fresh ES256 JWT, POSTs
/// [`apns_payload_json`] to `.../3/device/{token}` with the `apns-topic` header,
/// and maps the HTTP status onto a [`PushError`].
#[derive(Clone)]
pub struct ApnsGateway {
    http: reqwest::Client,
    topic: String,
    token_provider: TokenProvider,
}

impl ApnsGateway {
    /// Construct a gateway for app `topic` (bundle id), minting JWTs via `token_provider`.
    #[must_use]
    pub fn new(topic: impl Into<String>, token_provider: TokenProvider) -> Self {
        Self {
            http: reqwest::Client::new(),
            topic: topic.into(),
            token_provider,
        }
    }

    /// Construct with a caller-supplied `reqwest::Client` (shared pool, custom timeouts).
    #[must_use]
    pub fn with_client(
        http: reqwest::Client,
        topic: impl Into<String>,
        token_provider: TokenProvider,
    ) -> Self {
        Self { http, topic: topic.into(), token_provider }
    }
}

#[async_trait::async_trait]
impl PushGateway for ApnsGateway {
    async fn send(&self, token: &str, payload: &PushPayload) -> Result<(), PushError> {
        let jwt = (self.token_provider)().await?;
        let url = APNS_SEND_URL.replace("{token}", token);
        let body = apns_payload_json(payload);

        let resp = self
            .http
            .post(&url)
            .header("apns-topic", &self.topic)
            .header("authorization", format!("bearer {jwt}"))
            .json(&body)
            .send()
            .await
            .map_err(|e| PushError::Transport(e.to_string()))?;

        classify_response(resp).await
    }
}

/// Map an upstream HTTP response onto `Ok(())` or a [`PushError`].
///
/// 2xx is success. `401`/`403` are auth failures (stale credential → retry after
/// refresh). Everything else is a rejection carrying the upstream body, which for
/// both FCM and APNs is the JSON error describing the dead/invalid token.
async fn classify_response(resp: reqwest::Response) -> Result<(), PushError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        Err(PushError::Auth(format!("{status}: {body}")))
    } else {
        Err(PushError::Rejected(format!("{status}: {body}")))
    }
}

// -------------------------------------------------------------- fake gateway

/// In-memory [`PushGateway`] that records every send instead of hitting the network.
///
/// Used by the integrator (to exercise the dispatch hook without credentials) and
/// by tests (to assert which tokens received which payloads).
#[derive(Clone, Default)]
pub struct FakeGateway {
    sends: Arc<Mutex<Vec<(String, PushPayload)>>>,
}

impl FakeGateway {
    /// An empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of every `(token, payload)` seen so far, in send order.
    #[must_use]
    pub fn sent(&self) -> Vec<(String, PushPayload)> {
        self.sends.lock().expect("fake-gateway mutex not poisoned").clone()
    }
}

#[async_trait::async_trait]
impl PushGateway for FakeGateway {
    async fn send(&self, token: &str, payload: &PushPayload) -> Result<(), PushError> {
        self.sends
            .lock()
            .expect("fake-gateway mutex not poisoned")
            .push((token.to_string(), payload.clone()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_payload() -> PushPayload {
        PushPayload {
            title: "New message".into(),
            body: "hello world".into(),
            room_id: Some("room-1".into()),
            message_id: Some("msg-9".into()),
            badge: Some(3),
        }
    }

    fn bare_payload() -> PushPayload {
        PushPayload {
            title: "Ping".into(),
            body: "tap".into(),
            room_id: None,
            message_id: None,
            badge: None,
        }
    }

    #[test]
    fn fcm_message_json_full_shape() {
        let json = fcm_message_json("tok-abc", &full_payload());
        assert_eq!(json["message"]["token"], "tok-abc");
        assert_eq!(json["message"]["notification"]["title"], "New message");
        assert_eq!(json["message"]["notification"]["body"], "hello world");
        assert_eq!(json["message"]["data"]["room_id"], "room-1");
        assert_eq!(json["message"]["data"]["message_id"], "msg-9");
    }

    #[test]
    fn fcm_message_json_omits_none_data_keys() {
        let json = fcm_message_json("tok", &bare_payload());
        let data = json["message"]["data"].as_object().unwrap();
        assert!(data.is_empty(), "data must be present but empty when no ids");
        assert!(data.get("room_id").is_none());
        assert!(data.get("message_id").is_none());
    }

    #[test]
    fn fcm_message_json_partial_data() {
        let payload = PushPayload {
            room_id: Some("r".into()),
            message_id: None,
            ..bare_payload()
        };
        let json = fcm_message_json("tok", &payload);
        let data = json["message"]["data"].as_object().unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data["room_id"], "r");
    }

    #[test]
    fn apns_payload_json_full_shape() {
        let json = apns_payload_json(&full_payload());
        assert_eq!(json["aps"]["alert"]["title"], "New message");
        assert_eq!(json["aps"]["alert"]["body"], "hello world");
        assert_eq!(json["aps"]["badge"], 3);
        assert_eq!(json["aps"]["sound"], "default");
        assert_eq!(json["room_id"], "room-1");
        assert_eq!(json["message_id"], "msg-9");
    }

    #[test]
    fn apns_payload_json_omits_badge_and_custom_keys_when_none() {
        let json = apns_payload_json(&bare_payload());
        let aps = json["aps"].as_object().unwrap();
        assert!(aps.get("badge").is_none(), "badge omitted when None");
        assert_eq!(aps["sound"], "default");
        let root = json.as_object().unwrap();
        assert!(root.get("room_id").is_none());
        assert!(root.get("message_id").is_none());
    }

    #[test]
    fn push_error_display_prefixes() {
        assert_eq!(PushError::Auth("x".into()).to_string(), "auth: x");
        assert_eq!(PushError::Transport("y".into()).to_string(), "transport: y");
        assert_eq!(PushError::Rejected("z".into()).to_string(), "rejected: z");
    }

    #[test]
    fn push_error_is_std_error() {
        fn assert_error<E: std::error::Error>(_: &E) {}
        assert_error(&PushError::Auth("x".into()));
    }

    #[tokio::test]
    async fn fake_gateway_records_sends_in_order() {
        let gw = FakeGateway::new();
        gw.send("tok-a", &full_payload()).await.unwrap();
        gw.send("tok-b", &bare_payload()).await.unwrap();

        let sent = gw.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].0, "tok-a");
        assert_eq!(sent[0].1, full_payload());
        assert_eq!(sent[1].0, "tok-b");
        assert_eq!(sent[1].1, bare_payload());
    }

    #[tokio::test]
    async fn fake_gateway_is_usable_as_dyn_trait() {
        let gw: Arc<dyn PushGateway> = Arc::new(FakeGateway::new());
        gw.send("tok", &bare_payload()).await.unwrap();
    }
}
