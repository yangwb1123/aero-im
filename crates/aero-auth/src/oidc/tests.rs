use super::*;
use jsonwebtoken::{encode, EncodingKey, Header};
use rand::rngs::OsRng;
use rsa::pkcs1::EncodeRsaPublicKey;
use rsa::pkcs8::EncodePrivateKey;
use rsa::RsaPrivateKey;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Barrier;

/// Claims as the `IdP` would mint them. Mirrors [`OidcClaims`] plus the
/// registered `iss`/`aud`/`exp`/`iat` that `jsonwebtoken` validates.
#[derive(Serialize)]
struct IdTokenClaims {
    iss: String,
    aud: serde_json::Value,
    sub: String,
    exp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    nbf: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    iat: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    azp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nonce: Option<String>,
}

/// Claims emitted by Snaplink for a client-credentials access token. The
/// optional `nbf` lets tests verify that it is required, not merely checked
/// when a provider happens to include it.
#[derive(Clone, Serialize)]
struct ClientAccessTokenClaims {
    iss: String,
    aud: String,
    sub: String,
    client_id: String,
    exp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    nbf: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    iat: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    scopes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    jti: Option<String>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Generate an RSA keypair as (encoding, decoding) key material. 2048 bits is
/// the smallest size `jsonwebtoken` accepts for RS256.
fn keypair() -> (EncodingKey, DecodingKey) {
    let mut rng = OsRng;
    let private = RsaPrivateKey::new(&mut rng, 2048).expect("rsa keygen");
    let public = private.to_public_key();
    let private_pem = private
        .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
        .unwrap()
        .to_string();
    let public_pem = public.to_pkcs1_pem(rsa::pkcs8::LineEnding::LF).unwrap();
    let enc = EncodingKey::from_rsa_pem(private_pem.as_bytes()).expect("encoding key");
    let dec = DecodingKey::from_rsa_pem(public_pem.as_bytes()).expect("decoding key");
    (enc, dec)
}

const ED25519_PRIVATE_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\n\
MC4CAQAwBQYDK2VwBCIEIGrD/e7uKYqSY4twDEsRfMMuLSrODf14dpTiTK6K1YI0\n\
-----END PRIVATE KEY-----\n";
const ED25519_PUBLIC_X: &str = "2-Jj2UvNCvQiUPNYRgSi0cJSPiJI6Rs6D0UTeEpQVj8";

fn ed25519_keypair() -> (EncodingKey, DecodingKey) {
    let enc = EncodingKey::from_ed_pem(ED25519_PRIVATE_PEM).expect("ed25519 encoding key");
    let dec = DecodingKey::from_ed_components(ED25519_PUBLIC_X).expect("ed25519 decoding key");
    (enc, dec)
}

#[test]
fn unknown_kid_refresh_gate_is_immediate_then_throttled() {
    let start = Instant::now();
    let mut state = JwksCacheState::default();

    assert!(state.reserve_unknown_kid_refresh(start));
    let before_boundary = (start + UNKNOWN_KID_REFRESH_MIN_INTERVAL)
        .checked_sub(Duration::from_millis(1))
        .expect("test instant supports a one-millisecond subtraction");
    assert!(!state.reserve_unknown_kid_refresh(before_boundary));
    assert!(state.reserve_unknown_kid_refresh(start + UNKNOWN_KID_REFRESH_MIN_INTERVAL));
}

#[tokio::test]
async fn concurrent_unknown_kids_reserve_only_one_refresh() {
    const CALLERS: usize = 64;
    let state = Arc::new(Mutex::new(JwksCacheState::default()));
    let barrier = Arc::new(Barrier::new(CALLERS));
    let instant = Instant::now();
    let mut tasks = Vec::with_capacity(CALLERS);

    for _ in 0..CALLERS {
        let state = Arc::clone(&state);
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            state.lock().await.reserve_unknown_kid_refresh(instant)
        }));
    }

    let mut refreshes = 0;
    for task in tasks {
        refreshes += usize::from(task.await.expect("refresh reservation task"));
    }
    assert_eq!(refreshes, 1);
}

#[test]
fn unknown_kid_rotation_set_can_resolve_new_key_after_refresh() {
    let before = JwkSet {
        keys: vec![serde_json::from_value(serde_json::json!({
            "kty": "OKP",
            "use": "sig",
            "crv": "Ed25519",
            "x": ED25519_PUBLIC_X,
            "kid": "old-key",
            "alg": "EdDSA"
        }))
        .unwrap()],
    };
    assert!(key_from_set(&before, Some("new-key"), Algorithm::EdDSA).is_none());

    let after = JwkSet {
        keys: vec![serde_json::from_value(serde_json::json!({
            "kty": "OKP",
            "use": "sig",
            "crv": "Ed25519",
            "x": ED25519_PUBLIC_X,
            "kid": "new-key",
            "alg": "EdDSA"
        }))
        .unwrap()],
    };
    assert!(key_from_set(&after, Some("new-key"), Algorithm::EdDSA).is_some());
}

fn cfg() -> OidcConfig {
    OidcConfig {
        issuer: "https://idp.example.com".into(),
        audience: "aero-im-client".into(),
        jwks_uri: "https://idp.example.com/jwks".into(),
    }
}

fn client_credentials_cfg() -> ClientCredentialsTokenConfig {
    ClientCredentialsTokenConfig {
        issuer: "https://sso.example.com".into(),
        audience: "aero-im-integrations".into(),
        required_scopes: vec!["aero.notify.publish".into()],
    }
}

/// Sign `claims` with `enc`, optionally tagging the header with `kid`.
fn sign(enc: &EncodingKey, kid: Option<&str>, claims: &IdTokenClaims) -> String {
    sign_with_algorithm(enc, kid, claims, Algorithm::RS256)
}

fn sign_with_algorithm(
    enc: &EncodingKey,
    kid: Option<&str>,
    claims: &IdTokenClaims,
    algorithm: Algorithm,
) -> String {
    sign_id_token_with_type(enc, kid, claims, algorithm, Some("JWT"))
}

fn sign_id_token_with_type(
    enc: &EncodingKey,
    kid: Option<&str>,
    claims: &IdTokenClaims,
    algorithm: Algorithm,
    typ: Option<&str>,
) -> String {
    let mut header = Header::new(algorithm);
    header.kid = kid.map(ToOwned::to_owned);
    header.typ = typ.map(ToOwned::to_owned);
    encode(&header, claims, enc).expect("sign jwt")
}

fn good_claims(c: &OidcConfig) -> IdTokenClaims {
    let n = now();
    IdTokenClaims {
        iss: c.issuer.clone(),
        aud: serde_json::Value::String(c.audience.clone()),
        sub: "user-abc-123".into(),
        exp: n + 3600,
        nbf: None,
        iat: Some(n),
        azp: None,
        email: Some("alice@example.com".into()),
        name: Some("Alice Example".into()),
        nonce: None,
    }
}

fn good_client_access_claims(c: &ClientCredentialsTokenConfig) -> ClientAccessTokenClaims {
    let n = now();
    ClientAccessTokenClaims {
        iss: c.issuer.clone(),
        aud: c.audience.clone(),
        sub: "erp-production".into(),
        client_id: "erp-production".into(),
        exp: n + 3600,
        nbf: Some(n.saturating_sub(1)),
        iat: Some(n),
        scopes: vec!["aero.notify.publish".into()],
        scope: None,
        jti: Some("access-token-1".into()),
    }
}

fn sign_client_access_token(
    enc: &EncodingKey,
    claims: &ClientAccessTokenClaims,
    typ: &str,
) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("snaplink-access-key".into());
    header.typ = Some(typ.into());
    encode(&header, claims, enc).expect("sign client access token")
}

#[tokio::test]
async fn accepts_valid_token_and_extracts_claims() {
    let c = cfg();
    let (enc, dec) = keypair();
    let input = good_claims(&c);
    let issued_at = input.iat;
    let token = sign(&enc, Some("key-1"), &input);
    let keys = StaticKeyProvider::single(dec);

    let claims = validate_id_token(&token, &c, &keys)
        .await
        .expect("valid token");
    assert_eq!(claims.sub, "user-abc-123");
    assert_eq!(claims.email.as_deref(), Some("alice@example.com"));
    assert_eq!(claims.name.as_deref(), Some("Alice Example"));
    assert_eq!(claims.iat, issued_at);
}

#[tokio::test]
async fn ordinary_oidc_login_accepts_provider_without_iat() {
    let c = cfg();
    let (enc, dec) = keypair();
    let mut input = good_claims(&c);
    input.iat = None;
    let token = sign(&enc, Some("key-1"), &input);
    let claims = validate_id_token(&token, &c, &StaticKeyProvider::single(dec))
        .await
        .expect("iat remains optional for ordinary login");
    assert_eq!(claims.iat, None);
}

mod id_token_policy;

async fn assert_id_token_type_policy(
    config: &OidcConfig,
    encoding_key: &EncodingKey,
    decoding_key: &DecodingKey,
    algorithm: Algorithm,
) {
    for typ in [None, Some("JWT"), Some("jwt"), Some("application/jwt")] {
        let token = sign_id_token_with_type(
            encoding_key,
            Some("id-key"),
            &good_claims(config),
            algorithm,
            typ,
        );
        validate_id_token(
            &token,
            config,
            &StaticKeyProvider::single(decoding_key.clone()),
        )
        .await
        .unwrap_or_else(|error| panic!("ID token type {typ:?} was rejected: {error}"));
    }

    for typ in ["at+jwt", "application/at+jwt", "AT+JWT", "opaque+jwt"] {
        let token = sign_id_token_with_type(
            encoding_key,
            Some("id-key"),
            &good_claims(config),
            algorithm,
            Some(typ),
        );
        assert!(matches!(
            validate_id_token(
                &token,
                config,
                &StaticKeyProvider::single(decoding_key.clone()),
            )
            .await,
            Err(OidcError::Invalid(_))
        ));
    }
}

#[tokio::test]
async fn rs256_id_token_type_policy_rejects_access_token_confusion() {
    let config = cfg();
    let (encoding_key, decoding_key) = keypair();
    assert_id_token_type_policy(&config, &encoding_key, &decoding_key, Algorithm::RS256).await;
}

#[tokio::test]
async fn eddsa_id_token_type_policy_rejects_access_token_confusion() {
    let config = cfg();
    let (encoding_key, decoding_key) = ed25519_keypair();
    assert_id_token_type_policy(&config, &encoding_key, &decoding_key, Algorithm::EdDSA).await;
}

#[tokio::test]
async fn accepts_client_credentials_at_jwt_with_scopes_array() {
    let c = client_credentials_cfg();
    let (enc, dec) = keypair();
    let mut input = good_client_access_claims(&c);
    input.iat = Some(now() + LEEWAY_SECS.saturating_sub(5));
    let token = sign_client_access_token(&enc, &input, "at+jwt");

    let claims = validate_client_credentials_token(&token, &c, &StaticKeyProvider::single(dec))
        .await
        .expect("valid client-credentials token");

    assert_eq!(claims.sub, "erp-production");
    assert_eq!(claims.client_id, "erp-production");
    assert_eq!(claims.jti, "access-token-1");
    assert!(claims.granted_scopes().contains("aero.notify.publish"));
}

#[tokio::test]
async fn accepts_client_credentials_with_space_delimited_scope() {
    let c = client_credentials_cfg();
    let (enc, dec) = keypair();
    let mut input = good_client_access_claims(&c);
    input.scopes.clear();
    input.scope = Some("openid aero.notify.publish erp.read".into());
    let token = sign_client_access_token(&enc, &input, "application/at+jwt");

    let claims = validate_client_credentials_token(&token, &c, &StaticKeyProvider::single(dec))
        .await
        .expect("standard OAuth scope claim is accepted");

    assert!(claims.granted_scopes().contains("aero.notify.publish"));
}

#[tokio::test]
async fn rejects_client_credentials_token_with_wrong_typ() {
    let c = client_credentials_cfg();
    let (enc, dec) = keypair();
    let token = sign_client_access_token(&enc, &good_client_access_claims(&c), "JWT");

    let err = validate_client_credentials_token(&token, &c, &StaticKeyProvider::single(dec))
        .await
        .unwrap_err();
    assert!(matches!(err, OidcError::Invalid(_)), "got {err:?}");
}

#[tokio::test]
async fn rejects_client_credentials_token_when_sub_differs_from_client_id() {
    let c = client_credentials_cfg();
    let (enc, dec) = keypair();
    let mut input = good_client_access_claims(&c);
    input.sub = "different-principal".into();
    let token = sign_client_access_token(&enc, &input, "at+jwt");

    let err = validate_client_credentials_token(&token, &c, &StaticKeyProvider::single(dec))
        .await
        .unwrap_err();
    assert!(matches!(err, OidcError::Invalid(_)), "got {err:?}");
}

#[tokio::test]
async fn rejects_client_credentials_token_without_required_scope() {
    let c = client_credentials_cfg();
    let (enc, dec) = keypair();
    let mut input = good_client_access_claims(&c);
    input.scopes = vec!["erp.read".into()];
    let token = sign_client_access_token(&enc, &input, "at+jwt");

    let err = validate_client_credentials_token(&token, &c, &StaticKeyProvider::single(dec))
        .await
        .unwrap_err();
    assert!(matches!(err, OidcError::Invalid(_)), "got {err:?}");
}

#[tokio::test]
async fn rejects_client_credentials_token_with_wrong_issuer_or_audience() {
    let c = client_credentials_cfg();
    let (enc, dec) = keypair();

    let mut wrong_issuer = good_client_access_claims(&c);
    wrong_issuer.iss = "https://evil.example.com".into();
    let token = sign_client_access_token(&enc, &wrong_issuer, "at+jwt");
    assert!(matches!(
        validate_client_credentials_token(&token, &c, &StaticKeyProvider::single(dec.clone()),)
            .await,
        Err(OidcError::Invalid(_))
    ));

    let mut wrong_audience = good_client_access_claims(&c);
    wrong_audience.aud = "another-service".into();
    let token = sign_client_access_token(&enc, &wrong_audience, "at+jwt");
    assert!(matches!(
        validate_client_credentials_token(&token, &c, &StaticKeyProvider::single(dec),).await,
        Err(OidcError::Invalid(_))
    ));
}

#[tokio::test]
async fn rejects_expired_client_credentials_token_and_token_without_nbf() {
    let c = client_credentials_cfg();
    let (enc, dec) = keypair();

    let mut expired = good_client_access_claims(&c);
    expired.exp = now().saturating_sub(LEEWAY_SECS + 1);
    let token = sign_client_access_token(&enc, &expired, "at+jwt");
    assert!(matches!(
        validate_client_credentials_token(&token, &c, &StaticKeyProvider::single(dec.clone()),)
            .await,
        Err(OidcError::Invalid(_))
    ));

    let mut missing_nbf = good_client_access_claims(&c);
    missing_nbf.nbf = None;
    let token = sign_client_access_token(&enc, &missing_nbf, "at+jwt");
    assert!(matches!(
        validate_client_credentials_token(&token, &c, &StaticKeyProvider::single(dec),).await,
        Err(OidcError::Invalid(_))
    ));
}

#[tokio::test]
async fn rejects_missing_or_invalid_client_credentials_iat_and_jti() {
    let c = client_credentials_cfg();
    let (enc, dec) = keypair();
    let invalid = [
        ("missing iat", None, Some("token-1".into())),
        (
            "future iat",
            Some(now() + LEEWAY_SECS + 30),
            Some("token-2".into()),
        ),
        ("missing jti", Some(now()), None),
        ("empty jti", Some(now()), Some(String::new())),
        ("control jti", Some(now()), Some("token\n3".into())),
    ];

    for (case, iat, jti) in invalid {
        let mut input = good_client_access_claims(&c);
        input.iat = iat;
        input.jti = jti;
        let token = sign_client_access_token(&enc, &input, "at+jwt");
        let result =
            validate_client_credentials_token(&token, &c, &StaticKeyProvider::single(dec.clone()))
                .await;
        assert!(matches!(result, Err(OidcError::Invalid(_))), "{case}");
    }
}

#[tokio::test]
async fn accepts_ed25519_eddsa_token_and_extracts_nonce() {
    let c = cfg();
    let (enc, dec) = ed25519_keypair();
    let mut input = good_claims(&c);
    input.nonce = Some("browser-nonce".into());
    let token = sign_with_algorithm(&enc, Some("snaplink-ed25519"), &input, Algorithm::EdDSA);
    let keys = StaticKeyProvider::single(dec);

    let claims = validate_id_token(&token, &c, &keys)
        .await
        .expect("valid EdDSA token");
    assert_eq!(claims.sub, "user-abc-123");
    assert_eq!(claims.nonce.as_deref(), Some("browser-nonce"));
}

#[tokio::test]
async fn rejects_algorithm_outside_explicit_allowlist() {
    let c = cfg();
    let token = sign_with_algorithm(
        &EncodingKey::from_secret(b"not-an-oidc-signing-key"),
        Some("symmetric"),
        &good_claims(&c),
        Algorithm::HS256,
    );
    let (_enc, dec) = keypair();
    let keys = StaticKeyProvider::single(dec);

    let err = validate_id_token(&token, &c, &keys).await.unwrap_err();
    assert!(
        matches!(err, OidcError::UnsupportedAlgorithm),
        "got {err:?}"
    );
}

#[tokio::test]
async fn rejects_algorithm_and_key_family_mismatch() {
    let c = cfg();
    let (rsa_enc, rsa_dec) = keypair();
    let (ed_enc, ed_dec) = ed25519_keypair();
    let rsa_token = sign(&rsa_enc, Some("rsa"), &good_claims(&c));
    let ed_token = sign_with_algorithm(&ed_enc, Some("ed"), &good_claims(&c), Algorithm::EdDSA);

    let rsa_with_ed = StaticKeyProvider::single(ed_dec);
    assert!(matches!(
        validate_id_token(&rsa_token, &c, &rsa_with_ed).await,
        Err(OidcError::Invalid(_))
    ));

    let ed_with_rsa = StaticKeyProvider::single(rsa_dec);
    assert!(matches!(
        validate_id_token(&ed_token, &c, &ed_with_rsa).await,
        Err(OidcError::Invalid(_))
    ));
}

#[test]
fn jwk_conversion_accepts_only_compatible_ed25519_signing_key() {
    let valid: Jwk = serde_json::from_value(serde_json::json!({
        "kty": "OKP",
        "use": "sig",
        "key_ops": ["verify"],
        "crv": "Ed25519",
        "x": ED25519_PUBLIC_X,
        "kid": "snaplink-ed25519",
        "alg": "EdDSA"
    }))
    .unwrap();
    assert!(jwk_to_key(&valid, Algorithm::EdDSA).is_some());
    assert!(jwk_to_key(&valid, Algorithm::RS256).is_none());

    let wrong_curve: Jwk = serde_json::from_value(serde_json::json!({
        "kty": "OKP",
        "use": "sig",
        "crv": "X25519",
        "x": ED25519_PUBLIC_X,
        "alg": "EdDSA"
    }))
    .unwrap();
    assert!(jwk_to_key(&wrong_curve, Algorithm::EdDSA).is_none());

    let signing_only: Jwk = serde_json::from_value(serde_json::json!({
        "kty": "OKP",
        "use": "sig",
        "key_ops": ["sign"],
        "crv": "Ed25519",
        "x": ED25519_PUBLIC_X,
        "alg": "EdDSA"
    }))
    .unwrap();
    assert!(jwk_to_key(&signing_only, Algorithm::EdDSA).is_none());
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
    assert!(
        matches!(err, OidcError::UnknownKey(Some(ref k)) if k == "kid-unknown"),
        "got {err:?}"
    );
}

#[tokio::test]
async fn rejects_wrong_audience() {
    let c = cfg();
    let (enc, dec) = keypair();
    let mut claims = good_claims(&c);
    claims.aud = serde_json::json!("some-other-client");
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
    claims.iat = Some(n - 7200);
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
        nonce: None,
        iat: None,
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
        nonce: None,
        iat: None,
    };
    // All higher-priority fields are blank, so it falls through to sub.
    assert_eq!(c.best_display_name(), "sub-x");
}

#[test]
fn oidc_claims_debug_redacts_identity_and_profile_values() {
    let claims = OidcClaims {
        sub: "sensitive-subject".into(),
        email: Some("sensitive@example.test".into()),
        name: Some("Sensitive Name".into()),
        email_verified: Some(true),
        preferred_username: Some("sensitive-user".into()),
        nonce: Some("sensitive-nonce".into()),
        iat: Some(123),
    };
    let debug = format!("{claims:?}");
    for secret in [
        "sensitive-subject",
        "sensitive@example.test",
        "Sensitive Name",
        "sensitive-user",
        "sensitive-nonce",
    ] {
        assert!(!debug.contains(secret));
    }
    assert!(debug.contains("[redacted]"));
}

#[test]
fn config_from_env_requires_all_three() {
    // Use a unique prefix-free approach: set/unset the real keys around the
    // assertion. These tests run single-threaded per-process for env safety
    // is not guaranteed, so we only assert the "missing" path which needs no
    // env set (and is the default-off behavior we care about).
    // (Presence is exercised implicitly by integration wiring.)
    // Save & clear.
    let keys = [
        "AERO__OIDC__ISSUER",
        "AERO__OIDC__AUDIENCE",
        "AERO__OIDC__JWKS_URI",
    ];
    let saved: Vec<_> = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
    for k in keys {
        std::env::remove_var(k);
    }
    assert!(
        OidcConfig::from_env().is_none(),
        "unconfigured → None (OIDC off by default)"
    );
    // Restore whatever was there.
    for (k, v) in saved {
        match v {
            Some(val) => std::env::set_var(k, val),
            None => std::env::remove_var(k),
        }
    }
}

#[test]
fn jwks_uri_policy_allows_secure_remote_and_loopback_development() {
    for uri in [
        "https://idp.example.test/.well-known/jwks.json",
        "https://idp.example.test:8443/keys",
        "http://localhost:18080/keys",
        "http://127.0.0.1:18080/keys",
        "http://127.42.0.9/keys",
        "http://[::1]:18080/keys",
    ] {
        assert!(validate_jwks_uri(uri).is_ok(), "rejected {uri}");
    }
}

#[test]
fn jwks_uri_policy_rejects_cleartext_remote_and_ambiguous_urls() {
    for uri in [
        "http://idp.example.test/keys",
        "http://localhost.example.test/keys",
        "ftp://idp.example.test/keys",
        "https://user:password@idp.example.test/keys",
        "https://@idp.example.test/keys",
        "https://idp.example.test/keys?tenant=secret",
        "https://idp.example.test/keys#fragment",
        "https://idp.example.test/keys ",
        "/relative/keys",
    ] {
        assert!(validate_jwks_uri(uri).is_err(), "accepted {uri}");
    }
}

#[tokio::test]
async fn jwks_provider_rejects_unsafe_uri_before_network_fetch() {
    let provider = JwksKeyProvider::new("http://remote.example.test/private-path");
    let error = provider.fetch().await.unwrap_err();
    assert_eq!(error, "jwks URI rejected by policy");
    assert!(!error.contains("remote.example.test"));
    assert!(!error.contains("private-path"));
}
