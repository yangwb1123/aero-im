//! Snaplink-authenticated external application notifications.
//!
//! Human installation management uses normal Aero `AuthUser` + workspace RBAC.
//! The publish/upload surfaces accept only RFC 9068 Snaplink access tokens from
//! `client_credentials`; a machine token is never converted into a participant.

use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
    sync::{Arc, OnceLock},
    time::Duration,
};

use aero_auth::{
    validate_client_credentials_token, validate_jwks_uri, AuthUser, ClientCredentialsTokenConfig,
    JwksKeyProvider,
};
use aero_common::{
    Block, Error as AeroError, FileKind, Notification, ParticipantId, RoomId, WorkspaceId,
};
use aero_im_core::{
    moderation_text, validate_blocks, KeywordModerator, ModerationVerdict, Moderator, PiiDetector,
};
use aero_storage::{
    integration::{
        IntegrationBlobClaim, IntegrationBlobCommit, IntegrationBlobCommitResolution,
        IntegrationBlobOutcome, IntegrationBlobProbe, IntegrationBlobQuotaReservation,
        IntegrationNotificationClaim,
    },
    AuditRepo, AutoModRuleRepo, IntegrationReplayProbe, IntegrationRepo, IntegrationTarget,
    NewBlob, NewIntegrationInstallation, NewIntegrationNotification, ParticipantRepo, SsoRepo,
    UpdateIntegrationInstallation, WorkspaceRepo,
};
use axum::{
    extract::{DefaultBodyLimit, Multipart, Path, RawQuery, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::sleep,
};
use uuid::Uuid;

use crate::{
    account_summary_binding::{
        AccountSummaryTargetVerifier, ACCOUNT_SUMMARY_PATH, ACCOUNT_SUMMARY_SCOPE,
    },
    error::{ApiError, ApiResult},
    state::AppState,
};

const REQUIRED_PUBLISH_SCOPE: &str = "aero.notify.publish";
const REQUIRED_ACCOUNT_SUMMARY_SCOPE: &str = ACCOUNT_SUMMARY_SCOPE;
const ACCOUNT_ID_HEADER: &str = "x-aero-account-id";
const CANONICAL_UID_HEADER: &str = "x-aero-canonical-uid";
const TENANT_ID_HEADER: &str = "x-aero-tenant-id";
const REGION_HEADER: &str = "x-aero-region";
const MAX_ACCOUNT_SUMMARY_QUERY_BYTES: usize = 8 * 1024;
const MAX_ACCOUNT_SUMMARY_ACCOUNT_ID_BYTES: usize = 512;
const MAX_ACCOUNT_SUMMARY_REGION_BYTES: usize = 128;
const MAX_ACCOUNT_SUMMARY_DATASETS: usize = 4;
const ACCOUNT_SUMMARY_BINDING_UNAVAILABLE: &str = "account summary target binding is unavailable";
const ACCOUNT_NOTIFICATION_LIMIT: i64 = 50;

fn account_notification_limit_usize() -> usize {
    usize::try_from(ACCOUNT_NOTIFICATION_LIMIT).expect("notification limit fits usize")
}
const MAX_MACHINE_TOKEN_BYTES: usize = 48 * 1024;
const MAX_NOTIFICATION_BODY_BYTES: usize = 512 * 1024;
const MAX_INTEGRATION_UPLOAD_BODY_BYTES: usize = 33 * 1024 * 1024;
const MAX_INTEGRATION_BLOB_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_INTEGRATION_UPLOAD_CONCURRENCY: usize = 4;
const MAX_INTEGRATION_UPLOAD_CONCURRENCY: usize = 32;
const INTEGRATION_UPLOAD_CONCURRENCY_ENV: &str = "AERO__INTEGRATIONS__UPLOAD_MAX_CONCURRENCY";
const INTEGRATION_RATE_RETRY_AFTER_SECS: &str = "60";
const CLAIM_REPROBE_INITIAL: Duration = Duration::from_millis(40);
const CLAIM_REPROBE_MAX: Duration = Duration::from_millis(640);
const CLAIM_REPROBE_LIMIT: u8 = 7;
static INTEGRATION_JWKS: OnceLock<JwksKeyProvider> = OnceLock::new();
static INTEGRATION_UPLOAD_GATE: OnceLock<IntegrationUploadGate> = OnceLock::new();
mod upload;

#[derive(Debug)]
struct IntegrationApiError(AeroError);

impl<E: Into<AeroError>> From<E> for IntegrationApiError {
    fn from(error: E) -> Self {
        Self(error.into())
    }
}

impl IntoResponse for IntegrationApiError {
    fn into_response(self) -> Response {
        let rate_limited = matches!(&self.0, AeroError::RateLimited);
        let mut response = ApiError(self.0).into_response();
        if rate_limited {
            response.headers_mut().insert(
                header::RETRY_AFTER,
                HeaderValue::from_static(INTEGRATION_RATE_RETRY_AFTER_SECS),
            );
        }
        response
    }
}

type IntegrationApiResult<T> = Result<T, IntegrationApiError>;

#[derive(Clone)]
struct IntegrationUploadGate {
    semaphore: Arc<Semaphore>,
}

impl IntegrationUploadGate {
    fn new(max_concurrency: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrency)),
        }
    }

    async fn acquire(&self) -> Result<OwnedSemaphorePermit, AeroError> {
        self.semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|error| {
                AeroError::Internal(anyhow::anyhow!(
                    "integration upload concurrency gate closed: {error}"
                ))
            })
    }

    #[cfg(test)]
    fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }
}

fn integration_upload_gate() -> &'static IntegrationUploadGate {
    INTEGRATION_UPLOAD_GATE.get_or_init(|| {
        IntegrationUploadGate::new(integration_upload_concurrency_from_value(
            env_value(INTEGRATION_UPLOAD_CONCURRENCY_ENV).as_deref(),
        ))
    })
}

fn integration_upload_concurrency_from_value(value: Option<&str>) -> usize {
    value
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_INTEGRATION_UPLOAD_CONCURRENCY)
        .clamp(1, MAX_INTEGRATION_UPLOAD_CONCURRENCY)
}

#[derive(Debug, Clone, Copy)]
struct ClaimBackoff {
    reprobes: u8,
    delay: Duration,
}

impl ClaimBackoff {
    const fn new() -> Self {
        Self {
            reprobes: 0,
            delay: CLAIM_REPROBE_INITIAL,
        }
    }

    fn next_delay(&mut self) -> Option<Duration> {
        if self.reprobes >= CLAIM_REPROBE_LIMIT {
            return None;
        }
        let delay = self.delay;
        self.reprobes += 1;
        self.delay = self.delay.saturating_mul(2).min(CLAIM_REPROBE_MAX);
        Some(delay)
    }
}

fn request_pending_response() -> Response {
    let mut response = (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "code": "integration_request_pending",
            "msg": "an identical integration request is still processing; retry with the same Idempotency-Key",
        })),
    )
        .into_response();
    response
        .headers_mut()
        .insert("retry-after", HeaderValue::from_static("1"));
    response
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(ACCOUNT_SUMMARY_PATH, get(account_summary))
        .route(
            "/api/workspaces/:workspace_id/integrations",
            get(list_installations).post(create_installation),
        )
        .route(
            "/api/workspaces/:workspace_id/integrations/:installation_id",
            axum::routing::patch(update_installation).delete(revoke_installation),
        )
        .route(
            "/api/integrations/v1/installations/:installation_id/notifications",
            post(publish_notification).layer(DefaultBodyLimit::max(MAX_NOTIFICATION_BODY_BYTES)),
        )
        .route(
            "/api/integrations/v1/installations/:installation_id/blobs",
            post(upload::upload_blob)
                .layer(DefaultBodyLimit::max(MAX_INTEGRATION_UPLOAD_BODY_BYTES)),
        )
}

#[derive(Clone)]
struct IntegrationAuthConfig {
    token: ClientCredentialsTokenConfig,
    jwks_uri: String,
}

impl IntegrationAuthConfig {
    fn from_env(required_scope: &str) -> Result<Self, AeroError> {
        let issuer =
            env_value("AERO__INTEGRATIONS__ISSUER").or_else(|| env_value("AERO__OIDC__ISSUER"));
        let audience = env_value("AERO__INTEGRATIONS__AUDIENCE");
        let jwks_uri =
            env_value("AERO__INTEGRATIONS__JWKS_URI").or_else(|| env_value("AERO__OIDC__JWKS_URI"));
        let (Some(issuer), Some(audience), Some(jwks_uri)) = (issuer, audience, jwks_uri) else {
            return Err(AeroError::Invalid(
                "Snaplink integration auth is not fully configured".into(),
            ));
        };
        validate_jwks_uri(&jwks_uri).map_err(|_| {
            AeroError::Invalid("Snaplink integration JWKS URI is not allowed".into())
        })?;
        Ok(Self {
            token: ClientCredentialsTokenConfig {
                issuer,
                audience,
                required_scopes: vec![required_scope.into()],
            },
            jwks_uri,
        })
    }
}

#[derive(Clone)]
struct MachinePrincipal {
    issuer: String,
    client_id: String,
}

async fn authenticate_machine(
    headers: &HeaderMap,
    required_scope: &str,
) -> Result<MachinePrincipal, AeroError> {
    let token = bearer_token(headers)?;
    let config = IntegrationAuthConfig::from_env(required_scope)?;
    let provider = INTEGRATION_JWKS.get_or_init(|| JwksKeyProvider::new(config.jwks_uri.clone()));
    let claims = validate_client_credentials_token(token, &config.token, provider)
        .await
        .map_err(|_| {
            tracing::debug!("Snaplink machine token rejected");
            AeroError::Unauthorized("invalid integration access token".into())
        })?;
    Ok(MachinePrincipal {
        issuer: config.token.issuer,
        client_id: claims.client_id,
    })
}

#[derive(Debug)]
struct AccountSummaryRequest {
    /// Opaque Aero ID account identifier. This is deliberately not parsed as
    /// or converted to an Aero `ParticipantId`.
    account_id: String,
    region: Option<String>,
    datasets: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthorizedAccountSummaryTarget {
    account_id: String,
    canonical_uid: String,
    tenant_id: String,
    region: String,
    datasets: BTreeSet<String>,
    jti: String,
    exp: u64,
}

#[derive(Serialize)]
struct AccountSummaryResponse {
    source_region: String,
    version: i64,
    generated_at: String,
    complete: bool,
    datasets: BTreeMap<String, serde_json::Value>,
    sources: Vec<serde_json::Value>,
    memberships: Vec<serde_json::Value>,
}

/// Verify the owner-approved assertion before any source identity or
/// projection lookup. Unsigned headers are checked only as exact consistency
/// fields after the signed target has been verified.
async fn authorize_account_summary_target(
    binding: Option<&AccountSummaryTargetVerifier>,
    principal: &MachinePrincipal,
    request: &AccountSummaryRequest,
    source_region: &str,
    headers: &HeaderMap,
) -> Result<AuthorizedAccountSummaryTarget, AeroError> {
    let binding =
        binding.ok_or_else(|| AeroError::Upstream(ACCOUNT_SUMMARY_BINDING_UNAVAILABLE.into()))?;
    let expected_datasets = if request.datasets.is_empty() {
        all_account_summary_dataset_names()
            .into_iter()
            .map(str::to_owned)
            .collect()
    } else {
        request.datasets.clone()
    };
    let verified = binding
        .verify_request(
            &principal.client_id,
            &request.account_id,
            request.region.as_deref(),
            &expected_datasets,
            headers,
        )
        .await?;
    let target = AuthorizedAccountSummaryTarget {
        account_id: verified.account_id.clone(),
        canonical_uid: verified.canonical_uid.clone(),
        tenant_id: verified.tenant_id.clone(),
        region: verified.region.clone(),
        datasets: verified.datasets.clone(),
        jti: verified.jti.clone(),
        exp: verified.exp,
    };
    validate_bound_account_summary_target(request, &target, source_region)?;
    validate_account_summary_legacy_headers(headers, &target)?;
    // Replay consumption is the last gate. No source identity, participant,
    // workspace, notification, projection, or audit lookup may precede it.
    binding.consume_replay(&verified).await?;
    Ok(target)
}

fn validate_bound_account_summary_target(
    request: &AccountSummaryRequest,
    target: &AuthorizedAccountSummaryTarget,
    source_region: &str,
) -> Result<(), AeroError> {
    let expected_datasets = if request.datasets.is_empty() {
        all_account_summary_dataset_names()
            .into_iter()
            .map(str::to_owned)
            .collect()
    } else {
        request.datasets.clone()
    };
    let query_region_matches = request
        .region
        .as_deref()
        .is_some_and(|region| region == target.region);
    if target.account_id != request.account_id
        || target.datasets != expected_datasets
        || target.region != source_region
        || !query_region_matches
        || !valid_account_summary_identifier(
            &target.canonical_uid,
            MAX_ACCOUNT_SUMMARY_ACCOUNT_ID_BYTES,
        )
        || !valid_account_summary_identifier(
            &target.tenant_id,
            MAX_ACCOUNT_SUMMARY_ACCOUNT_ID_BYTES,
        )
    {
        return Err(AeroError::Forbidden(
            "account summary target binding mismatch".into(),
        ));
    }
    Ok(())
}

async fn account_summary(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> IntegrationApiResult<Json<AccountSummaryResponse>> {
    let principal = authenticate_machine(&headers, REQUIRED_ACCOUNT_SUMMARY_SCOPE).await?;
    let request = parse_account_summary_query(raw_query.as_deref())?;
    // This gate is intentionally before every identity and projection lookup.
    // Equal unsigned headers cannot authorize a target; they are only required
    // consistency inputs after the owner-approved assertion is verified.
    let source_region = env_value("AERO__ACCOUNT_SOURCE__REGION")
        .ok_or_else(|| AeroError::Upstream(ACCOUNT_SUMMARY_BINDING_UNAVAILABLE.into()))?;
    let target = authorize_account_summary_target(
        state.account_summary_binding.as_deref(),
        &principal,
        &request,
        &source_region,
        &headers,
    )
    .await?;
    let canonical_uid = target.canonical_uid.as_str();
    let human_issuer = env_value("AERO__OIDC__ISSUER").ok_or_else(|| {
        AeroError::Invalid("Snaplink human identity issuer is not configured".into())
    })?;

    let participant_id = SsoRepo::new(state.pg.clone())
        .find_participant(&human_issuer, canonical_uid)
        .await?;
    let now = time::OffsetDateTime::now_utc();
    let generated_at = now
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let version = i64::try_from(now.unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX);
    let requested = requested_account_datasets(&target.datasets);
    let mut datasets = BTreeMap::new();
    let mut sources = Vec::new();
    let mut memberships = Vec::new();

    if let Some(participant_id) = participant_id {
        let participant = ParticipantRepo::new(state.pg.clone())
            .get(participant_id)
            .await?
            .ok_or_else(|| AeroError::NotFound("participant".into()))?;
        let workspaces = WorkspaceRepo::new(state.pg.clone())
            .list_for_participant(participant_id)
            .await?;
        if requested.contains("aero-im.profile") {
            datasets.insert(
                "aero-im.profile".into(),
                serde_json::json!({
                    "display_name": participant.display_name,
                    "avatar_url": participant.avatar_url,
                    "status": "active",
                }),
            );
        }
        if requested.contains("aero-im.workspaces") {
            datasets.insert(
                "aero-im.workspaces".into(),
                serde_json::json!({
                    "items": workspaces.iter().map(|workspace| serde_json::json!({
                        "id": workspace.id, "name": workspace.name, "slug": workspace.slug,
                    })).collect::<Vec<_>>()
                }),
            );
        }
        if requested.contains("aero-im.activity_summary") {
            datasets.insert(
                "aero-im.activity_summary".into(),
                serde_json::json!({"workspace_count": workspaces.len()}),
            );
        }
        if requested.contains("aero-im.notifications") {
            let notifications = state
                .notifications
                .list(
                    participant_id,
                    None,
                    false,
                    Some(ACCOUNT_NOTIFICATION_LIMIT + 1),
                )
                .await?;
            let unread_count = state.notifications.unread_count(participant_id).await?;
            datasets.insert(
                "aero-im.notifications".into(),
                notification_dataset(notifications, unread_count, "active"),
            );
        }
        sources.push(serde_json::json!({
            "source_account_id": participant_id,
            "scope_type": "account",
            "scope_id": target.account_id,
            "status": "active",
            "data": {},
        }));
        memberships.extend(workspaces.iter().map(|workspace| {
            serde_json::json!({
                "scope_type": "workspace",
                "scope_id": workspace.id,
                "source_member_id": participant_id,
                "role": "member",
                "status": "active",
                "data": {},
            })
        }));
        AuditRepo::new(state.pg.clone())
            .append(
                WorkspaceId::nil(),
                None,
                "integration.account_summary.read",
                Some(&participant_id.to_string()),
                serde_json::json!({"client_id": principal.client_id, "datasets": requested}),
            )
            .await?;
    } else {
        for dataset in requested {
            let value = if dataset == "aero-im.notifications" {
                notification_dataset(Vec::new(), 0, "not_found")
            } else {
                serde_json::json!({"status": "not_found"})
            };
            datasets.insert(dataset.to_owned(), value);
        }
    }

    Ok(Json(AccountSummaryResponse {
        source_region,
        version,
        generated_at,
        complete: true,
        datasets,
        sources,
        memberships,
    }))
}

fn valid_account_summary_query_encoding(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}

fn valid_account_summary_identifier(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value == value.trim()
        && !value.contains('\u{FFFD}')
        && value
            .chars()
            .all(|character| !character.is_control() && !character.is_whitespace())
}

fn parse_account_summary_query(raw: Option<&str>) -> Result<AccountSummaryRequest, AeroError> {
    let raw = raw.unwrap_or_default();
    if raw.len() > MAX_ACCOUNT_SUMMARY_QUERY_BYTES {
        return Err(AeroError::Invalid(
            "account summary query is too large".into(),
        ));
    }
    if !valid_account_summary_query_encoding(raw)
        || raw.is_empty()
        || raw.starts_with('&')
        || raw.ends_with('&')
        || raw.contains("&&")
    {
        return Err(AeroError::Invalid(
            "account summary query is malformed".into(),
        ));
    }
    let mut account_id = None;
    let mut region = None;
    let mut last_dataset: Option<String> = None;
    let mut datasets = BTreeSet::new();
    for (name, value) in form_urlencoded::parse(raw.as_bytes()) {
        match name.as_ref() {
            "account_id" => {
                if account_id.is_some() {
                    return Err(AeroError::Invalid(
                        "account_id must appear exactly once".into(),
                    ));
                }
                let value = value.into_owned();
                if !valid_account_summary_identifier(&value, MAX_ACCOUNT_SUMMARY_ACCOUNT_ID_BYTES) {
                    return Err(AeroError::Invalid("account_id is invalid".into()));
                }
                account_id = Some(value);
            }
            "region" => {
                if region.is_some() {
                    return Err(AeroError::Invalid("region must appear at most once".into()));
                }
                let value = value.into_owned();
                if !valid_account_summary_identifier(&value, MAX_ACCOUNT_SUMMARY_REGION_BYTES) {
                    return Err(AeroError::Invalid("region is invalid".into()));
                }
                region = Some(value);
            }
            "dataset" => {
                let value = value.into_owned();
                if !matches!(
                    value.as_str(),
                    "aero-im.profile"
                        | "aero-im.workspaces"
                        | "aero-im.activity_summary"
                        | "aero-im.notifications"
                ) {
                    return Err(AeroError::Invalid(
                        "unsupported account summary dataset".into(),
                    ));
                }
                if last_dataset
                    .as_deref()
                    .is_some_and(|last| last.as_bytes() >= value.as_bytes())
                {
                    return Err(AeroError::Invalid(
                        "dataset parameters must be in canonical order".into(),
                    ));
                }
                last_dataset = Some(value.clone());
                if !datasets.insert(value) {
                    return Err(AeroError::Invalid("dataset must appear only once".into()));
                }
            }
            _ => {
                return Err(AeroError::Invalid(
                    "unknown account summary query parameter".into(),
                ));
            }
        }
    }
    if datasets.len() > MAX_ACCOUNT_SUMMARY_DATASETS {
        return Err(AeroError::Invalid(
            "too many account summary datasets".into(),
        ));
    }
    Ok(AccountSummaryRequest {
        account_id: account_id
            .ok_or_else(|| AeroError::Invalid("account_id is required".into()))?,
        region: Some(region.ok_or_else(|| AeroError::Invalid("region is required".into()))?),
        datasets,
    })
}

fn all_account_summary_dataset_names() -> [&'static str; 4] {
    [
        "aero-im.profile",
        "aero-im.workspaces",
        "aero-im.activity_summary",
        "aero-im.notifications",
    ]
}

fn requested_account_datasets(values: &BTreeSet<String>) -> BTreeSet<&str> {
    if values.is_empty() {
        return BTreeSet::from(all_account_summary_dataset_names());
    }
    values.iter().map(String::as_str).collect()
}

fn notification_dataset(
    mut notifications: Vec<Notification>,
    unread_count: u64,
    source_account_status: &str,
) -> serde_json::Value {
    let limit = account_notification_limit_usize();
    let truncated = notifications.len() > limit;
    notifications.truncate(limit);
    let items = notifications
        .iter()
        .map(|notification| {
            serde_json::json!({
                "notification_id": notification.id,
                "room_id": notification.room_id,
                "message_id": notification.message_id,
                "kind": notification.kind,
                "actor_id": notification.actor_id,
                "created_at": notification.created_at,
                "read_at": notification.read_at,
                "aggregate_count": notification.aggregate_count,
                "importance_score": notification.importance_score,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "items": items,
        "unread_count": unread_count,
        "truncated": truncated,
        "source_account_status": source_account_status,
    })
}

fn required_account_header<'a>(
    headers: &'a HeaderMap,
    name: &str,
    max_bytes: usize,
) -> Result<&'a str, AeroError> {
    let mut values = headers.get_all(name).iter();
    let value = values
        .next()
        .ok_or_else(|| AeroError::Invalid(format!("{name} is required")))?;
    if values.next().is_some() {
        return Err(AeroError::Invalid(format!(
            "exactly one {name} header is required"
        )));
    }
    let value = value
        .to_str()
        .map_err(|_| AeroError::Invalid(format!("{name} must be valid ASCII")))?;
    if !valid_account_summary_identifier(value, max_bytes) {
        return Err(AeroError::Invalid(format!("{name} is invalid")));
    }
    Ok(value)
}

/// Requires the signed target's identity headers as exact wire consistency
/// fields. They are never accepted as authorization without the assertion.
fn validate_account_summary_legacy_headers(
    headers: &HeaderMap,
    target: &AuthorizedAccountSummaryTarget,
) -> Result<(), AeroError> {
    for (name, expected, max_bytes) in [
        (
            ACCOUNT_ID_HEADER,
            target.account_id.as_str(),
            MAX_ACCOUNT_SUMMARY_ACCOUNT_ID_BYTES,
        ),
        (
            CANONICAL_UID_HEADER,
            target.canonical_uid.as_str(),
            MAX_ACCOUNT_SUMMARY_ACCOUNT_ID_BYTES,
        ),
        (
            TENANT_ID_HEADER,
            target.tenant_id.as_str(),
            MAX_ACCOUNT_SUMMARY_ACCOUNT_ID_BYTES,
        ),
        (
            REGION_HEADER,
            target.region.as_str(),
            MAX_ACCOUNT_SUMMARY_REGION_BYTES,
        ),
    ] {
        if required_account_header(headers, name, max_bytes)? != expected {
            return Err(AeroError::Invalid(
                "account summary target binding header mismatch".into(),
            ));
        }
    }
    // X-Aero-Actor-UID is deliberately not authorization material: the target
    // assertion has no actor claim, so this diagnostic value is ignored.
    Ok(())
}

fn bearer_token(headers: &HeaderMap) -> Result<&str, AeroError> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let raw = values
        .next()
        .ok_or_else(|| AeroError::Unauthorized("Bearer token required".into()))?;
    if values.next().is_some() {
        return Err(AeroError::Unauthorized(
            "exactly one Authorization header is required".into(),
        ));
    }
    let raw = raw
        .to_str()
        .map_err(|_| AeroError::Unauthorized("Bearer token required".into()))?;
    if raw.len() > MAX_MACHINE_TOKEN_BYTES {
        return Err(AeroError::Unauthorized(
            "integration access token is too large".into(),
        ));
    }
    let (scheme, token) = raw
        .split_once(' ')
        .ok_or_else(|| AeroError::Unauthorized("Bearer token required".into()))?;
    if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() || token != token.trim() {
        return Err(AeroError::Unauthorized("Bearer token required".into()));
    }
    Ok(token)
}

fn env_value(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn parse_workspace(raw: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(raw.trim())
        .map_err(|error| AeroError::Invalid(format!("workspace id: {error}")))
}

fn parse_installation(raw: &str) -> Result<Uuid, AeroError> {
    Uuid::parse_str(raw.trim())
        .map_err(|error| AeroError::Invalid(format!("installation id: {error}")))
}

async fn assert_admin(
    state: &AppState,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Result<(), AeroError> {
    let role = state
        .workspaces
        .effective_member_role(workspace, participant)
        .await?
        .ok_or_else(|| AeroError::Forbidden("workspace admin required".into()))?;
    if role.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden("workspace admin required".into()))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateInstallationReq {
    bot_id: ParticipantId,
    client_id: String,
    name: String,
    /// Optional administrator-selected human OIDC namespace. When omitted,
    /// the server's trusted `AERO__OIDC__ISSUER` is used.
    #[serde(default)]
    user_identity_issuer: Option<String>,
    #[serde(default)]
    allow_user_dm: bool,
    #[serde(default)]
    room_ids: Vec<RoomId>,
}

async fn create_installation(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(workspace_raw): Path<String>,
    Json(req): Json<CreateInstallationReq>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let workspace = parse_workspace(&workspace_raw)?;
    assert_admin(&state, workspace, auth.participant_id).await?;
    let config = IntegrationAuthConfig::from_env(REQUIRED_PUBLISH_SCOPE)?;
    let user_identity_issuer =
        select_user_identity_issuer(req.user_identity_issuer, env_value("AERO__OIDC__ISSUER"))?;
    let name = req.name.trim();
    if name.is_empty() || name.len() > 128 {
        return Err(AeroError::Invalid("name must contain 1 to 128 bytes".into()).into());
    }
    let installation = IntegrationRepo::new(state.pg.clone())
        .create(NewIntegrationInstallation {
            workspace_id: workspace,
            bot_id: req.bot_id,
            issuer: config.token.issuer,
            user_identity_issuer,
            client_id: req.client_id,
            name: name.to_owned(),
            allow_user_dm: req.allow_user_dm,
            room_ids: req.room_ids,
            created_by: auth.participant_id,
        })
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::to_value(installation).map_err(AeroError::from)?),
    ))
}

async fn list_installations(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(workspace_raw): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let workspace = parse_workspace(&workspace_raw)?;
    assert_admin(&state, workspace, auth.participant_id).await?;
    let installations = IntegrationRepo::new(state.pg.clone())
        .list(workspace)
        .await?;
    Ok(Json(serde_json::json!({ "installations": installations })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateInstallationReq {
    #[serde(default)]
    bot_id: Option<ParticipantId>,
    #[serde(default)]
    rotate_to_current_issuer: bool,
    /// Administrator-authenticated rotation of the human OIDC namespace. It
    /// never comes from the machine bearer or its claims.
    #[serde(default)]
    user_identity_issuer: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    allow_user_dm: Option<bool>,
    #[serde(default)]
    room_ids: Option<Vec<RoomId>>,
}

async fn update_installation(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((workspace_raw, installation_raw)): Path<(String, String)>,
    Json(req): Json<UpdateInstallationReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let workspace = parse_workspace(&workspace_raw)?;
    let installation = parse_installation(&installation_raw)?;
    assert_admin(&state, workspace, auth.participant_id).await?;
    let name = req.name.map(|name| name.trim().to_owned());
    if name
        .as_ref()
        .is_some_and(|name| name.is_empty() || name.len() > 128)
    {
        return Err(AeroError::Invalid("name must contain 1 to 128 bytes".into()).into());
    }
    let issuer = if req.rotate_to_current_issuer {
        Some(
            IntegrationAuthConfig::from_env(REQUIRED_PUBLISH_SCOPE)?
                .token
                .issuer,
        )
    } else {
        None
    };
    let user_identity_issuer = req
        .user_identity_issuer
        .map(|issuer| select_user_identity_issuer(Some(issuer), None))
        .transpose()?;
    let updated = IntegrationRepo::new(state.pg.clone())
        .update(
            workspace,
            installation,
            auth.participant_id,
            UpdateIntegrationInstallation {
                bot_id: req.bot_id,
                issuer,
                user_identity_issuer,
                client_id: req.client_id,
                name,
                active: req.active,
                allow_user_dm: req.allow_user_dm,
                room_ids: req.room_ids,
            },
        )
        .await?;
    Ok(Json(
        serde_json::to_value(updated).map_err(AeroError::from)?,
    ))
}

fn select_user_identity_issuer(
    explicit_admin_value: Option<String>,
    configured_oidc_issuer: Option<String>,
) -> Result<String, AeroError> {
    let issuer = explicit_admin_value
        .or(configured_oidc_issuer)
        .ok_or_else(|| {
            AeroError::Invalid(
                "Snaplink human identity issuer is not configured; set AERO__OIDC__ISSUER or provide user_identity_issuer as a workspace administrator"
                    .into(),
            )
        })?;
    if issuer.is_empty()
        || issuer != issuer.trim()
        || issuer.len() > 2_048
        || issuer.chars().any(char::is_control)
    {
        return Err(AeroError::Invalid(
            "user_identity_issuer must be a non-blank exact value of at most 2048 bytes".into(),
        ));
    }
    Ok(issuer)
}

async fn revoke_installation(
    State(state): State<AppState>,
    auth: AuthUser,
    Path((workspace_raw, installation_raw)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let workspace = parse_workspace(&workspace_raw)?;
    let installation = parse_installation(&installation_raw)?;
    assert_admin(&state, workspace, auth.participant_id).await?;
    IntegrationRepo::new(state.pg.clone())
        .update(
            workspace,
            installation,
            auth.participant_id,
            UpdateIntegrationInstallation {
                active: Some(false),
                ..UpdateIntegrationInstallation::default()
            },
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum NotificationTargetReq {
    Room { room_id: RoomId },
    SnaplinkUser { subject: String },
}

impl NotificationTargetReq {
    fn into_storage(self) -> IntegrationTarget {
        match self {
            Self::Room { room_id } => IntegrationTarget::Room(room_id),
            Self::SnaplinkUser { subject } => IntegrationTarget::SnaplinkUser(subject),
        }
    }
}

#[derive(Deserialize)]
struct PublishNotificationReq {
    target: NotificationTargetReq,
    blocks: Vec<Block>,
}

async fn publish_notification(
    State(state): State<AppState>,
    Path(installation_raw): Path<String>,
    headers: HeaderMap,
    Json(req): Json<PublishNotificationReq>,
) -> IntegrationApiResult<Response> {
    let installation_id = parse_installation(&installation_raw)?;
    let principal = authenticate_machine(&headers, REQUIRED_PUBLISH_SCOPE).await?;
    let idempotency_key = required_idempotency_key(&headers)?;
    let target = req.target.into_storage();
    let request_hash = notification_request_hash(&target, &req.blocks)?;
    // Pure structural validation can fail before creating durable lease state.
    validate_blocks(&req.blocks).map_err(AeroError::from)?;
    let repo = IntegrationRepo::new(state.pg.clone());
    let probe = IntegrationReplayProbe {
        installation_id,
        issuer: principal.issuer.clone(),
        client_id: principal.client_id.clone(),
        idempotency_key,
        request_hash,
        target: target.clone(),
    };
    let mut backoff = ClaimBackoff::new();
    let (lease_token, prepared) = loop {
        match repo.claim_notification(&probe).await? {
            IntegrationNotificationClaim::Replay(outcome) => {
                return Ok(publish_response(&state, outcome).await);
            }
            IntegrationNotificationClaim::Pending => {
                let Some(delay) = backoff.next_delay() else {
                    return Ok(request_pending_response());
                };
                sleep(delay).await;
            }
            IntegrationNotificationClaim::Acquired {
                lease_token,
                target,
            } => break (lease_token, target),
        }
    };

    let mut resolved_for_cleanup = None;
    let result: Result<_, AeroError> = async {
        assert_machine_content_policy(&state, prepared.installation.workspace_id, &req.blocks)
            .await?;
        // Charge the trusted installation workspace before a DM can exist.
        crate::ws_rate::check_ws_rate(&state, prepared.installation.workspace_id).await?;
        let resolved = repo.materialize_target(prepared).await?;
        resolved_for_cleanup = Some(resolved.clone());
        state
            .im
            .assert_external_message_send_preflight(
                resolved.installation.bot_id,
                resolved.room_id,
                &req.blocks,
            )
            .await?;
        let slowmode = crate::message_send_policy::reserve_slowmode(
            &state,
            resolved.installation.bot_id,
            resolved.room_id,
        )
        .await?;
        let notification = NewIntegrationNotification {
            installation_id,
            issuer: principal.issuer,
            client_id: principal.client_id,
            idempotency_key,
            request_hash,
            target: resolved.target.clone(),
            room_id: resolved.room_id,
            recipient: resolved.recipient,
            blocks: req.blocks,
            traceparent: aero_common::telemetry::current_traceparent(),
            lease_token: Some(lease_token),
        };
        slowmode.finish(repo.publish(notification).await).await
    }
    .await;

    match result {
        Ok(outcome) => Ok(publish_response(&state, outcome).await),
        Err(error) => {
            cleanup_failed_request(
                &repo,
                resolved_for_cleanup.as_ref(),
                installation_id,
                idempotency_key,
                lease_token,
                false,
            )
            .await;
            Err(error.into())
        }
    }
}

async fn assert_machine_content_policy(
    state: &AppState,
    workspace: WorkspaceId,
    blocks: &[Block],
) -> Result<(), AeroError> {
    if let Some(moderator) = KeywordModerator::from_env() {
        if let ModerationVerdict::Block(reason) = moderator.check(blocks) {
            return Err(AeroError::Invalid(reason));
        }
    }
    let text = moderation_text(blocks);
    if let Some(detector) = PiiDetector::from_env() {
        let kinds = detector.scan(&text);
        if !kinds.is_empty() {
            let tags = kinds
                .iter()
                .map(|kind| kind.tag())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(AeroError::Invalid(format!(
                "message blocked: it appears to contain sensitive personal information ({tags})"
            )));
        }
    }
    let rules = AutoModRuleRepo::new(state.pg.clone())
        .list_for_enforcement(workspace)
        .await?;
    let lowercase = text.to_lowercase();
    if rules.iter().any(|rule| rule.matches_lowercase(&lowercase)) {
        return Err(AeroError::Invalid("blocked by auto-mod rule".into()));
    }
    Ok(())
}

async fn publish_response(
    state: &AppState,
    outcome: aero_storage::IntegrationPublishOutcome,
) -> Response {
    if let Err(error) = state.im.dispatch_event_outbox_id(outcome.outbox_id).await {
        tracing::warn!(?error, message_id = %outcome.message.id, "integration fast outbox dispatch failed");
    }
    if let Err(error) = state
        .im
        .dispatch_message_side_effects_for(outcome.message.id)
        .await
    {
        tracing::warn!(?error, message_id = %outcome.message.id, "integration side-effect dispatch failed");
    }

    let status = if outcome.deduplicated {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    let location = format!("/api/messages/{}", outcome.message.id);
    let mut response = (
        status,
        Json(serde_json::json!({
            "message": outcome.message,
            "deduplicated": outcome.deduplicated,
        })),
    )
        .into_response();
    response.headers_mut().insert(
        "idempotency-replayed",
        HeaderValue::from_static(if outcome.deduplicated {
            "true"
        } else {
            "false"
        }),
    );
    if let Ok(location) = HeaderValue::from_str(&location) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    response
}

fn required_idempotency_key(headers: &HeaderMap) -> Result<Uuid, AeroError> {
    let mut values = headers.get_all("idempotency-key").iter();
    let value = values
        .next()
        .ok_or_else(|| AeroError::Invalid("Idempotency-Key UUID is required".into()))?;
    if values.next().is_some() {
        return Err(AeroError::Invalid(
            "exactly one Idempotency-Key is required".into(),
        ));
    }
    let value = value
        .to_str()
        .map_err(|_| AeroError::Invalid("Idempotency-Key must be an ASCII UUID".into()))?;
    Uuid::parse_str(value.trim())
        .map_err(|_| AeroError::Invalid("Idempotency-Key must be a UUID".into()))
}

fn notification_request_hash(
    target: &IntegrationTarget,
    blocks: &[Block],
) -> Result<[u8; 32], AeroError> {
    let encoded = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "target_kind": target.kind(),
        "target_key": target.key(),
        "blocks": blocks,
    }))?;
    Ok(Sha256::digest(encoded).into())
}

async fn cleanup_failed_request(
    repo: &IntegrationRepo,
    resolved: Option<&aero_storage::ResolvedIntegrationTarget>,
    installation: Uuid,
    key: Uuid,
    lease_token: Uuid,
    blob: bool,
) {
    if let Some(resolved) = resolved {
        if let Err(error) = repo.cleanup_empty_dm(resolved).await {
            tracing::warn!(?error, room_id = %resolved.room_id, "failed to clean empty integration DM");
        }
    }
    let released = if blob {
        repo.release_blob_claim(installation, key, lease_token)
            .await
    } else {
        repo.release_notification_claim(installation, key, lease_token)
            .await
    };
    if let Err(error) = released {
        tracing::warn!(?error, %installation, %key, "failed to release integration request lease");
    }
}

#[cfg(test)]
mod tests;
