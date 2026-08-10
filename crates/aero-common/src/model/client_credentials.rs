//! RFC 9068 `client_credentials` claim contract — single source of truth.
//!
//! Twin consumers: aero-auth `oidc.rs` (full validator) and the audit
//! connector's claims plane. This file is the only legal definition site for
//! the types and the only legal spelling of the claim names —
//! `scripts/truth-check.sh` §3 hard-fails on drift (AC4 pattern, mirroring
//! `model/audit.rs`). `exp`/`nbf` are deliberately NOT here: they are outside
//! the unified four-claim contract (connector keeps its when-present Value
//! checks; aero-auth keeps its jsonwebtoken Validation).
//!
//! Contract framing (RFC 9068 §2.1/§3.1 + RFC 7519 §4.1.x): `iss` exact
//! match, `aud` string|array membership, `scope` containment, and `sub`
//! equal to `client_id` are RFC-9068-core. The `client_id` claim itself is a
//! **`Snaplink` extension claim** (minted by the `Snaplink` `IdP`, enforced via
//! `required_spec_claims` in aero-auth) — its RFC-9068 consistency comes
//! only from the `sub == client_id` rule. Requiring `iat`/`jti` presence in
//! aero-auth is likewise stricter than RFC 9068 (iat RECOMMENDED, jti
//! OPTIONAL) and is a closed-contract policy, not RFC authority. The
//! array-form `scope` tolerance is a beyond-RFC compatibility shape (RFC
//! 6749 §3.3 mandates a space-delimited string).

use serde_json::Value;
use std::collections::BTreeSet;

/// Claim-name constants — the only legal spelling. Consumers must reference
/// these; bare literals are a truth-check §3 violation outside this file.
pub const CLAIM_ISS: &str = "iss";
pub const CLAIM_AUD: &str = "aud";
pub const CLAIM_SCOPE: &str = "scope";
pub const CLAIM_SCOPES: &str = "scopes";
pub const CLAIM_SUB: &str = "sub";
pub const CLAIM_CLIENT_ID: &str = "client_id";
pub const CLAIM_IAT: &str = "iat";
pub const CLAIM_JTI: &str = "jti";
pub const TOKEN_TYPE_AT_JWT: &str = "at+jwt";
pub const TOKEN_TYPE_AT_JWT_APPLICATION: &str = "application/at+jwt";

/// Shared verification policy (moved verbatim from oidc.rs). Both consumers
/// build one of these from their own config surface; the connector's is
/// `required_scopes: vec![expected_scope]` (single-scope, equivalent to its
/// historical any-match check).
#[derive(Debug, Clone)]
pub struct ClientCredentialsTokenConfig {
    pub issuer: String,
    pub audience: String,
    pub required_scopes: Vec<String>,
}

/// Full claims type (moved from oidc.rs). `sub`/`client_id` are required
/// (RFC 9068 core + aero-auth current contract); `iat`/`jti` are
/// Option-ized so each consumer's strictness policy is preserved:
/// aero-auth enforces presence via jsonwebtoken `required_spec_claims`
/// (checked against the payload map, not the struct), the connector
/// tolerates absence as today. A present non-numeric `iat` (including
/// fractional `NumericDate`) or non-string `jti` now fails serde on both
/// sides (connector delta ③, fail-closed; aero-auth behavior unchanged —
/// its `u64`/`String` fields already failed those shapes at decode).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClientCredentialsClaims {
    pub sub: String,
    pub client_id: String,
    pub iat: Option<u64>,
    pub jti: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// Minimal typed gate for the audit connector's claims plane. Deliberately
/// excludes `scope`/`scopes`: the connector's scope check stays on the
/// Value path ([`check_scope`]) so array-form `scope` tokens keep passing
/// (the dual-shape tolerance is part of the contract — gating on a
/// `String`-typed `scope` would regress them, delta ④). `iat`/`jti` are
/// Option-typed: presence is not required on the connector face (aero-auth
/// enforces it via `required_spec_claims`), but a present non-u64 `iat` /
/// non-string `jti` fails serde — fail-closed (deltas ①–③).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ClientCredentialsGateClaims {
    pub sub: String,
    pub client_id: String,
    #[serde(default)]
    pub iat: Option<u64>,
    #[serde(default)]
    pub jti: Option<String>,
}

impl ClientCredentialsClaims {
    /// Normalized union of Snaplink's `scopes` array and the standard OAuth
    /// space-delimited `scope` compatibility claim (moved verbatim from
    /// oidc.rs). Sole implementation point.
    #[must_use]
    pub fn granted_scopes(&self) -> BTreeSet<&str> {
        self.scopes
            .iter()
            .map(String::as_str)
            .chain(self.scope.iter().flat_map(|scope| scope.split_whitespace()))
            .collect()
    }

    /// Machine identity: `sub == client_id` (moved from oidc.rs). RFC 9068
    /// §2.1: in `client_credentials` the subject **is** the client.
    #[must_use]
    pub fn subject_is_client_id(&self) -> bool {
        self.sub == self.client_id
    }

    /// Every configured required scope granted (moved from oidc.rs).
    #[must_use]
    pub fn has_required_scopes(&self, cfg: &ClientCredentialsTokenConfig) -> bool {
        let granted = self.granted_scopes();
        cfg.required_scopes
            .iter()
            .all(|required| granted.contains(required.as_str()))
    }
}

/// Identity component validity: non-empty / trimmed / no control chars
/// (moved from oidc.rs free fn `valid_identity_component`; kept a free fn).
#[must_use]
pub fn is_valid_identity_component(value: &str) -> bool {
    !value.is_empty() && value == value.trim() && !value.chars().any(char::is_control)
}

/// Value-shape claim checks — the connector consumption face. Input
/// `&serde_json::Value` preserves the two consumers' decode-shape
/// differences (connector reads the raw payload; aero-auth checks via
/// jsonwebtoken Validation + the typed face above). Semantics = verbatim
/// move of the connector's `validate_token_claims_at` blocks (iss exact /
/// aud string|array membership / scope string space-split|array members /
/// sub exact). Return `bool`; the connector keeps its "has no X claim" vs
/// "does not match" reason split via a presence pre-check (presentation,
/// not contract logic).
///
/// `iss` equals the configured issuer (exact, case-sensitive).
#[must_use]
pub fn check_issuer(claims: &Value, cfg: &ClientCredentialsTokenConfig) -> bool {
    claims
        .get(CLAIM_ISS)
        .and_then(Value::as_str)
        .is_some_and(|iss| iss == cfg.issuer.as_str())
}

/// `aud` contains the configured audience: string equality or array
/// membership (case-sensitive, RFC 7519 §4.1.3).
#[must_use]
pub fn check_audience(claims: &Value, cfg: &ClientCredentialsTokenConfig) -> bool {
    match claims.get(CLAIM_AUD) {
        Some(Value::String(value)) => value == &cfg.audience,
        Some(Value::Array(values)) => values
            .iter()
            .any(|value| value.as_str() == Some(cfg.audience.as_str())),
        _ => false,
    }
}

/// Every configured required scope is granted via the dual-shape read:
/// `scope` as a space-delimited string (RFC 6749 §3.3) or as an array of
/// members (beyond-RFC compatibility tolerance). For the connector's
/// single-required-scope config this is equivalent to its historical
/// any-match; for aero-auth the typed [`ClientCredentialsClaims::granted_scopes`]
/// union (array `scopes` ∪ string `scope`) is the corresponding face.
#[must_use]
pub fn check_scope(claims: &Value, cfg: &ClientCredentialsTokenConfig) -> bool {
    let granted: Vec<&str> = match claims.get(CLAIM_SCOPE) {
        Some(Value::String(value)) => value.split_whitespace().collect(),
        Some(Value::Array(values)) => values.iter().filter_map(Value::as_str).collect(),
        _ => return false,
    };
    cfg.required_scopes
        .iter()
        .all(|required| granted.iter().any(|word| word == required))
}

/// `sub` equals the expected subject (exact, case-sensitive).
#[must_use]
pub fn check_subject(claims: &Value, expected_sub: &str) -> bool {
    claims
        .get(CLAIM_SUB)
        .and_then(Value::as_str)
        .is_some_and(|sub| sub == expected_sub)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Vocabulary canonical-value pins (mirror audit.rs
    /// `vocabulary_consts_are_pinned` — the leaf is the single definition
    /// point; truth-check §3 scans the same spellings).
    #[test]
    fn vocabulary_consts_are_pinned() {
        assert_eq!(CLAIM_ISS, "iss");
        assert_eq!(CLAIM_AUD, "aud");
        assert_eq!(CLAIM_SCOPE, "scope");
        assert_eq!(CLAIM_SCOPES, "scopes");
        assert_eq!(CLAIM_SUB, "sub");
        assert_eq!(CLAIM_CLIENT_ID, "client_id");
        assert_eq!(CLAIM_IAT, "iat");
        assert_eq!(CLAIM_JTI, "jti");
        assert_eq!(TOKEN_TYPE_AT_JWT, "at+jwt");
        assert_eq!(TOKEN_TYPE_AT_JWT_APPLICATION, "application/at+jwt");
    }

    fn cfg() -> ClientCredentialsTokenConfig {
        ClientCredentialsTokenConfig {
            issuer: "https://idp.example.test".into(),
            audience: "audit-governance".into(),
            required_scopes: vec!["audit:event:write".into()],
        }
    }

    #[test]
    fn granted_scopes_unions_array_and_space_delimited_string() {
        let claims = ClientCredentialsClaims {
            sub: "aero-im.source".into(),
            client_id: "aero-im.source".into(),
            iat: None,
            jti: None,
            scopes: vec!["aero.notify.publish".into(), "audit:event:write".into()],
            scope: Some("audit:event:write metering:read".into()),
        };
        let granted = claims.granted_scopes();
        assert!(granted.contains("aero.notify.publish"));
        assert!(granted.contains("audit:event:write"));
        assert!(granted.contains("metering:read"));
        assert_eq!(granted.len(), 3, "no duplicate across the union faces");
    }

    #[test]
    fn subject_is_client_id_matches_machine_identity_rule() {
        assert!(ClientCredentialsClaims {
            sub: "erp-production".into(),
            client_id: "erp-production".into(),
            iat: None,
            jti: None,
            scopes: Vec::new(),
            scope: None,
        }
        .subject_is_client_id());
        assert!(!ClientCredentialsClaims {
            sub: "erp-production".into(),
            client_id: "someone-else".into(),
            iat: None,
            jti: None,
            scopes: Vec::new(),
            scope: None,
        }
        .subject_is_client_id());
    }

    #[test]
    fn has_required_scopes_requires_every_configured_scope() {
        let claims = ClientCredentialsClaims {
            sub: "aero-im.source".into(),
            client_id: "aero-im.source".into(),
            iat: None,
            jti: None,
            scopes: vec!["audit:event:write".into()],
            scope: None,
        };
        assert!(claims.has_required_scopes(&cfg()));
        let mut multi = cfg();
        multi.required_scopes = vec!["audit:event:write".into(), "metering:read".into()];
        assert!(
            !claims.has_required_scopes(&multi),
            "missing one scope must fail"
        );
        let mut both = cfg();
        both.required_scopes = vec!["audit:event:write".into(), "metering:read".into()];
        let with_scope = ClientCredentialsClaims {
            sub: "aero-im.source".into(),
            client_id: "aero-im.source".into(),
            iat: None,
            jti: None,
            scopes: Vec::new(),
            scope: Some("audit:event:write metering:read".into()),
        };
        assert!(with_scope.has_required_scopes(&both));
    }

    #[test]
    fn is_valid_identity_component_rejects_empty_whitespace_and_control() {
        assert!(is_valid_identity_component("aero-im.source"));
        assert!(!is_valid_identity_component(""));
        assert!(!is_valid_identity_component("  padded  "));
        assert!(!is_valid_identity_component("aero\nim"));
    }

    /// The Value-path scope check keeps the dual-shape tolerance — the
    /// property the connector gate must not regress (delta ④).
    #[test]
    fn check_scope_accepts_string_and_array_forms() {
        assert!(check_scope(
            &json!({"scope": "audit:event:write metering:read"}),
            &cfg()
        ));
        assert!(check_scope(
            &json!({"scope": ["audit:event:write", "metering:read"]}),
            &cfg()
        ));
        assert!(!check_scope(
            &json!({"scope": "billing:entitlement:read"}),
            &cfg()
        ));
        assert!(!check_scope(&json!({}), &cfg()));
        assert!(!check_scope(&json!({"scope": 42}), &cfg()));
    }

    #[test]
    fn check_issuer_audience_subject_match_exactly() {
        assert!(check_issuer(
            &json!({"iss": "https://idp.example.test"}),
            &cfg()
        ));
        assert!(!check_issuer(
            &json!({"iss": "https://evil.example.test"}),
            &cfg()
        ));
        assert!(!check_issuer(&json!({}), &cfg()));
        assert!(check_audience(&json!({"aud": "audit-governance"}), &cfg()));
        assert!(check_audience(
            &json!({"aud": ["billing-api", "audit-governance"]}),
            &cfg()
        ));
        assert!(!check_audience(&json!({"aud": ["billing-api"]}), &cfg()));
        assert!(!check_audience(&json!({}), &cfg()));
        assert!(check_subject(
            &json!({"sub": "aero-im.source"}),
            "aero-im.source"
        ));
        assert!(!check_subject(
            &json!({"sub": "someone-else"}),
            "aero-im.source"
        ));
        assert!(!check_subject(&json!({}), "aero-im.source"));
    }

    /// Gate negatives: missing `sub`/`client_id` fail serde (deltas ①②);
    /// fractional `iat` (RFC 7519 `NumericDate` allows fractional seconds)
    /// fails closed (delta ③ + FM1 stall shape); a numeric `jti` fails
    /// closed; absent `iat`/`jti` are tolerated (Option face).
    #[test]
    fn gate_rejects_missing_or_mistyped_identity_claims() {
        assert!(
            serde_json::from_value::<ClientCredentialsGateClaims>(json!({
                "client_id": "aero-im.source",
            }))
            .is_err(),
            "missing sub must fail the gate"
        );
        assert!(
            serde_json::from_value::<ClientCredentialsGateClaims>(json!({
                "sub": "aero-im.source",
            }))
            .is_err(),
            "missing client_id must fail the gate"
        );
        assert!(
            serde_json::from_value::<ClientCredentialsGateClaims>(json!({
                "sub": "aero-im.source",
                "client_id": "aero-im.source",
                "iat": 1_234_567_890.5,
            }))
            .is_err(),
            "fractional iat must fail the gate"
        );
        assert!(
            serde_json::from_value::<ClientCredentialsGateClaims>(json!({
                "sub": "aero-im.source",
                "client_id": "aero-im.source",
                "jti": 42,
            }))
            .is_err(),
            "numeric jti must fail the gate"
        );
        let ok = serde_json::from_value::<ClientCredentialsGateClaims>(json!({
            "sub": "aero-im.source",
            "client_id": "aero-im.source",
        }))
        .expect("absent iat/jti are tolerated on the gate face");
        assert_eq!(ok.iat, None);
        assert_eq!(ok.jti, None);
    }

    /// The aero-auth face keeps its strict decode shape: array-form `scope`
    /// fails the full struct (jsonwebtoken `scope: Option<String>`), while
    /// the string form decodes — the tolerance widening is connector-only
    /// (Value path), never aero-auth.
    #[test]
    fn full_claims_keep_string_scope_shape() {
        let mut claims = json!({
            "sub": "aero-im.source",
            "client_id": "aero-im.source",
            "iat": 1_234_567_890,
            "jti": "token-1",
            "scope": "audit:event:write metering:read",
        });
        let decoded: ClientCredentialsClaims =
            serde_json::from_value(claims.clone()).expect("string scope decodes");
        assert_eq!(decoded.iat, Some(1_234_567_890));
        assert_eq!(decoded.jti.as_deref(), Some("token-1"));
        assert_eq!(decoded.granted_scopes().len(), 2);
        claims["scope"] = json!(["audit:event:write"]);
        assert!(
            serde_json::from_value::<ClientCredentialsClaims>(claims).is_err(),
            "array-form scope must keep failing the full (aero-auth) struct"
        );
    }
}
