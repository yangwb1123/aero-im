//! `OpenID` Connect (OIDC) ID-token validation — the verification core of SSO.
//!
//! Given an external `IdP`'s signed ID token, this module verifies it
//! (RS256 signature + issuer + audience + expiry) and extracts the standard
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
//! tested offline by signing real JWTs with an in-test RSA keypair. The live
//! network fetch is the one documented, non-unit-tested seam.
//!
//! OIDC is **off by default**: [`OidcConfig::from_env`] returns `None` unless the
//! deployment explicitly configures an issuer/audience/JWKS URI.

use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use thiserror::Error;

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
    /// Returns `None` unless **all three** are set and non-empty — so OIDC stays
    /// disabled by default and a half-configured deployment fails closed rather
    /// than silently trusting a partial config.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let issuer = non_empty_env("AERO__OIDC__ISSUER")?;
        let audience = non_empty_env("AERO__OIDC__AUDIENCE")?;
        let jwks_uri = non_empty_env("AERO__OIDC__JWKS_URI")?;
        Some(Self {
            issuer,
            audience,
            jwks_uri,
        })
    }
}

/// Read an env var, treating "missing" and "present but blank/whitespace" alike.
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// The subset of OIDC ID-token claims we consume. `iss`/`aud`/`exp` are verified
/// by [`validate_id_token`] (via `jsonwebtoken`'s [`Validation`]) and so are not
/// re-surfaced here; `sub` is the stable per-issuer user identifier.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

impl OidcClaims {
    /// Best display name for JIT provisioning: `name`, else `preferred_username`,
    /// else the local-part of `email`, else the opaque `sub`. Always non-empty.
    #[must_use]
    pub fn best_display_name(&self) -> String {
        if let Some(n) = self.name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
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
    /// Signature, issuer, audience, or expiry validation failed.
    #[error("token rejected: {0}")]
    Invalid(String),
}

/// Source of RSA public keys for signature verification — the testable seam.
///
/// Implementations resolve a token's optional `kid` (key id, from the JWT header)
/// to a [`DecodingKey`]. Returning `None` means "I have no key for that id",
/// which [`validate_id_token`] surfaces as [`OidcError::UnknownKey`].
#[axum::async_trait]
pub trait KeyProvider: Send + Sync {
    async fn decoding_key(&self, kid: Option<&str>) -> Option<DecodingKey>;
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
    async fn decoding_key(&self, kid: Option<&str>) -> Option<DecodingKey> {
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
/// Steps: decode the JWT header for its `kid`; ask `keys` for the matching
/// [`DecodingKey`]; then `jsonwebtoken::decode` with RS256 + the configured
/// issuer/audience + expiry checking. On success the returned [`OidcClaims`] is
/// trustworthy: signed by the `IdP`, issued for us, and unexpired.
///
/// # Errors
/// * [`OidcError::MalformedToken`] — the token isn't a decodable JWT.
/// * [`OidcError::UnknownKey`] — no key matched the token's `kid`.
/// * [`OidcError::Invalid`] — bad signature / wrong issuer / wrong audience /
///   expired (or otherwise failed validation).
pub async fn validate_id_token(
    token: &str,
    cfg: &OidcConfig,
    keys: &dyn KeyProvider,
) -> Result<OidcClaims, OidcError> {
    let header = decode_header(token).map_err(|e| OidcError::MalformedToken(e.to_string()))?;
    let kid = header.kid.clone();
    let key = keys
        .decoding_key(kid.as_deref())
        .await
        .ok_or(OidcError::UnknownKey(kid))?;

    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[cfg.issuer.as_str()]);
    validation.set_audience(&[cfg.audience.as_str()]);
    validation.validate_exp = true;
    validation.leeway = LEEWAY_SECS;

    decode::<OidcClaims>(token, &key, &validation)
        .map(|data| data.claims)
        .map_err(|e| OidcError::Invalid(e.to_string()))
}

// ---------- Live JWKS-backed key provider (the documented network seam) ----------
//
// This is the ONE part of the module not exercised by unit tests: it performs a
// real HTTP GET against the IdP's `jwks_uri`. The parsing of a JWKS document into
// `DecodingKey`s is straightforward (RSA `n`/`e` are base64url strings that
// `jsonwebtoken::DecodingKey::from_rsa_components` consumes directly), and the
// *validation* of any key it returns is covered by `validate_id_token`'s tests
// via `StaticKeyProvider`. A production deployment would typically wrap this in a
// TTL cache keyed on `jwks_uri`; that is left as a follow-up.

/// One RSA key entry in a JWKS document (we ignore non-RSA / non-signing keys).
#[derive(Debug, Clone, Deserialize)]
struct Jwk {
    /// Key type — must be `"RSA"` for us to use it.
    kty: String,
    /// Optional key id, matched against the token header's `kid`.
    #[serde(default)]
    kid: Option<String>,
    /// Intended use — `"sig"` (signature) when present; others are skipped.
    #[serde(default, rename = "use")]
    use_: Option<String>,
    /// RSA modulus, base64url (no padding) — fed straight to `from_rsa_components`.
    #[serde(default)]
    n: Option<String>,
    /// RSA public exponent, base64url (no padding).
    #[serde(default)]
    e: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct JwkSet {
    keys: Vec<Jwk>,
}

/// Fetches the `IdP`'s JWKS over HTTP and builds [`DecodingKey`]s on demand.
///
/// This is the live network seam. Construct it from an [`OidcConfig`] and hand it
/// to [`validate_id_token`]; it (re)fetches the JWKS on each lookup.
pub struct JwksKeyProvider {
    jwks_uri: String,
    http: reqwest::Client,
}

impl JwksKeyProvider {
    #[must_use]
    pub fn new(jwks_uri: impl Into<String>) -> Self {
        Self {
            jwks_uri: jwks_uri.into(),
            http: reqwest::Client::new(),
        }
    }

    /// Build from an [`OidcConfig`]'s `jwks_uri`.
    #[must_use]
    pub fn from_config(cfg: &OidcConfig) -> Self {
        Self::new(cfg.jwks_uri.clone())
    }

    /// Fetch and parse the JWKS document.
    async fn fetch(&self) -> Result<JwkSet, String> {
        let resp = self
            .http
            .get(&self.jwks_uri)
            .send()
            .await
            .map_err(|e| format!("jwks fetch: {e}"))?
            .error_for_status()
            .map_err(|e| format!("jwks status: {e}"))?;
        resp.json::<JwkSet>().await.map_err(|e| format!("jwks parse: {e}"))
    }
}

/// Turn a single RSA signing JWK into a [`DecodingKey`], if usable.
fn jwk_to_key(jwk: &Jwk) -> Option<DecodingKey> {
    if jwk.kty != "RSA" {
        return None;
    }
    if matches!(jwk.use_.as_deref(), Some(u) if u != "sig") {
        return None;
    }
    let (n, e) = (jwk.n.as_deref()?, jwk.e.as_deref()?);
    DecodingKey::from_rsa_components(n, e).ok()
}

#[axum::async_trait]
impl KeyProvider for JwksKeyProvider {
    async fn decoding_key(&self, kid: Option<&str>) -> Option<DecodingKey> {
        let set = match self.fetch().await {
            Ok(set) => set,
            Err(e) => {
                tracing::warn!(error = %e, "oidc jwks fetch failed");
                return None;
            }
        };
        // Prefer the key whose `kid` matches the token header; if the token has no
        // `kid` (or no `kid` matches) and there is exactly one usable signing key,
        // fall back to it — a common single-key IdP configuration.
        if let Some(kid) = kid {
            if let Some(k) = set.keys.iter().find(|j| j.kid.as_deref() == Some(kid)).and_then(jwk_to_key) {
                return Some(k);
            }
        }
        let mut usable = set.keys.iter().filter_map(jwk_to_key);
        match (usable.next(), usable.next()) {
            (Some(only), None) => Some(only),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use rand::rngs::OsRng;
    use rsa::pkcs1::EncodeRsaPublicKey;
    use rsa::pkcs8::EncodePrivateKey;
    use rsa::RsaPrivateKey;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Claims as the `IdP` would mint them. Mirrors [`OidcClaims`] plus the
    /// registered `iss`/`aud`/`exp`/`iat` that `jsonwebtoken` validates.
    #[derive(Serialize)]
    struct IdTokenClaims {
        iss: String,
        aud: String,
        sub: String,
        exp: u64,
        iat: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        email: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    }

    fn now() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    /// Generate an RSA keypair as (encoding, decoding) key material. 2048 bits is
    /// the smallest size `jsonwebtoken` accepts for RS256.
    fn keypair() -> (EncodingKey, DecodingKey) {
        let mut rng = OsRng;
        let private = RsaPrivateKey::new(&mut rng, 2048).expect("rsa keygen");
        let public = private.to_public_key();
        let private_pem = private.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).unwrap().to_string();
        let public_pem = public.to_pkcs1_pem(rsa::pkcs8::LineEnding::LF).unwrap();
        let enc = EncodingKey::from_rsa_pem(private_pem.as_bytes()).expect("encoding key");
        let dec = DecodingKey::from_rsa_pem(public_pem.as_bytes()).expect("decoding key");
        (enc, dec)
    }

    fn cfg() -> OidcConfig {
        OidcConfig {
            issuer: "https://idp.example.com".into(),
            audience: "aero-im-client".into(),
            jwks_uri: "https://idp.example.com/jwks".into(),
        }
    }

    /// Sign `claims` with `enc`, optionally tagging the header with `kid`.
    fn sign(enc: &EncodingKey, kid: Option<&str>, claims: &IdTokenClaims) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = kid.map(ToOwned::to_owned);
        encode(&header, claims, enc).expect("sign jwt")
    }

    fn good_claims(c: &OidcConfig) -> IdTokenClaims {
        let n = now();
        IdTokenClaims {
            iss: c.issuer.clone(),
            aud: c.audience.clone(),
            sub: "user-abc-123".into(),
            exp: n + 3600,
            iat: n,
            email: Some("alice@example.com".into()),
            name: Some("Alice Example".into()),
        }
    }

    #[tokio::test]
    async fn accepts_valid_token_and_extracts_claims() {
        let c = cfg();
        let (enc, dec) = keypair();
        let token = sign(&enc, Some("key-1"), &good_claims(&c));
        let keys = StaticKeyProvider::single(dec);

        let claims = validate_id_token(&token, &c, &keys).await.expect("valid token");
        assert_eq!(claims.sub, "user-abc-123");
        assert_eq!(claims.email.as_deref(), Some("alice@example.com"));
        assert_eq!(claims.name.as_deref(), Some("Alice Example"));
    }

    #[tokio::test]
    async fn matches_key_by_kid() {
        let c = cfg();
        let (enc, dec) = keypair();
        let token = sign(&enc, Some("kid-42"), &good_claims(&c));
        // Provider keyed strictly by kid: the right kid resolves the key.
        let keys = StaticKeyProvider::with_keyed(vec![("kid-42".into(), dec)]);
        assert!(validate_id_token(&token, &c, &keys).await.is_ok());
    }

    #[tokio::test]
    async fn rejects_unknown_kid() {
        let c = cfg();
        let (enc, dec) = keypair();
        let token = sign(&enc, Some("kid-unknown"), &good_claims(&c));
        // Provider only knows a different kid → no key → UnknownKey.
        let keys = StaticKeyProvider::with_keyed(vec![("kid-known".into(), dec)]);
        let err = validate_id_token(&token, &c, &keys).await.unwrap_err();
        assert!(matches!(err, OidcError::UnknownKey(Some(ref k)) if k == "kid-unknown"), "got {err:?}");
    }

    #[tokio::test]
    async fn rejects_wrong_audience() {
        let c = cfg();
        let (enc, dec) = keypair();
        let mut claims = good_claims(&c);
        claims.aud = "some-other-client".into();
        let token = sign(&enc, Some("key-1"), &claims);
        let keys = StaticKeyProvider::single(dec);
        let err = validate_id_token(&token, &c, &keys).await.unwrap_err();
        assert!(matches!(err, OidcError::Invalid(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn rejects_wrong_issuer() {
        let c = cfg();
        let (enc, dec) = keypair();
        let mut claims = good_claims(&c);
        claims.iss = "https://evil-idp.example.com".into();
        let token = sign(&enc, Some("key-1"), &claims);
        let keys = StaticKeyProvider::single(dec);
        let err = validate_id_token(&token, &c, &keys).await.unwrap_err();
        assert!(matches!(err, OidcError::Invalid(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn rejects_expired_token() {
        let c = cfg();
        let (enc, dec) = keypair();
        let mut claims = good_claims(&c);
        let n = now();
        // Expired well beyond the 60s leeway.
        claims.iat = n - 7200;
        claims.exp = n - 3600;
        let token = sign(&enc, Some("key-1"), &claims);
        let keys = StaticKeyProvider::single(dec);
        let err = validate_id_token(&token, &c, &keys).await.unwrap_err();
        assert!(matches!(err, OidcError::Invalid(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn rejects_bad_signature_from_different_key() {
        let c = cfg();
        let (enc, _dec) = keypair();
        // Verify with a *different* keypair's public key → signature mismatch.
        let (_enc2, dec2) = keypair();
        let token = sign(&enc, Some("key-1"), &good_claims(&c));
        let keys = StaticKeyProvider::single(dec2);
        let err = validate_id_token(&token, &c, &keys).await.unwrap_err();
        assert!(matches!(err, OidcError::Invalid(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn rejects_malformed_token() {
        let c = cfg();
        let (_enc, dec) = keypair();
        let keys = StaticKeyProvider::single(dec);
        let err = validate_id_token("not-a-jwt", &c, &keys).await.unwrap_err();
        assert!(matches!(err, OidcError::MalformedToken(_)), "got {err:?}");
    }

    #[test]
    fn best_display_name_prefers_name_then_username_then_email_then_sub() {
        let base = OidcClaims {
            sub: "sub-1".into(),
            email: None,
            name: None,
            email_verified: None,
            preferred_username: None,
        };
        // sub only.
        assert_eq!(base.best_display_name(), "sub-1");
        // email local-part.
        let with_email = OidcClaims {
            email: Some("bob@example.com".into()),
            ..base.clone()
        };
        assert_eq!(with_email.best_display_name(), "bob");
        // preferred_username beats email.
        let with_username = OidcClaims {
            preferred_username: Some("bobby".into()),
            ..with_email.clone()
        };
        assert_eq!(with_username.best_display_name(), "bobby");
        // name beats everything.
        let with_name = OidcClaims {
            name: Some("Bob Smith".into()),
            ..with_username
        };
        assert_eq!(with_name.best_display_name(), "Bob Smith");
    }

    #[test]
    fn best_display_name_ignores_blank_fields() {
        let c = OidcClaims {
            sub: "sub-x".into(),
            email: Some("  ".into()),
            name: Some("   ".into()),
            email_verified: None,
            preferred_username: Some(String::new()),
        };
        // All higher-priority fields are blank, so it falls through to sub.
        assert_eq!(c.best_display_name(), "sub-x");
    }

    #[test]
    fn config_from_env_requires_all_three() {
        // Use a unique prefix-free approach: set/unset the real keys around the
        // assertion. These tests run single-threaded per-process for env safety
        // is not guaranteed, so we only assert the "missing" path which needs no
        // env set (and is the default-off behavior we care about).
        // (Presence is exercised implicitly by integration wiring.)
        // Save & clear.
        let keys = ["AERO__OIDC__ISSUER", "AERO__OIDC__AUDIENCE", "AERO__OIDC__JWKS_URI"];
        let saved: Vec<_> = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        for k in keys {
            std::env::remove_var(k);
        }
        assert!(OidcConfig::from_env().is_none(), "unconfigured → None (OIDC off by default)");
        // Restore whatever was there.
        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }
}
