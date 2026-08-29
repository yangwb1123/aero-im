//! Verification for the dedicated Aero account-summary target assertion.
//!
//! This is deliberately separate from both the broad OAuth access-token
//! validator and the local Aero session JWT validator.  The assertion has its
//! own JOSE type, EdDSA-only algorithm policy, key source, issuer, audience,
//! subject, and scope.  Callers still have to bind the verified claims to the
//! HTTP request and consume the assertion's replay id before reading data.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use jsonwebtoken::{decode, decode_header, Algorithm, Validation};
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

use crate::oidc::KeyProvider;

/// JOSE `typ` for a target-bound account-summary assertion.
pub const ACCOUNT_TARGET_ASSERTION_TYPE: &str = "aero.account-target+jwt";
/// The only method covered by this assertion contract.
pub const ACCOUNT_TARGET_ASSERTION_METHOD: &str = "GET";
/// The only path covered by this assertion contract.
pub const ACCOUNT_TARGET_ASSERTION_PATH: &str = "/internal/account-summary";

const MAX_ASSERTION_BYTES: usize = 16 * 1024;
const MAX_COMPONENT_BYTES: usize = 512;
const MAX_REGION_BYTES: usize = 128;
const MAX_DATASETS: usize = 32;
const MAX_DATASET_BYTES: usize = 256;
const MAX_JTI_BYTES: usize = 256;
const REQUEST_HASH_BYTES: usize = 32;
const MAX_LIFETIME_SECS: u64 = 60;
const MAX_CLOCK_SKEW_SECS: u64 = 60;

/// Configuration for one dedicated target-assertion trust domain.
#[derive(Debug, Clone)]
pub struct TargetAssertionConfig {
    pub issuer: String,
    pub audience: String,
    pub subject: String,
    pub scope: String,
    pub max_lifetime_secs: u64,
    pub clock_skew_secs: u64,
}

impl TargetAssertionConfig {
    /// Validate the bounded policy before a verifier is made available.
    pub fn validate(&self) -> Result<(), TargetAssertionError> {
        if !valid_component(&self.issuer, MAX_COMPONENT_BYTES)
            || !valid_component(&self.audience, MAX_COMPONENT_BYTES)
            || !valid_component(&self.subject, MAX_COMPONENT_BYTES)
            || !valid_component(&self.scope, MAX_COMPONENT_BYTES)
            || self.max_lifetime_secs == 0
            || self.max_lifetime_secs > MAX_LIFETIME_SECS
            || self.clock_skew_secs > MAX_CLOCK_SKEW_SECS
        {
            return Err(TargetAssertionError::Invalid);
        }
        Ok(())
    }
}

/// The claims trusted after signature and registered-claim validation.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetAssertionClaims {
    /// Optional compatibility copy of the JOSE type.  When present it must be
    /// identical to the protected-header type; the signer emits only the
    /// protected-header form.
    #[serde(default)]
    pub typ: Option<String>,
    pub iss: String,
    pub aud: String,
    pub sub: String,
    pub client_id: String,
    pub account_id: String,
    pub canonical_uid: String,
    pub tenant_id: String,
    pub region: String,
    pub datasets: Vec<String>,
    pub method: String,
    pub path: String,
    pub request_hash: String,
    pub scope: String,
    pub jti: String,
    pub iat: u64,
    pub nbf: u64,
    pub exp: u64,
}

/// Safe, non-sensitive error classes for assertion verification.  None of the
/// variants carry a token, claim, key id, or configured endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TargetAssertionError {
    #[error("malformed target assertion")]
    Malformed,
    #[error("unknown target assertion key")]
    UnknownKey,
    #[error("target assertion key source unavailable")]
    KeySourceUnavailable,
    #[error("unsupported target assertion algorithm")]
    UnsupportedAlgorithm,
    #[error("invalid target assertion")]
    Invalid,
}

/// Validate a target assertion against the current wall clock.
pub async fn validate_target_assertion(
    token: &str,
    cfg: &TargetAssertionConfig,
    keys: &dyn KeyProvider,
) -> Result<TargetAssertionClaims, TargetAssertionError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| TargetAssertionError::Invalid)?
        .as_secs();
    validate_target_assertion_at(token, cfg, keys, now).await
}

/// Testable form of [`validate_target_assertion`] with an injected Unix clock.
pub async fn validate_target_assertion_at(
    token: &str,
    cfg: &TargetAssertionConfig,
    keys: &dyn KeyProvider,
    now: u64,
) -> Result<TargetAssertionClaims, TargetAssertionError> {
    cfg.validate()?;
    if token.is_empty()
        || token.len() > MAX_ASSERTION_BYTES
        || token.chars().any(char::is_whitespace)
    {
        return Err(TargetAssertionError::Malformed);
    }

    let header = decode_header(token).map_err(|_| TargetAssertionError::Malformed)?;
    if header.typ.as_deref() != Some(ACCOUNT_TARGET_ASSERTION_TYPE) {
        return Err(TargetAssertionError::Invalid);
    }
    if header.alg != Algorithm::EdDSA {
        return Err(TargetAssertionError::UnsupportedAlgorithm);
    }
    let kid = header
        .kid
        .as_deref()
        .filter(|value| valid_component(value, MAX_COMPONENT_BYTES))
        .ok_or(TargetAssertionError::Invalid)?;
    let key = keys
        .decoding_key_fallible(Some(kid), Algorithm::EdDSA)
        .await
        .map_err(|_| TargetAssertionError::KeySourceUnavailable)?
        .ok_or(TargetAssertionError::UnknownKey)?;

    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.set_issuer(&[cfg.issuer.as_str()]);
    validation.set_audience(&[cfg.audience.as_str()]);
    validation.validate_exp = true;
    validation.validate_nbf = true;
    validation.leeway = cfg.clock_skew_secs;
    // `iat` and `jti` are not registered claims understood by jsonwebtoken's
    // required-claim set.  They remain non-optional fields in the typed
    // payload, and are checked again below for their security policy.
    validation.set_required_spec_claims(&["iss", "aud", "exp", "nbf"]);
    let claims = decode::<TargetAssertionClaims>(token, &key, &validation)
        .map(|data| data.claims)
        .map_err(|_| TargetAssertionError::Invalid)?;

    if claims
        .typ
        .as_deref()
        .is_some_and(|typ| typ != ACCOUNT_TARGET_ASSERTION_TYPE)
        || claims.iss != cfg.issuer
        || claims.aud != cfg.audience
        || claims.sub != cfg.subject
        || claims.scope != cfg.scope
        || claims.method != ACCOUNT_TARGET_ASSERTION_METHOD
        || claims.path != ACCOUNT_TARGET_ASSERTION_PATH
        || !valid_component(&claims.iss, MAX_COMPONENT_BYTES)
        || !valid_component(&claims.aud, MAX_COMPONENT_BYTES)
        || !valid_component(&claims.sub, MAX_COMPONENT_BYTES)
        || !valid_component(&claims.client_id, MAX_COMPONENT_BYTES)
        || !valid_component(&claims.account_id, MAX_COMPONENT_BYTES)
        || !valid_component(&claims.canonical_uid, MAX_COMPONENT_BYTES)
        || !valid_component(&claims.tenant_id, MAX_COMPONENT_BYTES)
        || !valid_component(&claims.region, MAX_REGION_BYTES)
        || !valid_component(&claims.scope, MAX_COMPONENT_BYTES)
        || !valid_component(&claims.jti, MAX_JTI_BYTES)
        || claims.datasets.is_empty()
        || claims.datasets.len() > MAX_DATASETS
        || claims
            .datasets
            .iter()
            .any(|dataset| !valid_component(dataset, MAX_DATASET_BYTES))
        || !datasets_are_sorted_unique(&claims.datasets)
        || claims.exp <= now
        || claims.iat > now.saturating_add(cfg.clock_skew_secs)
        || claims.nbf > now.saturating_add(cfg.clock_skew_secs)
        || claims.nbf < claims.iat
        || claims.exp <= claims.iat
        || claims.exp - claims.iat > cfg.max_lifetime_secs
    {
        return Err(TargetAssertionError::Invalid);
    }

    let request_hash = URL_SAFE_NO_PAD
        .decode(claims.request_hash.as_bytes())
        .map_err(|_| TargetAssertionError::Invalid)?;
    if request_hash.len() != REQUEST_HASH_BYTES {
        return Err(TargetAssertionError::Invalid);
    }
    Ok(claims)
}

fn valid_component(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value == value.trim()
        && value
            .chars()
            .all(|character| !character.is_control() && !character.is_whitespace())
}

fn datasets_are_sorted_unique(values: &[String]) -> bool {
    values
        .windows(2)
        .all(|pair| pair[0].as_bytes() < pair[1].as_bytes())
}

#[cfg(test)]
mod tests;
