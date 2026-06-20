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
        use std::io::Read as _;
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
        let xml = r#"<saml:Assertion xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion">
          <saml:Issuer>https://idp.example/entity</saml:Issuer>
          <saml:Subject><saml:NameID>bob@example.com</saml:NameID></saml:Subject>
        </saml:Assertion>"#;
        let a = extract_assertion(xml).expect("extract");
        assert_eq!(a.name_id, "bob@example.com");
        assert_eq!(a.email().as_deref(), Some("bob@example.com"));
        // No displayName attr → falls back to email local-part.
        assert_eq!(a.best_display_name(), "bob");
    }

    #[test]
    fn extract_assertion_requires_a_nameid() {
        let xml = r#"<saml:Assertion xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion">
          <saml:Issuer>x</saml:Issuer></saml:Assertion>"#;
        let err = extract_assertion(xml).unwrap_err();
        assert!(matches!(err, AeroError::Invalid(_)), "got {err:?}");
    }

    #[test]
    fn extract_assertion_decodes_xml_entities_in_values() {
        let xml = r#"<saml:Assertion xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion">
          <saml:Subject><saml:NameID>u@x.com</saml:NameID></saml:Subject>
          <saml:Attribute Name="displayName"><saml:AttributeValue>Tom &amp; Jerry</saml:AttributeValue></saml:Attribute>
        </saml:Assertion>"#;
        let a = extract_assertion(xml).expect("extract");
        assert_eq!(a.best_display_name(), "Tom & Jerry");
    }

    // ---- THE security-critical test: signature verification is FAIL-CLOSED ----

    /// `AERO_SAML_EXPERIMENTAL_VERIFY` is a process-global env var, but the
    /// fail-closed test wants it OFF while the adversarial tests want it ON. This
    /// mutex serializes any test that touches the switch so they never race.
    static VERIFY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run `f` with the experimental-verify switch forced to `on`/`off`, restoring
    /// the prior value afterward. Holds [`VERIFY_ENV_LOCK`] for the duration.
    fn with_experimental_verify<R>(on: bool, f: impl FnOnce() -> R) -> R {
        let _guard = VERIFY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    struct TestIdp {
        cert_pem: String,
        key_pkcs8_pem: String,
    }

    /// Generate an RSA-2048 keypair and a self-signed cert over it (rcgen, RSA-
    /// SHA256). Done once per test; 2048-bit keygen is the slow part but real.
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
        let rc_key = rcgen::KeyPair::from_pkcs8_pem_and_sign_algo(
            &key_pkcs8_pem,
            &rcgen::PKCS_RSA_SHA256,
        )
        .expect("rcgen keypair");
        let params = rcgen::CertificateParams::new(vec!["idp.example".to_owned()])
            .expect("cert params");
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

    fn cfg_with_cert(cert_pem: &str) -> SamlConfig {
        let mut c = cfg();
        c.idp_cert_pem = cert_pem.to_owned();
        c
    }

    #[test]
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
                    m.contains("signature-wrapping") || m.contains("invalid") || m.contains("duplicate"),
                    "XSW rejection reason: {m}"
                ),
                other => panic!("XSW → Unauthorized, got {other:?}"),
            }
        });
    }

    #[test]
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
