use super::*;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{encode, Algorithm, DecodingKey, EncodingKey, Header};
use serde::Serialize;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::StaticKeyProvider;

const PRIVATE_KEY_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\n\
MC4CAQAwBQYDK2VwBCIEIGrD/e7uKYqSY4twDEsRfMMuLSrODf14dpTiTK6K1YI0\n\
-----END PRIVATE KEY-----\n";
const PUBLIC_KEY_X: &str = "2-Jj2UvNCvQiUPNYRgSi0cJSPiJI6Rs6D0UTeEpQVj8";

#[derive(Clone, Serialize)]
struct WireClaims {
    iss: String,
    aud: String,
    sub: String,
    client_id: String,
    account_id: String,
    canonical_uid: String,
    tenant_id: String,
    region: String,
    datasets: Vec<String>,
    method: String,
    path: String,
    request_hash: String,
    scope: String,
    jti: String,
    iat: u64,
    nbf: u64,
    exp: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_secs()
}

fn config() -> TargetAssertionConfig {
    TargetAssertionConfig {
        issuer: "https://aero-id.example.test/assertions".into(),
        audience: "aero-im-account-summary".into(),
        subject: "aero-id-sync".into(),
        scope: "aero.account.summary.read".into(),
        max_lifetime_secs: 60,
        clock_skew_secs: 30,
    }
}

fn claims(cfg: &TargetAssertionConfig, now: u64) -> WireClaims {
    WireClaims {
        iss: cfg.issuer.clone(),
        aud: cfg.audience.clone(),
        sub: cfg.subject.clone(),
        client_id: "aero-id-im-source".into(),
        account_id: "opaque-account-1".into(),
        canonical_uid: "canonical-user-1".into(),
        tenant_id: "tenant-1".into(),
        region: "local".into(),
        datasets: vec!["aero-im.profile".into(), "aero-im.workspaces".into()],
        method: ACCOUNT_TARGET_ASSERTION_METHOD.into(),
        path: ACCOUNT_TARGET_ASSERTION_PATH.into(),
        request_hash: URL_SAFE_NO_PAD.encode([7_u8; 32]),
        scope: cfg.scope.clone(),
        jti: "jti-1".into(),
        iat: now,
        nbf: now,
        exp: now + 30,
    }
}

fn sign(wire: &WireClaims, algorithm: Algorithm, typ: Option<&str>, kid: Option<&str>) -> String {
    let mut header = Header::new(algorithm);
    header.typ = typ.map(ToOwned::to_owned);
    header.kid = kid.map(ToOwned::to_owned);
    encode(
        &header,
        wire,
        &EncodingKey::from_ed_pem(PRIVATE_KEY_PEM).expect("ed25519 private key"),
    )
    .expect("target assertion")
}

fn keys() -> StaticKeyProvider {
    StaticKeyProvider::with_keyed(vec![(
        "assertion-key".into(),
        DecodingKey::from_ed_components(PUBLIC_KEY_X).expect("ed25519 public key"),
    )])
}

#[tokio::test]
async fn accepts_valid_dedicated_eddsa_target_assertion() {
    let cfg = config();
    let now = now();
    let token = sign(
        &claims(&cfg, now),
        Algorithm::EdDSA,
        Some(ACCOUNT_TARGET_ASSERTION_TYPE),
        Some("assertion-key"),
    );
    let verified = validate_target_assertion_at(&token, &cfg, &keys(), now)
        .await
        .expect("valid target assertion");
    assert_eq!(verified.account_id, "opaque-account-1");
    assert_eq!(verified.client_id, "aero-id-im-source");
    assert_eq!(verified.datasets.len(), 2);
}

#[tokio::test]
async fn rejects_not_before_after_expiry_even_with_clock_skew() {
    let cfg = config();
    let now = now();
    let mut value = claims(&cfg, now);
    value.iat = now.saturating_sub(10);
    value.nbf = now + 25;
    value.exp = now + 15;
    let token = sign(
        &value,
        Algorithm::EdDSA,
        Some(ACCOUNT_TARGET_ASSERTION_TYPE),
        Some("assertion-key"),
    );
    assert_eq!(
        validate_target_assertion_at(&token, &cfg, &keys(), now)
            .await
            .unwrap_err(),
        TargetAssertionError::Invalid
    );
}

#[tokio::test]
async fn rejects_wrong_header_algorithm_type_and_kid() {
    let cfg = config();
    let now = now();
    let valid = sign(
        &claims(&cfg, now),
        Algorithm::EdDSA,
        Some(ACCOUNT_TARGET_ASSERTION_TYPE),
        Some("assertion-key"),
    );
    let (_header, payload, signature) = valid
        .split_once('.')
        .and_then(|(left, rest)| {
            rest.split_once('.')
                .map(|(payload, signature)| (left, payload, signature))
        })
        .expect("compact jwt");
    let wrong_algorithm_header = URL_SAFE_NO_PAD
        .encode(br#"{"alg":"HS256","kid":"assertion-key","typ":"aero.account-target+jwt"}"#);
    let wrong_algorithm = format!("{wrong_algorithm_header}.{payload}.{signature}");
    let cases = [
        wrong_algorithm,
        sign(
            &claims(&cfg, now),
            Algorithm::EdDSA,
            Some("JWT"),
            Some("assertion-key"),
        ),
        sign(
            &claims(&cfg, now),
            Algorithm::EdDSA,
            Some(ACCOUNT_TARGET_ASSERTION_TYPE),
            Some("other-key"),
        ),
        sign(
            &claims(&cfg, now),
            Algorithm::EdDSA,
            Some(ACCOUNT_TARGET_ASSERTION_TYPE),
            None,
        ),
    ];
    for token in cases {
        let result = validate_target_assertion_at(&token, &cfg, &keys(), now).await;
        assert!(result.is_err(), "accepted invalid target assertion header");
    }
}

#[tokio::test]
async fn rejects_expiry_clock_and_claim_mutations() {
    let cfg = config();
    let now = now();
    let cases = [
        "expired",
        "future iat",
        "future nbf",
        "long lifetime",
        "wrong method",
        "wrong path",
        "wrong scope",
        "duplicate dataset",
        "unsorted dataset",
        "bad hash",
    ];
    for name in cases {
        let mut value = claims(&cfg, now);
        match name {
            "expired" => value.exp = now,
            "future iat" => value.iat = now + 31,
            "future nbf" => value.nbf = now + 31,
            "long lifetime" => value.exp = value.iat + 61,
            "wrong method" => value.method = "POST".into(),
            "wrong path" => value.path = "/other".into(),
            "wrong scope" => value.scope = "account:summary:read".into(),
            "duplicate dataset" => value.datasets.push("aero-im.profile".into()),
            "unsorted dataset" => value.datasets.reverse(),
            "bad hash" => value.request_hash = "not-a-hash".into(),
            _ => unreachable!("listed mutation has a matching arm"),
        }
        let token = sign(
            &value,
            Algorithm::EdDSA,
            Some(ACCOUNT_TARGET_ASSERTION_TYPE),
            Some("assertion-key"),
        );
        assert!(
            validate_target_assertion_at(&token, &cfg, &keys(), now)
                .await
                .is_err(),
            "accepted {name} mutation"
        );
    }
}

#[tokio::test]
async fn key_source_failure_is_distinct_and_fail_closed() {
    struct FailingKeyProvider;

    #[axum::async_trait]
    impl KeyProvider for FailingKeyProvider {
        async fn decoding_key(
            &self,
            _kid: Option<&str>,
            _algorithm: Algorithm,
        ) -> Option<DecodingKey> {
            None
        }

        async fn decoding_key_fallible(
            &self,
            _kid: Option<&str>,
            _algorithm: Algorithm,
        ) -> Result<Option<DecodingKey>, String> {
            Err("unavailable".into())
        }
    }

    let cfg = config();
    let now = 1_800_000_000;
    let token = sign(
        &claims(&cfg, now),
        Algorithm::EdDSA,
        Some(ACCOUNT_TARGET_ASSERTION_TYPE),
        Some("assertion-key"),
    );
    assert_eq!(
        validate_target_assertion_at(&token, &cfg, &FailingKeyProvider, now)
            .await
            .unwrap_err(),
        TargetAssertionError::KeySourceUnavailable
    );
}

#[test]
fn target_policy_is_bounded_and_rejects_empty_configuration() {
    let mut cfg = config();
    assert!(cfg.validate().is_ok());
    cfg.max_lifetime_secs = 61;
    assert_eq!(cfg.validate().unwrap_err(), TargetAssertionError::Invalid);
    cfg = config();
    cfg.clock_skew_secs = 61;
    assert_eq!(cfg.validate().unwrap_err(), TargetAssertionError::Invalid);
    cfg = config();
    cfg.subject.clear();
    assert_eq!(cfg.validate().unwrap_err(), TargetAssertionError::Invalid);
}
