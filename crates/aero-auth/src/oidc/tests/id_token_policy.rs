use super::*;

async fn assert_authorized_party_policy(
    config: &OidcConfig,
    encoding_key: &EncodingKey,
    decoding_key: &DecodingKey,
    algorithm: Algorithm,
) {
    let validate = |claims: &IdTokenClaims| {
        let token = sign_with_algorithm(encoding_key, Some("id-key"), claims, algorithm);
        let key = StaticKeyProvider::single(decoding_key.clone());
        async move { validate_id_token(&token, config, &key).await }
    };

    // The long-standing interoperable form: a single string audience needs no
    // `azp`. A one-element array is semantically the same single audience.
    let single = good_claims(config);
    validate(&single)
        .await
        .expect("single string audience without azp");
    let mut one_element = good_claims(config);
    one_element.aud = serde_json::json!([config.audience]);
    validate(&one_element)
        .await
        .expect("single array audience without azp");

    let mut multiple = good_claims(config);
    multiple.aud = serde_json::json!([config.audience, "another-client"]);
    assert!(matches!(
        validate(&multiple).await,
        Err(OidcError::Invalid(_))
    ));

    multiple.azp = Some("another-client".into());
    assert!(matches!(
        validate(&multiple).await,
        Err(OidcError::Invalid(_))
    ));

    multiple.azp = Some(config.audience.clone());
    validate(&multiple)
        .await
        .expect("multiple audiences with exact azp");

    let mut single_wrong_azp = good_claims(config);
    single_wrong_azp.azp = Some("another-client".into());
    assert!(matches!(
        validate(&single_wrong_azp).await,
        Err(OidcError::Invalid(_))
    ));
}

#[tokio::test]
async fn rs256_enforces_oidc_authorized_party_policy() {
    let config = cfg();
    let (encoding_key, decoding_key) = keypair();
    assert_authorized_party_policy(&config, &encoding_key, &decoding_key, Algorithm::RS256).await;
}

#[tokio::test]
async fn eddsa_enforces_oidc_authorized_party_policy() {
    let config = cfg();
    let (encoding_key, decoding_key) = ed25519_keypair();
    assert_authorized_party_policy(&config, &encoding_key, &decoding_key, Algorithm::EdDSA).await;
}

async fn assert_required_and_time_claim_policy(
    config: &OidcConfig,
    encoding_key: &EncodingKey,
    decoding_key: &DecodingKey,
    algorithm: Algorithm,
) {
    for missing in ["iss", "aud", "exp", "sub"] {
        let mut claims = serde_json::to_value(good_claims(config)).expect("serialize test claims");
        claims
            .as_object_mut()
            .expect("claims serialize as an object")
            .remove(missing);
        let mut header = Header::new(algorithm);
        header.kid = Some("id-key".into());
        header.typ = Some("JWT".into());
        let token = encode(&header, &claims, encoding_key).expect("sign claims with omitted field");
        assert!(
            matches!(
                validate_id_token(
                    &token,
                    config,
                    &StaticKeyProvider::single(decoding_key.clone()),
                )
                .await,
                Err(OidcError::Invalid(_))
            ),
            "accepted token missing required {missing} claim"
        );
    }

    let mut future = good_claims(config);
    future.nbf = Some(now() + LEEWAY_SECS + 30);
    let token = sign_with_algorithm(encoding_key, Some("id-key"), &future, algorithm);
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

#[tokio::test]
async fn rs256_requires_registered_claims_and_checks_present_nbf() {
    let config = cfg();
    let (encoding_key, decoding_key) = keypair();
    assert_required_and_time_claim_policy(&config, &encoding_key, &decoding_key, Algorithm::RS256)
        .await;
}

#[tokio::test]
async fn eddsa_requires_registered_claims_and_checks_present_nbf() {
    let config = cfg();
    let (encoding_key, decoding_key) = ed25519_keypair();
    assert_required_and_time_claim_policy(&config, &encoding_key, &decoding_key, Algorithm::EdDSA)
        .await;
}
