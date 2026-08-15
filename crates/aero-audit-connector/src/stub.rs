//! Hand-rolled HTTP stubs for tests and the A3 drill: a token endpoint that
//! mints JWTs with configurable claims and an audit endpoint that counts
//! POSTs and answers with configurable statuses / receipts. Built directly on
//! `tokio::net::TcpListener` — no new dev-dependencies.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header};
use rand::rngs::StdRng;
use rand::SeedableRng;
use rsa::pkcs1::EncodeRsaPublicKey;
use rsa::pkcs8::EncodePrivateKey;
use rsa::traits::PublicKeyParts;
use rsa::RsaPrivateKey;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

/// The embedded trusted signing keypair (kid `test-audit-1`), generated once
/// per process with a fixed seed so every stub and every injected
/// [`StaticKeyProvider`] agree deterministically.
static TRUSTED_KEY: OnceLock<(RsaPrivateKey, String)> = OnceLock::new();

/// The test/connector trusted signing key (deterministic, shared).
#[must_use]
pub fn trusted_key() -> (RsaPrivateKey, String) {
    TRUSTED_KEY
        .get_or_init(|| {
            let mut rng = StdRng::seed_from_u64(0xA0D1_7B52);
            let key = RsaPrivateKey::new(&mut rng, 2048).expect("stub rsa keygen");
            (key, "test-audit-1".to_owned())
        })
        .clone()
}

/// Duplicate-replay behavior of the stub audit sink (drill-spec D-W1/D-W4):
/// how a repeated `Idempotency-Key` is answered. The stub keys on the RAW
/// header value — the connector→sink spelling (`AuditId` Display = ULID
/// base32, `client.rs` `.header("Idempotency-Key", claim.event_id.to_string())`)
/// — pinning that spelling; the dual-format canonicalization (header base32
/// vs payload `event_id` `uuid::text`, both = `audit_events.id` value-level)
/// is the SINK-side contract this stub documents but cannot itself verify
/// (the sink must parse UUID first, else ULID, and dedup on the canonical
/// 128-bit value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DuplicateBehavior {
    /// Stateless (current behavior): every POST answered identically from the
    /// live behavior knobs.
    #[default]
    None,
    /// Conforming sink contract (D-W1 recovery terminal): a repeated
    /// `Idempotency-Key` replays the ORIGINAL 202 + ORIGINAL receipt (same
    /// `event_id` echo, `conflict:false`, same `accepted_at`) — the at-least-once
    /// crash-window recovery a conforming sink must answer with.
    ReplayOriginalReceipt,
    /// Non-conforming dedup signal: repeated key → HTTP 409 (permanent class
    /// — the connector deads the row after ≤1 retry, never settles).
    Conflict409,
    /// Non-conforming dedup signal: repeated key → 202 + receipt with
    /// `conflict:true` (permanent class — `ReceiptMismatch`).
    ConflictFlag,
}

/// Tamper modes for the stub token endpoint (negative-path fixtures): a
/// trusted-signature token that is corrupted *after* signing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TamperMode {
    /// Flip a character in the signature segment (bad-signature fixture).
    CorruptSignature,
    /// Present a payload whose claims differ from the signed payload
    /// (tampered-claims fixture — the signature no longer covers them).
    MutateClaims,
}

/// Behavior of the stub audit sink for one test/drill run.
#[derive(Debug, Clone)]
pub struct SinkBehavior {
    /// JWT payload claims the token endpoint mints for its first token
    /// request.
    pub token_claims: Value,
    /// Claims minted from the second token request onward (`None` = always
    /// [`Self::token_claims`]); exercises the 401-refresh path against a
    /// drifted identity provider.
    pub token_claims_after_first: Option<Value>,
    /// HTTP status for `POST /events` (202 for the happy path).
    pub events_status: u16,
    /// HTTP status for `POST /token` (`None` = 200, the happy path).
    /// Exercises RFC 6749 §5.2 token-endpoint error responses (401
    /// `invalid_client` / 400 `invalid_grant` / 5xx): the connector bails on
    /// ANY non-200 and classifies it Transient — the M2 all-transient
    /// posture pin. The error body mirrors §5.2 semantics for
    /// forward-compatibility with a future terminal-class refinement that
    /// parses the `error` field (the connector ignores the body today).
    pub token_status: Option<u16>,
    /// Echo the request's `event_id` into the durable receipt (`false` = the
    /// receipt `event_id` is corrupted → `ReceiptMismatch`).
    pub receipt_valid: bool,
    /// Override the echoed receipt `event_id` (when `receipt_valid` is
    /// `true`). Exercises the dual-format receipt comparison (D1): the
    /// payload echo is a hyphenated UUID string (0239 trigger format), while
    /// a sink echoing the `Idempotency-Key` header would produce ULID base32.
    pub receipt_event_id_override: Option<String>,
    /// First `POST /events` answers 401, subsequent ones use `events_status`
    /// (exercises the refresh-once retry).
    pub unauthorized_once: bool,
    /// How a repeated `Idempotency-Key` is answered (see [`DuplicateBehavior`]).
    pub duplicate: DuplicateBehavior,
    /// Answer every 202 receipt with `conflict: <this>` (drill knob;
    /// replaces the historical hardcoded `false`).
    pub receipt_conflict: bool,
    /// Sleep this long before answering `POST /events` (drives client-side
    /// request timeouts in relay transient tests). Token responses are never
    /// delayed.
    pub delay_ms: u64,
    /// `None` ⇒ mint the legacy `alg:none` fake-signature token (rejection
    /// fixture); `Some((key, kid))` ⇒ mint a real RS256-signed token.
    pub signing_key: Option<(RsaPrivateKey, String)>,
    /// Post-signing corruption mode for negative-path fixtures.
    pub tamper: Option<TamperMode>,
    /// The key set served by `/jwks` (`(key, kid)` pairs). Empty ⇒ `/jwks`
    /// answers 404 (fetch-failure fixture); swappable = rotation driver.
    pub jwks_keys: Vec<(RsaPrivateKey, String)>,
}

impl Default for SinkBehavior {
    fn default() -> Self {
        Self {
            token_claims: json!({
                "iss": "https://idp.example.test",
                "aud": ["audit-governance"],
                "scope": "audit:event:write",
                "sub": "aero-im.source",
                "client_id": "aero-im.source",
            }),
            token_claims_after_first: None,
            events_status: 202,
            token_status: None,
            receipt_valid: true,
            receipt_event_id_override: None,
            unauthorized_once: false,
            duplicate: DuplicateBehavior::None,
            receipt_conflict: false,
            delay_ms: 0,
            signing_key: None,
            tamper: None,
            jwks_keys: Vec::new(),
        }
    }
}

/// Running stub: one listener serving `/token` and `/events`.
pub struct StubSink {
    addr: SocketAddr,
    behavior: Arc<Mutex<SinkBehavior>>,
    posts: Arc<AtomicUsize>,
    token_requests: Arc<AtomicUsize>,
    unauthorized_fired: Arc<AtomicBool>,
    /// Every `Idempotency-Key` header observed on `POST /events`, in order
    /// (drill observation API — the header is parsed in zero historical
    /// tests; a regression swapping the header spelling must fail loudly).
    seen_keys: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for StubSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StubSink")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl StubSink {
    /// Bind a listener on an ephemeral loopback port and serve until
    /// [`Self::shutdown`].
    pub async fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let behavior = Arc::new(Mutex::new(SinkBehavior::default()));
        let posts = Arc::new(AtomicUsize::new(0));
        let token_requests = Arc::new(AtomicUsize::new(0));
        let unauthorized_fired = Arc::new(AtomicBool::new(false));
        let seen_keys = Arc::new(Mutex::new(Vec::new()));
        // First 202 receipt per seen key (the duplicate modes replay it). The
        // serve task's clone keeps this alive; the struct itself never reads
        // it (observation is via `seen_idempotency_keys`).
        let first_receipts = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let task = tokio::spawn(serve(
            listener,
            behavior.clone(),
            posts.clone(),
            token_requests.clone(),
            unauthorized_fired.clone(),
            seen_keys.clone(),
            first_receipts.clone(),
        ));
        Ok(Self {
            addr,
            behavior,
            posts,
            token_requests,
            unauthorized_fired,
            seen_keys,
            task,
        })
    }

    #[must_use]
    pub fn token_url(&self) -> String {
        format!("http://{}/token", self.addr)
    }

    #[must_use]
    pub fn events_url(&self) -> String {
        format!("http://{}/events", self.addr)
    }

    #[must_use]
    pub fn jwks_url(&self) -> String {
        format!("http://{}/jwks", self.addr)
    }

    /// Public half of the embedded trusted key, for
    /// [`StaticKeyProvider::single`] injection in JWKS-on tests.
    #[must_use]
    pub fn decoding_key(&self) -> DecodingKey {
        let (key, _) = trusted_key();
        let public = key.to_public_key();
        let pem = public
            .to_pkcs1_pem(rsa::pkcs8::LineEnding::LF)
            .expect("stub public key pem");
        DecodingKey::from_rsa_pem(pem.as_bytes()).expect("stub decoding key")
    }

    /// Number of `POST /events` deliveries observed.
    #[must_use]
    pub fn posts(&self) -> usize {
        self.posts.load(Ordering::SeqCst)
    }

    /// Number of `POST /token` requests observed.
    #[must_use]
    pub fn token_requests(&self) -> usize {
        self.token_requests.load(Ordering::SeqCst)
    }

    /// Every `Idempotency-Key` header observed on `POST /events`, in order.
    /// `set_behavior` deliberately does NOT reset this (a mid-test behavior
    /// swap — e.g. the crash-window drills — must preserve the observed
    /// sequence across the swap).
    pub async fn seen_idempotency_keys(&self) -> Vec<String> {
        self.seen_keys.lock().await.clone()
    }

    pub async fn set_behavior(&self, behavior: SinkBehavior) {
        *self.behavior.lock().await = behavior;
        self.unauthorized_fired.store(false, Ordering::SeqCst);
        self.token_requests.store(0, Ordering::SeqCst);
    }

    /// Stop the server task.
    pub fn shutdown(self) {
        self.task.abort();
    }
}

async fn serve(
    listener: TcpListener,
    behavior: Arc<Mutex<SinkBehavior>>,
    posts: Arc<AtomicUsize>,
    token_requests: Arc<AtomicUsize>,
    unauthorized_fired: Arc<AtomicBool>,
    seen_keys: Arc<Mutex<Vec<String>>>,
    first_receipts: Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>>,
) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let behavior = behavior.clone();
        let posts = posts.clone();
        let token_requests = token_requests.clone();
        let unauthorized_fired = unauthorized_fired.clone();
        let seen_keys = seen_keys.clone();
        let first_receipts = first_receipts.clone();
        tokio::spawn(async move {
            let _ = handle_connection(
                &mut stream,
                &behavior,
                &posts,
                &token_requests,
                &unauthorized_fired,
                &seen_keys,
                &first_receipts,
            )
            .await;
        });
    }
}

async fn handle_connection(
    stream: &mut TcpStream,
    behavior: &Mutex<SinkBehavior>,
    posts: &AtomicUsize,
    token_requests: &AtomicUsize,
    unauthorized_fired: &AtomicBool,
    seen_keys: &Mutex<Vec<String>>,
    first_receipts: &Mutex<std::collections::HashMap<String, Vec<u8>>>,
) -> std::io::Result<()> {
    let request = read_request(stream).await?;
    if request.path == "/token" {
        let behavior = behavior.lock().await;
        let request_index = token_requests.fetch_add(1, Ordering::SeqCst);
        if let Some(status) = behavior.token_status {
            // M2 knob: answer the token request with a §5.2-shaped error.
            // The counter increment above already proves the token path was
            // hit (non-vacuous — the drill's `posts() == 0` is not because
            // the relay skipped the row).
            let error = match status {
                400 => "invalid_grant",
                401 | 403 => "invalid_client",
                _ => "server_error",
            };
            let body = format!(
                "{{\"error\":\"{error}\",\"error_description\":\"token endpoint drill (M2)\"}}"
            );
            return respond(stream, status, body.as_bytes()).await;
        }
        let claims = if request_index == 0 {
            &behavior.token_claims
        } else {
            behavior
                .token_claims_after_first
                .as_ref()
                .unwrap_or(&behavior.token_claims)
        };
        let token = match (&behavior.signing_key, behavior.tamper) {
            (None, _) => make_jwt(claims),
            (Some((key, kid)), None) => make_rs256_jwt(claims, key, kid),
            (Some((key, kid)), Some(TamperMode::CorruptSignature)) => {
                corrupt_signature_segment(&make_rs256_jwt(claims, key, kid))
            }
            (Some((key, kid)), Some(TamperMode::MutateClaims)) => {
                let mut tampered = claims.clone();
                tampered["sub"] = json!("tampered-subject");
                // Sign the tampered claims, then swap in the ORIGINAL payload
                // segment: the signature no longer covers what is presented.
                swap_payload_segment(&make_rs256_jwt(&tampered, key, kid), claims)
            }
        };
        let body = json!({
            "access_token": token,
            "token_type": "Bearer",
            "expires_in": 3600,
        });
        return respond(stream, 200, &serde_json::to_vec(&body).expect("stub JSON")).await;
    }
    if request.path == "/jwks" {
        let behavior = behavior.lock().await;
        if behavior.jwks_keys.is_empty() {
            return respond(stream, 404, b"{\"error\":\"no jwks configured\"}").await;
        }
        let keys: Vec<Value> = behavior
            .jwks_keys
            .iter()
            .map(|(key, kid)| {
                let public = key.to_public_key();
                json!({
                    "kty": "RSA",
                    "kid": kid,
                    "alg": "RS256",
                    "use": "sig",
                    "n": URL_SAFE_NO_PAD.encode(public.n().to_bytes_be()),
                    "e": URL_SAFE_NO_PAD.encode(public.e().to_bytes_be()),
                })
            })
            .collect();
        let body = serde_json::to_vec(&json!({ "keys": keys })).expect("stub JSON");
        return respond(stream, 200, &body).await;
    }
    if request.path == "/events" {
        posts.fetch_add(1, Ordering::SeqCst);
        // Drill observation API: record every Idempotency-Key header, in
        // order (the connector always sends one; a regression dropping or
        // respelling it is caught by `seen_idempotency_keys()` assertions).
        let key = request.idempotency_key.clone();
        if let Some(key) = &key {
            seen_keys.lock().await.push(key.clone());
        }
        let behavior = behavior.lock().await;
        if behavior.delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(behavior.delay_ms)).await;
        }
        if behavior.unauthorized_once && !unauthorized_fired.swap(true, Ordering::SeqCst) {
            return respond(stream, 401, b"{\"error\":\"unauthorized\"}").await;
        }
        if behavior.events_status != 202 {
            return respond(stream, behavior.events_status, b"{}").await;
        }
        let event_id = serde_json::from_slice::<Value>(&request.body)
            .ok()
            .and_then(|payload| {
                payload
                    .get("event_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "missing".to_owned());
        let echoed = if behavior.receipt_valid {
            behavior
                .receipt_event_id_override
                .clone()
                .unwrap_or(event_id)
        } else {
            "corrupted-event-id".to_owned()
        };
        let receipt = json!({
            "receipt": {
                "event_id": echoed,
                "tenant_id": "tenant-a",
                "status": "ledgered",
                "accepted_at": "2026-08-06T00:00:00Z",
                "conflict": behavior.receipt_conflict,
            }
        });
        let built = serde_json::to_vec(&receipt).expect("stub JSON");
        // Duplicate-replay modes: the FIRST 202 receipt per key is stored;
        // a repeat is answered per `DuplicateBehavior`. First POSTs always
        // answer with the just-built receipt (identical to `None`).
        let key = key.unwrap_or_else(|| "missing".to_owned());
        let mut receipts = first_receipts.lock().await;
        let first = receipts.get(&key).cloned();
        if first.is_none() {
            receipts.insert(key.clone(), built.clone());
        }
        drop(receipts);
        let _ = match (behavior.duplicate, first) {
            (_, None) | (DuplicateBehavior::None, Some(_)) => respond(stream, 202, &built).await,
            (DuplicateBehavior::ReplayOriginalReceipt, Some(original)) => {
                respond(stream, 202, &original).await
            }
            (DuplicateBehavior::Conflict409, Some(original)) => {
                respond(stream, 409, &original).await
            }
            (DuplicateBehavior::ConflictFlag, Some(original)) => {
                let mut flagged: Value =
                    serde_json::from_slice(&original).expect("stored receipt is valid JSON");
                flagged["receipt"]["conflict"] = json!(true);
                let body = serde_json::to_vec(&flagged).expect("stub JSON");
                respond(stream, 202, &body).await
            }
        };
    }
    respond(stream, 404, b"{\"error\":\"not found\"}").await
}

/// Mint an unsigned-shaped JWT (`header.payload.fake-signature`) with the
/// given claims. The connector does not verify signatures (trusted `IdP` via
/// `client_credentials`), so the fake signature is enough for claim tests.
#[must_use]
pub fn make_jwt(claims: &Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let payload =
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("stub claims serialize"));
    format!("{header}.{payload}.ZmFrZS1zaWduYXR1cmU")
}

/// Mint a real RS256-signed JWT with the given keypair and `kid` (the
/// JWKS-on acceptance fixtures).
#[must_use]
pub fn make_rs256_jwt(claims: &Value, key: &RsaPrivateKey, kid: &str) -> String {
    let private_pem = key
        .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
        .expect("stub private key pem")
        .to_string();
    let encoding = EncodingKey::from_rsa_pem(private_pem.as_bytes()).expect("stub encoding key");
    let header = Header {
        alg: Algorithm::RS256,
        kid: Some(kid.to_owned()),
        ..Header::default()
    };
    jsonwebtoken::encode(&header, claims, &encoding).expect("stub jwt encode")
}

/// Flip one character of the signature segment (stays valid base64url, so
/// the tamper is a pure signature corruption).
fn corrupt_signature_segment(token: &str) -> String {
    let mut parts: Vec<String> = token.split('.').map(str::to_owned).collect();
    let last = parts.last_mut().expect("signature segment");
    let mut chars: Vec<char> = last.chars().collect();
    let index = chars.len() - 1;
    chars[index] = if chars[index] == 'A' { 'B' } else { 'A' };
    *last = chars.into_iter().collect();
    parts.join(".")
}

/// Replace the payload segment with `claims`' own encoding, keeping the
/// original header and signature (which no longer cover the presented
/// payload).
fn swap_payload_segment(signed: &str, claims: &Value) -> String {
    let header = signed.split('.').next().expect("header segment");
    let signature = signed.rsplit('.').next().expect("signature segment");
    let payload =
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("stub claims serialize"));
    format!("{header}.{payload}.{signature}")
}

struct StubRequest {
    path: String,
    body: Vec<u8>,
    idempotency_key: Option<String>,
}

async fn read_request(stream: &mut TcpStream) -> std::io::Result<StubRequest> {
    let mut buffer = Vec::new();
    let mut byte = [0_u8; 1];
    // Read until the end of the header block.
    loop {
        stream.read_exact(&mut byte).await?;
        buffer.push(byte[0]);
        if buffer.ends_with(b"\r\n\r\n") {
            break;
        }
        if buffer.len() > 16 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "stub request headers too large",
            ));
        }
    }
    let head = String::from_utf8_lossy(&buffer);
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let mut content_length = 0_usize;
    let mut idempotency_key = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().unwrap_or(0);
        }
        if name.eq_ignore_ascii_case("idempotency-key") {
            idempotency_key = Some(value.trim().to_owned());
        }
    }
    let mut body = vec![0_u8; content_length];
    stream.read_exact(&mut body).await?;
    Ok(StubRequest {
        path,
        body,
        idempotency_key,
    })
}

async fn respond(stream: &mut TcpStream, status: u16, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "ACCEPTED",
        401 => "UNAUTHORIZED",
        403 => "FORBIDDEN",
        404 => "NOT FOUND",
        409 => "CONFLICT",
        422 => "UNPROCESSABLE ENTITY",
        _ => "STATUS",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await
}
