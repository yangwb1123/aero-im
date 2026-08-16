//! `client_credentials` token acquisition, JWT claim validation, and delivery
//! classification.
//!
//! Clones the v1 Snaplink relay (`snaplink_commercial/http.rs`): cc grant with
//! `basic_auth` + `grant_type=client_credentials` + `scope` + `resource`, a
//! ttl-based token cache with 401 invalidation, `Idempotency-Key: event_id`,
//! expected 202 ACCEPTED, and a durable-receipt validator. New surface:
//! **claim validation (iss/aud/scope/sub) before every POST** — v1 only
//! shape-checks the bearer (`validate_token`) — and HTTP status classification
//! (v1 bails on any non-202): 403 → dead immediately, 422/409/receipt
//! mismatch → permanent (requeue once then dead), everything else transient.

use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

use aero_auth::{JwksKeyProvider, KeyProvider};
use anyhow::{anyhow, bail, Context};
use base64::{
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
    Engine as _,
};
use futures::TryStreamExt;
use jsonwebtoken::{Algorithm, Validation};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::Value;
use time::OffsetDateTime;
use tokio::sync::Mutex;
use tracing::warn;

use crate::config::RelayConfig;
use crate::outbox::Claim;
use aero_common::model::client_credentials::{
    check_audience, check_issuer, check_scope, check_subject, ClientCredentialsGateClaims,
    ClientCredentialsTokenConfig, CLAIM_ISS, CLAIM_SUB,
};
use aero_common::AuditId;
use uuid::Uuid;

const MAX_TOKEN_BODY: usize = 64 * 1024;
const MAX_AUDIT_RECEIPT_BODY: usize = 64 * 1024;
const MAX_ACCESS_TOKEN_BYTES: usize = 16 * 1024;

/// Clock-skew tolerance applied to the `exp`/`nbf` time-window checks
/// (named constant, deliberately not env-exposed — no misconfiguration
/// surface). Tests pin margins ≥ 2× this value.
pub const TIME_CLAIM_LEEWAY_SECS: i64 = 60;

/// The audit scope this connector requests and validates (v1 `SCOPE_AUDIT`).
pub const SCOPE_AUDIT: &str = "audit:event:write";

/// Permanent error classes. The relay treats every class identically
/// (requeue once → dead); the distinction is kept for `last_error` fidelity
/// and for the A1-3 parameterized assertions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermanentKind {
    /// HTTP 422.
    Unprocessable,
    /// HTTP 409.
    Conflict,
    /// 202 body failed durable-receipt validation.
    ReceiptMismatch,
    /// Local payload guard violation (tenant selection / source mismatch).
    PayloadGuard,
    /// Signature-plane rejection: alg outside the RS256 allowlist, bad
    /// signature, or an unknown kid after the key source refreshed. Requeued
    /// once then dead (≤1 retry), like every permanent class.
    SignatureRejected,
}

/// Delivery classification driving the relay's exactly-one transition.
#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    /// Transport, timeout, 5xx, unspecified 4xx, claim-validation drift,
    /// 401-after-refresh: re-park with backoff, never dead.
    #[error("transient audit delivery failure: {0}")]
    Transient(#[from] anyhow::Error),
    /// Permanent class: requeue once then dead.
    #[error("permanent audit delivery failure: {0:?}")]
    Permanent(PermanentKind),
    /// HTTP 403: dead immediately (T-11 fail-closed).
    #[error("audit sink rejected the service identity (HTTP 403)")]
    Forbidden,
}

/// Why a token's JWT claims failed validation (fail-closed: no POST).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimRejection {
    pub reason: String,
}

struct CachedToken {
    value: String,
    refresh_at: Instant,
}

impl std::fmt::Debug for CachedToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CachedToken")
            .field("value", &"<redacted>")
            .field("refresh_at", &self.refresh_at)
            .finish()
    }
}

/// Outbound audit client: cc token (cached, 401-invalidated), claim-validated
/// delivery, status classification.
pub struct AuditClient {
    config: RelayConfig,
    /// Leaf contract config (aero-common single source): issuer/audience
    /// copied from `RelayConfig.expected_*`, `required_scopes` = the single
    /// expected scope (equivalent to the historical any-match check).
    cc_cfg: ClientCredentialsTokenConfig,
    http: reqwest::Client,
    token_cache: Mutex<Option<CachedToken>>,
    /// `None` ⇒ signature verification off (JWKS-off); `Some` ⇒ every token
    /// must verify against the trusted key source before any POST.
    keys: Option<Arc<dyn KeyProvider>>,
}

impl std::fmt::Debug for AuditClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuditClient")
            .field("config", &self.config)
            .field("token_cache", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl AuditClient {
    /// Build the client. Signature verification is off unless the config
    /// carries a trusted-JWKS URI (production: `AERO_AUDIT_JWKS_URL`), in
    /// which case a [`JwksKeyProvider`] is constructed from it — the 12
    /// existing construction points with `jwks_uri: None` stay JWKS-off.
    pub fn new(config: RelayConfig) -> anyhow::Result<Self> {
        let keys = config
            .jwks_uri
            .as_ref()
            .map(|uri| Arc::new(JwksKeyProvider::new(uri.as_str())) as Arc<dyn KeyProvider>);
        Self::with_key_provider(config, keys)
    }

    /// Build the client with an explicit key source (test/drill injection
    /// seam; `None` = signature verification off).
    pub fn with_key_provider(
        config: RelayConfig,
        keys: Option<Arc<dyn KeyProvider>>,
    ) -> anyhow::Result<Self> {
        let cc_cfg = ClientCredentialsTokenConfig {
            issuer: config.expected_iss.clone(),
            audience: config.expected_aud.clone(),
            required_scopes: vec![config.expected_scope.clone()],
        };
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("build audit connector HTTP client")?;
        Ok(Self {
            config,
            cc_cfg,
            http,
            token_cache: Mutex::new(None),
            keys,
        })
    }

    /// Deliver one claimed row. Exactly one of the [`DeliveryError`] classes
    /// is returned; no POST ever carries a token whose claims failed
    /// validation.
    pub async fn deliver(&self, claim: &Claim) -> Result<(), DeliveryError> {
        validate_delivery_payload(&self.config, claim)
            .map_err(|_| DeliveryError::Permanent(PermanentKind::PayloadGuard))?;
        let mut token = self.access_token().await?;
        let mut refreshed = false;
        loop {
            // Signature plane first (D6): a structurally-invalid or
            // unsigned-shaped token is rejected before any claims read, and
            // a rejected token never reaches a POST. Fetch-plane failures
            // (JWKS unavailable) are Transient — requeue, never dead.
            self.verify_token_signature(&token).await?;
            if let Err(rejection) = self.validate_token_claims(&token) {
                // Fail-closed: discard the unusable token and re-park the row
                // (transient — the fault is IdP config drift, fixed by B5-4).
                crate::metrics::inc_token_rejection(
                    crate::metrics::classify_token_rejection(&rejection.reason),
                );
                self.invalidate_token(&token).await;
                warn!(
                    reason = %rejection.reason,
                    "audit token failed claim validation; no delivery attempted"
                );
                return Err(DeliveryError::Transient(anyhow!(
                    "audit token claim validation failed: {}",
                    rejection.reason
                )));
            }
            let response = self
                .http
                .post(self.config.events_url.clone())
                .bearer_auth(&token)
                .header("Idempotency-Key", claim.event_id.to_string())
                .json(&claim.payload)
                .send()
                .await
                .map_err(transport_error)?;
            match response.status() {
                StatusCode::ACCEPTED => {
                    let body = bounded_body(response, MAX_AUDIT_RECEIPT_BODY)
                        .await
                        .map_err(DeliveryError::Transient)?;
                    return validate_audit_receipt(&body, claim)
                        .map_err(|_| DeliveryError::Permanent(PermanentKind::ReceiptMismatch));
                }
                StatusCode::UNAUTHORIZED if !refreshed => {
                    self.invalidate_token(&token).await;
                    token = self.access_token().await?;
                    refreshed = true;
                }
                StatusCode::UNAUTHORIZED => {
                    return Err(DeliveryError::Transient(anyhow!(
                        "audit sink rejected a freshly refreshed token (HTTP 401)"
                    )));
                }
                StatusCode::FORBIDDEN => {
                    // 403 is an identity/provisioning rejection: the same
                    // rejected token must not keep being reused for the rest of
                    // its cache TTL (up to ~24h, `usable = ttl - 30`), deading
                    // successive batches of the whole backlog with zero retries.
                    // Invalidate now — the next `access_token()` mints a fresh
                    // credential, so a transient IdP/provisioning lag self-heals
                    // on the next batch, while a genuine provisioning fault still
                    // deads this row (fail-closed terminal unchanged).
                    self.invalidate_token(&token).await;
                    return Err(DeliveryError::Forbidden);
                }
                StatusCode::UNPROCESSABLE_ENTITY => {
                    return Err(DeliveryError::Permanent(PermanentKind::Unprocessable));
                }
                StatusCode::CONFLICT => {
                    return Err(DeliveryError::Permanent(PermanentKind::Conflict));
                }
                status if status.is_server_error() => {
                    return Err(DeliveryError::Transient(anyhow!(
                        "audit sink returned HTTP {}",
                        status.as_u16()
                    )));
                }
                status => {
                    warn!(
                        code = status.as_u16(),
                        "audit sink returned an unspecified client error; treating as transient"
                    );
                    return Err(DeliveryError::Transient(anyhow!(
                        "audit sink returned HTTP {}",
                        status.as_u16()
                    )));
                }
            }
        }
    }

    /// Validate a token's JWT claims (iss/aud/scope/sub) against the
    /// configured expectations, then the `exp`/`nbf` time window against the
    /// given clock. Pure and unit-testable; the signature is *not* verified
    /// here (that is [`Self::verify_token_signature`], run first when the
    /// client has a key source).
    pub fn validate_token_claims(&self, token: &str) -> Result<(), ClaimRejection> {
        self.validate_token_claims_at(token, OffsetDateTime::now_utc())
    }

    /// Clock-injected claim validation: `exp`/`nbf` are checked
    /// validated-when-present (missing allowed) with a 60s leeway; a present
    /// non-numeric value is a fail-closed rejection. `now_secs` (~1.7e9) is
    /// exactly representable in f64 (mantissa covers integers up to 2^53), so
    /// the `NumericDate` comparison casts lose no precision.
    #[allow(clippy::cast_precision_loss)]
    pub fn validate_token_claims_at(
        &self,
        token: &str,
        now: OffsetDateTime,
    ) -> Result<(), ClaimRejection> {
        if token.is_empty()
            || token.len() > MAX_ACCESS_TOKEN_BYTES
            || token.chars().any(char::is_control)
        {
            return Err(ClaimRejection {
                reason: "token has an invalid shape".into(),
            });
        }
        let claims = decode_jwt_claims(token).map_err(|error| ClaimRejection { reason: error })?;
        // Typed gate (leaf `ClientCredentialsGateClaims`, fail-closed deltas
        // ①–③): `sub`/`client_id` must be present strings; `iat`/`jti`, when
        // present, must be u64 / String — a fractional NumericDate `iat`
        // (RFC 7519 allows fractions) fails here (FM1 stall shape). `scope`/
        // `scopes` are deliberately NOT gated: `check_scope` below keeps the
        // dual-shape Value tolerance (string | array) so array-form `scope`
        // tokens keep delivering (delta ④ pin).
        if serde_json::from_value::<ClientCredentialsGateClaims>(claims.clone()).is_err() {
            return Err(ClaimRejection {
                reason: "token claims missing required fields (sub/client_id)".into(),
            });
        }
        let iss_present = claims.get(CLAIM_ISS).and_then(Value::as_str).is_some();
        if !check_issuer(&claims, &self.cc_cfg) {
            return Err(ClaimRejection {
                reason: if iss_present {
                    "token iss does not match the configured issuer".into()
                } else {
                    "token has no iss claim".into()
                },
            });
        }
        if !check_audience(&claims, &self.cc_cfg) {
            return Err(ClaimRejection {
                reason: "token aud does not contain the configured audience".into(),
            });
        }
        if !check_scope(&claims, &self.cc_cfg) {
            return Err(ClaimRejection {
                reason: "token scope does not contain the configured audit scope".into(),
            });
        }
        let sub_present = claims.get(CLAIM_SUB).and_then(Value::as_str).is_some();
        if !check_subject(&claims, &self.config.expected_sub) {
            return Err(ClaimRejection {
                reason: if sub_present {
                    "token sub does not match the configured identity".into()
                } else {
                    "token has no sub claim".into()
                },
            });
        }
        // RFC 7519 NumericDate time window (validated-when-present, D1): the
        // leeway absorbs boundary clock skew; a non-numeric present value is
        // malformed and fails closed.
        let now_secs = now.unix_timestamp();
        if let Some(exp) = claims.get("exp") {
            let exp = exp.as_f64().ok_or_else(|| ClaimRejection {
                reason: "token exp is not a number".into(),
            })?;
            if exp <= (now_secs + TIME_CLAIM_LEEWAY_SECS) as f64 {
                return Err(ClaimRejection {
                    reason: "token has expired".into(),
                });
            }
        }
        if let Some(nbf) = claims.get("nbf") {
            let nbf = nbf.as_f64().ok_or_else(|| ClaimRejection {
                reason: "token nbf is not a number".into(),
            })?;
            if nbf > (now_secs + TIME_CLAIM_LEEWAY_SECS) as f64 {
                return Err(ClaimRejection {
                    reason: "token is not yet valid".into(),
                });
            }
        }
        Ok(())
    }

    /// Signature plane (D6: runs before the claims plane on every token that
    /// will be used for a POST). JWKS-off → `Ok(())` unconditionally.
    /// JWKS-on: alg must be RS256, the key source must resolve the header's
    /// `kid`, and the signature must verify under an explicit `Validation`
    /// whose time/audience/spec-claim checks are all disabled — those belong
    /// to the claims plane (F1: jsonwebtoken 9.3.1 defaults `validate_aud`/
    /// `validate_exp`/`required_spec_claims` ON and would otherwise dead
    /// real tokens on the JWKS-on face).
    async fn verify_token_signature(&self, token: &str) -> Result<(), DeliveryError> {
        let Some(keys) = self.keys.as_ref() else {
            return Ok(());
        };
        let Ok(header) = jsonwebtoken::decode_header(token) else {
            return Err(self.signature_rejected(token, "malformed").await);
        };
        if header.alg != Algorithm::RS256 {
            return Err(self.signature_rejected(token, "unsupported_alg").await);
        }
        let key = match keys
            .decoding_key_fallible(header.kid.as_deref(), Algorithm::RS256)
            .await
        {
            Ok(Some(key)) => key,
            // Genuine unknown-kid (even after the key source refreshed): the
            // permanent plane — requeue once then dead.
            Ok(None) => return Err(self.signature_rejected(token, "unknown_key").await),
            // The key-source mechanism is unavailable (fetch/refresh
            // failure): the transient plane — requeue forever, never dead.
            Err(error) => {
                return Err(DeliveryError::Transient(anyhow!(
                    "audit jwks unavailable: {error}"
                )));
            }
        };
        let mut validation = Validation::new(Algorithm::RS256);
        validation.validate_aud = false;
        validation.validate_exp = false;
        validation.validate_nbf = false;
        validation.required_spec_claims.clear();
        match jsonwebtoken::decode::<Value>(token, &key, &validation) {
            Ok(_) => Ok(()),
            Err(_) => Err(self.signature_rejected(token, "other").await),
        }
    }

    /// Signature-plane rejection: invalidate the poisoned token cache (D7 —
    /// the retry must rotate a fresh token, or the ≤1-retry budget is fake)
    /// and classify permanent. `reason` feeds the bounded rejection counter
    /// (runbook vocabulary: {malformed, unsupported_alg, unknown_key, other}).
    async fn signature_rejected(&self, token: &str, reason: &str) -> DeliveryError {
        crate::metrics::inc_token_rejection(reason);
        self.invalidate_token(token).await;
        DeliveryError::Permanent(PermanentKind::SignatureRejected)
    }

    async fn access_token(&self) -> Result<String, DeliveryError> {
        {
            let cache = self.token_cache.lock().await;
            if let Some(token) = cache.as_ref() {
                if Instant::now() < token.refresh_at {
                    return Ok(token.value.clone());
                }
            }
        }
        // Never hold the cache lock across network I/O: serial timeout queues
        // could otherwise outlive every claimed delivery lease.
        let token = self.request_token().await?;
        let ttl = token.expires_in.unwrap_or(300).clamp(1, 86_400);
        let usable = if ttl > 60 { ttl - 30 } else { (ttl / 2).max(1) };
        let mut cache = self.token_cache.lock().await;
        if let Some(current) = cache.as_ref() {
            if Instant::now() < current.refresh_at {
                return Ok(current.value.clone());
            }
        }
        let value = token.access_token;
        *cache = Some(CachedToken {
            value: value.clone(),
            refresh_at: Instant::now() + StdDuration::from_secs(usable),
        });
        Ok(value)
    }

    async fn request_token(&self) -> anyhow::Result<TokenResponse> {
        let response = self
            .http
            .post(self.config.token_endpoint.clone())
            .basic_auth(&self.config.client_id, Some(&self.config.client_secret))
            .form(&[
                ("grant_type", "client_credentials"),
                ("scope", self.config.expected_scope.as_str()),
                ("resource", self.config.resource.as_str()),
            ])
            .send()
            .await
            .map_err(transport_error)?;
        if response.status() != StatusCode::OK {
            bail!(
                "audit token endpoint returned HTTP {}",
                response.status().as_u16()
            );
        }
        let body = bounded_body(response, MAX_TOKEN_BODY).await?;
        let token: TokenResponse =
            serde_json::from_slice(&body).context("decode audit token response")?;
        validate_token_shape(&token)?;
        Ok(token)
    }

    async fn invalidate_token(&self, rejected: &str) {
        let mut cache = self.token_cache.lock().await;
        if cache.as_ref().is_some_and(|token| token.value == rejected) {
            *cache = None;
        }
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokenResponse")
            .field("access_token", &"<redacted>")
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

#[derive(Deserialize)]
struct AuditReceiptEnvelope {
    receipt: AuditReceipt,
}

#[derive(Deserialize)]
struct AuditReceipt {
    event_id: String,
    tenant_id: String,
    status: String,
    #[serde(default, with = "time::serde::rfc3339::option")]
    accepted_at: Option<time::OffsetDateTime>,
    #[serde(default)]
    conflict: bool,
}

/// Outbound payload guard (v1 `validate_delivery_payload` clone): the payload
/// must not select a tenant and its `source_system` must equal the trusted
/// binding.
fn validate_delivery_payload(config: &RelayConfig, claim: &Claim) -> anyhow::Result<()> {
    if claim.payload.get("tenant_id").is_some() {
        bail!("outbound audit payload must not select a tenant");
    }
    if claim.payload.get("source_system").and_then(Value::as_str)
        != Some(config.source_system.as_str())
    {
        bail!("audit payload source does not match the trusted binding");
    }
    Ok(())
}

/// Value-level dual-format receipt comparison (D1).
///
/// The 0239 trigger / 0241 reconciler embed `NEW.id::text` (hyphenated UUID)
/// into the payload's `event_id`, and sinks echoing the payload field (the
/// in-repo stub contract) produce that UUID string. The outbound
/// `Idempotency-Key` header carries the same value in `AuditId`'s Display
/// form (ULID base32), so a sink echoing the header produces base32. A
/// naive `receipt.event_id != claim.event_id.to_string()` breaks against
/// the UUID echo — `claim.event_id.to_string()` is base32 — which would
/// misclassify every delivery as `ReceiptMismatch` and dead it after ≤1
/// retry. Compare value-level instead: parse the receipt echo as UUID first
/// (a 26-char base32 string is never a valid UUID shape, so the order is
/// deterministic for both echo sources), else as ULID.
fn receipt_event_id_matches(receipt_event_id: &str, claim: &Claim) -> bool {
    if let Ok(uuid) = Uuid::parse_str(receipt_event_id) {
        return AuditId::from_uuid(uuid) == claim.event_id;
    }
    receipt_event_id
        .parse::<AuditId>()
        .is_ok_and(|id| id == claim.event_id)
}

/// Durable receipt guard (v1 `validate_audit_receipt` clone): `event_id`
/// matches the claim (value-level, both echo formats), `accepted_at`
/// present, `conflict` false, status in {ledgered, indexed, archived}. Any
/// failure is a permanent class.
fn validate_audit_receipt(body: &[u8], claim: &Claim) -> anyhow::Result<()> {
    let envelope: AuditReceiptEnvelope =
        serde_json::from_slice(body).context("decode Audit Governance receipt")?;
    let receipt = envelope.receipt;
    if !receipt_event_id_matches(&receipt.event_id, claim)
        || receipt.tenant_id.is_empty()
        || receipt.accepted_at.is_none()
        || receipt.conflict
        || !matches!(receipt.status.as_str(), "ledgered" | "indexed" | "archived")
    {
        bail!("Audit Governance returned an invalid durable receipt");
    }
    Ok(())
}

/// Bearer shape check (v1 `validate_token` clone): non-empty, bounded, no
/// control characters, bearer type.
fn validate_token_shape(token: &TokenResponse) -> anyhow::Result<()> {
    if token.access_token.is_empty()
        || token.access_token.len() > MAX_ACCESS_TOKEN_BYTES
        || token.access_token.chars().any(char::is_control)
        || !token
            .token_type
            .as_deref()
            .is_some_and(|kind| kind.eq_ignore_ascii_case("bearer"))
    {
        bail!("audit token endpoint returned an invalid bearer token");
    }
    Ok(())
}

/// Decode the JWT payload segment without signature verification (base64url,
/// padding tolerated). The token is rejected fail-closed when it is opaque or
/// malformed.
fn decode_jwt_claims(token: &str) -> Result<Value, String> {
    let mut segments = token.split('.');
    let _header = segments
        .next()
        .ok_or_else(|| "token has no JWT header segment".to_owned())?;
    let payload = segments
        .next()
        .ok_or_else(|| "token has no JWT payload segment".to_owned())?;
    if segments.next().is_none() {
        return Err("token has no JWT signature segment".to_owned());
    }
    if segments.next().is_some() {
        return Err("token has too many JWT segments".to_owned());
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
        .map_err(|_| "JWT payload is not valid base64url".to_owned())?;
    serde_json::from_slice(&decoded).map_err(|_| "JWT payload is not valid JSON".to_owned())
}

async fn bounded_body(response: reqwest::Response, max: usize) -> anyhow::Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > max as u64)
    {
        bail!("audit connector response exceeded its size limit");
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.try_next().await.map_err(transport_error)? {
        if body.len().saturating_add(chunk.len()) > max {
            bail!("audit connector response exceeded its size limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn transport_error(error: reqwest::Error) -> anyhow::Error {
    anyhow!(
        "audit connector HTTP transport failed: {}",
        error.without_url()
    )
}
