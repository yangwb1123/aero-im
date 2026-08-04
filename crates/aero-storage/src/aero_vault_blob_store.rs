//! Aero Vault-backed [`BlobStore`](crate::BlobStore).
//!
//! Aero IM remains the attachment authorization boundary and metadata owner;
//! this adapter sends only opaque bytes to the sibling Aero Vault service. A
//! stable blob id is used as both the object key and upload idempotency key, so
//! an unknown network outcome is safe to retry. Browser/user tokens are never
//! forwarded: production deployments use a Snaplink `client_credentials`
//! grant, while a static bearer is available for local recovery and tests.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use aero_common::BlobId;
use async_trait::async_trait;
use bytes::Bytes;
use futures::TryStreamExt;
use reqwest::{Method, StatusCode, Url};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::blob_store::{BlobRange, BlobStore, BlobStoreError, BlobStream};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ATTEMPTS: u32 = 3;
const DEFAULT_PREFIX: &str = "im-attachments";
const MAX_SECRET_LEN: usize = 16 * 1024;
const MAX_TENANT_LEN: usize = 128;
const MAX_PREFIX_LEN: usize = 512;
const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;

/// Authentication used for service-to-service calls to Aero Vault.
#[derive(Clone)]
pub enum AeroVaultAuth {
    /// Recovery/development credential. Production should normally use
    /// [`Self::ClientCredentials`] so Snaplink owns expiry and revocation.
    Bearer(String),
    /// RFC 6749 client-credentials grant issued by Snaplink.
    ClientCredentials {
        token_endpoint: Url,
        client_id: String,
        client_secret: String,
        scope: Option<String>,
        resource: Option<String>,
    },
}

impl std::fmt::Debug for AeroVaultAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bearer(_) => f.write_str("Bearer(<redacted>)"),
            Self::ClientCredentials {
                token_endpoint,
                client_id,
                scope,
                resource,
                ..
            } => f
                .debug_struct("ClientCredentials")
                .field("token_endpoint", token_endpoint)
                .field("client_id", client_id)
                .field("client_secret", &"<redacted>")
                .field("scope", scope)
                .field("resource", resource)
                .finish(),
        }
    }
}

/// Connection, namespace, and machine-auth configuration.
#[derive(Clone)]
pub struct AeroVaultConfig {
    pub base_url: Url,
    pub tenant: String,
    pub prefix: String,
    pub auth: AeroVaultAuth,
}

impl std::fmt::Debug for AeroVaultConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AeroVaultConfig")
            .field("base_url", &self.base_url)
            .field("tenant", &self.tenant)
            .field("prefix", &self.prefix)
            .field("auth", &self.auth)
            .finish()
    }
}

impl AeroVaultConfig {
    /// Read the `AERO_VAULT_*` environment family. `None` means the backend is
    /// not configured; an explicitly selected backend turns that into a
    /// fail-loud startup error in the blob-store factory.
    ///
    /// # Errors
    /// Returns [`BlobStoreError::Config`] for incomplete, ambiguous, or unsafe
    /// values.
    pub fn try_from_env() -> Result<Option<Self>, BlobStoreError> {
        let Some(raw_url) = env_nonempty("AERO_VAULT_URL") else {
            return Ok(None);
        };
        let base_url = Url::parse(&raw_url)
            .map_err(|error| config_error(format!("invalid AERO_VAULT_URL: {error}")))?;
        let tenant = env_nonempty("AERO_VAULT_TENANT")
            .ok_or_else(|| config_error("AERO_VAULT_TENANT is required"))?;
        let prefix = env_nonempty("AERO_VAULT_PREFIX").unwrap_or_else(|| DEFAULT_PREFIX.into());

        let bearer = env_nonempty("AERO_VAULT_BEARER_TOKEN");
        let token_endpoint = env_nonempty("AERO_VAULT_OAUTH_TOKEN_ENDPOINT");
        let client_id = env_nonempty("AERO_VAULT_OAUTH_CLIENT_ID");
        let client_secret = env_nonempty("AERO_VAULT_OAUTH_CLIENT_SECRET");
        let scope = env_nonempty("AERO_VAULT_OAUTH_SCOPE");
        let resource = env_nonempty("AERO_VAULT_OAUTH_RESOURCE");
        let oauth_present = token_endpoint.is_some()
            || client_id.is_some()
            || client_secret.is_some()
            || scope.is_some()
            || resource.is_some();
        let auth = match (bearer, oauth_present) {
            (Some(_), true) => {
                return Err(config_error(
                    "configure either AERO_VAULT_BEARER_TOKEN or AERO_VAULT_OAUTH_*, not both",
                ))
            }
            (Some(token), false) => AeroVaultAuth::Bearer(token),
            (None, _) => AeroVaultAuth::ClientCredentials {
                token_endpoint: Url::parse(
                    token_endpoint.as_deref().ok_or_else(|| {
                        config_error("AERO_VAULT_OAUTH_TOKEN_ENDPOINT is required")
                    })?,
                )
                .map_err(|error| {
                    config_error(format!("invalid AERO_VAULT_OAUTH_TOKEN_ENDPOINT: {error}"))
                })?,
                client_id: client_id
                    .ok_or_else(|| config_error("AERO_VAULT_OAUTH_CLIENT_ID is required"))?,
                client_secret: client_secret
                    .ok_or_else(|| config_error("AERO_VAULT_OAUTH_CLIENT_SECRET is required"))?,
                scope: Some(scope.unwrap_or_else(|| "read write".into())),
                resource,
            },
        };
        let config = Self {
            base_url,
            tenant,
            prefix,
            auth,
        };
        config.validate()?;
        Ok(Some(config))
    }

    /// Validate all values that become a URL, header, or credential.
    ///
    /// # Errors
    /// Returns [`BlobStoreError::Config`] when the configuration could leak a
    /// credential, inject a header/path, or send one over cleartext off-host.
    pub fn validate(&self) -> Result<(), BlobStoreError> {
        validate_service_url("Aero Vault URL", &self.base_url)?;
        validate_header_value("Aero Vault tenant", &self.tenant, MAX_TENANT_LEN)?;
        if self.tenant == "*" {
            return Err(config_error(
                "AERO_VAULT_TENANT must be tenant-scoped, not '*'",
            ));
        }
        validate_prefix(&self.prefix)?;
        match &self.auth {
            AeroVaultAuth::Bearer(token) => {
                validate_secret("Aero Vault bearer token", token)?;
            }
            AeroVaultAuth::ClientCredentials {
                token_endpoint,
                client_id,
                client_secret,
                scope,
                resource,
            } => {
                validate_token_endpoint_url("Snaplink token endpoint", token_endpoint)?;
                validate_header_value("Snaplink client id", client_id, 512)?;
                validate_secret("Snaplink client secret", client_secret)?;
                if let Some(scope) = scope {
                    validate_form_value("Snaplink scope", scope, 2_048)?;
                }
                if let Some(resource) = resource {
                    validate_form_value("Snaplink resource", resource, 2_048)?;
                }
            }
        }
        Ok(())
    }
}

struct CachedToken {
    value: String,
    refresh_at: Instant,
}

impl std::fmt::Debug for CachedToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedToken")
            .field("value", &"<redacted>")
            .field("refresh_at", &self.refresh_at)
            .finish()
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field("access_token", &"<redacted>")
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// REST adapter for the Aero Vault `/v1/files/{key}` API.
pub struct AeroVaultBlobStore {
    cfg: AeroVaultConfig,
    client: reqwest::Client,
    cached_token: Mutex<Option<CachedToken>>,
}

impl AeroVaultBlobStore {
    /// Construct a validated client with redirects disabled and a bounded
    /// request timeout. A redirect must never carry a bearer credential to a
    /// different origin.
    ///
    /// # Errors
    /// Returns [`BlobStoreError::Config`] for invalid configuration or HTTP
    /// client initialization failure.
    pub fn try_new(cfg: AeroVaultConfig) -> Result<Self, BlobStoreError> {
        cfg.validate()?;
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| config_error(format!("build Aero Vault HTTP client: {error}")))?;
        Ok(Self {
            cfg,
            client,
            cached_token: Mutex::new(None),
        })
    }

    fn object_key(&self, id: BlobId) -> String {
        format!("{}/{id}", self.cfg.prefix)
    }

    fn object_url(&self, id: BlobId, hard_delete: bool) -> Url {
        let mut url = self.cfg.base_url.clone();
        url.set_path(&format!("/v1/files/{}", self.object_key(id)));
        if hard_delete {
            url.query_pairs_mut().append_pair("hard", "1");
        }
        url
    }

    async fn access_token(&self) -> Result<String, BlobStoreError> {
        let AeroVaultAuth::ClientCredentials {
            token_endpoint,
            client_id,
            client_secret,
            scope,
            resource,
        } = &self.cfg.auth
        else {
            if let AeroVaultAuth::Bearer(token) = &self.cfg.auth {
                return Ok(token.clone());
            }
            unreachable!();
        };

        let mut cached = self.cached_token.lock().await;
        if let Some(token) = cached.as_ref() {
            if Instant::now() < token.refresh_at {
                return Ok(token.value.clone());
            }
        }

        let mut form = vec![("grant_type", "client_credentials".to_owned())];
        if let Some(scope) = scope {
            form.push(("scope", scope.clone()));
        }
        if let Some(resource) = resource {
            form.push(("resource", resource.clone()));
        }
        let response = self
            .client
            .post(token_endpoint.clone())
            .timeout(REQUEST_TIMEOUT)
            .basic_auth(client_id, Some(client_secret))
            .form(&form)
            .send()
            .await
            .map_err(transport_error)?;
        if response.status() != StatusCode::OK {
            return Err(status_error("Snaplink token request", response.status()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_TOKEN_RESPONSE_BYTES as u64)
        {
            return Err(io_error(
                std::io::ErrorKind::InvalidData,
                "Snaplink token response exceeded the size limit",
            ));
        }
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.try_next().await.map_err(transport_error)? {
            if body.len().saturating_add(chunk.len()) > MAX_TOKEN_RESPONSE_BYTES {
                return Err(io_error(
                    std::io::ErrorKind::InvalidData,
                    "Snaplink token response exceeded the size limit",
                ));
            }
            body.extend_from_slice(&chunk);
        }
        let token: TokenResponse = serde_json::from_slice(&body).map_err(|_| {
            io_error(
                std::io::ErrorKind::InvalidData,
                "Snaplink token response was not valid JSON",
            )
        })?;
        validate_secret("Snaplink access token", &token.access_token).map_err(|_| {
            io_error(
                std::io::ErrorKind::InvalidData,
                "Snaplink returned an invalid access token",
            )
        })?;
        if token
            .token_type
            .as_deref()
            .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
        {
            return Err(io_error(
                std::io::ErrorKind::InvalidData,
                "Snaplink returned a non-Bearer token",
            ));
        }
        let ttl = token.expires_in.unwrap_or(300).clamp(1, 86_400);
        let usable = if ttl > 60 { ttl - 30 } else { (ttl / 2).max(1) };
        let result = token.access_token;
        *cached = Some(CachedToken {
            value: result.clone(),
            refresh_at: Instant::now() + Duration::from_secs(usable),
        });
        Ok(result)
    }

    async fn invalidate_token(&self, rejected: &str) -> bool {
        if !matches!(self.cfg.auth, AeroVaultAuth::ClientCredentials { .. }) {
            return false;
        }
        let mut cached = self.cached_token.lock().await;
        if cached.as_ref().is_some_and(|token| token.value == rejected) {
            *cached = None;
        }
        true
    }

    async fn send(
        &self,
        method: Method,
        url: Url,
        body: Option<Bytes>,
        range: Option<BlobRange>,
        idempotency_key: Option<String>,
    ) -> Result<reqwest::Response, BlobStoreError> {
        let mut backoff = Duration::from_millis(100);
        for attempt in 1..=MAX_ATTEMPTS {
            let token = self.access_token().await?;
            let mut request = self
                .client
                .request(method.clone(), url.clone())
                .timeout(REQUEST_TIMEOUT)
                .bearer_auth(&token)
                .header("X-Aero-Tenant", &self.cfg.tenant);
            if let Some(bytes) = &body {
                request = request
                    .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                    .header(reqwest::header::CONTENT_LENGTH, bytes.len())
                    .body(bytes.clone());
            }
            if let Some(range) = range {
                request = request.header(reqwest::header::RANGE, range.http_value());
            }
            if let Some(key) = &idempotency_key {
                request = request.header("Idempotency-Key", key);
            }

            match request.send().await {
                Ok(response) => {
                    let status = response.status();
                    if status == StatusCode::UNAUTHORIZED
                        && self.invalidate_token(&token).await
                        && attempt < MAX_ATTEMPTS
                    {
                        continue;
                    }
                    if retryable_status(status) && attempt < MAX_ATTEMPTS {
                        tokio::time::sleep(backoff).await;
                        backoff = backoff.saturating_mul(2);
                        continue;
                    }
                    return Ok(response);
                }
                Err(error) if attempt < MAX_ATTEMPTS => {
                    tracing::warn!(
                        attempt,
                        error = %error.without_url(),
                        "aero-vault request failed; retrying"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = backoff.saturating_mul(2);
                }
                Err(error) => return Err(transport_error(error)),
            }
        }
        Err(io_error(
            std::io::ErrorKind::Other,
            "Aero Vault request retries exhausted",
        ))
    }
}

#[async_trait]
impl BlobStore for AeroVaultBlobStore {
    async fn put(&self, id: BlobId, bytes: Bytes) -> Result<String, BlobStoreError> {
        let response = self
            .send(
                Method::PUT,
                self.object_url(id, false),
                Some(bytes),
                None,
                Some(format!("aero-im-blob:{id}")),
            )
            .await?;
        if response.status() != StatusCode::CREATED {
            return Err(status_error("PUT object", response.status()));
        }
        Ok(self.key_for(id))
    }

    async fn get(&self, id: BlobId) -> Result<Bytes, BlobStoreError> {
        let response = self
            .send(Method::GET, self.object_url(id, false), None, None, None)
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(BlobStoreError::NotFound);
        }
        if response.status() != StatusCode::OK {
            return Err(status_error("GET object", response.status()));
        }
        response.bytes().await.map_err(transport_error)
    }

    async fn get_stream(
        &self,
        id: BlobId,
        range: Option<BlobRange>,
    ) -> Result<BlobStream, BlobStoreError> {
        let response = self
            .send(Method::GET, self.object_url(id, false), None, range, None)
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(BlobStoreError::NotFound);
        }
        let expected = if range.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        };
        if response.status() != expected {
            return Err(status_error("stream GET object", response.status()));
        }
        Ok(Box::pin(response.bytes_stream().map_err(transport_error)))
    }

    async fn delete(&self, id: BlobId) -> Result<(), BlobStoreError> {
        let response = self
            .send(
                Method::DELETE,
                self.object_url(id, true),
                None,
                None,
                Some(format!("aero-im-blob-delete:{id}")),
            )
            .await?;
        match response.status() {
            StatusCode::NO_CONTENT | StatusCode::NOT_FOUND => Ok(()),
            status => Err(status_error("hard DELETE object", status)),
        }
    }

    fn key_for(&self, id: BlobId) -> String {
        format!("aero-vault://{}/{}", self.cfg.tenant, self.object_key(id))
    }

    async fn health_check(&self) -> Result<(), BlobStoreError> {
        let url = self
            .cfg
            .base_url
            .join("readyz")
            .map_err(|error| config_error(format!("build Aero Vault ready URL: {error}")))?;
        let response = self
            .client
            .get(url)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(transport_error)?;
        if response.status() == StatusCode::OK {
            Ok(())
        } else {
            Err(status_error("readiness probe", response.status()))
        }
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn validate_service_url(label: &str, url: &Url) -> Result<(), BlobStoreError> {
    let origin_only = url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.path() == "/";
    if !origin_only || !matches!(url.scheme(), "http" | "https") {
        return Err(config_error(format!(
            "invalid {label} (expected an HTTP(S) origin without credentials, path, query, or fragment)"
        )));
    }
    if url.scheme() == "http" {
        let host = url.host_str().unwrap_or_default();
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback());
        if !loopback {
            return Err(config_error(format!(
                "invalid {label} (cleartext HTTP is allowed only for loopback addresses)"
            )));
        }
    }
    Ok(())
}

fn validate_token_endpoint_url(label: &str, url: &Url) -> Result<(), BlobStoreError> {
    let endpoint = url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && !url.path().is_empty();
    if !endpoint || !matches!(url.scheme(), "http" | "https") {
        return Err(config_error(format!(
            "invalid {label} (expected an HTTP(S) URL without credentials, query, or fragment)"
        )));
    }
    if url.scheme() == "http" {
        let host = url.host_str().unwrap_or_default();
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback());
        if !loopback {
            return Err(config_error(format!(
                "invalid {label} (cleartext HTTP is allowed only for loopback addresses)"
            )));
        }
    }
    Ok(())
}

fn validate_prefix(prefix: &str) -> Result<(), BlobStoreError> {
    let valid = !prefix.is_empty()
        && prefix.len() <= MAX_PREFIX_LEN
        && prefix == prefix.trim_matches('/')
        && prefix.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        });
    if valid {
        Ok(())
    } else {
        Err(config_error(
            "invalid AERO_VAULT_PREFIX (use safe slash-separated ASCII path segments)",
        ))
    }
}

fn validate_header_value(label: &str, value: &str, max_len: usize) -> Result<(), BlobStoreError> {
    if value.is_empty()
        || value != value.trim()
        || value.len() > max_len
        || value.chars().any(char::is_control)
    {
        return Err(config_error(format!(
            "invalid {label} (must be trimmed, non-empty, control-free, and at most {max_len} bytes)"
        )));
    }
    Ok(())
}

fn validate_form_value(label: &str, value: &str, max_len: usize) -> Result<(), BlobStoreError> {
    validate_header_value(label, value, max_len)
}

fn validate_secret(label: &str, value: &str) -> Result<(), BlobStoreError> {
    validate_header_value(label, value, MAX_SECRET_LEN)
}

fn retryable_status(status: StatusCode) -> bool {
    status.is_server_error()
        || matches!(
            status,
            StatusCode::REQUEST_TIMEOUT
                | StatusCode::CONFLICT
                | StatusCode::TOO_EARLY
                | StatusCode::TOO_MANY_REQUESTS
        )
}

fn status_error(operation: &str, status: StatusCode) -> BlobStoreError {
    let kind = match status {
        StatusCode::BAD_REQUEST
        | StatusCode::PRECONDITION_FAILED
        | StatusCode::RANGE_NOT_SATISFIABLE => std::io::ErrorKind::InvalidInput,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => std::io::ErrorKind::PermissionDenied,
        StatusCode::REQUEST_TIMEOUT => std::io::ErrorKind::TimedOut,
        _ => std::io::ErrorKind::Other,
    };
    io_error(
        kind,
        format!("Aero Vault {operation} returned HTTP {}", status.as_u16()),
    )
}

fn transport_error(error: reqwest::Error) -> BlobStoreError {
    let kind = if error.is_timeout() {
        std::io::ErrorKind::TimedOut
    } else if error.is_connect() {
        std::io::ErrorKind::ConnectionRefused
    } else {
        std::io::ErrorKind::Other
    };
    io_error(kind, error.without_url())
}

fn io_error(kind: std::io::ErrorKind, error: impl std::fmt::Display) -> BlobStoreError {
    BlobStoreError::Io(std::io::Error::new(kind, error.to_string()))
}

fn config_error(error: impl Into<String>) -> BlobStoreError {
    BlobStoreError::Config(error.into())
}

#[cfg(test)]
mod http_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AeroVaultConfig {
        AeroVaultConfig {
            base_url: Url::parse("http://127.0.0.1:18081").unwrap(),
            tenant: "aero-im".into(),
            prefix: DEFAULT_PREFIX.into(),
            auth: AeroVaultAuth::Bearer("local-test-token".into()),
        }
    }

    #[test]
    fn validates_safe_loopback_and_rejects_cleartext_remote() {
        assert!(config().validate().is_ok());
        let mut unsafe_config = config();
        unsafe_config.base_url = Url::parse("http://vault.example.com").unwrap();
        assert!(matches!(
            unsafe_config.validate(),
            Err(BlobStoreError::Config(_))
        ));
    }

    #[test]
    fn rejects_namespace_and_header_injection() {
        let mut unsafe_config = config();
        unsafe_config.prefix = "../other-tenant".into();
        assert!(unsafe_config.validate().is_err());

        let mut unsafe_config = config();
        unsafe_config.tenant = "tenant\r\nX-Evil: yes".into();
        assert!(unsafe_config.validate().is_err());
    }

    #[test]
    fn debug_redacts_every_secret() {
        let bearer = "bearer-super-secret";
        let mut cfg = config();
        cfg.auth = AeroVaultAuth::Bearer(bearer.into());
        let output = format!("{cfg:?}");
        assert!(!output.contains(bearer));

        let client_secret = "oauth-super-secret";
        cfg.auth = AeroVaultAuth::ClientCredentials {
            token_endpoint: Url::parse("https://sso.example.com/token").unwrap(),
            client_id: "aero-im-vault".into(),
            client_secret: client_secret.into(),
            scope: Some("read write".into()),
            resource: Some("aero-vault".into()),
        };
        let output = format!("{cfg:?}");
        assert!(!output.contains(client_secret));
        assert!(output.contains("<redacted>"));

        let access_token = "access-token-super-secret";
        let cached = CachedToken {
            value: access_token.into(),
            refresh_at: Instant::now(),
        };
        let response = TokenResponse {
            access_token: access_token.into(),
            token_type: Some("Bearer".into()),
            expires_in: Some(300),
        };
        for output in [format!("{cached:?}"), format!("{response:?}")] {
            assert!(!output.contains(access_token));
            assert!(output.contains("<redacted>"));
        }
    }

    #[test]
    fn stable_storage_key_contains_no_credential() {
        let store = AeroVaultBlobStore::try_new(config()).unwrap();
        let id = BlobId::new();
        let key = store.key_for(id);
        assert_eq!(key, format!("aero-vault://aero-im/im-attachments/{id}"));
        assert!(!key.contains("local-test-token"));
    }

    #[test]
    fn retry_statuses_are_bounded_to_transient_failures() {
        assert!(retryable_status(StatusCode::CONFLICT));
        assert!(retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!retryable_status(StatusCode::FORBIDDEN));
        assert!(!retryable_status(StatusCode::RANGE_NOT_SATISFIABLE));
    }
}
