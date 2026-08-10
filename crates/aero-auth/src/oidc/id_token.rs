use aero_common::model::client_credentials::is_valid_identity_component;
use serde::Deserialize;

use super::{OidcClaims, OidcConfig, OidcError};

/// Signed claims used only while validating an ID token.
///
/// `aud` and `azp` deliberately stay out of the public identity projection:
/// callers need the verified human profile, while this boundary must retain the
/// original audience cardinality to enforce OIDC Core's authorized-party rule.
#[derive(Deserialize)]
pub(super) struct SignedOidcClaims {
    sub: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email_verified: Option<bool>,
    #[serde(default)]
    preferred_username: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    iat: Option<u64>,
    aud: TokenAudience,
    #[serde(default)]
    azp: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TokenAudience {
    Single(String),
    Multiple(Vec<String>),
}

impl TokenAudience {
    fn has_multiple_values(&self) -> bool {
        match self {
            Self::Single(_) => false,
            Self::Multiple(values) => values.len() > 1,
        }
    }

    fn contains(&self, expected: &str) -> bool {
        match self {
            Self::Single(value) => value == expected,
            Self::Multiple(values) => values.iter().any(|value| value == expected),
        }
    }
}

impl SignedOidcClaims {
    pub(super) fn into_verified(self, cfg: &OidcConfig) -> Result<OidcClaims, OidcError> {
        if !is_valid_identity_component(&self.sub) {
            return Err(OidcError::Invalid("subject claim is invalid".into()));
        }
        // `Validation::set_audience` already rejects a token that does not
        // contain our client id. Retain this direct check as a local invariant
        // and use the original cardinality for OIDC Core's `azp` requirement.
        if !self.aud.contains(&cfg.audience)
            || self
                .azp
                .as_deref()
                .is_some_and(|authorized_party| authorized_party != cfg.audience)
            || (self.aud.has_multiple_values()
                && self.azp.as_deref() != Some(cfg.audience.as_str()))
        {
            return Err(OidcError::Invalid(
                "token authorized party is invalid".into(),
            ));
        }
        Ok(OidcClaims {
            sub: self.sub,
            email: self.email,
            name: self.name,
            email_verified: self.email_verified,
            preferred_username: self.preferred_username,
            nonce: self.nonce,
            iat: self.iat,
        })
    }
}
