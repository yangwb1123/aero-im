use std::collections::HashSet;
use std::net::IpAddr;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use aero_common::WorkspaceId;
use aero_storage::SnaplinkBindingSpec;
use anyhow::{anyhow, bail, Context};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use reqwest::Url;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MAX_BINDINGS_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SECRET_BYTES: usize = 16 * 1024;

pub(super) enum CommercialMode {
    Unspecified,
    Disabled,
    Enabled(Box<CommercialConfig>),
}

#[derive(Clone)]
pub(super) struct CommercialConfig {
    pub token_endpoint: Url,
    pub billing_usage_url: Url,
    pub billing_entitlement_url: Url,
    pub audit_events_url: Url,
    pub billing_resource: String,
    pub audit_resource: String,
    pub request_timeout: Duration,
    pub projection_interval: Duration,
    pub delivery_poll_interval: Duration,
    pub delivery_lease: Duration,
    pub batch_size: i32,
    pub concurrency: usize,
    pub shutdown_drain: Duration,
    pub bindings: Vec<CommercialBinding>,
    log_pseudonym_key: Vec<u8>,
}

struct CommercialEndpoints {
    token: Url,
    usage: Url,
    entitlement: Url,
    audit: Url,
    billing_resource: String,
    audit_resource: String,
}

struct CommercialLimits {
    request_timeout: Duration,
    projection_interval: Duration,
    delivery_poll_interval: Duration,
    delivery_lease: Duration,
    batch_size: i32,
    concurrency: usize,
    shutdown_drain: Duration,
}

impl std::fmt::Debug for CommercialConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommercialConfig")
            .field("token_endpoint", &"<redacted>")
            .field("billing_usage_url", &"<redacted>")
            .field("billing_entitlement_url", &"<redacted>")
            .field("audit_events_url", &"<redacted>")
            .field("billing_resource", &self.billing_resource)
            .field("audit_resource", &self.audit_resource)
            .field("request_timeout", &self.request_timeout)
            .field("projection_interval", &self.projection_interval)
            .field("delivery_poll_interval", &self.delivery_poll_interval)
            .field("delivery_lease", &self.delivery_lease)
            .field("batch_size", &self.batch_size)
            .field("concurrency", &self.concurrency)
            .field("shutdown_drain", &self.shutdown_drain)
            .field("bindings", &self.bindings)
            .field("log_pseudonym_key", &"<redacted>")
            .finish()
    }
}

#[derive(Clone)]
pub(super) struct CommercialBinding {
    pub workspace_id: WorkspaceId,
    pub tenant_id: String,
    pub billing_client_id: String,
    pub billing_client_secret: String,
    pub audit_client_id: String,
    pub audit_client_secret: String,
    pub source_system: String,
    pub revision: i64,
    pub enabled: bool,
}

impl CommercialBinding {
    pub(super) fn storage_spec(&self) -> SnaplinkBindingSpec {
        SnaplinkBindingSpec {
            workspace_id: self.workspace_id,
            tenant_id: self.tenant_id.clone(),
            billing_client_id: self.billing_client_id.clone(),
            audit_client_id: self.audit_client_id.clone(),
            source_system: self.source_system.clone(),
            revision: self.revision,
            enabled: self.enabled,
        }
    }
}

impl std::fmt::Debug for CommercialBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommercialBinding")
            .field("workspace_id", &"<redacted>")
            .field("tenant_id", &"<redacted>")
            .field("billing_client_id", &"<redacted>")
            .field("billing_client_secret", &"<redacted>")
            .field("audit_client_id", &"<redacted>")
            .field("audit_client_secret", &"<redacted>")
            .field("source_system", &"<redacted>")
            .field("revision", &self.revision)
            .field("enabled", &self.enabled)
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingFile {
    version: u32,
    bindings: Vec<BindingEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingEntry {
    workspace_id: String,
    tenant_id: String,
    billing_client_id: String,
    billing_client_secret_env: String,
    audit_client_id: String,
    audit_client_secret_env: String,
    revision: i64,
    enabled: bool,
}

impl CommercialMode {
    pub(super) fn from_env() -> anyhow::Result<Self> {
        let Some(enabled) = explicit_bool("AERO_SNAPLINK_COMMERCIAL_ENABLED")? else {
            return Ok(Self::Unspecified);
        };
        if !enabled {
            return Ok(Self::Disabled);
        }
        Ok(Self::Enabled(Box::new(CommercialConfig::from_env()?)))
    }
}

impl CommercialConfig {
    fn from_env() -> anyhow::Result<Self> {
        let endpoints = commercial_endpoints_from_env()?;
        let limits = commercial_limits_from_env()?;
        let bindings = commercial_bindings_from_env()?;
        let log_pseudonym_key = secret_from_env_name("AERO_SNAPLINK_LOG_PSEUDONYM_KEY")?;
        validate_secret(&log_pseudonym_key).context("Snaplink log pseudonym key is invalid")?;
        Ok(Self {
            token_endpoint: endpoints.token,
            billing_usage_url: endpoints.usage,
            billing_entitlement_url: endpoints.entitlement,
            audit_events_url: endpoints.audit,
            billing_resource: endpoints.billing_resource,
            audit_resource: endpoints.audit_resource,
            request_timeout: limits.request_timeout,
            projection_interval: limits.projection_interval,
            delivery_poll_interval: limits.delivery_poll_interval,
            delivery_lease: limits.delivery_lease,
            batch_size: limits.batch_size,
            concurrency: limits.concurrency,
            shutdown_drain: limits.shutdown_drain,
            bindings,
            log_pseudonym_key: log_pseudonym_key.into_bytes(),
        })
    }

    pub(super) fn log_ref(&self, domain: &str, value: &str) -> String {
        pseudonym_ref(&self.log_pseudonym_key, domain, value)
    }
}

fn pseudonym_ref(key: &[u8], domain: &str, value: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts an arbitrary key length");
    mac.update(domain.as_bytes());
    mac.update(b"\0");
    mac.update(value.as_bytes());
    format!(
        "ref_{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    )
}

fn commercial_endpoints_from_env() -> anyhow::Result<CommercialEndpoints> {
    let allow_insecure = bool_or_default("AERO_SNAPLINK_ALLOW_INSECURE_LOOPBACK", false)?;
    let token_endpoint = service_url(
        "AERO_SNAPLINK_TOKEN_ENDPOINT",
        &required_env("AERO_SNAPLINK_TOKEN_ENDPOINT")?,
        allow_insecure,
        false,
    )?;
    let billing_base = service_url(
        "AERO_SNAPLINK_BILLING_BASE_URL",
        &required_env("AERO_SNAPLINK_BILLING_BASE_URL")?,
        allow_insecure,
        true,
    )?;
    let audit_base = service_url(
        "AERO_SNAPLINK_AUDIT_BASE_URL",
        &required_env("AERO_SNAPLINK_AUDIT_BASE_URL")?,
        allow_insecure,
        true,
    )?;
    Ok(CommercialEndpoints {
        token: token_endpoint,
        usage: endpoint(&billing_base, "api/v1/metering/usage")?,
        entitlement: endpoint(&billing_base, "api/v1/metering/entitlement")?,
        audit: audit_endpoint(&audit_base)?,
        billing_resource: resource_from_env("AERO_SNAPLINK_BILLING_RESOURCE", "billing-api")?,
        audit_resource: resource_from_env("AERO_SNAPLINK_AUDIT_RESOURCE", "audit-governance")?,
    })
}

fn commercial_bindings_from_env() -> anyhow::Result<Vec<CommercialBinding>> {
    let source_prefix =
        optional_env("AERO_SNAPLINK_SOURCE_PREFIX").unwrap_or_else(|| "aero-im".into());
    validate_source_prefix(&source_prefix)?;
    load_bindings(
        Path::new(&required_env("AERO_SNAPLINK_BINDINGS_FILE")?),
        &source_prefix,
    )
}

fn commercial_limits_from_env() -> anyhow::Result<CommercialLimits> {
    let request_timeout = duration_secs("AERO_SNAPLINK_REQUEST_TIMEOUT_SECS", 10, 1, 120)?;
    let delivery_lease = duration_secs("AERO_SNAPLINK_DELIVERY_LEASE_SECS", 30, 5, 300)?;
    if delivery_lease <= request_timeout.saturating_mul(2) + Duration::from_secs(2) {
        bail!("AERO_SNAPLINK_DELIVERY_LEASE_SECS must exceed two request timeouts by more than two seconds");
    }
    let shutdown_drain = duration_secs("AERO_SNAPLINK_SHUTDOWN_DRAIN_SECS", 5, 1, 60)?;
    let task_drain = duration_secs("AERO_TASK_DRAIN_SECS", 30, 1, 600)?;
    if task_drain <= request_timeout.saturating_mul(2) + shutdown_drain + Duration::from_secs(2) {
        bail!("AERO_TASK_DRAIN_SECS must exceed two Snaplink request timeouts and the shutdown drain budget by more than two seconds");
    }
    Ok(CommercialLimits {
        request_timeout,
        projection_interval: duration_secs("AERO_SNAPLINK_PROJECTION_INTERVAL_SECS", 60, 5, 3_600)?,
        delivery_poll_interval: duration_millis("AERO_SNAPLINK_DELIVERY_POLL_MS", 250, 10, 60_000)?,
        delivery_lease,
        batch_size: i32::try_from(integer_env("AERO_SNAPLINK_DELIVERY_BATCH", 100, 1, 500)?)
            .expect("bounded positive delivery batch"),
        concurrency: usize::try_from(integer_env(
            "AERO_SNAPLINK_DELIVERY_CONCURRENCY",
            16,
            1,
            128,
        )?)
        .expect("bounded positive concurrency"),
        shutdown_drain,
    })
}

fn load_bindings(path: &Path, prefix: &str) -> anyhow::Result<Vec<CommercialBinding>> {
    let metadata = std::fs::metadata(path).context("inspect Snaplink bindings file")?;
    if metadata.len() > MAX_BINDINGS_FILE_BYTES {
        bail!("Snaplink bindings file exceeds 2 MiB");
    }
    let raw = std::fs::read(path).context("read Snaplink bindings file")?;
    let parsed: BindingFile =
        serde_json::from_slice(&raw).context("parse Snaplink bindings file")?;
    if parsed.version != 2 {
        bail!("Snaplink bindings file version must be 2");
    }
    parsed
        .bindings
        .into_iter()
        .map(|entry| binding_from_entry(entry, prefix))
        .collect::<anyhow::Result<Vec<_>>>()
        .and_then(validate_binding_uniqueness)
}

fn binding_from_entry(entry: BindingEntry, prefix: &str) -> anyhow::Result<CommercialBinding> {
    if entry.workspace_id != entry.workspace_id.trim() {
        bail!("binding workspace_id must be canonical without surrounding whitespace");
    }
    let workspace_id = WorkspaceId::from_str(entry.workspace_id.trim())
        .map_err(|_| anyhow!("binding workspace_id is invalid"))?;
    validate_identity("tenant_id", &entry.tenant_id, 256)?;
    validate_identity("billing_client_id", &entry.billing_client_id, 512)?;
    validate_identity("audit_client_id", &entry.audit_client_id, 512)?;
    validate_env_name(&entry.billing_client_secret_env)?;
    validate_env_name(&entry.audit_client_secret_env)?;
    if entry.revision <= 0 {
        bail!("binding revision must be positive");
    }
    let billing_client_secret = secret_from_env_name(&entry.billing_client_secret_env)?;
    let audit_client_secret = secret_from_env_name(&entry.audit_client_secret_env)?;
    validate_secret(&billing_client_secret)?;
    validate_secret(&audit_client_secret)?;
    Ok(CommercialBinding {
        workspace_id,
        source_system: derive_source_system(prefix, &entry.tenant_id),
        tenant_id: entry.tenant_id,
        billing_client_id: entry.billing_client_id,
        billing_client_secret,
        audit_client_id: entry.audit_client_id,
        audit_client_secret,
        revision: entry.revision,
        enabled: entry.enabled,
    })
}

fn validate_binding_uniqueness(
    bindings: Vec<CommercialBinding>,
) -> anyhow::Result<Vec<CommercialBinding>> {
    let mut workspaces = HashSet::new();
    let mut tenants = HashSet::new();
    let mut clients = HashSet::new();
    let mut secrets = HashSet::new();
    let mut sources = HashSet::new();
    for binding in &bindings {
        if !workspaces.insert(binding.workspace_id)
            || !tenants.insert(binding.tenant_id.as_str())
            || !sources.insert(binding.source_system.as_str())
            || !clients.insert(binding.billing_client_id.as_str())
            || !clients.insert(binding.audit_client_id.as_str())
            || !secrets.insert(binding.billing_client_secret.as_str())
            || !secrets.insert(binding.audit_client_secret.as_str())
        {
            bail!("Snaplink bindings must use globally distinct workspace, tenant, source, client, and secret identities");
        }
    }
    Ok(bindings)
}

pub(super) fn derive_source_system(prefix: &str, tenant_id: &str) -> String {
    let digest = Sha256::digest(tenant_id.as_bytes());
    format!("{prefix}.{}", URL_SAFE_NO_PAD.encode(digest))
}

fn service_url(name: &str, raw: &str, insecure: bool, base: bool) -> anyhow::Result<Url> {
    let url = Url::parse(raw).with_context(|| format!("parse {name}"))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
    {
        bail!("{name} must not contain credentials, query, or fragment");
    }
    if base && !matches!(url.path(), "" | "/") {
        bail!("{name} must be an origin URL without a path");
    }
    match url.scheme() {
        "https" => {}
        "http" if insecure && is_loopback(&url) => {}
        _ => bail!("{name} must use HTTPS (HTTP is opt-in and loopback-only)"),
    }
    Ok(url)
}

fn is_loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn endpoint(base: &Url, path: &str) -> anyhow::Result<Url> {
    base.join(path).context("construct Snaplink endpoint")
}

fn audit_endpoint(base: &Url) -> anyhow::Result<Url> {
    let mut url = endpoint(base, "api/v1/events")?;
    url.query_pairs_mut().append_pair("wait_for", "ledgered");
    Ok(url)
}

fn validate_source_prefix(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        || !value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("AERO_SNAPLINK_SOURCE_PREFIX is invalid");
    }
    Ok(())
}

fn resource_from_env(name: &str, default: &str) -> anyhow::Result<String> {
    let value = optional_env(name).unwrap_or_else(|| default.to_owned());
    validate_identity("OAuth resource", &value, 256)?;
    Ok(value)
}

fn validate_identity(name: &str, value: &str, max: usize) -> anyhow::Result<()> {
    if value.is_empty()
        || value != value.trim()
        || value.len() > max
        || value.chars().any(char::is_control)
    {
        bail!("binding {name} is invalid");
    }
    Ok(())
}

fn validate_secret(value: &str) -> anyhow::Result<()> {
    if value.len() < 32
        || value != value.trim()
        || value.len() > MAX_SECRET_BYTES
        || value.chars().any(char::is_control)
    {
        bail!("Snaplink client secret is invalid");
    }
    Ok(())
}

fn validate_env_name(value: &str) -> anyhow::Result<()> {
    let mut bytes = value.bytes();
    let valid_first = bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_uppercase());
    if !valid_first
        || !bytes.all(|byte| byte == b'_' || byte.is_ascii_uppercase() || byte.is_ascii_digit())
    {
        bail!("client_secret_env must be an uppercase environment variable name");
    }
    Ok(())
}

fn required_env(name: &str) -> anyhow::Result<String> {
    optional_env(name).ok_or_else(|| anyhow!("{name} is required"))
}

fn secret_from_env_name(name: &str) -> anyhow::Result<String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => bail!("a required Snaplink secret is unavailable"),
    }
}

fn optional_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn explicit_bool(name: &str) -> anyhow::Result<Option<bool>> {
    let Ok(raw) = std::env::var(name) else {
        return Ok(None);
    };
    parse_bool(name, &raw).map(Some)
}

fn bool_or_default(name: &str, default: bool) -> anyhow::Result<bool> {
    match std::env::var(name) {
        Ok(raw) => parse_bool(name, &raw),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}

fn parse_bool(name: &str, raw: &str) -> anyhow::Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" => Ok(true),
        "0" | "false" => Ok(false),
        _ => bail!("{name} must be true/false or 1/0"),
    }
}

fn integer_env(name: &str, default: i64, min: i64, max: i64) -> anyhow::Result<i64> {
    let value = optional_env(name)
        .map(|raw| raw.parse::<i64>().with_context(|| format!("parse {name}")))
        .transpose()?
        .unwrap_or(default);
    if !(min..=max).contains(&value) {
        bail!("{name} must be between {min} and {max}");
    }
    Ok(value)
}

fn duration_secs(name: &str, default: i64, min: i64, max: i64) -> anyhow::Result<Duration> {
    Ok(Duration::from_secs(
        u64::try_from(integer_env(name, default, min, max)?).expect("positive duration"),
    ))
}

fn duration_millis(name: &str, default: i64, min: i64, max: i64) -> anyhow::Result<Duration> {
    Ok(Duration::from_millis(
        u64::try_from(integer_env(name, default, min, max)?).expect("positive duration"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_source_is_stable_bounded_and_tenant_specific() {
        let first = derive_source_system("aero-im", "tenant-a");
        assert_eq!(first, derive_source_system("aero-im", "tenant-a"));
        assert_ne!(first, derive_source_system("aero-im", "tenant-b"));
        assert_eq!(first, "aero-im.gKcHr33HfuEij5EnGA85ZINeW-tMSrDYEvD-dZNXmzo");
        assert!(validate_source_prefix(".invalid").is_err());
        assert!(validate_source_prefix("invalid-").is_err());
    }

    #[test]
    fn insecure_service_urls_are_loopback_only() {
        assert!(service_url("test", "http://127.0.0.1:8080", true, true).is_ok());
        assert!(service_url("test", "http://192.0.2.10:8080", true, true).is_err());
        assert!(service_url("test", "https://example.test", false, true).is_ok());
        assert!(service_url("test", "https://user@example.test", false, true).is_err());
    }

    #[test]
    fn audit_endpoint_requires_a_ledgered_receipt() {
        let base = Url::parse("https://audit.example.test").unwrap();
        let url = audit_endpoint(&base).unwrap();
        assert_eq!(url.path(), "/api/v1/events");
        assert_eq!(url.query(), Some("wait_for=ledgered"));
    }

    #[test]
    fn commercial_secrets_have_a_minimum_strength_floor() {
        assert!(validate_secret("short").is_err());
        assert!(validate_secret("0123456789abcdef0123456789abcdef").is_ok());
    }

    fn binding(
        billing_id: &str,
        billing_secret: &str,
        audit_id: &str,
        audit_secret: &str,
    ) -> CommercialBinding {
        CommercialBinding {
            workspace_id: WorkspaceId::new(),
            tenant_id: format!("tenant-{billing_id}"),
            billing_client_id: billing_id.into(),
            billing_client_secret: billing_secret.into(),
            audit_client_id: audit_id.into(),
            audit_client_secret: audit_secret.into(),
            source_system: format!("aero-im.{billing_id}"),
            revision: 1,
            enabled: true,
        }
    }

    #[test]
    fn billing_and_audit_credentials_must_be_distinct() {
        assert!(
            validate_binding_uniqueness(vec![binding("same", "billing", "same", "audit")]).is_err()
        );
        assert!(
            validate_binding_uniqueness(vec![binding("billing", "same", "audit", "same")]).is_err()
        );
        assert!(
            validate_binding_uniqueness(vec![binding("billing", "one", "audit", "two")]).is_ok()
        );
    }

    #[test]
    fn binding_debug_never_discloses_identity_or_secret() {
        let value = binding(
            "billing-sensitive",
            "secret-one",
            "audit-sensitive",
            "secret-two",
        );
        let rendered = format!("{value:?}");
        for sensitive in [
            "billing-sensitive",
            "audit-sensitive",
            "secret-one",
            "secret-two",
            &value.tenant_id,
            &value.source_system,
        ] {
            assert!(!rendered.contains(sensitive));
        }
    }

    #[test]
    fn log_references_are_keyed_fixed_length_and_domain_separated() {
        let key = b"0123456789abcdef0123456789abcdef";
        let tenant = pseudonym_ref(key, "tenant", "tenant-sensitive");
        assert_eq!(tenant.len(), 47);
        assert!(!tenant.contains("tenant-sensitive"));
        assert_ne!(tenant, pseudonym_ref(key, "workspace", "tenant-sensitive"));
        assert_ne!(
            tenant,
            pseudonym_ref(
                b"different-key-material-32-bytes!!",
                "tenant",
                "tenant-sensitive"
            )
        );
    }
}
