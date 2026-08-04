use super::*;
use std::collections::HashMap;

#[test]
fn default_workspace_is_the_all_zero_uuid() {
    // Must equal the well-known default workspace established by migration
    // 0006 (the same const `routes` uses for `register` enrollment).
    assert_eq!(DEFAULT_WORKSPACE_ID.to_uuid(), uuid::Uuid::nil());
    assert_eq!(random_urlsafe_value().len(), RANDOM_VALUE_LEN);
}

fn browser_config() -> BrowserOidcConfig {
    BrowserOidcConfig::new(
        "https://sso.ywbsd.site/login/",
        "https://sso.ywbsd.site/token",
        "aero-im".into(),
        "test-secret".into(),
        "https://im.ywbsd.site/callback",
    )
    .unwrap()
}

#[test]
fn authorization_redirect_has_state_nonce_and_pkce_s256() {
    let flow = BrowserFlowState {
        state: "A".repeat(RANDOM_VALUE_LEN),
        verifier: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into(),
        nonce: "N".repeat(RANDOM_VALUE_LEN),
    };
    // RFC 7636 Appendix B verifier/challenge test vector.
    assert_eq!(
        flow.code_challenge(),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    let url = Url::parse(&authorization_url(&browser_config(), &flow).unwrap()).unwrap();
    let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(
        url.as_str().split('?').next(),
        Some("https://sso.ywbsd.site/login/")
    );
    assert_eq!(query.get("response_type").map(String::as_str), Some("code"));
    assert_eq!(query.get("client_id").map(String::as_str), Some("aero-im"));
    assert_eq!(
        query.get("redirect_uri").map(String::as_str),
        Some("https://im.ywbsd.site/callback")
    );
    assert_eq!(query.get("state"), Some(&flow.state));
    assert_eq!(query.get("nonce"), Some(&flow.nonce));
    assert_eq!(
        query.get("code_challenge").map(String::as_str),
        Some("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM")
    );
    assert_eq!(
        query.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
}

#[test]
fn flow_cookies_are_short_lived_secure_and_http_only() {
    let flow = BrowserFlowState::generate();
    let mut headers = HeaderMap::new();
    append_flow_cookies(&mut headers, &flow).unwrap();
    let cookies: Vec<_> = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect();
    assert_eq!(cookies.len(), 4);
    assert!(cookies
        .iter()
        .any(|cookie| cookie.starts_with(&scoped_flow_cookie_name(&flow.state))));
    for cookie in cookies {
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("Secure"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Max-Age=900"));
        assert!(cookie.contains("Path=/callback"));
    }
}

#[test]
fn callback_query_rejects_duplicates_provider_errors_and_bad_state() {
    let state = "S".repeat(RANDOM_VALUE_LEN);
    let parsed = parse_callback_query(Some(&format!("code=ok&state={state}"))).unwrap();
    assert_eq!(parsed.code, "ok");
    assert_eq!(parsed.state, state);
    assert!(parsed.issuer.is_none());
    // Compatibility policy: RFC 9207's response `iss` is checked whenever
    // present; providers that do not emit it still rely on the mandatory ID
    // token issuer validation later in the callback.
    assert!(verify_authorization_issuer(&parsed, "https://sso.ywbsd.site").is_ok());
    let parsed = parse_callback_query(Some(&format!(
        "code=ok&state={state}&iss=https%3A%2F%2Fsso.ywbsd.site"
    )))
    .unwrap();
    assert_eq!(parsed.issuer.as_deref(), Some("https://sso.ywbsd.site"));
    assert!(verify_authorization_issuer(&parsed, "https://sso.ywbsd.site").is_ok());
    assert!(verify_authorization_issuer(&parsed, "https://other.example").is_err());
    assert!(parse_callback_query(Some(&format!("code=one&code=two&state={state}"))).is_err());
    assert!(parse_callback_query(Some(&format!("code=ok&state={state}&iss=one&iss=two"))).is_err());
    assert!(parse_callback_query(Some(&format!("error=access_denied&state={state}"))).is_err());
    assert!(parse_callback_query(Some("code=ok&state=short")).is_err());
}

#[test]
fn cleanup_state_survives_provider_errors_but_rejects_ambiguity() {
    let state = "S".repeat(RANDOM_VALUE_LEN);
    assert_eq!(
        cleanup_state_from_callback(Some(&format!(
            "error=access_denied&state={state}&iss=https%3A%2F%2Fsso.ywbsd.site"
        ))),
        Some(state.clone())
    );
    assert!(cleanup_state_from_callback(Some(&format!(
        "error=access_denied&state={state}&state={state}"
    )))
    .is_none());
    assert!(cleanup_state_from_callback(Some("error=access_denied&state=short")).is_none());
}

#[test]
fn cookie_reader_rejects_duplicates_and_state_compare_is_exact() {
    let state = "s".repeat(RANDOM_VALUE_LEN);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::COOKIE,
        HeaderValue::from_str(&format!("{STATE_COOKIE}={state}")).unwrap(),
    );
    assert_eq!(cookie_value(&headers, STATE_COOKIE).unwrap(), state);
    assert!(constant_time_eq(&state, &state));
    assert!(!constant_time_eq(&state, &"x".repeat(RANDOM_VALUE_LEN)));

    headers.append(
        header::COOKIE,
        HeaderValue::from_str(&format!("{STATE_COOKIE}={state}")).unwrap(),
    );
    assert!(cookie_value(&headers, STATE_COOKIE).is_err());
}

#[test]
fn state_keyed_cookies_keep_parallel_browser_flows_independent() {
    let first = BrowserFlowState {
        state: "A".repeat(RANDOM_VALUE_LEN),
        verifier: "V".repeat(RANDOM_VALUE_LEN),
        nonce: "N".repeat(RANDOM_VALUE_LEN),
    };
    let second = BrowserFlowState {
        state: "B".repeat(RANDOM_VALUE_LEN),
        verifier: "W".repeat(RANDOM_VALUE_LEN),
        nonce: "M".repeat(RANDOM_VALUE_LEN),
    };
    let raw = format!(
        "{}={}; {}={}; {STATE_COOKIE}={}; {VERIFIER_COOKIE}={}; {NONCE_COOKIE}={}",
        scoped_flow_cookie_name(&first.state),
        scoped_flow_cookie_value(&first),
        scoped_flow_cookie_name(&second.state),
        scoped_flow_cookie_value(&second),
        second.state,
        second.verifier,
        second.nonce,
    );
    let mut headers = HeaderMap::new();
    headers.insert(header::COOKIE, HeaderValue::from_str(&raw).unwrap());

    let recovered_first = flow_from_cookies(&headers, &first.state).unwrap();
    assert_eq!(recovered_first.state, first.state);
    assert_eq!(recovered_first.verifier, first.verifier);
    assert_eq!(recovered_first.nonce, first.nonce);

    let recovered_second = flow_from_cookies(&headers, &second.state).unwrap();
    assert_eq!(recovered_second.state, second.state);
    assert_eq!(recovered_second.verifier, second.verifier);
    assert_eq!(recovered_second.nonce, second.nonce);
}

#[test]
fn legacy_flow_cookies_remain_valid_during_rollout() {
    let flow = BrowserFlowState::generate();
    let raw = format!(
        "{STATE_COOKIE}={}; {VERIFIER_COOKIE}={}; {NONCE_COOKIE}={}",
        flow.state, flow.verifier, flow.nonce
    );
    let mut headers = HeaderMap::new();
    headers.insert(header::COOKIE, HeaderValue::from_str(&raw).unwrap());
    let recovered = flow_from_cookies(&headers, &flow.state).unwrap();
    assert_eq!(recovered.state, flow.state);
    assert_eq!(recovered.verifier, flow.verifier);
    assert_eq!(recovered.nonce, flow.nonce);
}

#[test]
fn callback_clears_only_the_matching_scoped_flow_plus_legacy_cookies() {
    let state = "S".repeat(RANDOM_VALUE_LEN);
    let mut request_headers = HeaderMap::new();
    request_headers.insert(
        header::COOKIE,
        HeaderValue::from_str(&format!("{STATE_COOKIE}={state}")).unwrap(),
    );
    let mut response_headers = HeaderMap::new();
    append_clear_flow_cookies(&mut response_headers, &request_headers, Some(&state));
    let cookies: Vec<_> = response_headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect();
    assert_eq!(cookies.len(), 4);
    assert!(cookies
        .iter()
        .any(|cookie| cookie.starts_with(&scoped_flow_cookie_name(&state))));
    assert!(cookies.iter().all(|cookie| cookie.contains("Max-Age=0")));
}

#[test]
fn callback_does_not_clear_another_tabs_legacy_fallback() {
    let first_state = "A".repeat(RANDOM_VALUE_LEN);
    let second_state = "B".repeat(RANDOM_VALUE_LEN);
    let mut request_headers = HeaderMap::new();
    request_headers.insert(
        header::COOKIE,
        HeaderValue::from_str(&format!("{STATE_COOKIE}={second_state}")).unwrap(),
    );
    let mut response_headers = HeaderMap::new();
    append_clear_flow_cookies(&mut response_headers, &request_headers, Some(&first_state));
    let cookies: Vec<_> = response_headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect();
    assert_eq!(cookies.len(), 1);
    assert!(cookies[0].starts_with(&scoped_flow_cookie_name(&first_state)));
    assert!(!cookies.iter().any(|cookie| {
        cookie.starts_with(STATE_COOKIE)
            || cookie.starts_with(VERIFIER_COOKIE)
            || cookie.starts_with(NONCE_COOKIE)
    }));
}

#[tokio::test]
async fn callback_failure_page_offers_a_fresh_safe_login() {
    let response = callback_failure_response(StatusCode::UNAUTHORIZED);
    let body = axum::body::to_bytes(response.into_body(), 16 * 1024)
        .await
        .unwrap();
    let body = std::str::from_utf8(&body).unwrap();
    assert!(body.contains("请勿刷新或复用当前回调链接"));
    assert!(body.contains("href=\"/api/auth/oidc/start\""));
    assert!(!body.contains("code="));
    assert!(!body.contains("state="));
}

#[test]
fn callback_nonce_is_mandatory_and_exact() {
    let mut claims = OidcClaims {
        sub: "snaplink-user".into(),
        email: None,
        name: None,
        email_verified: None,
        preferred_username: None,
        nonce: None,
        iat: None,
    };
    assert!(verify_nonce(&claims, "expected").is_err());
    claims.nonce = Some("expected".into());
    assert!(verify_nonce(&claims, "expected").is_ok());
    assert!(verify_nonce(&claims, "different").is_err());
}

#[test]
fn jwks_provider_is_reused_per_endpoint_and_isolates_config_changes() {
    let first_config = OidcConfig {
        issuer: "https://issuer-one.example".into(),
        audience: "client-one".into(),
        jwks_uri: "https://issuer-one.example/jwks".into(),
    };
    let same_endpoint = OidcConfig {
        issuer: "https://issuer-two.example".into(),
        audience: "client-two".into(),
        jwks_uri: first_config.jwks_uri.clone(),
    };
    let changed_endpoint = OidcConfig {
        issuer: "https://issuer-three.example".into(),
        audience: "client-three".into(),
        jwks_uri: "https://issuer-three.example/jwks".into(),
    };

    let first = oidc_jwks_provider(&first_config);
    let same = oidc_jwks_provider(&same_endpoint);
    let changed = oidc_jwks_provider(&changed_endpoint);
    assert!(Arc::ptr_eq(&first, &same));
    assert!(!Arc::ptr_eq(&first, &changed));
}

#[test]
fn browser_config_rejects_http_and_wrong_callback_path() {
    assert!(BrowserOidcConfig::new(
        "http://sso.ywbsd.site/login/",
        "https://sso.ywbsd.site/token",
        "aero-im".into(),
        "secret".into(),
        "https://im.ywbsd.site/callback",
    )
    .is_err());
    assert!(BrowserOidcConfig::new(
        "https://sso.ywbsd.site/login/",
        "https://sso.ywbsd.site/token",
        "aero-im".into(),
        "secret".into(),
        "https://im.ywbsd.site/wrong",
    )
    .is_err());
}

#[test]
fn login_page_mode_accepts_documented_aliases_and_fails_safe() {
    assert_eq!(parse_login_page_mode("local"), LoginPageMode::Local);
    assert_eq!(parse_login_page_mode(" aero "), LoginPageMode::Local);
    assert_eq!(parse_login_page_mode("hosted"), LoginPageMode::Snaplink);
    assert_eq!(parse_login_page_mode("SNAPLINK"), LoginPageMode::Snaplink);
    assert_eq!(parse_login_page_mode(""), LoginPageMode::Both);
    assert_eq!(parse_login_page_mode("dual"), LoginPageMode::Both);
    assert_eq!(parse_login_page_mode("unexpected"), LoginPageMode::Both);
}

#[test]
fn public_auth_config_never_contains_a_client_secret() {
    let json = serde_json::to_string(&PublicAuthConfig {
        login_page: LoginPageMode::Both.as_str(),
        snaplink: Some(PublicSnaplinkConfig {
            base_url: "https://sso.example".into(),
            authorization_endpoint: "https://sso.example/auth/login".into(),
            token_endpoint: "https://sso.example/token".into(),
            client_id: "im-demo".into(),
            redirect_uri: "https://im.example/callback".into(),
            scope: vec!["openid", "profile", "email"],
        }),
    })
    .unwrap();
    assert!(json.contains("client_id"));
    assert!(!json.contains("client_secret"));
    assert!(!json.contains("jwks"));
}
