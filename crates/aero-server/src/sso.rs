//! SSO via OIDC — ID-token login and browser authorization-code flow.
//!
//! `POST /api/auth/oidc` accepts an external `IdP`'s `OpenID` Connect ID token,
//! validates it ([`aero_auth::oidc::validate_id_token`]), resolves the
//! `(issuer, subject)` to an internal participant — JIT-provisioning one on first
//! sight — and returns *our own* access/refresh token pair, shaped exactly like
//! the `register`/`login` responses in [`crate::routes`].
//!
//! Browser clients use `GET /api/auth/oidc/start` and
//! `GET /callback`. The flow is confidential-client authorization
//! code with PKCE S256, state, and nonce. Short-lived `HttpOnly`/`Secure`
//! same-site cookies bind all three values to the initiating browser.
//!
//! The public `GET /api/auth/config` document also tells the bundled SPA which
//! login surface to render. `AERO__OIDC__LOGIN_PAGE` accepts `local`,
//! `snaplink`, or `both`; the default remains `both` for backwards
//! compatibility. This is presentation policy only — all credential and token
//! validation still happens server-side.
//!
//! ## Configuration & wiring
//!
//! OIDC is **off by default**. The handler reads [`OidcConfig::from_env`] per
//! request (issuer / audience / JWKS URI); when unset it fails closed. Browser
//! login additionally requires authorization endpoint, token endpoint, client
//! id/secret, and redirect URI. The signing keys come from a bounded live JWKS
//! fetch ([`JwksKeyProvider`]).
//!
//! Purely additive — registered via `pub mod sso;` in `crate::lib` and
//! `.merge(crate::sso::routes())` in [`crate::routes::build`].

use aero_auth::{JwksKeyProvider, OidcClaims, OidcConfig};
use aero_common::{Error as AeroError, WorkspaceId};
use aero_storage::{SsoRepo, SsoResolveError};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, RawQuery, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use futures::StreamExt as _;
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use url::Url;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

mod transaction;
#[cfg(test)]
use transaction::RANDOM_VALUE_LEN;
use transaction::{
    cleanup_state_from_callback, constant_time_eq, cookie_value, optional_cookie_value,
    set_unique_query_value, validate_random_value, MAX_CALLBACK_QUERY_BYTES,
};

/// The legacy / default workspace every login enrolls into — the all-zero UUID,
/// matching `crate::routes::DEFAULT_WORKSPACE_ID` (kept in sync; that const is
/// private to `routes`, so SSO declares its own copy of the same well-known id
/// established by migration `0006_workspaces.sql`).
const DEFAULT_WORKSPACE_ID: WorkspaceId = WorkspaceId(ulid::Ulid(0));
const MAX_OIDC_BODY_BYTES: usize = 64 * 1024;
const MAX_OIDC_TOKEN_BYTES: usize = 48 * 1024;
const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_AUTHORIZATION_CODE_BYTES: usize = 8 * 1024;
const MAX_ENDPOINT_BYTES: usize = 2 * 1024;
const MAX_CLIENT_ID_BYTES: usize = 512;
const MAX_CLIENT_SECRET_BYTES: usize = 4 * 1024;
const MAX_REDIRECT_LOCATION_BYTES: usize = 8 * 1024;
const OIDC_UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const OIDC_UPSTREAM_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const FLOW_COOKIE_MAX_AGE_SECS: u64 = 900;
const RANDOM_VALUE_BYTES: usize = 32;
const CALLBACK_PATH: &str = "/callback";
const FLOW_COOKIE_PREFIX: &str = "aero_oidc_flow_";
const STATE_COOKIE: &str = "aero_oidc_state";
const VERIFIER_COOKIE: &str = "aero_oidc_verifier";
const NONCE_COOKIE: &str = "aero_oidc_nonce";
const CALLBACK_CSP: &str = "default-src 'none'; script-src 'self'; base-uri 'none'; \
    form-action 'none'; frame-ancestors 'none'; connect-src 'none'; img-src 'none'";

/// Which login surface the bundled web client should expose.
///
/// `both` preserves the historical UI. `local` keeps the Aero-owned login form
/// visible (the form delegates credential verification to the Snaplink SDK) and
/// hides the hosted SSO affordance. `snaplink` shows only the hosted Snaplink
/// entry point. The value is intentionally read per request so a deployment can
/// rotate the setting with a normal process restart without adding another
/// application-state singleton.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum LoginPageMode {
    Local,
    Snaplink,
    Both,
}

impl LoginPageMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Snaplink => "snaplink",
            Self::Both => "both",
        }
    }
}

fn login_page_mode() -> LoginPageMode {
    let raw = std::env::var("AERO__OIDC__LOGIN_PAGE")
        .or_else(|_| std::env::var("AERO_LOGIN_PAGE"))
        .unwrap_or_default();
    parse_login_page_mode(&raw)
}

fn parse_login_page_mode(raw: &str) -> LoginPageMode {
    match raw.trim().to_ascii_lowercase().as_str() {
        "local" | "aero" | "self" => LoginPageMode::Local,
        "snaplink" | "hosted" => LoginPageMode::Snaplink,
        "" | "both" | "dual" => LoginPageMode::Both,
        invalid => {
            tracing::warn!(value = invalid, "invalid OIDC login-page mode; using both");
            LoginPageMode::Both
        }
    }
}

#[derive(Debug, Serialize)]
struct PublicSnaplinkConfig {
    /// Snaplink issuer/base URL used by its generated browser SDK.
    base_url: String,
    /// OAuth authorization API. Snaplink's hosted frontend may still be the
    /// first page rendered by this endpoint.
    authorization_endpoint: String,
    token_endpoint: String,
    client_id: String,
    redirect_uri: String,
    scope: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct PublicAuthConfig {
    login_page: &'static str,
    snaplink: Option<PublicSnaplinkConfig>,
}

/// One live JWKS provider per configured key endpoint. The provider owns the
/// bounded HTTP client, TTL key cache, and unknown-kid single-flight gate, so it
/// must outlive any one login request. Keying by the validated endpoint keeps
/// parallel tests and an explicitly changed process configuration isolated.
static OIDC_JWKS_PROVIDERS: OnceLock<Mutex<HashMap<String, Arc<JwksKeyProvider>>>> =
    OnceLock::new();

fn oidc_jwks_provider(cfg: &OidcConfig) -> Arc<JwksKeyProvider> {
    let providers = OIDC_JWKS_PROVIDERS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut providers = providers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(
        providers
            .entry(cfg.jwks_uri.clone())
            .or_insert_with(|| Arc::new(JwksKeyProvider::from_config(cfg))),
    )
}

/// Mount the SSO routes. Folded into the main router by [`crate::routes::build`].
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/auth/oidc",
            post(oidc_login).layer(DefaultBodyLimit::max(MAX_OIDC_BODY_BYTES)),
        )
        .route("/api/auth/config", get(auth_config))
        .route("/api/auth/oidc/start", get(oidc_start))
        .route(CALLBACK_PATH, get(oidc_callback))
}

#[derive(Deserialize)]
struct OidcLoginReq {
    /// The raw OIDC ID token (a signed JWT) issued by the external `IdP`.
    id_token: String,
}

fn public_auth_config() -> PublicAuthConfig {
    let mode = login_page_mode();
    let snaplink = (|| {
        let oidc = OidcConfig::from_env()?;
        let browser = BrowserOidcConfig::from_env().ok().flatten()?;
        let issuer = oidc.issuer.trim_end_matches('/');
        let authorization_endpoint = format!("{issuer}/auth/login");
        Some(PublicSnaplinkConfig {
            base_url: issuer.to_owned(),
            authorization_endpoint,
            token_endpoint: browser.token_endpoint.to_string(),
            client_id: browser.client_id,
            redirect_uri: browser.redirect_uri.to_string(),
            scope: vec!["openid", "profile", "email"],
        })
    })();
    PublicAuthConfig {
        login_page: mode.as_str(),
        snaplink,
    }
}

/// `GET /api/auth/config` — the non-secret login policy consumed by the
/// bundled SPA. Client secrets, JWKS URLs and signing details are deliberately
/// absent; the SPA only needs the issuer, client id and public redirect target
/// to select a page or let the Snaplink SDK start an authorization flow.
async fn auth_config() -> Json<PublicAuthConfig> {
    Json(public_auth_config())
}

#[derive(Serialize)]
struct OidcSession {
    access_token: String,
    refresh_token: String,
    participant: aero_common::Participant,
}

/// Browser-only OIDC RP configuration. The client secret is deliberately kept
/// in a type without `Debug` so an accidental structured log cannot reveal it.
#[derive(Clone)]
struct BrowserOidcConfig {
    authorization_endpoint: Url,
    token_endpoint: Url,
    client_id: String,
    client_secret: String,
    redirect_uri: Url,
}

impl BrowserOidcConfig {
    fn from_env() -> Result<Option<Self>, AeroError> {
        let authorization_endpoint = non_empty_env("AERO__OIDC__AUTHORIZATION_ENDPOINT");
        let token_endpoint = non_empty_env("AERO__OIDC__TOKEN_ENDPOINT");
        let client_id = non_empty_env("AERO__OIDC__CLIENT_ID");
        let client_secret = non_empty_env("AERO__OIDC__CLIENT_SECRET");
        let redirect_uri = non_empty_env("AERO__OIDC__REDIRECT_URI");

        let configured = [
            authorization_endpoint.is_some(),
            token_endpoint.is_some(),
            client_id.is_some(),
            client_secret.is_some(),
            redirect_uri.is_some(),
        ];
        if configured.iter().all(|present| !present) {
            return Ok(None);
        }
        if configured.iter().any(|present| !present) {
            return Err(AeroError::Invalid(
                "oidc browser flow is incompletely configured".into(),
            ));
        }

        let authorization_endpoint = authorization_endpoint.unwrap_or_default();
        let token_endpoint = token_endpoint.unwrap_or_default();
        let redirect_uri = redirect_uri.unwrap_or_default();
        Self::new(
            &authorization_endpoint,
            &token_endpoint,
            client_id.unwrap_or_default(),
            client_secret.unwrap_or_default(),
            &redirect_uri,
        )
        .map(Some)
    }

    fn new(
        authorization_endpoint: &str,
        token_endpoint: &str,
        client_id: String,
        client_secret: String,
        redirect_uri: &str,
    ) -> Result<Self, AeroError> {
        if client_id.len() > MAX_CLIENT_ID_BYTES || client_secret.len() > MAX_CLIENT_SECRET_BYTES {
            return Err(AeroError::Invalid(
                "oidc client configuration is too large".into(),
            ));
        }
        let authorization_endpoint =
            parse_https_url("authorization endpoint", authorization_endpoint)?;
        let token_endpoint = parse_https_url("token endpoint", token_endpoint)?;
        let redirect_uri = parse_https_url("redirect URI", redirect_uri)?;
        if redirect_uri.path() != CALLBACK_PATH
            || redirect_uri.query().is_some()
            || redirect_uri.fragment().is_some()
        {
            return Err(AeroError::Invalid(
                "oidc redirect URI must target the callback path".into(),
            ));
        }
        Ok(Self {
            authorization_endpoint,
            token_endpoint,
            client_id,
            client_secret,
            redirect_uri,
        })
    }
}

struct FlowConfig {
    oidc: OidcConfig,
    browser: BrowserOidcConfig,
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn parse_https_url(label: &str, value: &str) -> Result<Url, AeroError> {
    if value.len() > MAX_ENDPOINT_BYTES {
        return Err(AeroError::Invalid(format!("oidc {label} is too large")));
    }
    let url = Url::parse(value).map_err(|_| AeroError::Invalid(format!("invalid oidc {label}")))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(AeroError::Invalid(format!(
            "oidc {label} must be an HTTPS URL without credentials or fragment"
        )));
    }
    Ok(url)
}

fn require_flow_config() -> Result<FlowConfig, AeroError> {
    let oidc =
        OidcConfig::from_env().ok_or_else(|| AeroError::Invalid("oidc not configured".into()))?;
    let browser = BrowserOidcConfig::from_env()?
        .ok_or_else(|| AeroError::Invalid("oidc browser flow not configured".into()))?;
    if oidc.audience != browser.client_id {
        return Err(AeroError::Invalid(
            "oidc audience must equal browser client id".into(),
        ));
    }
    Ok(FlowConfig { oidc, browser })
}

struct BrowserFlowState {
    state: String,
    verifier: String,
    nonce: String,
}

impl BrowserFlowState {
    fn generate() -> Self {
        Self {
            state: random_urlsafe_value(),
            verifier: random_urlsafe_value(),
            nonce: random_urlsafe_value(),
        }
    }

    fn code_challenge(&self) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes()))
    }
}

fn random_urlsafe_value() -> String {
    let mut bytes = [0_u8; RANDOM_VALUE_BYTES];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn authorization_url(
    cfg: &BrowserOidcConfig,
    flow: &BrowserFlowState,
) -> Result<String, AeroError> {
    const RESERVED: [&str; 8] = [
        "response_type",
        "client_id",
        "redirect_uri",
        "scope",
        "state",
        "nonce",
        "code_challenge",
        "code_challenge_method",
    ];
    if cfg
        .authorization_endpoint
        .query_pairs()
        .any(|(name, _)| RESERVED.contains(&name.as_ref()))
    {
        return Err(AeroError::Invalid(
            "oidc authorization endpoint contains reserved parameters".into(),
        ));
    }
    let mut url = cfg.authorization_endpoint.clone();
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &cfg.client_id)
        .append_pair("redirect_uri", cfg.redirect_uri.as_str())
        .append_pair("scope", "openid profile email")
        .append_pair("state", &flow.state)
        .append_pair("nonce", &flow.nonce)
        .append_pair("code_challenge", &flow.code_challenge())
        .append_pair("code_challenge_method", "S256");
    let location = url.to_string();
    if location.len() > MAX_REDIRECT_LOCATION_BYTES {
        return Err(AeroError::Invalid(
            "oidc authorization redirect is too large".into(),
        ));
    }
    Ok(location)
}

fn flow_cookie(name: &str, value: &str) -> String {
    format!(
        "{name}={value}; Path={CALLBACK_PATH}; Max-Age={FLOW_COOKIE_MAX_AGE_SECS}; \
         HttpOnly; Secure; SameSite=Lax"
    )
}

fn scoped_flow_cookie_name(state: &str) -> String {
    format!("{FLOW_COOKIE_PREFIX}{state}")
}

fn scoped_flow_cookie_value(flow: &BrowserFlowState) -> String {
    format!("{}.{}.{}", flow.state, flow.verifier, flow.nonce)
}

fn append_flow_cookies(headers: &mut HeaderMap, flow: &BrowserFlowState) -> Result<(), AeroError> {
    // The state-keyed cookie is the authoritative transaction. It allows
    // multiple tabs to complete independently instead of having every new
    // `/start` overwrite the previous verifier and nonce.
    let scoped_name = scoped_flow_cookie_name(&flow.state);
    let scoped_value = scoped_flow_cookie_value(flow);
    for (name, value) in [
        (scoped_name.as_str(), scoped_value.as_str()),
        // Keep the fixed-name cookies during the rolling compatibility window
        // so a callback that lands on an older instance can still complete.
        (STATE_COOKIE, flow.state.as_str()),
        (VERIFIER_COOKIE, flow.verifier.as_str()),
        (NONCE_COOKIE, flow.nonce.as_str()),
    ] {
        let value = HeaderValue::from_str(&flow_cookie(name, value))
            .map_err(|_| AeroError::Internal(anyhow::anyhow!("invalid oidc flow cookie")))?;
        headers.append(header::SET_COOKIE, value);
    }
    Ok(())
}

fn append_clear_cookie(headers: &mut HeaderMap, name: &str) {
    let cookie = format!(
        "{name}=; Path={CALLBACK_PATH}; Max-Age=0; \
         Expires=Thu, 01 Jan 1970 00:00:00 GMT; HttpOnly; Secure; SameSite=Lax"
    );
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        headers.append(header::SET_COOKIE, value);
    }
}

fn append_clear_flow_cookies(
    response_headers: &mut HeaderMap,
    request_headers: &HeaderMap,
    state: Option<&str>,
) {
    if let Some(state) = state.filter(|value| validate_random_value(value, "state").is_ok()) {
        append_clear_cookie(response_headers, &scoped_flow_cookie_name(state));
        // During the rolling compatibility window, fixed-name cookies may
        // belong to a different tab. Clear them only when their state belongs
        // to this callback; the scoped transaction remains authoritative.
        let legacy_matches = optional_cookie_value(request_headers, STATE_COOKIE)
            .ok()
            .flatten()
            .is_some_and(|legacy_state| constant_time_eq(&legacy_state, state));
        if legacy_matches {
            for name in [STATE_COOKIE, VERIFIER_COOKIE, NONCE_COOKIE] {
                append_clear_cookie(response_headers, name);
            }
        }
    }
}

fn apply_sensitive_response_headers(headers: &mut HeaderMap) {
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CALLBACK_CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
}

/// `GET /api/auth/oidc/start` — create replay/CSRF/PKCE state and redirect to
/// the configured authorization UI (for Snaplink, `/login/`).
async fn oidc_start() -> ApiResult<Response> {
    let cfg = require_flow_config()?;
    let flow = BrowserFlowState::generate();
    let location = authorization_url(&cfg.browser, &flow)?;
    let mut response = Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, location)
        .body(Body::empty())
        .map_err(|e| ApiError(AeroError::Internal(anyhow::Error::new(e))))?;
    append_flow_cookies(response.headers_mut(), &flow)?;
    apply_sensitive_response_headers(response.headers_mut());
    Ok(response)
}

#[derive(Debug)]
struct CallbackQuery {
    code: String,
    state: String,
    issuer: Option<String>,
}

fn parse_callback_query(raw: Option<&str>) -> Result<CallbackQuery, AeroError> {
    let raw = raw.ok_or_else(|| AeroError::Invalid("oidc callback query missing".into()))?;
    if raw.len() > MAX_CALLBACK_QUERY_BYTES {
        return Err(AeroError::Invalid(
            "oidc callback query is too large".into(),
        ));
    }
    let mut code = None;
    let mut state = None;
    let mut issuer = None;
    let mut provider_error = false;
    for (name, value) in form_urlencoded::parse(raw.as_bytes()) {
        match name.as_ref() {
            "code" => set_unique_query_value(&mut code, value.into_owned(), "code")?,
            "state" => set_unique_query_value(&mut state, value.into_owned(), "state")?,
            "iss" => set_unique_query_value(&mut issuer, value.into_owned(), "issuer")?,
            "error" => provider_error = true,
            _ => {}
        }
    }
    if provider_error {
        return Err(AeroError::Unauthorized(
            "oidc authorization was not completed".into(),
        ));
    }
    let code = code.ok_or_else(|| AeroError::Invalid("oidc callback code missing".into()))?;
    let state = state.ok_or_else(|| AeroError::Invalid("oidc callback state missing".into()))?;
    if code.is_empty() || code.len() > MAX_AUTHORIZATION_CODE_BYTES {
        return Err(AeroError::Invalid(
            "oidc authorization code has invalid length".into(),
        ));
    }
    validate_random_value(&state, "state")?;
    if issuer
        .as_ref()
        .is_some_and(|value| value.is_empty() || value.len() > MAX_ENDPOINT_BYTES)
    {
        return Err(AeroError::Unauthorized(
            "invalid oidc authorization issuer".into(),
        ));
    }
    Ok(CallbackQuery {
        code,
        state,
        issuer,
    })
}

fn flow_from_cookies(
    headers: &HeaderMap,
    expected_state: &str,
) -> Result<BrowserFlowState, AeroError> {
    let scoped_name = scoped_flow_cookie_name(expected_state);
    if let Some(value) = optional_cookie_value(headers, &scoped_name)? {
        let mut parts = value.split('.');
        let state = parts.next().unwrap_or_default().to_owned();
        let verifier = parts.next().unwrap_or_default().to_owned();
        let nonce = parts.next().unwrap_or_default().to_owned();
        if parts.next().is_some() {
            return Err(AeroError::Unauthorized(
                "invalid oidc scoped flow cookie".into(),
            ));
        }
        validate_random_value(&state, "state")?;
        validate_random_value(&verifier, "verifier")?;
        validate_random_value(&nonce, "nonce")?;
        if !constant_time_eq(&state, expected_state) {
            return Err(AeroError::Unauthorized("oidc state mismatch".into()));
        }
        return Ok(BrowserFlowState {
            state,
            verifier,
            nonce,
        });
    }

    // Compatibility fallback for transactions started before state-keyed
    // cookies were deployed.
    let state = cookie_value(headers, STATE_COOKIE)?;
    let verifier = cookie_value(headers, VERIFIER_COOKIE)?;
    let nonce = cookie_value(headers, NONCE_COOKIE)?;
    Ok(BrowserFlowState {
        state,
        verifier,
        nonce,
    })
}

fn verify_authorization_issuer(query: &CallbackQuery, expected: &str) -> Result<(), AeroError> {
    if let Some(issuer) = query.issuer.as_deref() {
        if !constant_time_eq(issuer, expected) {
            return Err(AeroError::Unauthorized(
                "oidc authorization issuer mismatch".into(),
            ));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct TokenEndpointResponse {
    id_token: String,
}

async fn exchange_authorization_code(
    cfg: &BrowserOidcConfig,
    code: &str,
    verifier: &str,
) -> Result<String, AeroError> {
    let client = reqwest::Client::builder()
        .connect_timeout(OIDC_UPSTREAM_CONNECT_TIMEOUT)
        .timeout(OIDC_UPSTREAM_REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| {
            tracing::warn!(%error, "oidc token client construction failed");
            AeroError::Upstream("oidc token exchange unavailable".into())
        })?;
    let response = client
        .post(cfg.token_endpoint.clone())
        .basic_auth(&cfg.client_id, Some(&cfg.client_secret))
        .header(header::ACCEPT, "application/json")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", cfg.redirect_uri.as_str()),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|error| {
            tracing::warn!(%error, "oidc token request failed");
            AeroError::Upstream("oidc token exchange unavailable".into())
        })?;
    let status = response.status();
    if !status.is_success() {
        tracing::warn!(%status, "oidc token endpoint rejected authorization code");
        return Err(AeroError::Unauthorized(
            "oidc authorization code rejected".into(),
        ));
    }
    if response
        .content_length()
        .is_some_and(|len| len > MAX_TOKEN_RESPONSE_BYTES as u64)
    {
        return Err(AeroError::Upstream(
            "oidc token response is too large".into(),
        ));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            tracing::warn!(%error, "oidc token response read failed");
            AeroError::Upstream("oidc token exchange unavailable".into())
        })?;
        if body.len().saturating_add(chunk.len()) > MAX_TOKEN_RESPONSE_BYTES {
            return Err(AeroError::Upstream(
                "oidc token response is too large".into(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    let token_response: TokenEndpointResponse = serde_json::from_slice(&body).map_err(|error| {
        tracing::warn!(%error, "oidc token response was not valid JSON");
        AeroError::Upstream("invalid oidc token response".into())
    })?;
    if token_response.id_token.is_empty() || token_response.id_token.len() > MAX_OIDC_TOKEN_BYTES {
        return Err(AeroError::Upstream("invalid oidc token response".into()));
    }
    Ok(token_response.id_token)
}

fn verify_nonce(claims: &OidcClaims, expected: &str) -> Result<(), AeroError> {
    let actual = claims
        .nonce
        .as_deref()
        .ok_or_else(|| AeroError::Unauthorized("oidc token nonce missing".into()))?;
    if !constant_time_eq(actual, expected) {
        return Err(AeroError::Unauthorized("oidc token nonce mismatch".into()));
    }
    Ok(())
}

async fn complete_oidc_login(
    s: &AppState,
    headers: &HeaderMap,
    id_token: &str,
    cfg: &OidcConfig,
    expected_nonce: Option<&str>,
) -> Result<OidcSession, AeroError> {
    if id_token.len() > MAX_OIDC_TOKEN_BYTES {
        return Err(AeroError::Invalid("oidc token is too large".into()));
    }

    let keys = oidc_jwks_provider(cfg);
    let claims = aero_auth::validate_id_token(id_token, cfg, keys.as_ref())
        .await
        .map_err(|_| {
            // OIDC errors can contain attacker-selected header values such as
            // `kid`; keep request rejection logs categorical.
            tracing::warn!("oidc id token rejected");
            AeroError::Unauthorized("oidc token rejected".into())
        })?;
    if let Some(expected_nonce) = expected_nonce {
        verify_nonce(&claims, expected_nonce)?;
    }

    let sso = SsoRepo::new(s.participants.pool().clone());
    let participant_id = sso
        .resolve_or_provision_human(
            &cfg.issuer,
            &claims.sub,
            &claims.best_display_name(),
            claims.email.as_deref(),
            DEFAULT_WORKSPACE_ID,
        )
        .await
        .map_err(map_sso_resolve_error)?;

    let tokens = s.auth.issue_for_participant(participant_id)?;
    let participant = s
        .participants
        .get(participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("participant".into()))?;
    // Persist the exact stable id embedded in both JWTs before either token is
    // released. A missing row would make access-token revocation unenforceable.
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok());
    let hash = aero_storage::revoked_token::hash_token(&tokens.refresh_token);
    aero_storage::SessionRepo::new(s.participants.pool().clone())
        .record_with_id(participant_id, tokens.session_id, &hash, user_agent)
        .await
        .map_err(AeroError::from)?;
    Ok(OidcSession {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        participant,
    })
}

fn map_sso_resolve_error(error: SsoResolveError) -> AeroError {
    match error {
        SsoResolveError::InvalidIdentity => {
            AeroError::Invalid("external identity key is invalid".into())
        }
        SsoResolveError::Tombstoned => {
            AeroError::Forbidden("external identity was deprovisioned".into())
        }
        SsoResolveError::Storage(error) => AeroError::from(error),
    }
}

/// `POST /api/auth/oidc` — exchange a validated external OIDC ID token for our
/// own session tokens, JIT-provisioning a participant on first login.
///
/// Returns the same `{access_token, refresh_token, participant}` envelope as
/// `register`/`login`. Errors:
/// * `400` if OIDC is not configured, or the token is malformed/invalid.
/// * `401`/`400` collapse all token-validation failures (we never leak which
///   check failed to the client).
async fn oidc_login(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<OidcLoginReq>,
) -> ApiResult<Json<OidcSession>> {
    let cfg =
        OidcConfig::from_env().ok_or_else(|| AeroError::Invalid("oidc not configured".into()))?;
    Ok(Json(
        complete_oidc_login(&s, &headers, &req.id_token, &cfg, None).await?,
    ))
}

async fn callback_inner(
    s: &AppState,
    headers: &HeaderMap,
    raw_query: Option<&str>,
) -> Result<OidcSession, AeroError> {
    let cfg = require_flow_config()?;
    let query = parse_callback_query(raw_query)?;
    verify_authorization_issuer(&query, &cfg.oidc.issuer)?;
    let flow = flow_from_cookies(headers, &query.state)?;
    if !constant_time_eq(&query.state, &flow.state) {
        return Err(AeroError::Unauthorized("oidc state mismatch".into()));
    }
    let id_token = exchange_authorization_code(&cfg.browser, &query.code, &flow.verifier).await?;
    complete_oidc_login(s, headers, &id_token, &cfg.oidc, Some(&flow.nonce)).await
}

fn callback_success_response(session: &OidcSession) -> Result<Response, AeroError> {
    let payload = serde_json::to_string(&serde_json::json!({
        "access_token": session.access_token,
        "refresh_token": session.refresh_token,
        "participant_id": session.participant.id.to_string(),
    }))?
    .replace('&', "\\u0026")
    .replace('<', "\\u003c")
    .replace('>', "\\u003e")
    .replace('\u{2028}', "\\u2028")
    .replace('\u{2029}', "\\u2029");
    let body = format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
         <meta name=\"referrer\" content=\"no-referrer\"><meta name=\"viewport\" \
         content=\"width=device-width,initial-scale=1\"><title>正在登录 Aero IM</title>\
         </head><body><p>正在完成 Snaplink SSO 登录…</p>\
         <script id=\"oidc-session\" type=\"application/json\">{payload}</script>\
         <script src=\"/oidc_callback.js\" defer></script></body></html>"
    );
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response())
}

fn callback_failure_response(status: StatusCode) -> Response {
    let body = "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
        <meta name=\"referrer\" content=\"no-referrer\"><title>SSO 登录失败</title></head>\
        <body><p>Snaplink SSO 登录会话已失效或验证未通过。</p>\
        <p>请勿刷新或复用当前回调链接。</p>\
        <p><a href=\"/api/auth/oidc/start\">重新通过 Snaplink 登录</a></p>\
        <p><a href=\"/\">返回 Aero IM</a></p></body></html>";
    (
        status,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

/// The callback clears the matching scoped transaction on every path. Legacy
/// fixed-name cookies are cleared only when they belong to this callback, so a
/// parallel flow remains usable during a rolling upgrade.
async fn oidc_callback(
    State(s): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let cleanup_state = cleanup_state_from_callback(raw_query.as_deref());
    let mut response = match callback_inner(&s, &headers, raw_query.as_deref()).await {
        Ok(session) => callback_success_response(&session).unwrap_or_else(|error| {
            tracing::error!(%error, "oidc callback page serialization failed");
            callback_failure_response(StatusCode::INTERNAL_SERVER_ERROR)
        }),
        Err(error) => {
            let status = StatusCode::from_u16(error.status_code())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            if status.is_server_error() {
                tracing::error!(%error, "oidc browser callback failed");
            } else {
                tracing::warn!(%error, "oidc browser callback rejected");
            }
            callback_failure_response(status)
        }
    };
    append_clear_flow_cookies(response.headers_mut(), &headers, cleanup_state.as_deref());
    apply_sensitive_response_headers(response.headers_mut());
    response
}

#[cfg(test)]
mod tests;
