//! Incoming webhook HTTP surface and durable, HMAC-signed outgoing dispatcher.
//! Credential, request-shaping and persistence primitives live in `aero-storage`.

#[cfg(test)]
#[path = "webhooks/audit_tests.rs"]
mod audit_tests;
mod runtime;

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use aero_auth::AuthUser;
use aero_common::metrics::{self, names};
use aero_common::{Block, Error as AeroError, RoomEvent, RoomId, WebhookId};
use aero_storage::webhook::{build_delivery_from_bytes, WebhookWriteError};
use aero_storage::{
    build_delivery, generate_secret, generate_token, hash_token, outcome_of,
    ConsumerEventReceiptRepo, OutgoingTarget, WebhookDelivery, WebhookDeliveryRepo, WebhookRepo,
    WebhookSender,
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use futures::{stream, StreamExt as _};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::error::ApiResult;
use crate::state::AppState;
use crate::task_shutdown::{self, NextOrCancelled};
use runtime::{DeliveryLimiter, DeliveryOutcome, DeliveryStage};

// ---------- Router ----------

/// Mount webhook ingress and room-scoped management routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        // Inbound: the token in the path IS the credential (no AuthUser).
        .route("/hooks/in/:token", post(incoming_post))
        // Management (AuthUser + room access).
        .route("/api/rooms/:id/webhooks", get(list_webhooks))
        .route("/api/rooms/:id/webhooks/incoming", post(create_incoming))
        .route("/api/rooms/:id/webhooks/outgoing", post(create_outgoing))
        .route(
            "/api/webhooks/incoming/:id",
            axum::routing::delete(revoke_incoming),
        )
        .route(
            "/api/webhooks/outgoing/:id",
            axum::routing::delete(revoke_outgoing),
        )
}

fn repo(s: &AppState) -> WebhookRepo {
    WebhookRepo::new(s.participants.pool().clone())
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

fn parse_webhook(s: &str) -> Result<WebhookId, AeroError> {
    WebhookId::from_str(s).map_err(|e| AeroError::Invalid(format!("webhook id: {e}")))
}

const MAX_OUTGOING_EVENT_FILTERS: usize = 32;
const MAX_EVENT_KIND_CHARS: usize = 64;
const MAX_WEBHOOK_LABEL_CHARS: usize = 64;
const WEBHOOK_EVENT_KINDS: &[&str] = &[
    "message",
    "edited",
    "deleted",
    "reaction",
    "read",
    "typing",
    "notify",
    "pin",
    "membership",
    "poll",
    "canvas_op",
    "message_seen",
    "interaction",
    "call",
];

fn normalize_webhook_label(raw: Option<&str>) -> Result<Option<String>, AeroError> {
    let Some(label) = raw.map(str::trim).filter(|label| !label.is_empty()) else {
        return Ok(None);
    };
    if label.chars().count() > MAX_WEBHOOK_LABEL_CHARS {
        return Err(AeroError::Invalid(format!(
            "label too long (max {MAX_WEBHOOK_LABEL_CHARS} chars)"
        )));
    }
    Ok(Some(label.to_owned()))
}

fn normalize_outgoing_events(raw: Option<&[String]>) -> Result<Vec<String>, AeroError> {
    let events = raw.unwrap_or_default();
    if events.len() > MAX_OUTGOING_EVENT_FILTERS {
        return Err(AeroError::Invalid(format!(
            "too many webhook events (max {MAX_OUTGOING_EVENT_FILTERS})"
        )));
    }

    let mut seen = HashSet::with_capacity(events.len());
    let mut normalized = Vec::with_capacity(events.len());
    for event in events {
        if event.chars().count() > MAX_EVENT_KIND_CHARS {
            return Err(AeroError::Invalid(format!(
                "webhook event kind too long (max {MAX_EVENT_KIND_CHARS} chars)"
            )));
        }
        let event = event.trim().to_ascii_lowercase();
        if !WEBHOOK_EVENT_KINDS.contains(&event.as_str()) {
            return Err(AeroError::Invalid(format!(
                "unknown webhook event kind `{event}`"
            )));
        }
        if seen.insert(event.clone()) {
            normalized.push(event);
        }
    }
    Ok(normalized)
}

fn map_webhook_write_error(error: WebhookWriteError, not_found: &'static str) -> AeroError {
    match error {
        WebhookWriteError::NotFound => AeroError::NotFound(not_found.into()),
        WebhookWriteError::FixedMembership => AeroError::Invalid(
            "incoming webhooks are not supported for direct or group-DM rooms".into(),
        ),
        WebhookWriteError::Forbidden => {
            AeroError::Forbidden("webhook authorization changed before commit".into())
        }
        WebhookWriteError::Storage(error) => AeroError::from(error),
    }
}

// ---------- Pure body parsing (unit-tested) ----------

/// The accepted inbound body: either `{ "text": "..." }` (rendered as a single
/// text block) or `{ "blocks": [...] }` (Aero blocks verbatim). `text` wins if
/// both are present.
#[derive(Deserialize)]
struct IncomingBody {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    blocks: Option<Vec<Block>>,
}

/// Turn an inbound body into the `Vec<Block>` to post. Pure + total so the
/// text-vs-blocks precedence and the empty-input rejection are unit-tested
/// without a DB.
///
/// # Errors
/// [`AeroError::Invalid`] when neither a non-empty `text` nor a non-empty
/// `blocks` is supplied (there's nothing to post).
fn blocks_from_body(body: IncomingBody) -> Result<Vec<Block>, AeroError> {
    if let Some(text) = body
        .text
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        return Ok(vec![Block::text(text)]);
    }
    match body.blocks {
        Some(blocks) if !blocks.is_empty() => Ok(blocks),
        _ => Err(AeroError::Invalid(
            "webhook body needs non-empty `text` or `blocks`".into(),
        )),
    }
}

// ---------- Inbound handler ----------

/// `POST /hooks/in/:token` — inbound message webhook. No `AuthUser`: the token is
/// the credential. Resolves the hook by `sha256(token)`; an unknown or revoked
/// token is a `404`. On success the body is posted into the bound room as the
/// hook's bot and `202 Accepted` is returned.
async fn incoming_post(
    State(s): State<AppState>,
    Path(token): Path<String>,
    Json(body): Json<IncomingBody>,
) -> ApiResult<StatusCode> {
    let r = repo(&s);
    let hook = r
        .find_incoming_by_token_hash(&hash_token(&token))
        .await
        .map_err(AeroError::from)?
        // Unknown token: 404 (same as revoked below) so a probe learns nothing.
        .ok_or_else(|| AeroError::NotFound("webhook".into()))?;
    if hook.revoked {
        return Err(AeroError::NotFound("webhook".into()).into());
    }
    let blocks = blocks_from_body(body)?;
    // The hook's bot was added to the room at creation, so send_message's
    // membership check passes.
    s.im.send_message(hook.bot_id, hook.room_id, blocks, None, None)
        .await?;
    Ok(StatusCode::ACCEPTED)
}

// ---------- Management handlers ----------

#[derive(Deserialize)]
struct CreateIncomingReq {
    #[serde(default)]
    label: Option<String>,
    /// Optional display name for the hook's bot participant.
    #[serde(default)]
    bot_name: Option<String>,
}

/// `POST /api/rooms/:id/webhooks/incoming` — create an inbound webhook. Mints a
/// dedicated bot participant (`kind='bot'`), adds it to the room (so its posts
/// pass the membership check), generates a one-time token, and returns
/// `{ id, token, url }`. The token is shown ONCE — only its hash is stored.
async fn create_incoming(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<CreateIncomingReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;

    // Dedicated bot participant for this hook (mirrors `routes::create_agent`).
    let bot_name = match req.bot_name.as_deref() {
        Some(name) => crate::routes::agents::validate_bot_name(name)?,
        None => "Webhook".to_owned(),
    };
    let label = normalize_webhook_label(req.label.as_deref())?;
    let token = generate_token();
    let created = repo(&s)
        .create_incoming(
            room,
            &bot_name,
            &hash_token(&token),
            label.as_deref(),
            auth.participant_id,
        )
        .await
        .map_err(|error| map_webhook_write_error(error, "room"))?;
    s.room_member_cache.invalidate(&room);

    let url = format!(
        "{}/hooks/in/{}",
        s.public_base_url.trim_end_matches('/'),
        token
    );
    Ok(Json(serde_json::json!({
        "id": created.webhook_id,
        // Shown exactly once; the server only stored its hash.
        "token": token,
        "url": url,
        "bot_id": created.bot_id,
    })))
}

#[derive(Deserialize)]
struct CreateOutgoingReq {
    url: String,
    #[serde(default)]
    events: Option<Vec<String>>,
    #[serde(default)]
    label: Option<String>,
}

/// Registration-time SSRF check shared with bot subscriptions. Transport repeats
/// it at connect time and pins every validated address.
pub(crate) async fn assert_webhook_url_safe(url: &str) -> Result<(), AeroError> {
    aero_storage::webhook::validate_webhook_url(url)
        .await
        .map_err(AeroError::Invalid)
}

/// Register an outbound room webhook and return its signing secret once.
async fn create_outgoing(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<CreateOutgoingReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;

    let url = req.url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(AeroError::Invalid("url must be http(s)".into()).into());
    }
    let events = normalize_outgoing_events(req.events.as_deref())?;
    let label = normalize_webhook_label(req.label.as_deref())?;
    // ReqwestSender repeats this validation and pins DNS on every delivery.
    assert_webhook_url_safe(url).await?;
    let secret = generate_secret();
    let id = repo(&s)
        .create_outgoing(
            room,
            url,
            &secret,
            &events,
            label.as_deref(),
            auth.participant_id,
        )
        .await
        .map_err(|error| map_webhook_write_error(error, "room"))?;
    Ok(Json(serde_json::json!({
        "id": id,
        // Shown once; used by the receiver to verify HMAC signatures.
        "secret": secret,
    })))
}

/// `GET /api/rooms/:id/webhooks` — list a room's webhooks (both directions),
/// never exposing tokens or secrets.
async fn list_webhooks(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let r = repo(&s);
    let incoming = r.list_incoming(room).await.map_err(AeroError::from)?;
    let outgoing = r.list_outgoing(room).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({
        "incoming": incoming,
        "outgoing": outgoing,
    })))
}

/// `DELETE /api/webhooks/incoming/:id` — revoke an inbound webhook (idempotent).
///
/// Storage resolves the GLOBAL webhook id, locks its workspace/room/actor edges,
/// and revalidates authorization in the same transaction as cleanup. Unknown
/// ids are `404`; actors outside the owning room are `403`.
async fn revoke_incoming(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<StatusCode> {
    let id = parse_webhook(&id_str)?;
    let (room, _bot) = repo(&s)
        .revoke_incoming_and_cleanup(id, auth.participant_id)
        .await
        .map_err(|error| map_webhook_write_error(error, "webhook"))?;
    s.room_member_cache.invalidate(&room);
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/webhooks/outgoing/:id` — revoke an outbound webhook (idempotent).
///
/// Same atomic authorization as [`revoke_incoming`]: the global id is resolved
/// and tenant-contained under locks in the storage transaction.
async fn revoke_outgoing(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<StatusCode> {
    let id = parse_webhook(&id_str)?;
    repo(&s)
        .revoke_outgoing(id, auth.participant_id)
        .await
        .map_err(|error| map_webhook_write_error(error, "webhook"))?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------- Outgoing dispatcher ----------

/// The bus subject for the event kind, used both for filter matching and logging.
/// Matches the `#[serde(tag = "kind", rename_all = "snake_case")]` discriminant
/// on [`RoomEvent`]. Pure, so the mapping is unit-tested.
#[must_use]
fn event_kind(event: &RoomEvent) -> &'static str {
    match event {
        RoomEvent::Message(_) => "message",
        RoomEvent::Edited(_) => "edited",
        RoomEvent::Recalled(_) => "recalled",
        RoomEvent::Deleted { .. } => "deleted",
        RoomEvent::Reaction { .. } => "reaction",
        RoomEvent::Read { .. } => "read",
        RoomEvent::Typing { .. } => "typing",
        RoomEvent::Notify { .. } | RoomEvent::NotifyBatch { .. } => "notify",
        RoomEvent::Pin { .. } => "pin",
        RoomEvent::Membership { .. } => "membership",
        RoomEvent::Poll { .. } => "poll",
        RoomEvent::CanvasOp { .. } => "canvas_op",
        RoomEvent::MessageSeen { .. } => "message_seen",
        RoomEvent::Interaction { .. } => "interaction",
        RoomEvent::Call(_) => "call",
    }
}

/// A stable correlation handle for an event, recorded as the delivery log's
/// `event_id` so an admin can tie a delivery row back to its source. Today only
/// `Message` carries a natural id; other kinds log `None`. Pure.
#[must_use]
fn event_correlation_id(event: &RoomEvent) -> Option<String> {
    match event {
        RoomEvent::Message(e) => Some(e.message.id.to_string()),
        RoomEvent::CanvasOp { op_id, .. } => Some(op_id.to_string()),
        _ => None,
    }
}

/// Whether an HTTP status counts as a successful delivery (any 2xx). Pure.
#[must_use]
fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

fn defer_deadline(now: i64, attempts: i32, not_before: Option<i64>) -> time::OffsetDateTime {
    let base = time::OffsetDateTime::from_unix_timestamp(now)
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    let retry_at = aero_storage::next_attempt_at(base, attempts.max(1));
    not_before
        .and_then(|timestamp| time::OffsetDateTime::from_unix_timestamp(timestamp).ok())
        .map_or(retry_at, |gate| retry_at.max(gate))
}

async fn release_claim(
    deliveries: &WebhookDeliveryRepo,
    delivery_id: aero_common::WebhookDeliveryId,
    claim_token: uuid::Uuid,
    reason: &str,
) -> bool {
    match deliveries.release_claim(delivery_id, claim_token).await {
        Ok(true) => true,
        Ok(false) => {
            warn!(
                delivery = %delivery_id,
                %claim_token,
                reason,
                "webhook delivery: release lost claim fence"
            );
            true
        }
        Err(error) => {
            warn!(
                ?error,
                delivery = %delivery_id,
                %claim_token,
                reason,
                "webhook delivery: failed to release untouched claim"
            );
            false
        }
    }
}

async fn defer_claim(
    deliveries: &WebhookDeliveryRepo,
    delivery_id: aero_common::WebhookDeliveryId,
    claim_token: uuid::Uuid,
    attempts: i32,
    now: i64,
    not_before: Option<i64>,
    reason: &str,
) -> bool {
    let deadline = defer_deadline(now, attempts, not_before);
    match deliveries
        .defer_claim(delivery_id, claim_token, deadline, reason)
        .await
    {
        Ok(true) => true,
        Ok(false) => {
            warn!(
                delivery = %delivery_id,
                %claim_token,
                reason,
                "webhook delivery: defer lost claim fence"
            );
            true
        }
        Err(error) => {
            warn!(
                ?error,
                delivery = %delivery_id,
                %claim_token,
                reason,
                "webhook delivery: failed to defer untouched claim"
            );
            false
        }
    }
}

async fn begin_attempt(
    deliveries: &WebhookDeliveryRepo,
    delivery_id: aero_common::WebhookDeliveryId,
    claim_token: uuid::Uuid,
) -> Option<i32> {
    match deliveries.begin_attempt(delivery_id, claim_token).await {
        Ok(Some(attempts)) => Some(attempts),
        Ok(None) => {
            warn!(
                delivery = %delivery_id,
                %claim_token,
                "webhook delivery: HTTP attempt suppressed after claim fence was lost"
            );
            None
        }
        Err(error) => {
            warn!(
                ?error,
                delivery = %delivery_id,
                %claim_token,
                "webhook delivery: failed to begin HTTP attempt"
            );
            None
        }
    }
}

async fn persist_delivery_result(
    deliveries: &WebhookDeliveryRepo,
    delivery_id: aero_common::WebhookDeliveryId,
    claim_token: uuid::Uuid,
    attempts: i32,
    result: &Result<aero_storage::DeliveryResponse, String>,
) {
    let stored = match result {
        Ok(response) if is_success(response.status) => {
            deliveries
                .mark_delivered(delivery_id, claim_token, i32::from(response.status))
                .await
        }
        Ok(response) => {
            deliveries
                .mark_failed_with_backoff(
                    delivery_id,
                    claim_token,
                    attempts,
                    Some(i32::from(response.status)),
                    &format!("HTTP {}", response.status),
                )
                .await
        }
        Err(error) => {
            deliveries
                .mark_failed_with_backoff(delivery_id, claim_token, attempts, None, error)
                .await
        }
    };
    match stored {
        Ok(true) => {}
        Ok(false) => warn!(
            delivery = %delivery_id,
            %claim_token,
            attempts,
            "webhook delivery: outcome dropped because claim fence was lost"
        ),
        Err(error) => {
            warn!(
            ?error,
                delivery = %delivery_id,
                %claim_token,
                attempts,
                "webhook delivery: outcome persistence failed; stale-pending recovery will retry"
            );
        }
    }
}

struct InitialDispatch<'a, S: WebhookSender + ?Sized> {
    repo: &'a WebhookRepo,
    deliveries: &'a WebhookDeliveryRepo,
    sender: &'a S,
    limiter: &'a DeliveryLimiter,
    cancel: &'a CancellationToken,
    body: &'a serde_json::Value,
    event_id: Option<&'a str>,
    room: RoomId,
    kind: &'static str,
    now: i64,
}

async fn dispatch_target<S: WebhookSender + ?Sized>(
    context: &InitialDispatch<'_, S>,
    target: OutgoingTarget,
) -> bool {
    let mut delivery = build_delivery(&target.url, &target.secret, context.body, context.now);
    if let Some(traceparent) = aero_common::telemetry::current_traceparent() {
        delivery
            .headers
            .push(("traceparent".to_owned(), traceparent));
    }
    let retry_headers = delivery.retry_headers();
    let claim = match context
        .deliveries
        .record_attempt(target.id, context.event_id, &delivery.body, &retry_headers)
        .await
    {
        Ok(Some(claim)) => claim,
        Ok(None) => {
            runtime::record_outcome(DeliveryStage::Initial, DeliveryOutcome::Deduplicated);
            return true;
        }
        Err(error) => {
            runtime::record_outcome(DeliveryStage::Initial, DeliveryOutcome::RecordError);
            warn!(
                ?error,
                room = %context.room,
                kind = context.kind,
                hook = %target.id,
                "webhook dispatch: durable claim record failed"
            );
            return false;
        }
    };
    if target.breaker.is_open_at(context.now) {
        metrics::inc_counter(names::WEBHOOK_BREAKER_OPEN_SKIPS_TOTAL, 1);
        runtime::record_outcome(DeliveryStage::Initial, DeliveryOutcome::BreakerOpen);
        return defer_claim(
            context.deliveries,
            claim.id,
            claim.claim_token,
            0,
            context.now,
            target.breaker.open_until,
            "circuit breaker open",
        )
        .await;
    }
    let Some(permit) = context
        .limiter
        .acquire(&target.url, DeliveryStage::Initial, context.cancel)
        .await
    else {
        return release_claim(
            context.deliveries,
            claim.id,
            claim.claim_token,
            "shutdown while waiting for initial delivery permit",
        )
        .await;
    };
    let Some(attempts) = begin_attempt(context.deliveries, claim.id, claim.claim_token).await
    else {
        // A successor owns the durable row (or a transient DB failure left this
        // claim pending for stale recovery). In either case this worker must not
        // POST.
        return true;
    };
    let result = permit.deliver(context.sender, &delivery).await;
    persist_delivery_result(
        context.deliveries,
        claim.id,
        claim.claim_token,
        attempts,
        &result,
    )
    .await;
    if let Err(error) = context
        .repo
        .apply_breaker_outcome(target.id, outcome_of(&result), context.now)
        .await
    {
        warn!(?error, hook = %target.id, "webhook breaker: persist failed");
    }
    true
}

async fn dispatch_event<S: WebhookSender + ?Sized>(
    repo: &WebhookRepo,
    deliveries: &WebhookDeliveryRepo,
    sender: &S,
    limiter: &DeliveryLimiter,
    cancel: &CancellationToken,
    event: &RoomEvent,
    now: i64,
) -> bool {
    let Some(room) = event.room_id() else {
        return true;
    };
    let kind = event_kind(event);
    let body = serde_json::to_value(event).unwrap_or(serde_json::Value::Null);
    let event_id = event_correlation_id(event);
    dispatch_named_event(
        repo,
        deliveries,
        sender,
        limiter,
        cancel,
        room,
        kind,
        &body,
        event_id.as_deref(),
        now,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_named_event<S: WebhookSender + ?Sized>(
    repo: &WebhookRepo,
    deliveries: &WebhookDeliveryRepo,
    sender: &S,
    limiter: &DeliveryLimiter,
    cancel: &CancellationToken,
    room: RoomId,
    kind: &'static str,
    body: &serde_json::Value,
    event_id: Option<&str>,
    now: i64,
) -> bool {
    let targets = tokio::select! {
        biased;
        () = cancel.cancelled() => return false,
        targets = repo.list_outgoing_for_room_event(room, kind) => targets,
    };
    let targets = match targets {
        Ok(targets) => targets,
        Err(error) => {
            warn!(?error, %room, kind, "webhook dispatch: target lookup failed");
            return false;
        }
    };
    let grouped = targets
        .into_iter()
        .map(|target| (target.url.clone(), target))
        .collect();
    let context = InitialDispatch {
        repo,
        deliveries,
        sender,
        limiter,
        cancel,
        body,
        event_id,
        room,
        kind,
        now,
    };
    let complete = AtomicBool::new(true);
    runtime::for_each_endpoint_bounded(limiter, grouped, |target| {
        let context = &context;
        let complete = &complete;
        async move {
            if !dispatch_target(context, target).await {
                complete.store(false, Ordering::Release);
            }
        }
    })
    .await;
    complete.load(Ordering::Acquire)
}

struct PreparedRetry {
    delivery: WebhookDelivery,
    target: OutgoingTarget,
}

async fn repark_unattempted(
    deliveries: &WebhookDeliveryRepo,
    delivery: &WebhookDelivery,
    now: i64,
    not_before: Option<i64>,
    reason: &str,
) {
    let _ = defer_claim(
        deliveries,
        delivery.id,
        delivery.claim_token,
        delivery.attempts,
        now,
        not_before,
        reason,
    )
    .await;
}

async fn prepare_redelivery(
    repo: &WebhookRepo,
    deliveries: &WebhookDeliveryRepo,
    delivery: WebhookDelivery,
    now: i64,
    cancel: &CancellationToken,
) -> Option<PreparedRetry> {
    if delivery.request_body.is_none() {
        runtime::record_outcome(DeliveryStage::Retry, DeliveryOutcome::RecordError);
        match deliveries
            .mark_dead(
                delivery.id,
                delivery.claim_token,
                None,
                "immutable request body is unavailable",
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => warn!(
                delivery = %delivery.id,
                claim_token = %delivery.claim_token,
                "webhook retry: missing-body DLQ settlement lost claim fence"
            ),
            Err(error) => warn!(
                ?error,
                delivery = %delivery.id,
                "webhook retry: failed to park delivery without request body"
            ),
        }
        return None;
    }
    let target = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            runtime::record_outcome(DeliveryStage::Retry, DeliveryOutcome::Cancelled);
            let _ = release_claim(
                deliveries,
                delivery.id,
                delivery.claim_token,
                "shutdown before target lookup",
            )
            .await;
            return None;
        }
        target = repo.outgoing_target(delivery.webhook_id) => target,
    };
    let target = match target {
        Ok(Some(target)) => target,
        Ok(None) => {
            runtime::record_outcome(DeliveryStage::Retry, DeliveryOutcome::TargetUnavailable);
            // Revocation is terminal: repeatedly deferring this claimed row
            // would manufacture a permanent failed↔pending zombie because no
            // supported path makes the hook active again. A concurrent hard
            // delete cascades the delivery, in which case the token-fenced
            // update simply observes zero rows.
            match deliveries
                .mark_dead(
                    delivery.id,
                    delivery.claim_token,
                    None,
                    "hook revoked or deleted",
                )
                .await
            {
                Ok(true) => {}
                Ok(false) => warn!(
                    delivery = %delivery.id,
                    claim_token = %delivery.claim_token,
                    "webhook retry: terminal target settlement lost claim fence"
                ),
                Err(error) => warn!(
                    ?error,
                    delivery = %delivery.id,
                    "webhook retry: failed to park revoked target"
                ),
            }
            return None;
        }
        Err(error) => {
            runtime::record_outcome(DeliveryStage::Retry, DeliveryOutcome::TargetUnavailable);
            warn!(?error, hook = %delivery.webhook_id, "webhook retry: target lookup failed");
            repark_unattempted(deliveries, &delivery, now, None, "target lookup failed").await;
            return None;
        }
    };
    if target.breaker.is_open_at(now) {
        metrics::inc_counter(names::WEBHOOK_BREAKER_OPEN_SKIPS_TOTAL, 1);
        runtime::record_outcome(DeliveryStage::Retry, DeliveryOutcome::BreakerOpen);
        repark_unattempted(
            deliveries,
            &delivery,
            now,
            target.breaker.open_until,
            "circuit breaker open",
        )
        .await;
        return None;
    }
    Some(PreparedRetry { delivery, target })
}

fn rebuild_retry_delivery(prepared: &PreparedRetry, now: i64) -> Option<aero_storage::Delivery> {
    let body = prepared.delivery.request_body.as_deref()?;
    Some(build_delivery_from_bytes(
        &prepared.target.url,
        &prepared.target.secret,
        body,
        &prepared.delivery.request_headers,
        now,
    ))
}

async fn redeliver_prepared<S: WebhookSender + ?Sized>(
    repo: &WebhookRepo,
    deliveries: &WebhookDeliveryRepo,
    sender: &S,
    limiter: &DeliveryLimiter,
    cancel: &CancellationToken,
    prepared: PreparedRetry,
    now: i64,
) {
    let Some(built) = rebuild_retry_delivery(&prepared, now) else {
        match deliveries
            .mark_dead(
                prepared.delivery.id,
                prepared.delivery.claim_token,
                None,
                "immutable request body unavailable",
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => warn!(
                delivery = %prepared.delivery.id,
                claim_token = %prepared.delivery.claim_token,
                "webhook retry: corrupt-row DLQ settlement lost claim fence"
            ),
            Err(error) => warn!(
                ?error,
                delivery = %prepared.delivery.id,
                "webhook retry: failed to park corrupt row"
            ),
        }
        return;
    };
    let Some(permit) = limiter
        .acquire(&prepared.target.url, DeliveryStage::Retry, cancel)
        .await
    else {
        let _ = release_claim(
            deliveries,
            prepared.delivery.id,
            prepared.delivery.claim_token,
            "shutdown while waiting for delivery permit",
        )
        .await;
        return;
    };
    let Some(attempts) = begin_attempt(
        deliveries,
        prepared.delivery.id,
        prepared.delivery.claim_token,
    )
    .await
    else {
        return;
    };
    let result = permit.deliver(sender, &built).await;
    persist_delivery_result(
        deliveries,
        prepared.delivery.id,
        prepared.delivery.claim_token,
        attempts,
        &result,
    )
    .await;
    if let Err(error) = repo
        .apply_breaker_outcome(prepared.target.id, outcome_of(&result), now)
        .await
    {
        warn!(
            ?error,
            hook = %prepared.target.id,
            "webhook breaker: persist failed"
        );
    }
}

async fn process_retry_batch<S: WebhookSender + ?Sized>(
    repo: &WebhookRepo,
    deliveries: &WebhookDeliveryRepo,
    sender: &S,
    limiter: &DeliveryLimiter,
    cancel: &CancellationToken,
    due: Vec<WebhookDelivery>,
    now: i64,
) {
    let prepared: Vec<_> = stream::iter(due)
        .map(|delivery| prepare_redelivery(repo, deliveries, delivery, now, cancel))
        .buffer_unordered(limiter.global_limit())
        .filter_map(|prepared| async move { prepared })
        .collect()
        .await;
    let grouped = prepared
        .into_iter()
        .map(|prepared| (prepared.target.url.clone(), prepared))
        .collect();
    runtime::for_each_endpoint_bounded(limiter, grouped, |prepared| {
        redeliver_prepared(repo, deliveries, sender, limiter, cancel, prepared, now)
    })
    .await;
}

#[cfg(test)]
async fn settle_bus_delivery(
    subscription: Box<dyn aero_bus::Subscription + Send>,
    complete: bool,
) -> aero_bus::traits::BusResult<()> {
    if complete {
        subscription.ack().await
    } else {
        subscription.nack().await
    }
}

/// Run the durable `im.room.*` outgoing-webhook consumer.
pub async fn run_webhook_dispatcher(state: AppState) -> anyhow::Result<()> {
    run_webhook_dispatcher_until_cancelled(state, CancellationToken::new()).await
}

/// Run the outgoing-webhook listener until `cancel` is triggered, finishing and
/// acknowledging any event already received before returning.
pub async fn run_webhook_dispatcher_until_cancelled(
    state: AppState,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    use aero_bus::EventBus;
    let bus: Arc<dyn EventBus> = state.bus.clone();
    let repo = WebhookRepo::new(state.participants.pool().clone());
    let deliveries = WebhookDeliveryRepo::new(state.pg.clone());
    let receipts = ConsumerEventReceiptRepo::new(state.pg.clone());
    let sender = aero_storage::ReqwestSender::new();
    let limiter = runtime::production_limiter();
    // Durable cursor resumes after reconnect; incomplete fanout is NAKed.
    loop {
        let subscribed = task_shutdown::subscribe_or_cancelled(
            &bus,
            "im.room.*",
            Some("aero-webhooks"),
            &cancel,
        )
        .await;
        let mut stream = match subscribed {
            None => return Ok(()),
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                warn!(error = %e, "webhook dispatcher subscribe failed; retrying");
                if task_shutdown::delay_or_cancelled(std::time::Duration::from_secs(1), &cancel)
                    .await
                {
                    return Ok(());
                }
                continue;
            }
        };
        info!("webhook dispatcher started");
        loop {
            let sub = match task_shutdown::next_or_cancelled(&mut stream, &cancel).await {
                NextOrCancelled::Item(sub) => sub,
                NextOrCancelled::Ended => break,
                NextOrCancelled::Cancelled => return Ok(()),
            };
            let event = serde_json::from_slice::<RoomEvent>(sub.payload());
            let handler_repo = &repo;
            let handler_deliveries = &deliveries;
            let handler_sender = &sender;
            let handler_limiter = &limiter;
            let handler_cancel = &cancel;
            let settled = crate::consumer_event_receipt::process(
                &receipts,
                "aero-webhooks",
                sub,
                || async move {
                    // Only new messages are dispatched today.
                    if let Ok(event @ RoomEvent::Message(_)) = event {
                        let now = time::OffsetDateTime::now_utc().unix_timestamp();
                        if !dispatch_event(
                            handler_repo,
                            handler_deliveries,
                            handler_sender,
                            handler_limiter,
                            handler_cancel,
                            &event,
                            now,
                        )
                        .await
                        {
                            anyhow::bail!("outgoing webhook fanout incomplete");
                        }
                    }
                    Ok(())
                },
            )
            .await;
            if !settled
                && task_shutdown::delay_or_cancelled(std::time::Duration::from_secs(1), &cancel)
                    .await
            {
                return Ok(());
            }
        }
        if cancel.is_cancelled() {
            return Ok(());
        }
        warn!("webhook dispatcher subscription stream ended; resubscribing");
        if task_shutdown::delay_or_cancelled(std::time::Duration::from_secs(1), &cancel).await {
            return Ok(());
        }
    }
}

/// Poll, claim and retry due webhook deliveries with bounded concurrency.
pub async fn run_webhook_retry_loop(state: AppState, interval_secs: u64) {
    run_webhook_retry_loop_until_cancelled(state, interval_secs, CancellationToken::new()).await;
}

/// Retry failed outgoing-webhook deliveries until `cancel` is triggered.
pub async fn run_webhook_retry_loop_until_cancelled(
    state: AppState,
    interval_secs: u64,
    cancel: CancellationToken,
) {
    let repo = WebhookRepo::new(state.participants.pool().clone());
    let deliveries = WebhookDeliveryRepo::new(state.pg.clone());
    let sender = aero_storage::ReqwestSender::new();
    let limiter = runtime::production_limiter();
    let period = std::time::Duration::from_secs(interval_secs.max(1));
    info!(interval_secs, "webhook retry loop started");
    loop {
        if task_shutdown::delay_or_cancelled(period, &cancel).await {
            return;
        }
        let now = time::OffsetDateTime::now_utc();
        let due = match deliveries.claim_due(now, 50).await {
            Ok(d) => d,
            Err(e) => {
                warn!(error = ?e, "webhook retry: claim_due failed");
                continue;
            }
        };
        if due.is_empty() {
            continue;
        }
        let stamp = now.unix_timestamp();
        process_retry_batch(&repo, &deliveries, &sender, &limiter, &cancel, due, stamp).await;
        if cancel.is_cancelled() {
            return;
        }
    }
}

#[cfg(test)]
#[path = "webhooks/tests.rs"]
mod tests;
