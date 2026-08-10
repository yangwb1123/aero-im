//! Tests for [`super`], split out to keep the parent under the 1200-line HARD limit.

use super::*;

fn cfg() -> SamlConfig {
    SamlConfig {
        sp_entity_id: "https://aero.example/saml/metadata".into(),
        acs_url: "https://aero.example/saml/acs".into(),
        idp_entity_id: "https://idp.example/entity".into(),
        idp_sso_url: "https://idp.example/sso".into(),
        idp_cert_pem: "-----BEGIN CERTIFICATE-----\nMIID\n-----END CERTIFICATE-----".into(),
    }
}

// ---- metadata generation -------------------------------------------------

#[test]
fn metadata_contains_entity_acs_and_wants_signed_assertions() {
    let m = build_sp_metadata(&cfg());
    assert!(m.contains(r#"entityID="https://aero.example/saml/metadata""#));
    assert!(m.contains(r#"Location="https://aero.example/saml/acs""#));
    assert!(m.contains("HTTP-POST"));
    // Fail-closed posture is reflected in metadata.
    assert!(m.contains(r#"WantAssertionsSigned="true""#));
    assert!(m.starts_with("<?xml"));
}

#[test]
fn metadata_escapes_special_chars() {
    let mut c = cfg();
    c.sp_entity_id = "https://x/?a=1&b=2".into();
    let m = build_sp_metadata(&c);
    assert!(m.contains("a=1&amp;b=2"), "ampersand must be escaped: {m}");
    assert!(!m.contains("a=1&b=2"));
}

// ---- AuthnRequest construction ------------------------------------------

#[test]
fn authn_request_has_required_attributes() {
    let x = build_authn_request(&cfg(), "_abc123", "2026-06-19T00:00:00Z");
    assert!(x.contains(r#"ID="_abc123""#));
    assert!(x.contains(r#"Version="2.0""#));
    assert!(x.contains(r#"IssueInstant="2026-06-19T00:00:00Z""#));
    assert!(x.contains(r#"Destination="https://idp.example/sso""#));
    assert!(x.contains(r#"AssertionConsumerServiceURL="https://aero.example/saml/acs""#));
    assert!(x.contains("<saml:Issuer>https://aero.example/saml/metadata</saml:Issuer>"));
    assert!(x.contains("AuthnRequest"));
}

#[test]
fn redirect_url_roundtrips_through_deflate_base64() {
    use std::io::Read as _;
    let c = cfg();
    let xml = build_authn_request(&c, "_id1", "2026-06-19T00:00:00Z");
    let url = redirect_url_for_authn_request(&c, &xml).expect("encode");
    assert!(url.starts_with("https://idp.example/sso?SAMLRequest="));

    // Pull the SAMLRequest param back out, url-decode, base64-decode, inflate,
    // and confirm we recover the original XML — proving the binding encoding.
    let (_, value) = form_urlencoded::parse(url.split_once('?').unwrap().1.as_bytes())
        .find(|(k, _)| k == "SAMLRequest")
        .expect("SAMLRequest param");
    let raw = base64::engine::general_purpose::STANDARD
        .decode(value.as_bytes())
        .expect("base64");
    let mut inflated = String::new();
    flate2::read::DeflateDecoder::new(&raw[..])
        .read_to_string(&mut inflated)
        .expect("inflate");
    assert_eq!(inflated, xml);
}

#[test]
fn redirect_url_appends_with_ampersand_when_idp_url_has_query() {
    let mut c = cfg();
    c.idp_sso_url = "https://idp.example/sso?tenant=acme".into();
    let xml = build_authn_request(&c, "_id2", "2026-06-19T00:00:00Z");
    let url = redirect_url_for_authn_request(&c, &xml).expect("encode");
    assert!(url.starts_with("https://idp.example/sso?tenant=acme&SAMLRequest="));
}

// ---- SAMLResponse decode + assertion extraction (pure logic) -------------

const SAMPLE_RESPONSE: &str = r#"<?xml version="1.0"?>
<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion">
  <saml:Issuer>https://idp.example/entity</saml:Issuer>
  <samlp:Status><samlp:StatusCode Value="urn:oasis:names:tc:SAML:2.0:status:Success"/></samlp:Status>
  <saml:Assertion>
    <saml:Issuer>https://idp.example/entity</saml:Issuer>
    <saml:Subject>
      <saml:NameID Format="urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress">alice@example.com</saml:NameID>
    </saml:Subject>
    <saml:AttributeStatement>
      <saml:Attribute Name="email"><saml:AttributeValue>alice@example.com</saml:AttributeValue></saml:Attribute>
      <saml:Attribute Name="displayName"><saml:AttributeValue>Alice Example</saml:AttributeValue></saml:Attribute>
    </saml:AttributeStatement>
  </saml:Assertion>
</samlp:Response>"#;

#[test]
fn decode_saml_response_handles_whitespace_and_utf8() {
    let b64 = base64::engine::general_purpose::STANDARD.encode(SAMPLE_RESPONSE);
    // Inject the line-wrapping some IdPs apply.
    let wrapped = format!("{}\n  {}", &b64[..20], &b64[20..]);
    let xml = decode_saml_response(&wrapped).expect("decode");
    assert_eq!(xml, SAMPLE_RESPONSE);
}

#[test]
fn decode_saml_response_rejects_non_base64() {
    let err = decode_saml_response("!!!not-base64!!!").unwrap_err();
    assert!(matches!(err, AeroError::Invalid(_)), "got {err:?}");
}

#[test]
fn decode_saml_response_rejects_oversized_encoded_input() {
    let oversized = "A".repeat(MAX_SAML_RESPONSE_B64_BYTES + 1);
    let err = decode_saml_response(&oversized).unwrap_err();
    assert!(matches!(err, AeroError::Invalid(_)), "got {err:?}");
}

#[test]
fn extract_assertion_pulls_issuer_nameid_and_attributes() {
    let a = extract_assertion(SAMPLE_RESPONSE).expect("extract");
    assert_eq!(a.issuer, "https://idp.example/entity");
    assert_eq!(a.name_id, "alice@example.com");
    assert_eq!(a.email().as_deref(), Some("alice@example.com"));
    assert_eq!(a.best_display_name(), "Alice Example");
    assert_eq!(a.attr_any(&["email"]).as_deref(), Some("alice@example.com"));
}

#[test]
fn extract_assertion_email_falls_back_to_email_shaped_nameid() {
    let xml = r#"<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion">
          <saml:Assertion>
            <saml:Issuer>https://idp.example/entity</saml:Issuer>
            <saml:Subject><saml:NameID>bob@example.com</saml:NameID></saml:Subject>
          </saml:Assertion>
        </samlp:Response>"#;
    let a = extract_assertion(xml).expect("extract");
    assert_eq!(a.name_id, "bob@example.com");
    assert_eq!(a.email().as_deref(), Some("bob@example.com"));
    // No displayName attr → falls back to email local-part.
    assert_eq!(a.best_display_name(), "bob");
}

#[test]
fn extract_assertion_requires_a_nameid() {
    let xml = r#"<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion">
          <saml:Assertion><saml:Issuer>x</saml:Issuer></saml:Assertion>
        </samlp:Response>"#;
    let err = extract_assertion(xml).unwrap_err();
    assert!(matches!(err, AeroError::Unauthorized(_)), "got {err:?}");
}

#[test]
fn extract_assertion_decodes_xml_entities_in_values() {
    let xml = r#"<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion">
          <saml:Assertion>
            <saml:Issuer>https://idp.example/entity</saml:Issuer>
            <saml:Subject><saml:NameID>u@x.com</saml:NameID></saml:Subject>
            <saml:AttributeStatement>
              <saml:Attribute Name="displayName"><saml:AttributeValue>Tom &amp; Jerry</saml:AttributeValue></saml:Attribute>
            </saml:AttributeStatement>
          </saml:Assertion>
        </samlp:Response>"#;
    let a = extract_assertion(xml).expect("extract");
    assert_eq!(a.best_display_name(), "Tom & Jerry");
}

fn conditioned_response(
    not_before: &str,
    not_on_or_after: &str,
    audience: &str,
    destination: &str,
    response_request_id: &str,
    recipient: &str,
    subject_request_id: &str,
) -> String {
    format!(
        r#"<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion" Destination="{destination}" InResponseTo="{response_request_id}">
          <samlp:Status><samlp:StatusCode Value="urn:oasis:names:tc:SAML:2.0:status:Success"/></samlp:Status>
          <saml:Assertion>
            <saml:Issuer>https://idp.example/entity</saml:Issuer>
            <saml:Subject>
              <saml:NameID>alice@example.com</saml:NameID>
              <saml:SubjectConfirmation Method="urn:oasis:names:tc:SAML:2.0:cm:bearer">
                <saml:SubjectConfirmationData Recipient="{recipient}" InResponseTo="{subject_request_id}" NotOnOrAfter="{not_on_or_after}"/>
              </saml:SubjectConfirmation>
            </saml:Subject>
            <saml:Conditions NotBefore="{not_before}" NotOnOrAfter="{not_on_or_after}">
              <saml:AudienceRestriction><saml:Audience>{audience}</saml:Audience></saml:AudienceRestriction>
            </saml:Conditions>
          </saml:Assertion>
        </samlp:Response>"#
    )
}

fn condition_test_now() -> time::OffsetDateTime {
    time::OffsetDateTime::parse(
        "2026-07-29T12:00:00Z",
        &time::format_description::well_known::Rfc3339,
    )
    .expect("fixed timestamp")
}

fn valid_conditioned_response() -> String {
    conditioned_response(
        "2026-07-29T11:59:00Z",
        "2026-07-29T12:05:00Z",
        "https://aero.example/saml/metadata",
        "https://aero.example/saml/acs",
        "_01HZX4M8M8J5GSY7CX3F2TZC9Q",
        "https://aero.example/saml/acs",
        "_01HZX4M8M8J5GSY7CX3F2TZC9Q",
    )
}

#[test]
fn response_conditions_accept_expected_sp_and_correlation() {
    let validated =
        validate_response_conditions(&cfg(), &valid_conditioned_response(), condition_test_now())
            .expect("valid conditions");
    assert_eq!(validated.request_id, "_01HZX4M8M8J5GSY7CX3F2TZC9Q");
}

#[test]
fn response_conditions_enforce_subject_not_before() {
    let within_skew = valid_conditioned_response().replace(
        "<saml:SubjectConfirmationData ",
        r#"<saml:SubjectConfirmationData NotBefore="2026-07-29T12:01:00Z" "#,
    );
    validate_response_conditions(&cfg(), &within_skew, condition_test_now())
        .expect("subject NotBefore within explicit clock skew");

    let future = valid_conditioned_response().replace(
        "<saml:SubjectConfirmationData ",
        r#"<saml:SubjectConfirmationData NotBefore="2026-07-29T12:03:00Z" "#,
    );
    assert!(
        validate_response_conditions(&cfg(), &future, condition_test_now()).is_err(),
        "a signed bearer confirmation must not be accepted before its own NotBefore"
    );
}

#[test]
fn response_conditions_fail_closed_on_unknown_signed_conditions() {
    let unknown = valid_conditioned_response().replace(
        "<saml:AudienceRestriction>",
        "<saml:CustomCondition/><saml:AudienceRestriction>",
    );
    assert!(validate_response_conditions(&cfg(), &unknown, condition_test_now()).is_err());

    let one_time = valid_conditioned_response().replace(
        "<saml:AudienceRestriction>",
        "<saml:OneTimeUse/><saml:AudienceRestriction>",
    );
    validate_response_conditions(&cfg(), &one_time, condition_test_now())
        .expect("standard OneTimeUse is enforced by atomic request-state consumption");
}

#[test]
fn response_conditions_apply_small_clock_skew_but_reject_expiry() {
    let within_skew = conditioned_response(
        "2026-07-29T12:01:00Z",
        "2026-07-29T12:05:00Z",
        "https://aero.example/saml/metadata",
        "https://aero.example/saml/acs",
        "_01HZX4M8M8J5GSY7CX3F2TZC9Q",
        "https://aero.example/saml/acs",
        "_01HZX4M8M8J5GSY7CX3F2TZC9Q",
    );
    validate_response_conditions(&cfg(), &within_skew, condition_test_now())
        .expect("60-second IdP clock lead is within the explicit skew");

    let expired = conditioned_response(
        "2026-07-29T11:50:00Z",
        "2026-07-29T11:58:00Z",
        "https://aero.example/saml/metadata",
        "https://aero.example/saml/acs",
        "_01HZX4M8M8J5GSY7CX3F2TZC9Q",
        "https://aero.example/saml/acs",
        "_01HZX4M8M8J5GSY7CX3F2TZC9Q",
    );
    assert!(
        validate_response_conditions(&cfg(), &expired, condition_test_now()).is_err(),
        "an assertion expired beyond the 90-second skew must fail"
    );
}

#[test]
fn response_conditions_reject_excessively_broad_validity_window() {
    let xml = conditioned_response(
        "2026-07-29T11:59:00Z",
        "2026-07-29T12:20:00Z",
        "https://aero.example/saml/metadata",
        "https://aero.example/saml/acs",
        "_01HZX4M8M8J5GSY7CX3F2TZC9Q",
        "https://aero.example/saml/acs",
        "_01HZX4M8M8J5GSY7CX3F2TZC9Q",
    );
    assert!(validate_response_conditions(&cfg(), &xml, condition_test_now()).is_err());
}

#[test]
fn response_conditions_reject_wrong_audience() {
    let xml = valid_conditioned_response().replace(
        "https://aero.example/saml/metadata",
        "https://other-sp.example/metadata",
    );
    assert!(validate_response_conditions(&cfg(), &xml, condition_test_now()).is_err());
}

#[test]
fn response_conditions_reject_saml_lookalikes_in_wrong_namespace() {
    let xml = valid_conditioned_response().replace(
        "urn:oasis:names:tc:SAML:2.0:assertion",
        "urn:attacker:lookalike",
    );
    assert!(validate_response_conditions(&cfg(), &xml, condition_test_now()).is_err());
}

#[test]
fn response_conditions_reject_wrong_destination_or_recipient() {
    let wrong_destination = valid_conditioned_response().replacen(
        r#"Destination="https://aero.example/saml/acs""#,
        r#"Destination="https://attacker.example/acs""#,
        1,
    );
    assert!(
        validate_response_conditions(&cfg(), &wrong_destination, condition_test_now()).is_err()
    );

    let wrong_recipient = valid_conditioned_response().replacen(
        r#"Recipient="https://aero.example/saml/acs""#,
        r#"Recipient="https://attacker.example/acs""#,
        1,
    );
    assert!(validate_response_conditions(&cfg(), &wrong_recipient, condition_test_now()).is_err());
}

#[test]
fn response_conditions_reject_mismatched_in_response_to() {
    let xml = valid_conditioned_response().replacen(
        r#"InResponseTo="_01HZX4M8M8J5GSY7CX3F2TZC9Q""#,
        r#"InResponseTo="_01HZX4M8M8J5GSY7CX3F2TZC9R""#,
        1,
    );
    assert!(validate_response_conditions(&cfg(), &xml, condition_test_now()).is_err());
}

#[test]
fn identity_extraction_ignores_unsigned_response_lookalikes() {
    let xml = valid_conditioned_response().replacen(
        "<samlp:Status>",
        "<saml:Subject><saml:NameID>attacker@example.com</saml:NameID></saml:Subject><samlp:Status>",
        1,
    );
    let assertion = extract_assertion(&xml).expect("signed assertion identity");
    assert_eq!(assertion.name_id, "alice@example.com");
}

// ---- THE security-critical test: signature verification is FAIL-CLOSED ----

/// `AERO_SAML_EXPERIMENTAL_VERIFY` is a process-global env var, but the
/// fail-closed test wants it OFF while the adversarial tests want it ON. This
/// mutex serializes any test that touches the switch so they never race.
static VERIFY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run `f` with the experimental-verify switch forced to `on`/`off`, restoring
/// the prior value afterward. Holds [`VERIFY_ENV_LOCK`] for the duration.
fn with_experimental_verify<R>(on: bool, f: impl FnOnce() -> R) -> R {
    let _guard = VERIFY_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let saved = std::env::var(EXPERIMENTAL_VERIFY_ENV).ok();
    if on {
        std::env::set_var(EXPERIMENTAL_VERIFY_ENV, "1");
    } else {
        std::env::remove_var(EXPERIMENTAL_VERIFY_ENV);
    }
    let out = f();
    match saved {
        Some(v) => std::env::set_var(EXPERIMENTAL_VERIFY_ENV, v),
        None => std::env::remove_var(EXPERIMENTAL_VERIFY_ENV),
    }
    out
}

#[test]
fn signature_verification_is_fail_closed_and_rejects_everything() {
    with_experimental_verify(false, || {
        let c = cfg();
        // Even a perfectly well-formed, issuer-matching response must be
        // rejected: the opt-in verifier is OFF, so we fail closed.
        let err = verify_response_signature(&c, SAMPLE_RESPONSE).unwrap_err();
        match err {
            AeroError::Unauthorized(m) => {
                assert!(
                    m.contains("signature validation not yet wired"),
                    "must clearly state why it failed: {m}"
                );
            }
            other => panic!("expected Unauthorized fail-closed, got {other:?}"),
        }

        // Empty / garbage documents are likewise rejected (never silently pass).
        assert!(verify_response_signature(&c, "").is_err());
        assert!(verify_response_signature(&c, "<not-saml/>").is_err());
    });
}

// ======================================================================
// ADVERSARIAL XML-DSig tests (opt-in bergshamra verifier).
//
// These mint a throwaway RSA keypair + self-signed X.509 cert, sign a SAML
// Response with bergshamra, then prove the verifier ACCEPTS the genuine
// signature and REJECTS every forgery class: tampered assertion, signature
// wrapping (XSW), wrong cert, and unsigned. If any forgery is accepted the
// verifier is worthless — so these asserts are the real proof of correctness.
// ======================================================================

/// A freshly generated RSA signer: PKCS#8 private-key PEM (fed to bergshamra's
/// signer) plus the matching self-signed X.509 certificate PEM (the trusted
/// IdP cert the verifier checks against).
#[cfg(feature = "saml-experimental-bergshamra")]
struct TestIdp {
    cert_pem: String,
    key_pkcs8_pem: String,
}

/// Generate an RSA-2048 keypair and a self-signed cert over it (rcgen, RSA-
/// SHA256). Done once per test; 2048-bit keygen is the slow part but real.
#[cfg(feature = "saml-experimental-bergshamra")]
fn gen_test_idp() -> TestIdp {
    use rsa::pkcs8::EncodePrivateKey as _;
    // 1. Real RSA-2048 private key.
    let mut rng = rand::rngs::OsRng;
    let priv_key = rsa::RsaPrivateKey::new(&mut rng, 2048).expect("rsa keygen");
    let key_pkcs8_pem = priv_key
        .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
        .expect("pkcs8 pem")
        .to_string();

    // 2. Self-signed X.509 cert over that exact key (rcgen + RSA-SHA256).
    let rc_key =
        rcgen::KeyPair::from_pkcs8_pem_and_sign_algo(&key_pkcs8_pem, &rcgen::PKCS_RSA_SHA256)
            .expect("rcgen keypair");
    let params =
        rcgen::CertificateParams::new(vec!["idp.example".to_owned()]).expect("cert params");
    let cert = params.self_signed(&rc_key).expect("self-sign");

    TestIdp {
        cert_pem: cert.pem(),
        key_pkcs8_pem,
    }
}

/// A SAML Response whose single `<Assertion id="…">` envelopes a `<Signature>`
/// template (empty DigestValue/SignatureValue) referencing the assertion by
/// `URI="#assertion-1"`, with the enveloped-signature + exclusive-c14n
/// transforms. bergshamra's signer fills the digest + signature in place.
// NB: `r##"…"##` (not `r#"…"#`) because the XML contains the byte pair `"#`
// (e.g. `URI="#assertion-1"`), which would close a single-hash raw string early.
#[cfg(feature = "saml-experimental-bergshamra")]
const SIGN_TEMPLATE: &str = r##"<?xml version="1.0"?>
<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol" xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion">
  <saml:Issuer>https://idp.example/entity</saml:Issuer>
  <saml:Assertion ID="assertion-1">
    <saml:Issuer>https://idp.example/entity</saml:Issuer>
    <ds:Signature xmlns:ds="http://www.w3.org/2000/09/xmldsig#">
      <ds:SignedInfo>
        <ds:CanonicalizationMethod Algorithm="http://www.w3.org/2001/10/xml-exc-c14n#"/>
        <ds:SignatureMethod Algorithm="http://www.w3.org/2001/04/xmldsig-more#rsa-sha256"/>
        <ds:Reference URI="#assertion-1">
          <ds:Transforms>
            <ds:Transform Algorithm="http://www.w3.org/2000/09/xmldsig#enveloped-signature"/>
            <ds:Transform Algorithm="http://www.w3.org/2001/10/xml-exc-c14n#"/>
          </ds:Transforms>
          <ds:DigestMethod Algorithm="http://www.w3.org/2001/04/xmlenc#sha256"/>
          <ds:DigestValue></ds:DigestValue>
        </ds:Reference>
      </ds:SignedInfo>
      <ds:SignatureValue></ds:SignatureValue>
    </ds:Signature>
    <saml:Subject>
      <saml:NameID Format="urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress">alice@example.com</saml:NameID>
    </saml:Subject>
    <saml:AttributeStatement>
      <saml:Attribute Name="displayName"><saml:AttributeValue>Alice Example</saml:AttributeValue></saml:Attribute>
    </saml:AttributeStatement>
  </saml:Assertion>
</samlp:Response>"##;

/// Sign [`SIGN_TEMPLATE`] with `idp`'s RSA key → a genuinely-signed Response.
#[cfg(feature = "saml-experimental-bergshamra")]
fn sign_response(idp: &TestIdp) -> String {
    use bergshamra::dsig::{sign::sign, DsigContext};
    use bergshamra::keys::{loader::load_rsa_private_pem, KeysManager};
    let key = load_rsa_private_pem(idp.key_pkcs8_pem.as_bytes()).expect("load priv");
    let mut km = KeysManager::new();
    km.add_key(key);
    // Permissive context for *signing* (strict/trusted-keys flags only gate
    // verification); the genuine signature is then checked by the hardened
    // verify path under test.
    let ctx = DsigContext::new_permissive(km);
    sign(&ctx, SIGN_TEMPLATE).expect("sign template")
}

#[cfg(feature = "saml-experimental-bergshamra")]
fn cfg_with_cert(cert_pem: &str) -> SamlConfig {
    let mut c = cfg();
    c.idp_cert_pem = cert_pem.to_owned();
    c
}

#[test]
#[cfg(feature = "saml-experimental-bergshamra")]
fn adversarial_genuine_signature_is_accepted_and_nameid_extracts() {
    with_experimental_verify(true, || {
        let idp = gen_test_idp();
        let signed = sign_response(&idp);
        let c = cfg_with_cert(&idp.cert_pem);

        // ACCEPT: genuine signature + correct trusted cert.
        verify_response_signature(&c, &signed)
            .expect("genuine signature with matching cert must verify");

        // And the consumed identity is the real one.
        let a = extract_assertion(&signed).expect("extract");
        assert_eq!(a.name_id, "alice@example.com");
        assert_eq!(a.best_display_name(), "Alice Example");
    });
}

#[test]
#[cfg(feature = "saml-experimental-bergshamra")]
fn adversarial_tampered_assertion_is_rejected() {
    with_experimental_verify(true, || {
        let idp = gen_test_idp();
        let signed = sign_response(&idp);
        let c = cfg_with_cert(&idp.cert_pem);
        // Sanity: it verifies before tampering.
        verify_response_signature(&c, &signed).expect("pre-tamper genuine");

        // Flip the signed NameID *after* signing → digest mismatch.
        let tampered = signed.replace("alice@example.com", "attacker@evil.com");
        assert_ne!(tampered, signed, "tamper must change the bytes");
        let err = verify_response_signature(&c, &tampered)
            .expect_err("tampered assertion MUST be rejected");
        assert!(
            matches!(err, AeroError::Unauthorized(_)),
            "tamper → Unauthorized, got {err:?}"
        );
    });
}

#[test]
#[cfg(feature = "saml-experimental-bergshamra")]
fn adversarial_signature_wrapping_is_rejected() {
    with_experimental_verify(true, || {
        let idp = gen_test_idp();
        let signed = sign_response(&idp);
        let c = cfg_with_cert(&idp.cert_pem);

        // Classic XSW: keep the genuinely-signed assertion as a hidden decoy
        // and inject a SECOND, unsigned assertion carrying the attacker's
        // identity that a naive consumer would read instead. We graft the
        // forged assertion in right after the Response's <Issuer>.
        let forged_assertion = r#"<saml:Assertion ID="evil-1">
    <saml:Issuer>https://idp.example/entity</saml:Issuer>
    <saml:Subject>
      <saml:NameID Format="urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress">attacker@evil.com</saml:NameID>
    </saml:Subject>
  </saml:Assertion>"#;
        let needle = "</saml:Issuer>";
        let pos = signed.find(needle).expect("response issuer") + needle.len();
        let mut wrapped = String::with_capacity(signed.len() + forged_assertion.len());
        wrapped.push_str(&signed[..pos]);
        wrapped.push_str(forged_assertion);
        wrapped.push_str(&signed[pos..]);

        // Now there are TWO <Assertion> elements; the XSW guard must refuse.
        let err = verify_response_signature(&c, &wrapped)
            .expect_err("signature-wrapping MUST be rejected");
        match err {
            AeroError::Unauthorized(m) => assert!(
                m.contains("signature-wrapping")
                    || m.contains("invalid")
                    || m.contains("duplicate")
                    || m.contains("exactly one Assertion"),
                "XSW rejection reason: {m}"
            ),
            other => panic!("XSW → Unauthorized, got {other:?}"),
        }
    });
}

#[test]
#[cfg(feature = "saml-experimental-bergshamra")]
fn adversarial_wrong_idp_certificate_is_rejected() {
    with_experimental_verify(true, || {
        let signer = gen_test_idp();
        let other = gen_test_idp(); // a different keypair/cert
        let signed = sign_response(&signer);
        // Configure the SP to trust the WRONG cert (not the signer's).
        let c = cfg_with_cert(&other.cert_pem);
        let err = verify_response_signature(&c, &signed)
            .expect_err("signature under an untrusted key MUST be rejected");
        assert!(
            matches!(err, AeroError::Unauthorized(_)),
            "wrong cert → Unauthorized, got {err:?}"
        );
    });
}

#[test]
#[cfg(feature = "saml-experimental-bergshamra")]
fn adversarial_unsigned_response_is_rejected() {
    with_experimental_verify(true, || {
        let idp = gen_test_idp();
        let c = cfg_with_cert(&idp.cert_pem);
        // SAMPLE_RESPONSE carries no <Signature> at all.
        let err = verify_response_signature(&c, SAMPLE_RESPONSE)
            .expect_err("unsigned response MUST be rejected");
        assert!(
            matches!(err, AeroError::Unauthorized(_)),
            "unsigned → Unauthorized, got {err:?}"
        );
    });
}

#[test]
#[cfg(feature = "saml-experimental-bergshamra")]
fn adversarial_default_off_rejects_even_a_genuine_signature() {
    // Belt-and-suspenders: with the switch OFF, even a perfectly genuine
    // signature is refused — opt-in really is required.
    let idp = gen_test_idp();
    let signed = with_experimental_verify(true, || sign_response(&idp));
    with_experimental_verify(false, || {
        let c = cfg_with_cert(&idp.cert_pem);
        let err = verify_response_signature(&c, &signed).unwrap_err();
        assert!(
            matches!(err, AeroError::Unauthorized(ref m) if m.contains("not yet wired")),
            "default-off must fail closed, got {err:?}"
        );
    });
}

#[test]
fn config_from_env_requires_all_five_keys() {
    let keys = [
        "AERO__SAML__SP_ENTITY_ID",
        "AERO__SAML__ACS_URL",
        "AERO__SAML__IDP_ENTITY_ID",
        "AERO__SAML__IDP_SSO_URL",
        "AERO__SAML__IDP_CERT_PEM",
    ];
    let saved: Vec<_> = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
    for k in keys {
        std::env::remove_var(k);
    }
    assert!(
        SamlConfig::from_env().is_none(),
        "unconfigured → None (SAML off by default)"
    );
    for (k, v) in saved {
        match v {
            Some(val) => std::env::set_var(k, val),
            None => std::env::remove_var(k),
        }
    }
}

#[test]
fn default_workspace_is_the_all_zero_uuid() {
    assert_eq!(DEFAULT_WORKSPACE_ID.to_uuid(), uuid::Uuid::nil());
}

#[test]
fn acs_consumes_request_before_any_jit_persistence() {
    let source = include_str!("../saml.rs");
    let acs = &source[source.find("async fn acs(").expect("ACS handler")..];
    let consume = acs
        .find("consume_request(&s.redis_client")
        .expect("one-shot request consume");
    let jit = acs
        .find("let sso = SsoRepo::new")
        .expect("transactional JIT provisioning");
    assert!(
        consume < jit,
        "an unknown, expired, or replayed request must fail before JIT can persist an account"
    );
}

#[test]
fn metadata_handler_404s_style_when_unconfigured() {
    // require_config returns the "not configured" Invalid error when env unset.
    let keys = [
        "AERO__SAML__SP_ENTITY_ID",
        "AERO__SAML__ACS_URL",
        "AERO__SAML__IDP_ENTITY_ID",
        "AERO__SAML__IDP_SSO_URL",
        "AERO__SAML__IDP_CERT_PEM",
    ];
    let saved: Vec<_> = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
    for k in keys {
        std::env::remove_var(k);
    }
    let err = require_config().unwrap_err();
    assert!(matches!(err, AeroError::Invalid(ref m) if m.contains("not configured")));
    for (k, v) in saved {
        match v {
            Some(val) => std::env::set_var(k, val),
            None => std::env::remove_var(k),
        }
    }
}
