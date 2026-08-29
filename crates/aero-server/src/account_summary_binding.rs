//! Dedicated target binding for the Aero ID account-summary connector.
//!
//! The broad OAuth client-credentials token authenticates the machine caller;
//! this module verifies the separate Aero ID-signed assertion that authorizes
//! one exact account-summary request.  The verifier owns a separate JWKS
//! provider and a cluster-wide Redis one-time-use guard.  No assertion
//! configuration means no target binding and therefore no projection access.

use std::{
    collections::BTreeSet,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use aero_auth::{
    validate_jwks_uri, validate_target_assertion, JwksKeyProvider, KeyProvider,
    TargetAssertionClaims, TargetAssertionConfig, TargetAssertionError,
    ACCOUNT_TARGET_ASSERTION_METHOD, ACCOUNT_TARGET_ASSERTION_PATH,
};
use aero_common::Error as AeroError;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use fred::{
    prelude::{KeysInterface, RedisClient},
    types::{Expiration, SetOptions},
};
use sha2::{Digest as _, Sha256};

pub(crate) const ACCOUNT_SUMMARY_SCOPE: &str = "aero.account.summary.read";
pub(crate) const ACCOUNT_SUMMARY_AUDIENCE: &str = "aero-im-account-summary";
pub(crate) const ACCOUNT_SUMMARY_SUBJECT: &str = "aero-id-sync";
pub(crate) const ACCOUNT_SUMMARY_PATH: &str = "/internal/account-summary";
pub(crate) const ACCOUNT_SUMMARY_TARGET_ASSERTION_HEADER: &str = "x-aero-target-assertion";
const ACCOUNT_SUMMARY_CANONICAL_VERSION: &str = "aero-account-summary-v1";
const ACCOUNT_SUMMARY_ASSERTION_PREFIX: &str = "aero:account-summary:assertion:jti:";
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_LIFETIME_SECS: u64 = 60;
const MAX_CLOCK_SKEW_SECS: u64 = 60;
const MAX_REPLAY_TTL_SECS: u64 = MAX_LIFETIME_SECS + (MAX_CLOCK_SKEW_SECS * 2);
const DEFAULT_LIFETIME_SECS: u64 = 60;
const DEFAULT_CLOCK_SKEW_SECS: u64 = 30;

/// The result of cryptographic and request binding checks, before one-time
/// replay consumption.  The caller must consume it before any data lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedAccountSummaryTarget {
    pub(crate) account_id: String,
    pub(crate) canonical_uid: String,
    pub(crate) tenant_id: String,
    pub(crate) region: String,
    pub(crate) datasets: BTreeSet<String>,
    pub(crate) issuer: String,
    pub(crate) jti: String,
    pub(crate) exp: u64,
}

/// One-time replay store.  The trait keeps the security gate testable without
/// weakening the production Redis requirement.
#[axum::async_trait]
pub(crate) trait ReplayStore: Send + Sync {
    async fn claim(&self, key: String, ttl: Duration) -> Result<bool, String>;
}

#[derive(Clone)]
struct RedisReplayStore {
    client: RedisClient,
}

#[axum::async_trait]
impl ReplayStore for RedisReplayStore {
    async fn claim(&self, key: String, ttl: Duration) -> Result<bool, String> {
        let seconds = i64::try_from(ttl.as_secs().clamp(1, i64::MAX as u64)).unwrap_or(i64::MAX);
        let result: Option<String> = self
            .client
            .set(
                key,
                "used",
                Some(Expiration::EX(seconds)),
                Some(SetOptions::NX),
                false,
            )
            .await
            .map_err(|_| "replay store unavailable".to_owned())?;
        Ok(result.is_some())
    }
}

/// JWKS-backed verifier for one account-summary trust domain.
#[derive(Clone)]
pub struct AccountSummaryTargetVerifier {
    config: TargetAssertionConfig,
    keys: Arc<dyn KeyProvider>,
    replay: Arc<dyn ReplayStore>,
}

impl AccountSummaryTargetVerifier {
    /// Build from the dedicated configuration.  The general OIDC/integration
    /// issuer and JWKS settings are intentionally not consulted here.
    pub fn from_env(redis: RedisClient) -> Result<Option<Self>, AeroError> {
        let Some(config) = target_assertion_config_from_env()? else {
            return Ok(None);
        };
        let jwks_uri = target_assertion_jwks_uri_from_env().ok_or_else(|| {
            AeroError::Invalid("account summary target assertion is incomplete".into())
        })?;
        validate_jwks_uri(&jwks_uri).map_err(|_| {
            AeroError::Invalid("account summary target assertion JWKS URI is not allowed".into())
        })?;
        if dedicated_jwks_reuses_general_uri(&jwks_uri)
            || dedicated_assertion_reuses_general_issuer(&config.issuer)
        {
            return Err(AeroError::Invalid(
                "account summary target assertion requires a dedicated trust domain".into(),
            ));
        }
        let keys = Arc::new(JwksKeyProvider::new(jwks_uri));
        Ok(Some(Self {
            config,
            keys,
            replay: Arc::new(RedisReplayStore { client: redis }),
        }))
    }

    #[cfg(test)]
    pub(crate) fn with_dependencies(
        config: TargetAssertionConfig,
        keys: Arc<dyn KeyProvider>,
        replay: Arc<dyn ReplayStore>,
    ) -> Result<Self, TargetAssertionError> {
        config.validate()?;
        Ok(Self {
            config,
            keys,
            replay,
        })
    }

    /// Verify the assertion and bind it to the exact request target.  This does
    /// not consume `jti`; callers use [`Self::consume_replay`] only after all
    /// unsigned consistency headers have also passed.
    pub(crate) async fn verify_request(
        &self,
        client_id: &str,
        account_id: &str,
        region: Option<&str>,
        datasets: &BTreeSet<String>,
        headers: &axum::http::HeaderMap,
    ) -> Result<VerifiedAccountSummaryTarget, AeroError> {
        let assertion = target_assertion_header(headers)?;
        let claims = validate_target_assertion(assertion, &self.config, self.keys.as_ref())
            .await
            .map_err(map_assertion_error)?;
        Self::bind_claims(&claims, client_id, account_id, region, datasets)
    }

    /// Consume an accepted assertion exactly once in the cluster-wide store.
    pub(crate) async fn consume_replay(
        &self,
        target: &VerifiedAccountSummaryTarget,
    ) -> Result<(), AeroError> {
        let now = unix_now();
        if now >= target.exp {
            return Err(AeroError::Unauthorized(
                "account summary target assertion expired".into(),
            ));
        }
        let ttl_secs = target
            .exp
            .saturating_add(self.config.clock_skew_secs)
            .saturating_sub(now)
            .clamp(1, MAX_REPLAY_TTL_SECS);
        let key = replay_key(&target.issuer, &target.jti);
        let claimed = self
            .replay
            .claim(key, Duration::from_secs(ttl_secs))
            .await
            .map_err(|_| binding_unavailable())?;
        if claimed {
            Ok(())
        } else {
            Err(AeroError::Unauthorized(
                "account summary target assertion replayed".into(),
            ))
        }
    }

    fn bind_claims(
        claims: &TargetAssertionClaims,
        client_id: &str,
        account_id: &str,
        region: Option<&str>,
        datasets: &BTreeSet<String>,
    ) -> Result<VerifiedAccountSummaryTarget, AeroError> {
        let Some(region) = region else {
            return Err(binding_mismatch());
        };
        let claim_datasets = claims.datasets.iter().cloned().collect::<BTreeSet<_>>();
        if claims.client_id != client_id
            || claims.account_id != account_id
            || claims.region != region
            || claim_datasets != *datasets
        {
            return Err(binding_mismatch());
        }
        let expected_hash = canonical_request_hash(
            ACCOUNT_TARGET_ASSERTION_METHOD,
            ACCOUNT_TARGET_ASSERTION_PATH,
            account_id,
            region,
            datasets,
        )
        .map_err(|_| binding_mismatch())?;
        let signed_hash = URL_SAFE_NO_PAD
            .decode(claims.request_hash.as_bytes())
            .map_err(|_| binding_mismatch())?;
        if signed_hash.as_slice() != expected_hash.as_slice() {
            return Err(binding_mismatch());
        }
        Ok(VerifiedAccountSummaryTarget {
            account_id: claims.account_id.clone(),
            canonical_uid: claims.canonical_uid.clone(),
            tenant_id: claims.tenant_id.clone(),
            region: claims.region.clone(),
            datasets: claims.datasets.iter().cloned().collect(),
            issuer: claims.iss.clone(),
            jti: claims.jti.clone(),
            exp: claims.exp,
        })
    }
}

fn target_assertion_header(headers: &axum::http::HeaderMap) -> Result<&str, AeroError> {
    let mut values = headers
        .get_all(ACCOUNT_SUMMARY_TARGET_ASSERTION_HEADER)
        .iter();
    let value = values.next().ok_or_else(|| {
        AeroError::Unauthorized("account summary target assertion required".into())
    })?;
    if values.next().is_some() {
        return Err(AeroError::Unauthorized(
            "exactly one account summary target assertion is required".into(),
        ));
    }
    let value = value.to_str().map_err(|_| {
        AeroError::Unauthorized("account summary target assertion is invalid".into())
    })?;
    if value.is_empty() || value.len() > MAX_HEADER_BYTES || value.chars().any(char::is_whitespace)
    {
        return Err(AeroError::Unauthorized(
            "account summary target assertion is invalid".into(),
        ));
    }
    Ok(value)
}

fn canonical_request_hash(
    method: &str,
    path: &str,
    account_id: &str,
    region: &str,
    datasets: &BTreeSet<String>,
) -> Result<[u8; 32], AeroError> {
    let bytes = canonical_request_bytes(method, path, account_id, region, datasets)?;
    Ok(Sha256::digest(bytes).into())
}

/// Canonical bytes shared with the Aero ID Go connector.  The version marker is
/// the first length-delimited field, followed by method, path, account ID,
/// region, and the sorted dataset set.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn canonical_request_bytes(
    method: &str,
    path: &str,
    account_id: &str,
    region: &str,
    datasets: &BTreeSet<String>,
) -> Result<Vec<u8>, AeroError> {
    let mut fields = vec![
        ACCOUNT_SUMMARY_CANONICAL_VERSION,
        method,
        path,
        account_id,
        region,
    ];
    let mut sorted = datasets.iter().map(String::as_str).collect::<Vec<_>>();
    sorted.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    fields.extend(sorted);

    let mut encoded = Vec::new();
    for field in fields {
        let length = u32::try_from(field.len())
            .map_err(|_| AeroError::Invalid("account summary request is too large".into()))?;
        encoded.extend_from_slice(&length.to_be_bytes());
        encoded.extend_from_slice(field.as_bytes());
    }
    Ok(encoded)
}

fn target_assertion_config_from_env() -> Result<Option<TargetAssertionConfig>, AeroError> {
    let issuer = env_value("AERO__ACCOUNT_SUMMARY__TARGET_ASSERTION_ISSUER");
    let audience = env_value("AERO__ACCOUNT_SUMMARY__TARGET_ASSERTION_AUDIENCE");
    let subject = env_value("AERO__ACCOUNT_SUMMARY__TARGET_ASSERTION_SUBJECT");
    let jwks = env_value("AERO__ACCOUNT_SUMMARY__TARGET_ASSERTION_JWKS_URI");
    let configured = [
        "AERO__ACCOUNT_SUMMARY__TARGET_ASSERTION_ISSUER",
        "AERO__ACCOUNT_SUMMARY__TARGET_ASSERTION_AUDIENCE",
        "AERO__ACCOUNT_SUMMARY__TARGET_ASSERTION_SUBJECT",
        "AERO__ACCOUNT_SUMMARY__TARGET_ASSERTION_JWKS_URI",
        "AERO__ACCOUNT_SUMMARY__TARGET_ASSERTION_MAX_LIFETIME_SECS",
        "AERO__ACCOUNT_SUMMARY__TARGET_ASSERTION_CLOCK_SKEW_SECS",
    ]
    .iter()
    .any(|key| std::env::var(key).is_ok());
    if !configured {
        return Ok(None);
    }
    let (Some(issuer), Some(audience), Some(subject), Some(_jwks)) =
        (issuer, audience, subject, jwks)
    else {
        return Err(AeroError::Invalid(
            "account summary target assertion is incompletely configured".into(),
        ));
    };
    let max_lifetime_secs = bounded_seconds(
        "AERO__ACCOUNT_SUMMARY__TARGET_ASSERTION_MAX_LIFETIME_SECS",
        DEFAULT_LIFETIME_SECS,
        MAX_LIFETIME_SECS,
    )?;
    let clock_skew_secs = bounded_seconds(
        "AERO__ACCOUNT_SUMMARY__TARGET_ASSERTION_CLOCK_SKEW_SECS",
        DEFAULT_CLOCK_SKEW_SECS,
        MAX_CLOCK_SKEW_SECS,
    )?;
    if audience != ACCOUNT_SUMMARY_AUDIENCE || subject != ACCOUNT_SUMMARY_SUBJECT {
        return Err(AeroError::Invalid(
            "account summary target assertion audience or subject is invalid".into(),
        ));
    }
    let config = TargetAssertionConfig {
        issuer,
        audience,
        subject,
        scope: ACCOUNT_SUMMARY_SCOPE.into(),
        max_lifetime_secs,
        clock_skew_secs,
    };
    config.validate().map_err(|_| {
        AeroError::Invalid("account summary target assertion configuration is invalid".into())
    })?;
    Ok(Some(config))
}

fn target_assertion_jwks_uri_from_env() -> Option<String> {
    env_value("AERO__ACCOUNT_SUMMARY__TARGET_ASSERTION_JWKS_URI")
}

fn dedicated_jwks_reuses_general_uri(uri: &str) -> bool {
    ["AERO__INTEGRATIONS__JWKS_URI", "AERO__OIDC__JWKS_URI"]
        .iter()
        .filter_map(|key| env_value(key))
        .any(|configured| configured == uri)
}

fn dedicated_assertion_reuses_general_issuer(issuer: &str) -> bool {
    ["AERO__INTEGRATIONS__ISSUER", "AERO__OIDC__ISSUER"]
        .iter()
        .filter_map(|key| env_value(key))
        .any(|configured| configured == issuer)
}

fn replay_key(issuer: &str, jti: &str) -> String {
    let digest = Sha256::digest([issuer.as_bytes(), b"\0", jti.as_bytes()].concat());
    format!(
        "{ACCOUNT_SUMMARY_ASSERTION_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(digest)
    )
}

fn bounded_seconds(key: &str, default: u64, maximum: u64) -> Result<u64, AeroError> {
    let Some(value) = std::env::var(key).ok() else {
        return Ok(default);
    };
    let value = value.trim().parse::<u64>().map_err(|_| {
        AeroError::Invalid("account summary target assertion timing is invalid".into())
    })?;
    if value == 0 || value > maximum {
        return Err(AeroError::Invalid(
            "account summary target assertion timing is out of bounds".into(),
        ));
    }
    Ok(value)
}

fn env_value(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn map_assertion_error(error: TargetAssertionError) -> AeroError {
    match error {
        TargetAssertionError::KeySourceUnavailable => binding_unavailable(),
        TargetAssertionError::Malformed
        | TargetAssertionError::UnknownKey
        | TargetAssertionError::UnsupportedAlgorithm
        | TargetAssertionError::Invalid => {
            AeroError::Unauthorized("invalid account summary target assertion".into())
        }
    }
}

fn binding_unavailable() -> AeroError {
    AeroError::Upstream("account summary target binding is unavailable".into())
}

fn binding_mismatch() -> AeroError {
    AeroError::Forbidden("account summary target binding mismatch".into())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests;
