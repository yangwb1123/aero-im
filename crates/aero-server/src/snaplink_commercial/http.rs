use std::collections::HashMap;
use std::time::{Duration, Instant};

use aero_storage::{
    SnaplinkDeliveryClaim, SnaplinkDeliveryDestination, SnaplinkEntitlementProjection,
    SnaplinkLimitProjection,
};
use anyhow::{anyhow, bail, Context};
use futures::TryStreamExt;
use reqwest::StatusCode;
use serde::Deserialize;
use tokio::sync::Mutex;

use super::config::{CommercialBinding, CommercialConfig};

const MAX_TOKEN_BODY: usize = 64 * 1024;
const MAX_ENTITLEMENT_BODY: usize = 256 * 1024;
const MAX_AUDIT_RECEIPT_BODY: usize = 64 * 1024;
const MAX_ACCESS_TOKEN_BYTES: usize = 16 * 1024;

const SCOPE_ENTITLEMENT: &str = "billing:entitlement:read";
const SCOPE_USAGE: &str = "metering:write";
const SCOPE_AUDIT: &str = "audit:event:write";

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
enum CredentialRole {
    Billing,
    Audit,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct TokenKey {
    role: CredentialRole,
    scope: &'static str,
    resource: String,
}

struct CachedToken {
    value: String,
    refresh_at: Instant,
}

impl std::fmt::Debug for CachedToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CachedToken")
            .field("value", &"<redacted>")
            .field("refresh_at", &self.refresh_at)
            .finish()
    }
}

pub(super) struct MachineBinding {
    pub binding: CommercialBinding,
    tokens: Mutex<HashMap<TokenKey, CachedToken>>,
}

impl MachineBinding {
    pub(super) fn new(binding: CommercialBinding) -> Self {
        Self {
            binding,
            tokens: Mutex::new(HashMap::new()),
        }
    }
}

impl std::fmt::Debug for MachineBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MachineBinding")
            .field("binding", &self.binding)
            .field("tokens", &"<redacted cache>")
            .finish()
    }
}

#[derive(Clone)]
pub(super) struct SnaplinkHttpClient {
    config: CommercialConfig,
    client: reqwest::Client,
}

impl SnaplinkHttpClient {
    pub(super) fn new(config: CommercialConfig) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("build Snaplink commercial HTTP client")?;
        Ok(Self { config, client })
    }

    pub(super) async fn fetch_entitlement(
        &self,
        machine: &MachineBinding,
    ) -> anyhow::Result<SnaplinkEntitlementProjection> {
        let token = self
            .access_token(
                machine,
                CredentialRole::Billing,
                SCOPE_ENTITLEMENT,
                &self.config.billing_resource,
            )
            .await?;
        let response = self
            .client
            .get(self.config.billing_entitlement_url.clone())
            .bearer_auth(&token)
            .send()
            .await
            .map_err(transport_error)?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.invalidate_token(machine, CredentialRole::Billing, SCOPE_ENTITLEMENT, &token)
                .await;
        }
        if response.status() != StatusCode::OK {
            bail!(
                "Snaplink entitlement endpoint returned HTTP {}",
                response.status().as_u16()
            );
        }
        let body = bounded_body(response, MAX_ENTITLEMENT_BODY).await?;
        let response: EntitlementResponse =
            serde_json::from_slice(&body).context("decode Snaplink entitlement response")?;
        response
            .entitlement
            .into_projection(machine.binding.workspace_id, &machine.binding.tenant_id)
    }

    pub(super) async fn deliver(
        &self,
        claim: &SnaplinkDeliveryClaim,
        machine: &MachineBinding,
    ) -> anyhow::Result<()> {
        validate_delivery_payload(claim)?;
        let (role, scope, resource, url, expected) = match claim.destination {
            SnaplinkDeliveryDestination::Usage => (
                CredentialRole::Billing,
                SCOPE_USAGE,
                &self.config.billing_resource,
                &self.config.billing_usage_url,
                StatusCode::CREATED,
            ),
            SnaplinkDeliveryDestination::Audit => (
                CredentialRole::Audit,
                SCOPE_AUDIT,
                &self.config.audit_resource,
                &self.config.audit_events_url,
                StatusCode::ACCEPTED,
            ),
        };
        let token = self.access_token(machine, role, scope, resource).await?;
        let response = self
            .client
            .post(url.clone())
            .bearer_auth(&token)
            .header("Idempotency-Key", &claim.idempotency_key)
            .json(&claim.payload)
            .send()
            .await
            .map_err(transport_error)?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.invalidate_token(machine, role, scope, &token).await;
        }
        if response.status() != expected {
            bail!(
                "Snaplink {:?} delivery returned HTTP {}",
                claim.destination,
                response.status().as_u16()
            );
        }
        if claim.destination == SnaplinkDeliveryDestination::Audit {
            let body = bounded_body(response, MAX_AUDIT_RECEIPT_BODY).await?;
            validate_audit_receipt(&body, claim)?;
        }
        Ok(())
    }

    async fn access_token(
        &self,
        machine: &MachineBinding,
        role: CredentialRole,
        scope: &'static str,
        resource: &str,
    ) -> anyhow::Result<String> {
        let key = TokenKey {
            role,
            scope,
            resource: resource.to_owned(),
        };
        {
            let tokens = machine.tokens.lock().await;
            if let Some(token) = tokens.get(&key) {
                if Instant::now() < token.refresh_at {
                    return Ok(token.value.clone());
                }
            }
        }
        // Never hold the per-workspace cache lock across network I/O. During a
        // token outage, serial timeout queues could otherwise outlive every
        // claimed delivery lease and the configured shutdown budget.
        let token = self.request_token(&machine.binding, &key).await?;
        let value = token.access_token;
        let ttl = token.expires_in.unwrap_or(300).clamp(1, 86_400);
        let usable = if ttl > 60 { ttl - 30 } else { (ttl / 2).max(1) };
        let mut tokens = machine.tokens.lock().await;
        if let Some(current) = tokens.get(&key) {
            if Instant::now() < current.refresh_at {
                return Ok(current.value.clone());
            }
        }
        tokens.insert(
            key,
            CachedToken {
                value: value.clone(),
                refresh_at: Instant::now() + Duration::from_secs(usable),
            },
        );
        Ok(value)
    }

    async fn request_token(
        &self,
        binding: &CommercialBinding,
        key: &TokenKey,
    ) -> anyhow::Result<TokenResponse> {
        let (client_id, client_secret) = binding.credential(key.role);
        let response = self
            .client
            .post(self.config.token_endpoint.clone())
            .basic_auth(client_id, Some(client_secret))
            .form(&[
                ("grant_type", "client_credentials"),
                ("scope", key.scope),
                ("resource", key.resource.as_str()),
            ])
            .send()
            .await
            .map_err(transport_error)?;
        if response.status() != StatusCode::OK {
            bail!(
                "Snaplink token endpoint returned HTTP {}",
                response.status().as_u16()
            );
        }
        let body = bounded_body(response, MAX_TOKEN_BODY).await?;
        let token: TokenResponse =
            serde_json::from_slice(&body).context("decode Snaplink token response")?;
        validate_token(&token)?;
        Ok(token)
    }

    async fn invalidate_token(
        &self,
        machine: &MachineBinding,
        role: CredentialRole,
        scope: &'static str,
        rejected: &str,
    ) {
        let mut tokens = machine.tokens.lock().await;
        tokens
            .retain(|key, token| key.role != role || key.scope != scope || token.value != rejected);
    }
}

impl CommercialBinding {
    fn credential(&self, role: CredentialRole) -> (&str, &str) {
        match role {
            CredentialRole::Billing => (&self.billing_client_id, &self.billing_client_secret),
            CredentialRole::Audit => (&self.audit_client_id, &self.audit_client_secret),
        }
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
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TokenResponse")
            .field("access_token", &"<redacted>")
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

#[derive(Deserialize)]
struct EntitlementResponse {
    entitlement: Entitlement,
}

#[derive(Deserialize)]
struct AuditReceiptEnvelope {
    receipt: AuditReceipt,
}

#[derive(Deserialize)]
struct AuditReceipt {
    event_id: String,
    tenant_id: String,
    status: String,
    #[serde(default, with = "time::serde::rfc3339::option")]
    accepted_at: Option<time::OffsetDateTime>,
    #[serde(default)]
    conflict: bool,
}

#[derive(Deserialize)]
struct Entitlement {
    tenant_id: String,
    revision: u64,
    active: bool,
    features: HashMap<String, bool>,
    limits: HashMap<String, LimitGrant>,
    #[serde(with = "time::serde::rfc3339")]
    effective_at: time::OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    expires_at: Option<time::OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    generated_at: time::OffsetDateTime,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct LimitGrant {
    soft: i64,
    hard: i64,
    #[serde(default)]
    unlimited: bool,
}

impl Entitlement {
    fn into_projection(
        self,
        workspace_id: aero_common::WorkspaceId,
        expected_tenant: &str,
    ) -> anyhow::Result<SnaplinkEntitlementProjection> {
        if self.tenant_id != expected_tenant {
            bail!("Snaplink entitlement tenant did not match the trusted workspace binding");
        }
        let messages = required_limit(&self.limits, "messages_per_month")?;
        let notifications = required_limit(&self.limits, "notifications_per_month")?;
        Ok(SnaplinkEntitlementProjection {
            workspace_id,
            tenant_id: self.tenant_id,
            revision: i64::try_from(self.revision).context("entitlement revision exceeds i64")?,
            active: self.active,
            im_enabled: self.features.get("im").copied().unwrap_or(false),
            notifications_enabled: self.features.get("notifications").copied().unwrap_or(false),
            messages,
            notifications,
            effective_at: self.effective_at,
            expires_at: self.expires_at,
            generated_at: self.generated_at,
        })
    }
}

fn required_limit(
    limits: &HashMap<String, LimitGrant>,
    key: &str,
) -> anyhow::Result<SnaplinkLimitProjection> {
    let limit = limits
        .get(key)
        .ok_or_else(|| anyhow!("Snaplink entitlement omitted required limit {key}"))?;
    if limit.soft < 0
        || limit.hard < 0
        || (limit.unlimited && (limit.soft != 0 || limit.hard != 0))
        || (!limit.unlimited && limit.soft > limit.hard)
    {
        bail!("Snaplink entitlement returned invalid limit {key}");
    }
    Ok(SnaplinkLimitProjection {
        soft: limit.soft,
        hard: limit.hard,
        unlimited: limit.unlimited,
    })
}

fn validate_delivery_payload(claim: &SnaplinkDeliveryClaim) -> anyhow::Result<()> {
    if claim.payload.get("tenant_id").is_some() {
        bail!("outbound Snaplink payload must not select a tenant");
    }
    match claim.destination {
        SnaplinkDeliveryDestination::Usage if claim.payload.get("source_system").is_some() => {
            bail!("usage payload must not select a source")
        }
        SnaplinkDeliveryDestination::Audit => {
            if claim
                .payload
                .get("source_system")
                .and_then(|value| value.as_str())
                != Some(claim.source_system.as_str())
            {
                bail!("audit payload source does not match the trusted binding");
            }
        }
        SnaplinkDeliveryDestination::Usage => {}
    }
    Ok(())
}

fn validate_audit_receipt(body: &[u8], claim: &SnaplinkDeliveryClaim) -> anyhow::Result<()> {
    let envelope: AuditReceiptEnvelope =
        serde_json::from_slice(body).context("decode Audit Governance receipt")?;
    let expected_event = claim
        .payload
        .get("event_id")
        .and_then(serde_json::Value::as_str)
        .context("audit delivery has no trusted event ID")?;
    let receipt = envelope.receipt;
    if receipt.event_id != expected_event
        || receipt.tenant_id != claim.tenant_id
        || receipt.accepted_at.is_none()
        || receipt.conflict
        || !matches!(receipt.status.as_str(), "ledgered" | "indexed" | "archived")
    {
        bail!("Audit Governance returned an invalid durable receipt");
    }
    Ok(())
}

fn validate_token(token: &TokenResponse) -> anyhow::Result<()> {
    if token.access_token.is_empty()
        || token.access_token.len() > MAX_ACCESS_TOKEN_BYTES
        || token.access_token.chars().any(char::is_control)
        || !token
            .token_type
            .as_deref()
            .is_some_and(|kind| kind.eq_ignore_ascii_case("bearer"))
    {
        bail!("Snaplink returned an invalid bearer token");
    }
    Ok(())
}

async fn bounded_body(response: reqwest::Response, max: usize) -> anyhow::Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > max as u64)
    {
        bail!("Snaplink response exceeded its size limit");
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.try_next().await.map_err(transport_error)? {
        if body.len().saturating_add(chunk.len()) > max {
            bail!("Snaplink response exceeded its size limit");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn transport_error(error: reqwest::Error) -> anyhow::Error {
    anyhow!("Snaplink HTTP transport failed: {}", error.without_url())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hard_zero_is_not_treated_as_unlimited() {
        let limits = HashMap::from([(
            "messages_per_month".into(),
            LimitGrant {
                soft: 0,
                hard: 0,
                unlimited: false,
            },
        )]);
        let projected = required_limit(&limits, "messages_per_month").unwrap();
        assert_eq!(projected.hard, 0);
        assert!(!projected.unlimited);
    }

    #[test]
    fn unlimited_requires_zero_numeric_limits() {
        let limits = HashMap::from([(
            "messages_per_month".into(),
            LimitGrant {
                soft: 0,
                hard: 1,
                unlimited: true,
            },
        )]);
        assert!(required_limit(&limits, "messages_per_month").is_err());
    }

    #[test]
    fn audit_delivery_requires_a_matching_durable_receipt() {
        let claim = audit_claim("tenant-a", "event-a");
        let valid = br#"{"receipt":{"event_id":"event-a","tenant_id":"tenant-a","status":"ledgered","accepted_at":"2026-08-04T00:00:00Z","conflict":false}}"#;
        assert!(validate_audit_receipt(valid, &claim).is_ok());

        let wrong_tenant = br#"{"receipt":{"event_id":"event-a","tenant_id":"tenant-b","status":"ledgered","accepted_at":"2026-08-04T00:00:00Z"}}"#;
        assert!(validate_audit_receipt(wrong_tenant, &claim).is_err());
        let accepted_only = br#"{"receipt":{"event_id":"event-a","tenant_id":"tenant-a","status":"accepted","accepted_at":"2026-08-04T00:00:00Z"}}"#;
        assert!(validate_audit_receipt(accepted_only, &claim).is_err());
    }

    fn audit_claim(tenant_id: &str, event_id: &str) -> SnaplinkDeliveryClaim {
        SnaplinkDeliveryClaim {
            delivery_id: format!("audit:{event_id}"),
            destination: SnaplinkDeliveryDestination::Audit,
            workspace_id: aero_common::WorkspaceId::new(),
            tenant_id: tenant_id.into(),
            client_id: "audit-client".into(),
            source_system: "aero-im.source".into(),
            idempotency_key: event_id.into(),
            payload: serde_json::json!({"event_id": event_id, "source_system": "aero-im.source"}),
            occurred_at: time::OffsetDateTime::now_utc(),
            attempts: 1,
            claim_token: uuid::Uuid::new_v4(),
            lease_expires_at: time::OffsetDateTime::now_utc() + time::Duration::minutes(1),
        }
    }
}
