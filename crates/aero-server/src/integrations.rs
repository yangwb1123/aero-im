//! Snaplink-authenticated external application notifications.
//!
//! Human installation management uses normal Aero `AuthUser` + workspace RBAC.
//! The publish/upload surfaces accept only RFC 9068 Snaplink access tokens from
//! `client_credentials`; a machine token is never converted into a participant.

use std::{
    str::FromStr,
    sync::{Arc, OnceLock},
    time::Duration,
};

use aero_auth::{
    validate_client_credentials_token, validate_jwks_uri, AuthUser, ClientCredentialsTokenConfig,
    JwksKeyProvider,
};
use aero_common::{Block, Error as AeroError, FileKind, ParticipantId, RoomId, WorkspaceId};
use aero_im_core::{
    moderation_text, validate_blocks, KeywordModerator, ModerationVerdict, Moderator, PiiDetector,
};
use aero_storage::{
    integration::{
        IntegrationBlobClaim, IntegrationBlobCommit, IntegrationBlobCommitResolution,
        IntegrationBlobOutcome, IntegrationBlobProbe, IntegrationBlobQuotaReservation,
        IntegrationNotificationClaim,
    },
    AutoModRuleRepo, IntegrationReplayProbe, IntegrationRepo, IntegrationTarget, NewBlob,
    NewIntegrationInstallation, NewIntegrationNotification, UpdateIntegrationInstallation,
};
use axum::{
    extract::{DefaultBodyLimit, Multipart, Path, State},
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
    error::{ApiError, ApiResult},
    state::AppState,
};

const REQUIRED_PUBLISH_SCOPE: &str = "aero.notify.publish";
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
mod tests {
    use super::*;

    #[test]
    fn bearer_and_idempotency_headers_are_strict() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer token"),
        );
        assert_eq!(bearer_token(&headers).unwrap(), "token");
        headers.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Basic attacker"),
        );
        assert!(bearer_token(&headers).is_err());
        headers.remove(header::AUTHORIZATION);
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer token"),
        );
        headers.insert("idempotency-key", HeaderValue::from_static("not-a-uuid"));
        assert!(required_idempotency_key(&headers).is_err());
        let key = Uuid::new_v4();
        headers.insert(
            "idempotency-key",
            HeaderValue::from_str(&key.to_string()).unwrap(),
        );
        assert_eq!(required_idempotency_key(&headers).unwrap(), key);
        headers.append(
            "idempotency-key",
            HeaderValue::from_str(&Uuid::new_v4().to_string()).unwrap(),
        );
        assert!(required_idempotency_key(&headers).is_err());
    }

    #[test]
    fn request_hash_binds_target_and_blocks() {
        let room = IntegrationTarget::Room(RoomId::new());
        let base = notification_request_hash(&room, &[Block::text("hello")]).unwrap();
        assert_eq!(
            base,
            notification_request_hash(&room, &[Block::text("hello")]).unwrap()
        );
        assert_ne!(
            base,
            notification_request_hash(&room, &[Block::text("changed")]).unwrap()
        );
        assert_ne!(
            base,
            notification_request_hash(
                &IntegrationTarget::SnaplinkUser("user-1".into()),
                &[Block::text("hello")],
            )
            .unwrap()
        );
    }

    #[test]
    fn installation_user_dm_policy_is_opt_in() {
        let request: CreateInstallationReq = serde_json::from_value(serde_json::json!({
            "bot_id": ParticipantId::new(),
            "client_id": "erp-client",
            "name": "ERP",
            "room_ids": [],
        }))
        .unwrap();
        assert!(!request.allow_user_dm);
        assert!(request.user_identity_issuer.is_none());

        let enabled: CreateInstallationReq = serde_json::from_value(serde_json::json!({
            "bot_id": ParticipantId::new(),
            "client_id": "erp-client",
            "name": "ERP",
            "user_identity_issuer": "https://human-sso.example",
            "allow_user_dm": true,
        }))
        .unwrap();
        assert!(enabled.allow_user_dm);
        assert_eq!(
            enabled.user_identity_issuer.as_deref(),
            Some("https://human-sso.example")
        );

        let update: UpdateInstallationReq = serde_json::from_value(serde_json::json!({
            "rotate_to_current_issuer": true,
            "user_identity_issuer": "https://rotated-human-sso.example"
        }))
        .unwrap();
        assert!(update.rotate_to_current_issuer);
        assert_eq!(
            update.user_identity_issuer.as_deref(),
            Some("https://rotated-human-sso.example")
        );
        assert!(
            serde_json::from_value::<UpdateInstallationReq>(serde_json::json!({
                "issuer": "https://attacker-controlled.example"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<CreateInstallationReq>(serde_json::json!({
                "bot_id": ParticipantId::new(),
                "client_id": "erp-client",
                "name": "ERP",
                "issuer": "https://attacker-controlled.example"
            }))
            .is_err()
        );
    }

    #[test]
    fn human_identity_issuer_comes_only_from_trusted_admin_or_oidc_config() {
        assert_eq!(
            select_user_identity_issuer(None, Some("https://configured-human-sso.example".into()))
                .unwrap(),
            "https://configured-human-sso.example"
        );
        assert_eq!(
            select_user_identity_issuer(
                Some("https://admin-selected-human-sso.example".into()),
                Some("https://configured-human-sso.example".into())
            )
            .unwrap(),
            "https://admin-selected-human-sso.example"
        );
        assert!(select_user_identity_issuer(None, None).is_err());
        assert!(select_user_identity_issuer(Some(" bad".into()), None).is_err());
        assert!(select_user_identity_issuer(Some("https://bad\nissuer".into()), None).is_err());
    }

    #[test]
    fn pending_claim_reprobes_are_bounded_and_back_off() {
        let mut backoff = ClaimBackoff::new();
        let mut delays = Vec::new();
        while let Some(delay) = backoff.next_delay() {
            delays.push(delay);
        }
        assert_eq!(delays.len(), usize::from(CLAIM_REPROBE_LIMIT));
        assert_eq!(delays[0], CLAIM_REPROBE_INITIAL);
        assert_eq!(delays.last().copied(), Some(CLAIM_REPROBE_MAX));
        assert!(delays.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(backoff.next_delay().is_none());

        let response = request_pending_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(response.headers().get("retry-after").unwrap(), "1");
    }

    #[test]
    fn integration_rate_limit_response_has_retry_after_without_changing_other_errors() {
        let response = IntegrationApiError::from(AeroError::RateLimited).into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response.headers().get(header::RETRY_AFTER).unwrap(),
            INTEGRATION_RATE_RETRY_AFTER_SECS
        );

        let response =
            IntegrationApiError::from(AeroError::Invalid("bad request".into())).into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response.headers().get(header::RETRY_AFTER).is_none());
    }

    #[test]
    fn integration_upload_concurrency_is_bounded() {
        assert_eq!(
            integration_upload_concurrency_from_value(None),
            DEFAULT_INTEGRATION_UPLOAD_CONCURRENCY
        );
        assert_eq!(integration_upload_concurrency_from_value(Some("0")), 1);
        assert_eq!(integration_upload_concurrency_from_value(Some("7")), 7);
        assert_eq!(
            integration_upload_concurrency_from_value(Some("999999")),
            MAX_INTEGRATION_UPLOAD_CONCURRENCY
        );
        assert_eq!(
            integration_upload_concurrency_from_value(Some("invalid")),
            DEFAULT_INTEGRATION_UPLOAD_CONCURRENCY
        );
    }

    #[tokio::test]
    async fn integration_upload_gate_limits_concurrency_and_releases_permits() {
        let gate = IntegrationUploadGate::new(1);
        let first = gate.acquire().await.unwrap();
        assert_eq!(gate.available_permits(), 0);

        let waiting_gate = gate.clone();
        let mut waiting = tokio::spawn(async move { waiting_gate.acquire().await.unwrap() });
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut waiting)
                .await
                .is_err()
        );

        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("waiting upload must acquire a released permit")
            .unwrap();
        assert_eq!(gate.available_permits(), 0);
        drop(second);
        assert_eq!(gate.available_permits(), 1);

        let final_permit = gate.acquire().await.unwrap();
        drop(final_permit);
        assert_eq!(gate.available_permits(), 1);
    }
}
