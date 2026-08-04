//! `OpenID` Connect (OIDC) ID-token validation — the verification core of SSO.
//!
//! Given an external `IdP`'s signed ID token, this module verifies it
//! (RS256 or Ed25519/EdDSA signature + issuer + audience + expiry) and extracts the standard
//! claims (`sub`, `email`, `name`). The HTTP layer then maps the resulting
//! `(issuer, sub)` onto an internal participant (JIT-provisioning on first sight)
//! and mints *our own* tokens — the external token never leaves this boundary.
//!
//! ## The key seam: [`KeyProvider`]
//!
//! Signature verification needs the `IdP`'s public key. Production fetches the
//! JWKS document from the `IdP`'s `jwks_uri` ([`JwksKeyProvider`]); tests inject a
//! known key ([`StaticKeyProvider`]). [`validate_id_token`] depends only on the
//! trait, so the *validation logic* — the security-critical part — is fully unit
//! tested offline by signing real JWTs with in-test RSA and Ed25519 keypairs. The live
//! network fetch is the one documented, non-unit-tested seam.
//!
//! OIDC is **off by default**: [`OidcConfig::from_env`] returns `None` unless the
//! deployment explicitly configures an issuer/audience/JWKS URI.

use futures::StreamExt as _;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::sync::Mutex;

mod id_token;
use id_token::SignedOidcClaims;

/// A JWKS endpoint that violates Aero's outbound-network policy.
///
/// The error deliberately carries no copy of the rejected value: deployment
/// URLs can contain operationally sensitive path components and must not leak
/// through configuration or fetch diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("JWKS URI is not allowed")]
pub struct JwksUriError;

const MAX_JWKS_URI_BYTES: usize = 2 * 1024;

/// Validate a JWKS endpoint before it reaches the network layer.
///
/// Remote endpoints must use HTTPS. Plain HTTP is supported only for an exact
/// loopback IP address or `localhost`, which keeps local development possible
/// without allowing cleartext remote key retrieval. Userinfo, query strings,
/// and fragments are rejected; explicit ports remain supported.
///
/// # Errors
/// Returns [`JwksUriError`] for malformed URLs or URLs outside this policy.
pub fn validate_jwks_uri(value: &str) -> Result<(), JwksUriError> {
    parse_jwks_uri(value).map(|_| ())
}

fn parse_jwks_uri(value: &str) -> Result<reqwest::Url, JwksUriError> {
    if value.is_empty() || value.len() > MAX_JWKS_URI_BYTES || value != value.trim() {
        return Err(JwksUriError);
    }
    let url = reqwest::Url::parse(value).map_err(|_| JwksUriError)?;
    let host = url.host_str().ok_or(JwksUriError)?;
    let authority = value
        .split_once("://")
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .ok_or(JwksUriError)?;
    if authority.contains('@')
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(JwksUriError);
    }

    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(JwksUriError);
    }
    Ok(url)
}

/// Static OIDC relying-party configuration.
///
/// `issuer` and `audience` are checked against the token's `iss`/`aud` claims;
/// `jwks_uri` is where [`JwksKeyProvider`] fetches signing keys.
#[derive(Debug, Clone)]
pub struct OidcConfig {
    pub issuer: String,
    pub audience: String,
    pub jwks_uri: String,
}

impl OidcConfig {
    /// Load from `AERO__OIDC__ISSUER` / `AERO__OIDC__AUDIENCE` /
    /// `AERO__OIDC__JWKS_URI`.
    ///
    /// Returns `None` unless **all three** are set and non-empty and the JWKS URI
    /// passes [`validate_jwks_uri`] — so OIDC stays disabled by default and a
    /// partial or unsafe deployment configuration fails closed.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let issuer = non_empty_env("AERO__OIDC__ISSUER")?;
        let audience = non_empty_env("AERO__OIDC__AUDIENCE")?;
        let jwks_uri = non_empty_env("AERO__OIDC__JWKS_URI")?;
        validate_jwks_uri(&jwks_uri).ok()?;
        Some(Self {
            issuer,
            audience,
            jwks_uri,
        })
    }
}

/// Read an env var, treating "missing" and "present but blank/whitespace" alike.
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// The subset of OIDC ID-token claims we consume. `iss`/`aud`/`exp` are verified
/// by [`validate_id_token`] (via `jsonwebtoken`'s [`Validation`]) and so are not
/// re-surfaced here; `sub` is the stable per-issuer user identifier.
#[derive(Clone, Serialize, Deserialize)]
pub struct OidcClaims {
    /// Stable subject identifier at the issuer — the identity key.
    pub sub: String,
    /// End-user email, when the `IdP` releases it.
    #[serde(default)]
    pub email: Option<String>,
    /// Human-readable display name, when present.
    #[serde(default)]
    pub name: Option<String>,
    /// Whether the `IdP` asserts the email is verified (passed through; the caller
    /// decides whether to require it).
    #[serde(default)]
    pub email_verified: Option<bool>,
    /// Preferred username (some `IdPs` send this instead of `name`).
    #[serde(default)]
    pub preferred_username: Option<String>,
    /// OIDC replay-binding value. Browser authorization-code flows must compare
    /// this to the nonce they generated before redirecting to the provider.
    #[serde(default)]
    pub nonce: Option<String>,
    /// Token issue time. Ordinary login remains compatible with providers that
    /// omit it; security-sensitive proof-of-possession flows may require and
    /// apply a tighter freshness window at their boundary.
    #[serde(default)]
    pub iat: Option<u64>,
}

// Identity subjects and profile data are credentials-adjacent PII. Keep the
// type debuggable for `Result::unwrap_err` and diagnostics without ever
// formatting those values.
impl fmt::Debug for OidcClaims {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OidcClaims")
            .field("sub", &"[redacted]")
            .field("email", &self.email.as_ref().map(|_| "[redacted]"))
            .field("name", &self.name.as_ref().map(|_| "[redacted]"))
            .field("email_verified", &self.email_verified)
            .field(
                "preferred_username",
                &self.preferred_username.as_ref().map(|_| "[redacted]"),
            )
            .field("nonce", &self.nonce.as_ref().map(|_| "[redacted]"))
            .field("iat", &self.iat)
            .finish()
    }
}

impl OidcClaims {
    /// Best display name for JIT provisioning: `name`, else `preferred_username`,
    /// else the local-part of `email`, else the opaque `sub`. Always non-empty.
    #[must_use]
    pub fn best_display_name(&self) -> String {
        if let Some(n) = self
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return n.to_owned();
        }
        if let Some(u) = self
            .preferred_username
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return u.to_owned();
        }
        if let Some(local) = self
            .email
            .as_deref()
            .and_then(|e| e.split('@').next())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return local.to_owned();
        }
        self.sub.clone()
    }
}

/// Failure modes of [`validate_id_token`]. Distinct variants so the caller (and
/// tests) can reason about *why* a token was rejected; the HTTP layer collapses
/// them all to `401`/`400` without leaking specifics to the client.
#[derive(Debug, Error)]
pub enum OidcError {
    /// The token isn't a well-formed JWT (header could not be decoded).
    #[error("malformed token: {0}")]
    MalformedToken(String),
    /// No verification key matched the token's `kid` (unknown key / JWKS miss).
    #[error("no signing key for kid {0:?}")]
    UnknownKey(Option<String>),
    /// The JOSE header requested an algorithm outside the explicit OIDC
    /// allowlist. Never infer an algorithm from key material.
    #[error("unsupported signing algorithm")]
    UnsupportedAlgorithm,
    /// Signature, issuer, audience, or expiry validation failed.
    #[error("token rejected: {0}")]
    Invalid(String),
}

/// Source of public keys for signature verification — the testable seam.
///
/// Implementations resolve a token's optional `kid` (key id, from the JWT header)
/// and already-allowlisted algorithm to a [`DecodingKey`]. Returning `None`
/// means "I have no compatible key for that id", which [`validate_id_token`]
/// surfaces as [`OidcError::UnknownKey`].
#[axum::async_trait]
pub trait KeyProvider: Send + Sync {
    async fn decoding_key(&self, kid: Option<&str>, algorithm: Algorithm) -> Option<DecodingKey>;
}

/// In-memory key provider holding one or more known keys.
///
/// Used by tests and for a statically-configured signing key. Keys may be tagged
/// with a `kid`; an untagged key (registered via [`StaticKeyProvider::single`])
/// matches any lookup, which is the common single-key case.
pub struct StaticKeyProvider {
    /// `kid`-tagged keys, matched exactly against the token header's `kid`.
    keyed: Vec<(String, DecodingKey)>,
    /// A fallback key used when no `kid` matches (or the token carries no `kid`).
    fallback: Option<DecodingKey>,
}

impl StaticKeyProvider {
    /// Provider with a single key that matches every lookup regardless of `kid`.
    #[must_use]
    pub fn single(key: DecodingKey) -> Self {
        Self {
            keyed: Vec::new(),
            fallback: Some(key),
        }
    }

    /// Provider from a set of `(kid, key)` pairs. A token's header `kid` must
    /// match one of them; tokens without a `kid` are rejected (no fallback).
    #[must_use]
    pub fn with_keyed(keys: Vec<(String, DecodingKey)>) -> Self {
        Self {
            keyed: keys,
            fallback: None,
        }
    }
}

#[axum::async_trait]
impl KeyProvider for StaticKeyProvider {
    async fn decoding_key(&self, kid: Option<&str>, _algorithm: Algorithm) -> Option<DecodingKey> {
        if let Some(kid) = kid {
            if let Some((_, k)) = self.keyed.iter().find(|(k, _)| k == kid) {
                return Some(k.clone());
            }
        }
        self.fallback.clone()
    }
}

/// Default clock-skew tolerance (seconds) applied to `exp`/`nbf`/`iat`.
const LEEWAY_SECS: u64 = 60;

/// Verify an OIDC ID token and return its claims.
///
/// Steps: decode the JWT header; reject access-token or nonstandard `typ` values
/// while remaining compatible with omitted `typ`, `JWT`, and `application/jwt`;
/// reject every algorithm except `RS256` and `EdDSA`; ask `keys` for a key
/// compatible with that exact algorithm and `kid`; then `jsonwebtoken::decode`
/// with required issuer/audience/expiry/subject checking. A present `nbf` is
/// enforced. Finally, OIDC's `azp` rule is applied: it is mandatory for a token
/// with multiple audience values, and whenever present it must equal our exact
/// configured audience. On success the returned [`OidcClaims`] is trustworthy:
/// signed by the `IdP`, issued for us, and currently valid.
///
/// # Errors
/// * [`OidcError::MalformedToken`] — the token isn't a decodable JWT.
/// * [`OidcError::UnsupportedAlgorithm`] — `alg` is not `RS256` or `EdDSA`.
/// * [`OidcError::UnknownKey`] — no key matched the token's `kid`.
/// * [`OidcError::Invalid`] — bad signature / wrong issuer / wrong audience /
///   expired (or otherwise failed validation).
pub async fn validate_id_token(
    token: &str,
    cfg: &OidcConfig,
    keys: &dyn KeyProvider,
) -> Result<OidcClaims, OidcError> {
    let header = decode_header(token).map_err(|e| OidcError::MalformedToken(e.to_string()))?;
    if !valid_id_token_type(header.typ.as_deref()) {
        return Err(OidcError::Invalid("token is not an OIDC ID token".into()));
    }
    let algorithm = match header.alg {
        Algorithm::RS256 => Algorithm::RS256,
        Algorithm::EdDSA => Algorithm::EdDSA,
        _ => return Err(OidcError::UnsupportedAlgorithm),
    };
    let kid = header.kid.clone();
    let key = keys
        .decoding_key(kid.as_deref(), algorithm)
        .await
        .ok_or(OidcError::UnknownKey(kid))?;

    // `Validation::new` allowlists exactly this one header-selected algorithm.
    // The selection above is intentionally closed; do not accept a provider-
    // supplied arbitrary algorithm here.
    let mut validation = Validation::new(algorithm);
    validation.set_issuer(&[cfg.issuer.as_str()]);
    validation.set_audience(&[cfg.audience.as_str()]);
    validation.validate_exp = true;
    validation.validate_nbf = true;
    validation.leeway = LEEWAY_SECS;

    // jsonwebtoken intentionally skips issuer/audience checks when those
    // claims are absent unless they are also required. Keep the relying-party
    // boundary explicit instead of relying on library defaults.
    validation.set_required_spec_claims(&["iss", "aud", "exp", "sub"]);
    let claims = decode::<SignedOidcClaims>(token, &key, &validation)
        .map(|data| data.claims)
        .map_err(|e| OidcError::Invalid(e.to_string()))?;
    claims.into_verified(cfg)
}

fn valid_id_token_type(kind: Option<&str>) -> bool {
    match kind {
        None => true,
        Some(kind) => {
            kind.eq_ignore_ascii_case("jwt") || kind.eq_ignore_ascii_case("application/jwt")
        }
    }
}

/// Verification policy for a Snaplink OAuth client-credentials access token.
#[derive(Debug, Clone)]
pub struct ClientCredentialsTokenConfig {
    pub issuer: String,
    pub audience: String,
    pub required_scopes: Vec<String>,
}

/// Trusted machine identity extracted from a validated access token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientCredentialsClaims {
    pub sub: String,
    pub client_id: String,
    pub iat: u64,
    pub jti: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

impl ClientCredentialsClaims {
    /// Normalized union of Snaplink's `scopes` array and the standard OAuth
    /// space-delimited `scope` compatibility claim.
    #[must_use]
    pub fn granted_scopes(&self) -> std::collections::BTreeSet<&str> {
        self.scopes
            .iter()
            .map(String::as_str)
            .chain(self.scope.iter().flat_map(|scope| scope.split_whitespace()))
            .collect()
    }
}

/// Validate a Snaplink RFC 9068 access token minted by `client_credentials`.
/// This is deliberately separate from [`validate_id_token`]: it requires an
/// `at+jwt` JOSE type, a machine subject equal to `client_id`, exact issuer and
/// audience, expiry/nbf/issued-at, a usable token id, and all configured
/// application scopes.
///
/// # Errors
/// Returns [`OidcError`] for malformed headers, unknown keys, disallowed
/// algorithms, invalid registered claims, user-token confusion, or missing
/// scopes.
pub async fn validate_client_credentials_token(
    token: &str,
    cfg: &ClientCredentialsTokenConfig,
    keys: &dyn KeyProvider,
) -> Result<ClientCredentialsClaims, OidcError> {
    let header = decode_header(token).map_err(|e| OidcError::MalformedToken(e.to_string()))?;
    if !matches!(
        header.typ.as_deref(),
        Some(kind)
            if kind.eq_ignore_ascii_case("at+jwt")
                || kind.eq_ignore_ascii_case("application/at+jwt")
    ) {
        return Err(OidcError::Invalid(
            "token is not an RFC 9068 access token".into(),
        ));
    }
    let algorithm = match header.alg {
        Algorithm::RS256 => Algorithm::RS256,
        Algorithm::EdDSA => Algorithm::EdDSA,
        _ => return Err(OidcError::UnsupportedAlgorithm),
    };
    let kid = header.kid.clone();
    let key = keys
        .decoding_key(kid.as_deref(), algorithm)
        .await
        .ok_or(OidcError::UnknownKey(kid))?;

    let mut validation = Validation::new(algorithm);
    validation.set_issuer(&[cfg.issuer.as_str()]);
    validation.set_audience(&[cfg.audience.as_str()]);
    validation.validate_exp = true;
    validation.validate_nbf = true;
    validation.leeway = LEEWAY_SECS;
    for claim in ["iss", "aud", "exp", "nbf", "iat", "jti", "sub", "client_id"] {
        validation.required_spec_claims.insert(claim.to_owned());
    }
    let claims = decode::<ClientCredentialsClaims>(token, &key, &validation)
        .map(|data| data.claims)
        .map_err(|e| OidcError::Invalid(e.to_string()))?;
    if !valid_identity_component(&claims.sub)
        || !valid_identity_component(&claims.client_id)
        || claims.sub != claims.client_id
    {
        return Err(OidcError::Invalid(
            "token is not a client_credentials machine identity".into(),
        ));
    }
    if claims.iat > jsonwebtoken::get_current_timestamp().saturating_add(LEEWAY_SECS) {
        return Err(OidcError::Invalid(
            "token issued-at time is in the future".into(),
        ));
    }
    if claims.jti.is_empty() || claims.jti.len() > 1_024 || claims.jti.chars().any(char::is_control)
    {
        return Err(OidcError::Invalid("token id is invalid".into()));
    }
    let granted = claims.granted_scopes();
    if cfg
        .required_scopes
        .iter()
        .any(|required| !granted.contains(required.as_str()))
    {
        return Err(OidcError::Invalid(
            "token lacks a required application scope".into(),
        ));
    }
    Ok(claims)
}

fn valid_identity_component(value: &str) -> bool {
    !value.is_empty() && value == value.trim() && !value.chars().any(char::is_control)
}

// ---------- Live JWKS-backed key provider (the documented network seam) ----------
//
// This is the ONE part of the module not exercised by unit tests: it performs a
// real bounded HTTP GET against the IdP's `jwks_uri`. The parsing of RSA and
// Ed25519 keys is covered below; signature validation is exercised through both
// live-format JWK conversion and `StaticKeyProvider` tests. A production
// deployment would typically wrap this in a TTL cache keyed on `jwks_uri`.

/// One signing-key entry in a JWKS document.
#[derive(Debug, Clone, Deserialize)]
struct Jwk {
    /// Key type — only `"RSA"` and `"OKP"` are accepted.
    kty: String,
    /// Optional key id, matched against the token header's `kid`.
    #[serde(default)]
    kid: Option<String>,
    /// Intended use — `"sig"` (signature) when present; others are skipped.
    #[serde(default, rename = "use")]
    use_: Option<String>,
    /// Declared JOSE algorithm. When present it must match the token header.
    #[serde(default)]
    alg: Option<String>,
    /// Permitted operations. When present, it must include `verify`.
    #[serde(default)]
    key_ops: Option<Vec<String>>,
    /// RSA modulus, base64url (no padding) — fed straight to `from_rsa_components`.
    #[serde(default)]
    n: Option<String>,
    /// RSA public exponent, base64url (no padding).
    #[serde(default)]
    e: Option<String>,
    /// OKP curve — must be `Ed25519`.
    #[serde(default)]
    crv: Option<String>,
    /// Ed25519 public key, base64url (no padding).
    #[serde(default)]
    x: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct JwkSet {
    keys: Vec<Jwk>,
}

/// Fetches the `IdP`'s JWKS over HTTP and builds [`DecodingKey`]s on demand.
///
/// This is the live network seam. Construct it from an [`OidcConfig`] and hand it
/// to [`validate_id_token`]. Successful sets are TTL-cached; an unknown key id
/// forces a refresh to pick up provider rotation, globally throttled per provider
/// so invalid key ids cannot cause an outbound JWKS request storm.
pub struct JwksKeyProvider {
    jwks_uri: Option<reqwest::Url>,
    http: Option<reqwest::Client>,
    cache: Mutex<JwksCacheState>,
    cache_ttl: Duration,
}

#[derive(Clone)]
struct CachedJwks {
    set: JwkSet,
    expires_at: Instant,
}

#[derive(Default)]
struct JwksCacheState {
    entry: Option<CachedJwks>,
    last_unknown_kid_refresh: Option<Instant>,
}

impl JwksCacheState {
    /// Reserve the single outbound refresh allowed in an unknown-kid window.
    /// The timestamp is recorded before I/O so a failing `IdP` cannot turn invalid
    /// tokens into an unbounded request amplifier.
    fn reserve_unknown_kid_refresh(&mut self, now: Instant) -> bool {
        if self.last_unknown_kid_refresh.is_some_and(|last| {
            now.saturating_duration_since(last) < UNKNOWN_KID_REFRESH_MIN_INTERVAL
        }) {
            return false;
        }
        self.last_unknown_kid_refresh = Some(now);
        true
    }
}

const JWKS_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const JWKS_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_JWKS_BYTES: usize = 256 * 1024;
const DEFAULT_JWKS_CACHE_TTL: Duration = Duration::from_secs(300);
const UNKNOWN_KID_REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(10);

impl JwksKeyProvider {
    #[must_use]
    pub fn new(jwks_uri: impl Into<String>) -> Self {
        let jwks_uri = jwks_uri.into();
        Self {
            // Keep construction infallible for existing callers, but retain no
            // request target unless it passes the same policy as env config.
            // `fetch` therefore fails closed for manually-built invalid config.
            jwks_uri: parse_jwks_uri(&jwks_uri).ok(),
            // Redirects are disabled so an IdP-controlled redirect cannot move
            // key retrieval to an unintended host. A construction failure is
            // retained as an unavailable provider rather than panicking.
            http: reqwest::Client::builder()
                .connect_timeout(JWKS_CONNECT_TIMEOUT)
                .timeout(JWKS_REQUEST_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .ok(),
            cache: Mutex::new(JwksCacheState::default()),
            cache_ttl: DEFAULT_JWKS_CACHE_TTL,
        }
    }

    /// Build from an [`OidcConfig`]'s `jwks_uri`.
    #[must_use]
    pub fn from_config(cfg: &OidcConfig) -> Self {
        Self::new(cfg.jwks_uri.clone())
    }

    /// Fetch and parse the JWKS document.
    async fn fetch(&self) -> Result<JwkSet, String> {
        let jwks_uri = self
            .jwks_uri
            .as_ref()
            .ok_or_else(|| "jwks URI rejected by policy".to_owned())?;
        let http = self
            .http
            .as_ref()
            .ok_or_else(|| "jwks client unavailable".to_owned())?;
        let resp = http
            .get(jwks_uri.clone())
            .send()
            .await
            .map_err(|error| safe_jwks_request_error(&error))?;
        if !resp.status().is_success() {
            return Err(format!(
                "jwks endpoint returned HTTP {}",
                resp.status().as_u16()
            ));
        }
        if resp
            .content_length()
            .is_some_and(|len| len > MAX_JWKS_BYTES as u64)
        {
            return Err("jwks response too large".to_owned());
        }
        let mut stream = resp.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| {
                if error.is_timeout() {
                    "jwks response timed out".to_owned()
                } else {
                    "jwks response body failed".to_owned()
                }
            })?;
            if body.len().saturating_add(chunk.len()) > MAX_JWKS_BYTES {
                return Err("jwks response too large".to_owned());
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice::<JwkSet>(&body)
            .map_err(|_| "jwks response is not valid JSON".to_owned())
    }

    async fn cached_set(&self, force_refresh: bool) -> Result<JwkSet, String> {
        // Holding the mutex across refresh intentionally coalesces a burst of
        // requests after startup/rotation into one bounded network fetch.
        let mut state = self.cache.lock().await;
        if !force_refresh {
            if let Some(entry) = state.entry.as_ref() {
                if Instant::now() < entry.expires_at {
                    return Ok(entry.set.clone());
                }
            }
        } else if !state.reserve_unknown_kid_refresh(Instant::now()) {
            // The caller already inspected this set and did not find its key.
            // Returning it again deliberately avoids outbound I/O; key lookup
            // below will reject the token while the short throttle window runs.
            return state
                .entry
                .as_ref()
                .map(|entry| entry.set.clone())
                .ok_or_else(|| "jwks unknown-kid refresh throttled".to_owned());
        }
        let set = self.fetch().await?;
        state.entry = Some(CachedJwks {
            set: set.clone(),
            expires_at: Instant::now() + self.cache_ttl,
        });
        Ok(set)
    }
}

fn safe_jwks_request_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "jwks request timed out".to_owned()
    } else if error.is_connect() {
        "jwks connection failed".to_owned()
    } else if error.is_builder() {
        "jwks request could not be built".to_owned()
    } else {
        "jwks request failed".to_owned()
    }
}

/// Turn a signing JWK into a [`DecodingKey`] compatible with `algorithm`.
fn jwk_to_key(jwk: &Jwk, algorithm: Algorithm) -> Option<DecodingKey> {
    if matches!(jwk.use_.as_deref(), Some(u) if u != "sig") {
        return None;
    }
    if jwk
        .key_ops
        .as_ref()
        .is_some_and(|ops| !ops.iter().any(|op| op == "verify"))
    {
        return None;
    }

    match algorithm {
        Algorithm::RS256 => {
            if jwk.kty != "RSA" || matches!(jwk.alg.as_deref(), Some(alg) if alg != "RS256") {
                return None;
            }
            let (n, e) = (jwk.n.as_deref()?, jwk.e.as_deref()?);
            DecodingKey::from_rsa_components(n, e).ok()
        }
        Algorithm::EdDSA => {
            if jwk.kty != "OKP"
                || jwk.crv.as_deref() != Some("Ed25519")
                || matches!(jwk.alg.as_deref(), Some(alg) if alg != "EdDSA")
            {
                return None;
            }
            DecodingKey::from_ed_components(jwk.x.as_deref()?).ok()
        }
        _ => None,
    }
}

#[axum::async_trait]
impl KeyProvider for JwksKeyProvider {
    async fn decoding_key(&self, kid: Option<&str>, algorithm: Algorithm) -> Option<DecodingKey> {
        let set = match self.cached_set(false).await {
            Ok(set) => set,
            Err(e) => {
                tracing::warn!(error = %e, "oidc jwks fetch failed");
                return None;
            }
        };
        if let Some(key) = key_from_set(&set, kid, algorithm) {
            return Some(key);
        }
        // Rotation seam: a previously unseen `kid` (or a formerly ambiguous
        // no-kid set) gets an immediate refresh before rejection. The provider-
        // wide gate limits subsequent misses to one outbound attempt per window.
        let refreshed = match self.cached_set(true).await {
            Ok(set) => set,
            Err(error) => {
                tracing::warn!(%error, "oidc jwks refresh failed");
                return None;
            }
        };
        key_from_set(&refreshed, kid, algorithm)
    }
}

fn key_from_set(set: &JwkSet, kid: Option<&str>, algorithm: Algorithm) -> Option<DecodingKey> {
    // A supplied `kid` must match exactly one JWK. Never fall back to a
    // differently-named key: accepting a token under a key id the provider did
    // not select makes rotation and incident response ambiguous.
    if let Some(kid) = kid {
        let mut matches = set.keys.iter().filter(|j| j.kid.as_deref() == Some(kid));
        let only = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        return jwk_to_key(only, algorithm);
    }
    // Without `kid`, accept only a single compatible verification key.
    let mut usable = set.keys.iter().filter_map(|jwk| jwk_to_key(jwk, algorithm));
    match (usable.next(), usable.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
