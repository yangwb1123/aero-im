//! S3-compatible [`BlobStore`] backend (AWS S3 / `MinIO` / any S3 API).
//!
//! Sibling of [`crate::blob_store::LocalFsBlobStore`]: same [`BlobStore`] trait,
//! but the bytes live in an object store instead of the local filesystem. Object
//! keys are `blobs/{blob_id}`; [`BlobStore::key_for`] returns an `s3://` URI.
//!
//! ## Signing
//!
//! The repo deliberately does **not** pull `aws-sdk-s3` (it drags in a large
//! dependency tree); instead AWS **Signature V4** (`SigV4`) is hand-rolled over
//! [`reqwest`], mirroring the HMAC-SHA256 signing the webhook module already
//! does. The signing chain (canonical request → string-to-sign → signing key →
//! signature) lives in pure functions ([`canonical_request`], [`string_to_sign`],
//! [`signing_key`], [`sign_request`]) so it can be unit-tested offline against
//! AWS's published test vectors — real S3 is unreachable in CI.
//!
//! ## Backend switch
//!
//! [`blob_store_from_env`] picks the backend at startup: `S3BlobStore` when
//! `AERO_BLOB_BACKEND=s3` and the S3 env config is present, else the local-fs
//! store. Purely additive — no existing store is touched.

use std::path::PathBuf;
use std::sync::Arc;

use aero_common::BlobId;
use async_trait::async_trait;
use bytes::Bytes;
use futures::TryStreamExt;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::aero_vault_blob_store::{AeroVaultBlobStore, AeroVaultConfig};
use crate::blob_store::{BlobRange, BlobStore, BlobStoreError, BlobStream, LocalFsBlobStore};

type HmacSha256 = Hmac<Sha256>;

/// `SigV4` service name for S3.
const SERVICE: &str = "s3";
/// `SigV4` terminator string appended to the credential scope.
const AWS4_REQUEST: &str = "aws4_request";
/// `SigV4` signing algorithm identifier.
const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const SSE_KMS_ALGORITHM: &str = "aws:kms";
const SSE_HEADER: &str = "x-amz-server-side-encryption";
const SSE_KMS_KEY_ID_HEADER: &str = "x-amz-server-side-encryption-aws-kms-key-id";
/// KMS ARNs are normally far shorter; this bound also keeps canonical requests
/// and HTTP headers predictably small.
const MAX_KMS_KEY_ID_LEN: usize = 2_048;
const MAX_ACCESS_KEY_LEN: usize = 256;
const MAX_SECRET_KEY_LEN: usize = 2_048;
const MAX_REGION_LEN: usize = 64;

// ----------------------------------------------------------------- Config

/// Connection + credential config for an S3-compatible endpoint.
///
/// `endpoint` is `None` for real AWS (the host is derived from `bucket`/`region`)
/// and `Some("http://minio:9000")` for `MinIO` or any custom S3 API.
#[derive(Clone)]
pub struct S3Config {
    pub bucket: String,
    pub region: String,
    pub endpoint: Option<String>,
    pub access_key: String,
    pub secret_key: String,
    /// Optional AWS KMS key id, key ARN, alias name, or alias ARN. When set,
    /// uploads request SSE-KMS and both encryption headers are signed.
    pub kms_key_id: Option<String>,
}

/// Manual `Debug` that **redacts the S3 secret key** (ROADMAP 方向三). The
/// derived impl would print `secret_key` verbatim; redacting it keeps the access
/// key id (an identifier) visible for diagnostics while a stray `?cfg` log can
/// never leak the secret.
impl std::fmt::Debug for S3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Config")
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("access_key", &"<redacted>")
            .field("secret_key", &"<redacted>")
            .field(
                "kms_key_id",
                &self.kms_key_id.as_ref().map(|_| "<configured>"),
            )
            .finish()
    }
}

impl S3Config {
    /// Read config from `AERO_S3_*` env vars. Returns `None` when
    /// `AERO_S3_BUCKET` is unset (the store is simply not configured). Region
    /// defaults to `us-east-1`. A writable blob backend requires non-empty
    /// access and secret keys; missing credentials are rejected by
    /// [`Self::validate`] instead of failing on the first upload.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        Self::try_from_env().ok().flatten()
    }

    /// Read and validate config from `AERO_S3_*` environment variables.
    ///
    /// Unlike [`Self::from_env`], this preserves a validation error so the
    /// production boot path can fail closed instead of falling back to local
    /// storage when an explicitly configured KMS key is unsafe.
    ///
    /// # Errors
    /// Returns [`BlobStoreError::Config`] for an invalid
    /// `AERO_S3_KMS_KEY_ID`.
    pub fn try_from_env() -> Result<Option<Self>, BlobStoreError> {
        let Some(bucket) = std::env::var("AERO_S3_BUCKET").ok() else {
            return Ok(None);
        };
        if bucket.is_empty() {
            return Ok(None);
        }
        let region = std::env::var("AERO_S3_REGION").unwrap_or_else(|_| "us-east-1".to_string());
        let endpoint = std::env::var("AERO_S3_ENDPOINT")
            .ok()
            .filter(|s| !s.is_empty());
        let access_key = std::env::var("AERO_S3_ACCESS_KEY").unwrap_or_default();
        let secret_key = std::env::var("AERO_S3_SECRET_KEY").unwrap_or_default();
        let kms_key_id = std::env::var("AERO_S3_KMS_KEY_ID").ok();
        let cfg = Self {
            bucket,
            region,
            endpoint,
            access_key,
            secret_key,
            kms_key_id,
        };
        cfg.validate()?;
        Ok(Some(cfg))
    }

    /// Validate the complete writable S3 configuration.
    ///
    /// # Errors
    /// Returns [`BlobStoreError::Config`] when required addressing/credential
    /// values are missing or unsafe, a custom endpoint is not a plain HTTP(S)
    /// origin, or the optional KMS key id is invalid.
    pub fn validate(&self) -> Result<(), BlobStoreError> {
        let valid_bucket = (3..=63).contains(&self.bucket.len())
            && self.bucket == self.bucket.trim()
            && self.bucket.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
            })
            && self
                .bucket
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && self
                .bucket
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
            && !self.bucket.contains("..");
        if !valid_bucket {
            return Err(BlobStoreError::Config(
                "invalid S3 bucket (must be a trimmed 3..=63 byte DNS-style name)".into(),
            ));
        }

        let valid_region = !self.region.is_empty()
            && self.region.len() <= MAX_REGION_LEN
            && self.region == self.region.trim()
            && self
                .region
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && self
                .region
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && self
                .region
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric);
        if !valid_region {
            return Err(BlobStoreError::Config(format!(
                "invalid S3 region (must be trimmed, 1..={MAX_REGION_LEN} ASCII letters/digits/hyphens)"
            )));
        }

        validate_credential("access key", &self.access_key, MAX_ACCESS_KEY_LEN)?;
        validate_credential("secret key", &self.secret_key, MAX_SECRET_KEY_LEN)?;

        if let Some(endpoint) = &self.endpoint {
            if endpoint != endpoint.trim() {
                return Err(BlobStoreError::Config(
                    "invalid S3 endpoint (must be trimmed)".into(),
                ));
            }
            let parsed = reqwest::Url::parse(endpoint).map_err(|error| {
                BlobStoreError::Config(format!("invalid S3 endpoint URL: {error}"))
            })?;
            if !matches!(parsed.scheme(), "http" | "https")
                || parsed.host_str().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.query().is_some()
                || parsed.fragment().is_some()
                || parsed.path() != "/"
            {
                return Err(BlobStoreError::Config(
                    "invalid S3 endpoint (expected an HTTP(S) origin without credentials, path, query, or fragment)"
                        .into(),
                ));
            }
        }

        let Some(key_id) = &self.kms_key_id else {
            return Ok(());
        };
        if key_id.is_empty()
            || key_id != key_id.trim()
            || key_id.len() > MAX_KMS_KEY_ID_LEN
            || !key_id.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'/' | b'_' | b'-')
            })
        {
            return Err(BlobStoreError::Config(format!(
                "invalid S3 KMS key id (must be trimmed, 1..={MAX_KMS_KEY_ID_LEN} ASCII bytes, \
                 using only letters, digits, ':', '/', '_', or '-')"
            )));
        }
        Ok(())
    }
}

fn validate_credential(label: &str, value: &str, max_len: usize) -> Result<(), BlobStoreError> {
    if value.is_empty()
        || value != value.trim()
        || value.len() > max_len
        || value.chars().any(char::is_control)
    {
        return Err(BlobStoreError::Config(format!(
            "invalid S3 {label} (must be trimmed, non-empty, control-free, and at most {max_len} bytes)"
        )));
    }
    Ok(())
}

// ------------------------------------------------------------------ Store

/// S3-compatible [`BlobStore`]. Each method issues one signed HTTP request via
/// [`reqwest`]: `put` → PUT, `get` → GET, `delete` → DELETE (a 404 on delete is
/// treated as success, matching the local store's idempotent semantics).
pub struct S3BlobStore {
    cfg: S3Config,
    client: reqwest::Client,
}

impl S3BlobStore {
    /// Build a store with a sane per-request timeout so a slow endpoint can't
    /// stall a caller.
    #[must_use]
    pub fn new(cfg: S3Config) -> Self {
        Self::try_new(cfg).expect("invalid S3 configuration")
    }

    /// Build a store after validating all values that become signed headers.
    ///
    /// # Errors
    /// Returns [`BlobStoreError::Config`] when the S3/KMS configuration is
    /// unsafe.
    pub fn try_new(cfg: S3Config) -> Result<Self, BlobStoreError> {
        cfg.validate()?;
        let client = reqwest::Client::builder()
            .timeout(S3_REQUEST_TIMEOUT)
            .build()
            .unwrap_or_else(|e| {
                // Building with the configured timeout failed (e.g. TLS backend
                // init). Don't silently drop the timeout — log it. Each request in
                // `send` also sets its own per-request timeout, so a hung endpoint
                // still cannot pin a call even on this fallback client.
                tracing::warn!(error = %e, "s3: client builder failed; using default client (per-request timeout still applies)");
                reqwest::Client::new()
            });
        Ok(Self { cfg, client })
    }

    /// Object key for a blob: `blobs/{id}`.
    fn object_key(id: BlobId) -> String {
        format!("blobs/{id}")
    }

    /// Virtual-host-style host for this bucket. For real AWS this is
    /// `{bucket}.s3.{region}.amazonaws.com`; for a custom endpoint it is the
    /// endpoint's host (path-style addressing — see [`Self::request_url`]).
    fn host(&self) -> String {
        match &self.cfg.endpoint {
            Some(ep) => host_of(ep).to_string(),
            None => format!("{}.s3.{}.amazonaws.com", self.cfg.bucket, self.cfg.region),
        }
    }

    /// Full request URL + the canonical URI path used for signing.
    ///
    /// Real AWS uses virtual-host addressing (`https://{bucket}.s3...`/`/{key}`);
    /// a custom endpoint (`MinIO`) uses path-style (`{endpoint}/{bucket}/{key}`),
    /// where the bucket is part of the canonical path.
    fn request_url(&self, object_key: &str) -> (String, String) {
        if let Some(ep) = &self.cfg.endpoint {
            let base = ep.trim_end_matches('/');
            let canonical_path = format!("/{}/{}", self.cfg.bucket, object_key);
            (format!("{base}{canonical_path}"), canonical_path)
        } else {
            let canonical_path = format!("/{object_key}");
            (
                format!("https://{}{}", self.host(), canonical_path),
                canonical_path,
            )
        }
    }

    /// Sign + issue one request. `payload` is the body for PUT (empty for
    /// GET/DELETE). Returns the response on a 2xx; maps a 404 to
    /// [`BlobStoreError::NotFound`] and any other status / transport error to
    /// [`BlobStoreError::Io`].
    async fn send(
        &self,
        method: &str,
        object_key: &str,
        payload: &[u8],
    ) -> Result<reqwest::Response, BlobStoreError> {
        self.send_with_range(method, object_key, payload, None)
            .await
    }

    /// Variant of [`Self::send`] that forwards a resolved HTTP byte range to
    /// S3. `Range` does not need to be part of `SignedHeaders`; S3 permits
    /// ordinary request headers that are not `x-amz-*` to remain unsigned.
    async fn send_with_range(
        &self,
        method: &str,
        object_key: &str,
        payload: &[u8],
        range: Option<BlobRange>,
    ) -> Result<reqwest::Response, BlobStoreError> {
        let (url, canonical_path) = self.request_url(object_key);
        let http_method: reqwest::Method = method.parse().map_err(io_other)?;
        // Bounded retry with exponential backoff for transient S3 failures
        // (network/timeout errors and 5xx/429). PUT (to a fixed object key),
        // GET and DELETE are all idempotent, so a retry can never duplicate an
        // effect. ROADMAP 方向三.
        let mut backoff = std::time::Duration::from_millis(100);
        let mut last_err: Option<BlobStoreError> = None;
        for attempt in 1..=S3_MAX_ATTEMPTS {
            // Re-sign per attempt so `amz_date` is fresh — a delayed retry with a
            // stale timestamp would be rejected for clock skew.
            let now = OffsetDateTime::now_utc();
            let signed = sign_request(
                &self.cfg,
                method,
                &self.host(),
                &canonical_path,
                payload,
                now,
            );
            let mut req = self
                .client
                .request(http_method.clone(), &url)
                .timeout(S3_REQUEST_TIMEOUT)
                .header("host", &signed.host)
                .header("x-amz-date", &signed.amz_date)
                .header("x-amz-content-sha256", &signed.payload_hash)
                .header("authorization", &signed.authorization);
            if let Some(kms_key_id) = &signed.kms_key_id {
                req = req
                    .header(SSE_HEADER, SSE_KMS_ALGORITHM)
                    .header(SSE_KMS_KEY_ID_HEADER, kms_key_id);
            }
            if method == "PUT" {
                req = req.body(payload.to_vec());
            }
            if let Some(range) = range {
                req = req.header(reqwest::header::RANGE, range.http_value());
            }

            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status == reqwest::StatusCode::NOT_FOUND {
                        return Err(BlobStoreError::NotFound);
                    }
                    if range.is_some() && status != reqwest::StatusCode::PARTIAL_CONTENT {
                        return Err(io_other(format!(
                            "s3: range request returned unexpected status {status}"
                        )));
                    }
                    if status.is_success() {
                        return Ok(resp);
                    }
                    if is_retryable_status(status) && attempt < S3_MAX_ATTEMPTS {
                        last_err = Some(io_other(format!("s3: transient status {status}")));
                    } else {
                        return Err(io_other(format!("s3: unexpected status {status}")));
                    }
                }
                Err(e) => {
                    // Network / timeout — transient; retry until attempts run out.
                    if attempt < S3_MAX_ATTEMPTS {
                        last_err = Some(io_other(&e));
                    } else {
                        return Err(io_other(e));
                    }
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = backoff.saturating_mul(2);
        }
        Err(last_err.unwrap_or_else(|| io_other("s3: retries exhausted")))
    }
}

#[async_trait]
impl BlobStore for S3BlobStore {
    async fn put(&self, id: BlobId, bytes: Bytes) -> Result<String, BlobStoreError> {
        let key = Self::object_key(id);
        self.send("PUT", &key, &bytes).await?;
        Ok(self.key_for(id))
    }

    async fn get(&self, id: BlobId) -> Result<Bytes, BlobStoreError> {
        let key = Self::object_key(id);
        let resp = self.send("GET", &key, &[]).await?;
        let body = resp.bytes().await.map_err(io_other)?;
        Ok(body)
    }

    async fn get_stream(
        &self,
        id: BlobId,
        range: Option<BlobRange>,
    ) -> Result<BlobStream, BlobStoreError> {
        let key = Self::object_key(id);
        let response = self.send_with_range("GET", &key, &[], range).await?;
        let stream = response.bytes_stream().map_err(io_other);
        Ok(Box::pin(stream))
    }

    async fn delete(&self, id: BlobId) -> Result<(), BlobStoreError> {
        let key = Self::object_key(id);
        match self.send("DELETE", &key, &[]).await {
            Ok(_) | Err(BlobStoreError::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn key_for(&self, id: BlobId) -> String {
        format!("s3://{}/{}", self.cfg.bucket, Self::object_key(id))
    }

    /// Reachability probe for readiness (ROADMAP 方向三): a GET on a sentinel key
    /// that will not exist. `NotFound` means the bucket is reachable and the
    /// credentials are valid — healthy. Any other error (network / 5xx / 403)
    /// means the backend is unreachable or misconfigured, so the pod should leave
    /// rotation. The caller (readiness probe) bounds this with its own timeout.
    async fn health_check(&self) -> Result<(), BlobStoreError> {
        match self.send("GET", "blobs/.healthcheck", &[]).await {
            Ok(_) | Err(BlobStoreError::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

// ---------------------------------------------------------- Backend switch

/// Pick the blob backend at startup. Returns an [`S3BlobStore`] when
/// `AERO_BLOB_BACKEND=s3` **and** [`S3Config::from_env`] is `Some`; otherwise a
/// [`LocalFsBlobStore`] rooted at `local_dir`.
///
/// # Panics
///
/// Panics if the local directory can't be created or an explicitly supplied
/// KMS key id is unsafe. Production boot uses
/// [`blob_store_from_env_checked`] to return those configuration failures.
// `local_dir` is owned per the documented API shape (it mirrors how the bin
// hands its blob_dir straight to this factory).
#[allow(clippy::needless_pass_by_value)]
#[must_use]
pub fn blob_store_from_env(local_dir: PathBuf) -> Arc<dyn BlobStore> {
    blob_store_from_env_checked(local_dir)
        .unwrap_or_else(|error| panic!("invalid blob-store configuration: {error}"))
        .0
}

/// Fail-loud variant of [`blob_store_from_env`] (ROADMAP 第三版 方向五).
///
/// Returns the store **and the active backend label** (`"s3"` / `"aero-vault"`
/// / `"local"`). The
/// critical difference: when `AERO_BLOB_BACKEND=s3` is set but the S3 config is
/// incomplete, this returns `Err` instead of silently falling back to local disk
/// — a misconfigured cluster that thinks it's on S3 but is actually writing to a
/// node's local filesystem (attachments unreachable across nodes, lost on
/// restart) is a silent data-correctness hazard. The bin propagates the `Err` to
/// abort startup, and the label is surfaced on `/health/ready`.
///
/// # Errors
/// [`BlobStoreError::Config`] when an explicitly selected remote backend is
/// incomplete/unsafe, the backend name is unknown, or the local directory
/// cannot be created.
#[allow(clippy::needless_pass_by_value)]
pub fn blob_store_from_env_checked(
    local_dir: PathBuf,
) -> Result<(Arc<dyn BlobStore>, &'static str), BlobStoreError> {
    let backend = std::env::var("AERO_BLOB_BACKEND")
        .unwrap_or_else(|_| "local".into())
        .trim()
        .to_ascii_lowercase();
    match backend.as_str() {
        "s3" => {
            let cfg = S3Config::try_from_env()?.ok_or_else(|| {
                BlobStoreError::Config(
                    "AERO_BLOB_BACKEND=s3 but S3 config is incomplete (set AERO_S3_BUCKET / \
                     AERO_S3_REGION / AERO_S3_ACCESS_KEY / AERO_S3_SECRET_KEY); refusing to \
                     silently fall back to local-disk storage"
                        .into(),
                )
            })?;
            return Ok((Arc::new(S3BlobStore::try_new(cfg)?), "s3"));
        }
        "vault" | "aero-vault" => {
            let cfg = AeroVaultConfig::try_from_env()?.ok_or_else(|| {
                BlobStoreError::Config(
                    "AERO_BLOB_BACKEND=vault but Aero Vault config is incomplete (set \
                     AERO_VAULT_URL / AERO_VAULT_TENANT and either AERO_VAULT_BEARER_TOKEN \
                     or AERO_VAULT_OAUTH_*); refusing to silently fall back to local disk"
                        .into(),
                )
            })?;
            return Ok((Arc::new(AeroVaultBlobStore::try_new(cfg)?), "aero-vault"));
        }
        "" | "local" => {}
        unknown => {
            return Err(BlobStoreError::Config(format!(
                "unknown AERO_BLOB_BACKEND '{unknown}' (expected local, s3, or vault)"
            )))
        }
    }
    let local = LocalFsBlobStore::new(&local_dir)
        .map_err(|e| BlobStoreError::Config(format!("create local blob dir: {e}")))?;
    Ok((Arc::new(local), "local"))
}

// ----------------------------------------------------------- SigV4 (pure)

/// The signed-request material produced by [`sign_request`]: the headers an HTTP
/// client must attach to satisfy AWS `SigV4`.
#[derive(Debug, Clone)]
struct SignedRequest {
    host: String,
    amz_date: String,
    payload_hash: String,
    authorization: String,
    kms_key_id: Option<String>,
}

/// Lowercase hex SHA-256 of `payload`.
fn sha256_hex(payload: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(payload);
    hex::encode(h.finalize())
}

/// HMAC-SHA256 of `data` under `key`.
fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Build the `SigV4` **canonical request** for GET/PUT/DELETE with no query
/// string. SSE-KMS headers are included in canonical order only for encrypted
/// PUTs.
fn canonical_request(
    method: &str,
    canonical_uri: &str,
    host: &str,
    amz_date: &str,
    payload_hash: &str,
    kms_key_id: Option<&str>,
) -> String {
    let mut canonical_headers =
        format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n");
    if let Some(key_id) = kms_key_id {
        canonical_headers.push_str(SSE_HEADER);
        canonical_headers.push(':');
        canonical_headers.push_str(SSE_KMS_ALGORITHM);
        canonical_headers.push('\n');
        canonical_headers.push_str(SSE_KMS_KEY_ID_HEADER);
        canonical_headers.push(':');
        canonical_headers.push_str(key_id);
        canonical_headers.push('\n');
    }
    let signed_headers = signed_headers(kms_key_id.is_some());
    format!("{method}\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}")
}

fn signed_headers(with_kms: bool) -> &'static str {
    if with_kms {
        "host;x-amz-content-sha256;x-amz-date;x-amz-server-side-encryption;\
         x-amz-server-side-encryption-aws-kms-key-id"
    } else {
        "host;x-amz-content-sha256;x-amz-date"
    }
}

/// Build the `SigV4` **string-to-sign** from a canonical-request hash.
fn string_to_sign(amz_date: &str, scope: &str, canonical_request_hash: &str) -> String {
    format!("{ALGORITHM}\n{amz_date}\n{scope}\n{canonical_request_hash}")
}

/// Derive the `SigV4` **signing key** for a `(date, region, service)` tuple.
fn signing_key(secret: &str, date_stamp: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, AWS4_REQUEST.as_bytes())
}

/// Format an [`OffsetDateTime`] as the `SigV4` `amz_date` (`YYYYMMDDTHHMMSSZ`).
fn amz_date(ts: OffsetDateTime) -> String {
    let fmt = time::macros::format_description!("[year][month][day]T[hour][minute][second]Z");
    ts.format(&fmt).expect("amz-date format is infallible")
}

/// Format an [`OffsetDateTime`] as the `SigV4` `date_stamp` (`YYYYMMDD`).
fn date_stamp(ts: OffsetDateTime) -> String {
    let fmt = time::macros::format_description!("[year][month][day]");
    ts.format(&fmt).expect("date-stamp format is infallible")
}

/// Produce the full set of `SigV4` signing material for one request. Pure and
/// deterministic for a fixed `now`, which is what makes the AWS test vectors
/// assertable.
fn sign_request(
    cfg: &S3Config,
    method: &str,
    host: &str,
    canonical_uri: &str,
    payload: &[u8],
    now: OffsetDateTime,
) -> SignedRequest {
    let amz_date = amz_date(now);
    let date_stamp = date_stamp(now);
    let payload_hash = sha256_hex(payload);
    let kms_key_id = (method == "PUT").then(|| cfg.kms_key_id.clone()).flatten();

    let scope = format!(
        "{date_stamp}/{region}/{SERVICE}/{AWS4_REQUEST}",
        region = cfg.region
    );
    let creq = canonical_request(
        method,
        canonical_uri,
        host,
        &amz_date,
        &payload_hash,
        kms_key_id.as_deref(),
    );
    let creq_hash = sha256_hex(creq.as_bytes());
    let sts = string_to_sign(&amz_date, &scope, &creq_hash);

    let key = signing_key(&cfg.secret_key, &date_stamp, &cfg.region, SERVICE);
    let signature = hex::encode(hmac_sha256(&key, sts.as_bytes()));

    let signed_headers = signed_headers(kms_key_id.is_some());
    let authorization = format!(
        "{ALGORITHM} Credential={access}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        access = cfg.access_key
    );

    SignedRequest {
        host: host.to_string(),
        amz_date,
        payload_hash,
        authorization,
        kms_key_id,
    }
}

// ----------------------------------------------------------------- Helpers

/// Extract the `host[:port]` from a URL like `http://minio:9000/...`. Falls back
/// to the whole string if no `//` scheme separator is present.
fn host_of(url: &str) -> &str {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    after_scheme.split('/').next().unwrap_or(after_scheme)
}

/// Wrap any displayable error as a [`BlobStoreError::Io`] (the store's only
/// transport-error channel).
fn io_other<E: std::fmt::Display>(e: E) -> BlobStoreError {
    BlobStoreError::Io(std::io::Error::other(e.to_string()))
}

/// Per-request timeout applied on every S3 attempt (ROADMAP 方向三). Set on the
/// `RequestBuilder` itself so it holds even if the client builder fell back to a
/// default client with no global timeout — a hung endpoint can never pin a call.
const S3_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Total send attempts (1 initial + 2 retries) for transient S3 failures.
const S3_MAX_ATTEMPTS: u32 = 3;

/// Whether an S3 HTTP status is a transient failure worth retrying: server-side
/// errors (5xx) and throttling (429) — both of which AWS explicitly recommends
/// retrying with backoff. Other 4xx are caller/auth errors that won't improve on
/// retry, and 404 is handled separately as `NotFound`.
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

// -------------------------------------------------------------------- Tests

#[cfg(test)]
#[path = "s3_blob_store/range_tests.rs"]
mod range_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    /// AWS's published `SigV4` example credentials.
    const EXAMPLE_ACCESS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
    const EXAMPLE_SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

    #[test]
    fn retryable_status_covers_5xx_and_429_only() {
        use reqwest::StatusCode;
        // Transient → retry.
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        // Permanent / caller errors → fail fast.
        assert!(!is_retryable_status(StatusCode::OK));
        assert!(!is_retryable_status(StatusCode::FORBIDDEN));
        assert!(!is_retryable_status(StatusCode::NOT_FOUND));
        assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
    }

    #[test]
    fn s3_config_debug_redacts_secret_key() {
        let cfg = S3Config {
            bucket: "my-bucket".into(),
            region: "us-east-1".into(),
            endpoint: None,
            access_key: EXAMPLE_ACCESS_KEY.into(),
            secret_key: EXAMPLE_SECRET_KEY.into(),
            kms_key_id: Some("arn:aws:kms:us-east-1:123456789012:key/not-a-secret".into()),
        };
        let dbg = format!("{cfg:?}");
        assert!(
            !dbg.contains(EXAMPLE_SECRET_KEY),
            "secret key must never appear in Debug output:\n{dbg}"
        );
        assert!(
            !dbg.contains("arn:aws:kms"),
            "KMS resource metadata must not appear in Debug output:\n{dbg}"
        );
        assert!(dbg.contains("<redacted>"), "redaction marker present");
        assert!(
            !dbg.contains(EXAMPLE_ACCESS_KEY),
            "credential identifiers are redacted too"
        );
        assert!(dbg.contains("my-bucket"));
    }

    /// `hmac`/`sha2` determinism guard: SHA-256 of the empty string is a fixed,
    /// well-known constant (also the `SigV4` hash of an unsigned empty payload).
    #[test]
    fn sha256_empty_is_known_vector() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// HMAC-SHA256 determinism against RFC 4231 test case 1
    /// (key = 20 × 0x0b, data = "Hi There").
    #[test]
    fn hmac_sha256_rfc4231_vector() {
        let key = [0x0bu8; 20];
        let tag = hmac_sha256(&key, b"Hi There");
        assert_eq!(
            hex::encode(tag),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    // ---- AWS S3 GetObject SigV4 published example -------------------------
    //
    // From AWS's "Signature Calculations for the Authorization Header"
    // documentation: GET an object `test.txt` from `examplebucket` in
    // us-east-1 at 20130524T000000Z, range bytes=0-9, with an empty
    // (unsigned-as-SHA256-of-empty) payload. We reproduce the documented
    // canonical-request hash, string-to-sign and final signature.
    //
    // Note: AWS's full example signs four headers (host, range,
    // x-amz-content-sha256, x-amz-date). Our request shape never sends a
    // `range` header, so we assert the three-header variant we actually emit
    // is internally consistent (canonical-request → string-to-sign →
    // signature chain), plus the AWS-derived *signing key* which is
    // header-independent.

    fn example_cfg() -> S3Config {
        S3Config {
            bucket: "examplebucket".to_string(),
            region: "us-east-1".to_string(),
            endpoint: None,
            access_key: EXAMPLE_ACCESS_KEY.to_string(),
            secret_key: EXAMPLE_SECRET_KEY.to_string(),
            kms_key_id: None,
        }
    }

    /// The `SigV4` **signing key** is independent of the request headers, so we
    /// can assert it against AWS's documented derivation for
    /// `(20130524, us-east-1, s3)`. AWS publishes the expected key bytes for
    /// this exact tuple.
    #[test]
    fn signing_key_matches_aws_example() {
        let key = signing_key(EXAMPLE_SECRET_KEY, "20130524", "us-east-1", "s3");
        // Signing key for this (date, region, service), derived via the
        // documented AWS4 → kDate → kRegion → kService → kSigning chain.
        assert_eq!(
            hex::encode(&key),
            "f117494eff5d09da21cbf7f0339559ea04fc9582d31299cb992be70a6b27c97a"
        );
    }

    /// Full canonical-request → string-to-sign → signature chain, exactly as we
    /// emit it (three signed headers, empty payload), asserted self-consistent
    /// and stable. The `payload_hash` is the SHA-256 of the empty body, which is
    /// also AWS's documented value for an empty GET payload.
    #[test]
    fn sign_request_chain_is_stable() {
        let cfg = example_cfg();
        let now = datetime!(2013-05-24 00:00:00 UTC);

        let signed = sign_request(
            &cfg,
            "GET",
            "examplebucket.s3.amazonaws.com",
            "/test.txt",
            b"",
            now,
        );

        assert_eq!(signed.amz_date, "20130524T000000Z");
        // SHA-256 of the empty payload — AWS's documented empty-body hash.
        assert_eq!(
            signed.payload_hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        // Recompute the chain pieces the same way sign_request does and assert
        // the authorization header embeds a signature that re-derives. These are
        // the exact values for our three-signed-header GET on /test.txt with an
        // empty payload (independently computed from the SigV4 spec).
        let creq = canonical_request(
            "GET",
            "/test.txt",
            "examplebucket.s3.amazonaws.com",
            &signed.amz_date,
            &signed.payload_hash,
            None,
        );
        let creq_hash = sha256_hex(creq.as_bytes());
        assert_eq!(
            creq_hash,
            "e155673fa5bcd4b855a77a15b98fce3d10f286f93a203d6d98d2eb51f885f9b7"
        );
        let scope = "20130524/us-east-1/s3/aws4_request";
        let sts = string_to_sign(&signed.amz_date, scope, &creq_hash);
        let key = signing_key(EXAMPLE_SECRET_KEY, "20130524", "us-east-1", "s3");
        let expected_sig = hex::encode(hmac_sha256(&key, sts.as_bytes()));
        assert_eq!(
            expected_sig,
            "14f6a0997b2b70a86f4726658a6575b5109092ccb5fd328f51b369c44b4ac958"
        );

        assert!(
            signed
                .authorization
                .contains(&format!("Signature={expected_sig}")),
            "authorization header must embed the derived signature"
        );
        assert!(signed.authorization.starts_with(ALGORITHM));
        assert!(signed
            .authorization
            .contains("Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request"));
        assert!(signed
            .authorization
            .contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date"));
    }

    /// Canonical-request format guard: a fixed input must produce a byte-exact
    /// canonical request (newlines, header ordering, trailing payload hash).
    #[test]
    fn canonical_request_format_is_exact() {
        let creq = canonical_request(
            "GET",
            "/test.txt",
            "examplebucket.s3.amazonaws.com",
            "20130524T000000Z",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            None,
        );
        let expected = concat!(
            "GET\n",
            "/test.txt\n",
            "\n",
            "host:examplebucket.s3.amazonaws.com\n",
            "x-amz-content-sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n",
            "x-amz-date:20130524T000000Z\n",
            "\n",
            "host;x-amz-content-sha256;x-amz-date\n",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
        assert_eq!(creq, expected);
    }

    #[test]
    fn kms_put_canonical_request_and_authorization_sign_all_x_amz_headers() {
        let mut cfg = example_cfg();
        let kms_key_id = "arn:aws:kms:us-east-1:123456789012:key/1234abcd";
        cfg.kms_key_id = Some(kms_key_id.to_owned());
        let now = datetime!(2013-05-24 00:00:00 UTC);
        let signed = sign_request(
            &cfg,
            "PUT",
            "examplebucket.s3.amazonaws.com",
            "/test.txt",
            b"hello",
            now,
        );
        let canonical = canonical_request(
            "PUT",
            "/test.txt",
            "examplebucket.s3.amazonaws.com",
            &signed.amz_date,
            &signed.payload_hash,
            Some(kms_key_id),
        );
        let expected = format!(
            concat!(
                "PUT\n/test.txt\n\n",
                "host:examplebucket.s3.amazonaws.com\n",
                "x-amz-content-sha256:{}\n",
                "x-amz-date:20130524T000000Z\n",
                "x-amz-server-side-encryption:aws:kms\n",
                "x-amz-server-side-encryption-aws-kms-key-id:{}\n\n",
                "host;x-amz-content-sha256;x-amz-date;",
                "x-amz-server-side-encryption;",
                "x-amz-server-side-encryption-aws-kms-key-id\n",
                "{}"
            ),
            signed.payload_hash, kms_key_id, signed.payload_hash
        );
        assert_eq!(canonical, expected);
        assert_eq!(signed.kms_key_id.as_deref(), Some(kms_key_id));
        assert!(signed.authorization.contains(
            "SignedHeaders=host;x-amz-content-sha256;x-amz-date;\
             x-amz-server-side-encryption;\
             x-amz-server-side-encryption-aws-kms-key-id"
        ));
    }

    #[test]
    fn kms_headers_are_not_signed_or_emitted_for_non_put_methods() {
        let mut cfg = example_cfg();
        cfg.kms_key_id = Some("alias/aero-blobs".to_owned());
        let now = datetime!(2013-05-24 00:00:00 UTC);
        for method in ["GET", "DELETE"] {
            let signed = sign_request(
                &cfg,
                method,
                "examplebucket.s3.amazonaws.com",
                "/test.txt",
                b"",
                now,
            );
            assert!(signed.kms_key_id.is_none());
            assert!(signed
                .authorization
                .contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date,"));
            assert!(!signed.authorization.contains(SSE_HEADER));
        }
    }

    #[test]
    fn kms_key_id_validation_accepts_aws_forms_and_rejects_unsafe_values() {
        for valid in [
            "1234abcd-12ab-34cd-56ef-1234567890ab",
            "mrk-1234abcd",
            "alias/aero_blobs-prod",
            "arn:aws:kms:us-east-1:123456789012:key/1234abcd",
            "arn:aws-us-gov:kms:us-gov-west-1:123456789012:alias/aero",
        ] {
            let mut cfg = example_cfg();
            cfg.kms_key_id = Some(valid.to_owned());
            assert!(cfg.validate().is_ok(), "{valid} should be accepted");
        }

        for invalid in [
            "",
            " alias/aero",
            "alias/aero ",
            "alias/aero\r\nx-amz-meta-injected:value",
            "alias/aero blobs",
            "alias/aero.blobs",
        ] {
            let mut cfg = example_cfg();
            cfg.kms_key_id = Some(invalid.to_owned());
            assert!(matches!(cfg.validate(), Err(BlobStoreError::Config(_))));
        }

        let mut cfg = example_cfg();
        cfg.kms_key_id = Some("a".repeat(MAX_KMS_KEY_ID_LEN + 1));
        assert!(matches!(cfg.validate(), Err(BlobStoreError::Config(_))));
    }

    #[test]
    fn store_construction_fails_closed_for_invalid_kms_key_id() {
        let mut cfg = example_cfg();
        cfg.kms_key_id = Some("alias/aero\r\nx-amz-meta-injected:value".to_owned());
        assert!(matches!(
            S3BlobStore::try_new(cfg),
            Err(BlobStoreError::Config(_))
        ));
    }

    #[test]
    fn writable_config_requires_complete_addressing_and_credentials() {
        let mutations: [fn(&mut S3Config); 6] = [
            |cfg: &mut S3Config| cfg.bucket.clear(),
            |cfg: &mut S3Config| cfg.bucket = "../escape".into(),
            |cfg: &mut S3Config| cfg.region.clear(),
            |cfg: &mut S3Config| cfg.access_key.clear(),
            |cfg: &mut S3Config| cfg.secret_key.clear(),
            |cfg: &mut S3Config| cfg.secret_key = " injected\nheader".into(),
        ];
        for mutate in mutations {
            let mut cfg = example_cfg();
            mutate(&mut cfg);
            assert!(matches!(cfg.validate(), Err(BlobStoreError::Config(_))));
        }
    }

    #[test]
    fn custom_endpoint_must_be_a_plain_http_origin() {
        for invalid in [
            "ftp://minio.example.test",
            "http://user:pass@minio.example.test",
            "http://minio.example.test/prefix",
            "http://minio.example.test?query=1",
            "http://minio.example.test/#fragment",
        ] {
            let mut cfg = example_cfg();
            cfg.endpoint = Some(invalid.into());
            assert!(
                matches!(cfg.validate(), Err(BlobStoreError::Config(_))),
                "{invalid}"
            );
        }
        let mut cfg = example_cfg();
        cfg.endpoint = Some("http://127.0.0.1:9000".into());
        assert!(cfg.validate().is_ok());
    }

    /// String-to-sign format guard (algorithm, date, scope, creq-hash on four
    /// lines).
    #[test]
    fn string_to_sign_format_is_exact() {
        let sts = string_to_sign(
            "20130524T000000Z",
            "20130524/us-east-1/s3/aws4_request",
            "9e0e90d9c76de8fa5b200d8c849cd5b8dc7a3be3951ddb7f6a76b4158342019d",
        );
        let expected = concat!(
            "AWS4-HMAC-SHA256\n",
            "20130524T000000Z\n",
            "20130524/us-east-1/s3/aws4_request\n",
            "9e0e90d9c76de8fa5b200d8c849cd5b8dc7a3be3951ddb7f6a76b4158342019d",
        );
        assert_eq!(sts, expected);
    }

    #[test]
    fn date_helpers_format_correctly() {
        let ts = datetime!(2013-05-24 00:00:00 UTC);
        assert_eq!(amz_date(ts), "20130524T000000Z");
        assert_eq!(date_stamp(ts), "20130524");
    }

    #[test]
    fn from_env_none_without_bucket() {
        // Snapshot + clear so the test is order-independent.
        let prev = std::env::var("AERO_S3_BUCKET").ok();
        std::env::remove_var("AERO_S3_BUCKET");
        assert!(S3Config::from_env().is_none());
        if let Some(v) = prev {
            std::env::set_var("AERO_S3_BUCKET", v);
        }
    }

    #[test]
    fn key_for_uses_s3_uri() {
        let store = S3BlobStore::new(example_cfg());
        let id = BlobId::new();
        let key = store.key_for(id);
        assert_eq!(key, format!("s3://examplebucket/blobs/{id}"));
    }

    #[test]
    fn host_of_extracts_authority() {
        assert_eq!(host_of("http://minio:9000/bucket/key"), "minio:9000");
        assert_eq!(host_of("https://s3.amazonaws.com"), "s3.amazonaws.com");
        assert_eq!(host_of("minio:9000"), "minio:9000");
    }

    #[test]
    fn path_style_request_url_for_custom_endpoint() {
        let mut cfg = example_cfg();
        cfg.endpoint = Some("http://minio:9000".to_string());
        let store = S3BlobStore::new(cfg);
        let (url, path) = store.request_url("blobs/abc");
        assert_eq!(url, "http://minio:9000/examplebucket/blobs/abc");
        assert_eq!(path, "/examplebucket/blobs/abc");
    }

    #[test]
    fn virtual_host_request_url_for_aws() {
        let store = S3BlobStore::new(example_cfg());
        let (url, path) = store.request_url("blobs/abc");
        assert_eq!(
            url,
            "https://examplebucket.s3.us-east-1.amazonaws.com/blobs/abc"
        );
        assert_eq!(path, "/blobs/abc");
    }
}
