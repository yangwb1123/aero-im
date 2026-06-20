//! Webhooks HTTP surface + outgoing dispatcher (integration plane).
//!
//! Two directions, both backed by [`aero_storage::WebhookRepo`] (migration
//! 0013):
//!
//! * **Incoming** — `POST /hooks/in/:token` carries no [`AuthUser`]; the token
//!   *is* the credential. We resolve the hook by `sha256(token)`, and if active
//!   post the body (`{text}` or `{blocks}`) into the bound room as the hook's
//!   dedicated bot participant. Unknown/revoked tokens are rejected with `404` so
//!   a probe can't distinguish "never existed" from "revoked".
//! * **Outgoing** — the room-scoped management routes (all `AuthUser` + room
//!   access) create/list/revoke hooks; [`run_webhook_dispatcher`] subscribes to
//!   the bus and POSTs each new message to that room's active outgoing hooks,
//!   HMAC-signed via [`aero_storage::ReqwestSender`].
//!
//! Thin handlers: the signing, token hashing, request shaping and SQL all live in
//! `aero-storage`; this module is routing + the bot-membership wiring that lets a
//! hook's bot pass [`aero_im_core::ImService::send_message`]'s membership check.

use std::str::FromStr;
use std::sync::Arc;

use aero_auth::AuthUser;
use aero_common::{Block, Error as AeroError, ParticipantKind, RoomEvent, RoomId, WebhookId};
use aero_common::metrics::{self, names};
use aero_storage::{
    build_delivery, hash_token, generate_secret, generate_token, outcome_of, WebhookDeliveryRepo,
    WebhookRepo, WebhookSender,
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use futures::StreamExt;
use serde::Deserialize;
use tracing::{info, warn};

use crate::error::ApiResult;
use crate::state::AppState;

// ---------- Router ----------

/// Mount the webhook routes. Folded into the main router by
/// [`crate::routes::build`]; kept here next to the dispatcher and the pure
/// request-shaping it depends on.
pub fn routes() -> Router<AppState> {
    Router::new()
        // Inbound: the token in the path IS the credential (no AuthUser).
        .route("/hooks/in/:token", post(incoming_post))
        // Management (AuthUser + room access).
        .route("/api/rooms/:id/webhooks", get(list_webhooks))
        .route("/api/rooms/:id/webhooks/incoming", post(create_incoming))
        .route("/api/rooms/:id/webhooks/outgoing", post(create_outgoing))
        .route("/api/webhooks/incoming/:id", axum::routing::delete(revoke_incoming))
        .route("/api/webhooks/outgoing/:id", axum::routing::delete(revoke_outgoing))
}

/// Construct the webhook repo from the shared pool. The `AppState` in this branch
/// exposes the pool via `participants.pool()` (repos are cheap `Arc<PgPool>`
/// wrappers), so no new `AppState` field is needed.
fn repo(s: &AppState) -> WebhookRepo {
    WebhookRepo::new(s.participants.pool().clone())
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

fn parse_webhook(s: &str) -> Result<WebhookId, AeroError> {
    WebhookId::from_str(s).map_err(|e| AeroError::Invalid(format!("webhook id: {e}")))
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
    if let Some(text) = body.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        return Ok(vec![Block::text(text)]);
    }
    match body.blocks {
        Some(blocks) if !blocks.is_empty() => Ok(blocks),
        _ => Err(AeroError::Invalid("webhook body needs non-empty `text` or `blocks`".into())),
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
    s.im.send_message(hook.bot_id, hook.room_id, blocks, None, None).await?;
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
    let bot_name = req
        .bot_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("Webhook");
    let bot = s
        .participants
        .create_bot(bot_name, ParticipantKind::Bot, Some(auth.participant_id), None)
        .await
        .map_err(AeroError::from)?;
    // Add the bot to the room so `send_message(bot, room, ...)` passes.
    s.rooms.add_member(room, bot.id).await.map_err(AeroError::from)?;

    let token = generate_token();
    let id = repo(&s)
        .create_incoming(
            room,
            bot.id,
            &hash_token(&token),
            req.label.as_deref(),
            auth.participant_id,
        )
        .await
        .map_err(AeroError::from)?;

    let url = format!("{}/hooks/in/{}", s.public_base_url.trim_end_matches('/'), token);
    Ok(Json(serde_json::json!({
        "id": id,
        // Shown exactly once; the server only stored its hash.
        "token": token,
        "url": url,
        "bot_id": bot.id,
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

/// `POST /api/rooms/:id/webhooks/outgoing` — register an outbound webhook for the
/// room. Generates a per-hook signing `secret` and returns `{ id, secret }` (the
/// secret is shown once; the receiver uses it to verify `X-Aero-Signature`).
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
    let events = req.events.unwrap_or_default();
    let secret = generate_secret();
    let id = repo(&s)
        .create_outgoing(room, url, &secret, &events, req.label.as_deref(), auth.participant_id)
        .await
        .map_err(AeroError::from)?;
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
/// The path carries only the GLOBAL webhook id, so authorization resolves the
/// hook's owning room and asserts the caller's access to it (mirroring
/// `create_incoming` / `list_webhooks`). Without this any authenticated user
/// could revoke another room's hook by id (IDOR). An unknown id is `404`.
async fn revoke_incoming(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<StatusCode> {
    let id = parse_webhook(&id_str)?;
    let r = repo(&s);
    let (room, bot) = r
        .incoming_room_and_bot(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("webhook".into()))?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    r.revoke_incoming(id).await.map_err(AeroError::from)?;
    // Cross-feature cleanup: the dedicated bot created for this hook
    // (`create_incoming`) should leave the room once the hook is revoked — it has
    // no credential and can no longer post, so a lingering member row is just stale
    // state. Idempotent (DELETE ... WHERE) and best-effort: a cleanup miss must not
    // fail the revoke the caller already authorized.
    if let Err(e) = s.rooms.remove_member(room, bot).await {
        tracing::warn!(error = ?e, %room, %bot, "revoke_incoming: webhook bot room-cleanup failed");
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/webhooks/outgoing/:id` — revoke an outbound webhook (idempotent).
///
/// Same authorization as [`revoke_incoming`]: resolve the hook's owning room
/// (via [`WebhookRepo::outgoing_room`]) and assert caller access before revoking,
/// so a webhook id alone can't break another room's integration. `404` if unknown.
async fn revoke_outgoing(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<StatusCode> {
    let id = parse_webhook(&id_str)?;
    let r = repo(&s);
    let room = r
        .outgoing_room(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("webhook".into()))?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    r.revoke_outgoing(id).await.map_err(AeroError::from)?;
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
        RoomEvent::Deleted { .. } => "deleted",
        RoomEvent::Reaction { .. } => "reaction",
        RoomEvent::Read { .. } => "read",
        RoomEvent::Typing { .. } => "typing",
        RoomEvent::Notify { .. } => "notify",
        RoomEvent::NotifyBatch { .. } => "notify",
        RoomEvent::Pin { .. } => "pin",
        RoomEvent::Membership { .. } => "membership",
        RoomEvent::Poll { .. } => "poll",
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
        _ => None,
    }
}

/// Whether an HTTP status counts as a successful delivery (any 2xx). Pure.
#[must_use]
fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

/// Deliver one room event to every active outgoing hook for its room whose filter
/// matches the event kind, recording each attempt in the delivery log so failures
/// retry with backoff and exhausted ones land in the DLQ.
///
/// Per target: record a `pending` row (attempt 1) → send → on 2xx `mark_delivered`,
/// otherwise `mark_failed_with_backoff` (which parks at `dead` once the cap is
/// reached). A bookkeeping error (recording the row) is logged and the send is
/// skipped — we never deliver something we can't track. Generic over the
/// [`WebhookSender`] seam so the fan-out is testable with a `FakeSender`.
async fn dispatch_event<S: WebhookSender + ?Sized>(
    repo: &WebhookRepo,
    deliveries: &WebhookDeliveryRepo,
    sender: &S,
    event: &RoomEvent,
    now: i64,
) {
    let Some(room) = event.room_id() else { return };
    let kind = event_kind(event);
    let targets = match repo.list_outgoing_for_room_event(room, kind).await {
        Ok(t) => t,
        Err(e) => {
            warn!(error = ?e, %room, kind, "webhook dispatch: target lookup failed");
            return;
        }
    };
    if targets.is_empty() {
        return;
    }
    // Sign the SAME JSON the receiver gets, so it can re-verify the signature.
    let body = serde_json::to_value(event).unwrap_or(serde_json::Value::Null);
    let event_id = event_correlation_id(event);
    for target in targets {
        // Circuit breaker: skip an endpoint whose breaker is open (it's been
        // failing) until the cooldown elapses, so a chronically-down receiver isn't
        // hammered on every event. A skip records no attempt and burns no connection.
        if target.breaker.is_open_at(now) {
            metrics::inc_counter(names::WEBHOOK_BREAKER_OPEN_SKIPS_TOTAL, 1);
            continue;
        }
        // Claim + record the (first) attempt up front so even a crash mid-send
        // leaves a durable trace. The claim is IDEMPOTENT on (webhook_id, event_id):
        // `None` means this exact event→endpoint was already delivered (a JetStream
        // redelivery after a crash-before-ack), so skip the duplicate external POST.
        let delivery_id = match deliveries.record_attempt(target.id, event_id.as_deref()).await {
            Ok(Some(id)) => id,
            Ok(None) => continue,
            Err(e) => {
                warn!(error = ?e, %room, kind, hook = %target.id, "webhook dispatch: record failed");
                continue;
            }
        };
        let mut delivery = build_delivery(&target.url, &target.secret, &body, now);
        // Propagate the W3C trace context on HTTP EGRESS so a receiver continues the
        // same distributed trace (build_delivery stays pure — the ambient span is
        // read here at the send site, not inside it). Inbound + the NATS bus already
        // propagate; this closes the outbound-HTTP leg.
        if let Some(tp) = aero_common::telemetry::current_traceparent() {
            delivery.headers.push(("traceparent".to_string(), tp));
        }
        let result = sender.deliver(&delivery).await;
        // Delivery-log bookkeeping keys on the numeric status (2xx = delivered, else
        // failed-with-backoff); the breaker (below) reads the same response's
        // Retry-After.
        match &result {
            Ok(resp) if is_success(resp.status) => {
                if let Err(e) = deliveries.mark_delivered(delivery_id, i32::from(resp.status)).await {
                    warn!(error = ?e, %room, kind, "webhook delivery: mark_delivered failed");
                }
            }
            Ok(resp) => {
                let status = resp.status;
                warn!(%room, kind, url = %target.url, status, "webhook delivery non-2xx");
                let _ = deliveries
                    .mark_failed_with_backoff(
                        delivery_id,
                        1,
                        Some(i32::from(status)),
                        &format!("HTTP {status}"),
                    )
                    .await;
            }
            Err(e) => {
                warn!(%room, kind, url = %target.url, error = %e, "webhook delivery failed");
                let _ = deliveries
                    .mark_failed_with_backoff(delivery_id, 1, None, e)
                    .await;
            }
        }
        // Fold the outcome into this endpoint's breaker ATOMICALLY (row-locked
        // read-fold-write), so concurrent/batched deliveries to the same endpoint
        // can't lose updates — undercount failures, reset a just-opened gate, or
        // shorten a 429 cooldown. Persists only on a real change internally.
        if let Err(e) = repo.apply_breaker_outcome(target.id, outcome_of(&result), now).await {
            warn!(error = ?e, hook = %target.id, "webhook breaker: persist failed");
        }
    }
}

/// Re-send one already-claimed delivery (a `pending` row the retry loop bumped
/// from `failed`). On 2xx `mark_delivered`; otherwise `mark_failed_with_backoff`
/// with the post-claim `attempts`, which re-parks it at `dead` once the cap is
/// reached. Best-effort + generic over the sender seam.
async fn redeliver<S: WebhookSender + ?Sized>(
    repo: &WebhookRepo,
    deliveries: &WebhookDeliveryRepo,
    sender: &S,
    delivery: &aero_storage::WebhookDelivery,
    now: i64,
) {
    // Resolve the hook's current url+secret; a revoked/deleted hook can't be
    // re-sent, so mark the attempt failed (it will eventually die out).
    let Some(target) = (match repo.outgoing_target(delivery.webhook_id).await {
        Ok(t) => t,
        Err(e) => {
            warn!(error = ?e, hook = %delivery.webhook_id, "webhook retry: target lookup failed");
            return;
        }
    }) else {
        let _ = deliveries
            .mark_failed_with_backoff(delivery.id, delivery.attempts, None, "hook revoked or deleted")
            .await;
        return;
    };
    // Circuit breaker: if the endpoint's breaker is open, defer this retry without
    // sending (re-park with backoff) so the down receiver isn't hammered by the
    // retry loop either. Sustained outages still park the delivery at `dead`.
    if target.breaker.is_open_at(now) {
        metrics::inc_counter(names::WEBHOOK_BREAKER_OPEN_SKIPS_TOTAL, 1);
        let _ = deliveries
            .mark_failed_with_backoff(delivery.id, delivery.attempts, None, "circuit breaker open")
            .await;
        return;
    }
    // Re-build a fresh, freshly-signed delivery for `now` (the original body is not
    // retained; the signature would be stale anyway). We re-send an empty retry
    // marker body keyed by the recorded event_id so the receiver can correlate.
    let body = serde_json::json!({ "retry": true, "event_id": delivery.event_id });
    let built = build_delivery(&target.url, &target.secret, &body, now);
    let result = sender.deliver(&built).await;
    // Numeric status drives the delivery-log state (2xx = delivered, else
    // failed-with-backoff); the breaker (below) folds in the same response's
    // Retry-After.
    match &result {
        Ok(resp) if is_success(resp.status) => {
            let _ = deliveries.mark_delivered(delivery.id, i32::from(resp.status)).await;
        }
        Ok(resp) => {
            let status = resp.status;
            let _ = deliveries
                .mark_failed_with_backoff(
                    delivery.id,
                    delivery.attempts,
                    Some(i32::from(status)),
                    &format!("HTTP {status}"),
                )
                .await;
        }
        Err(e) => {
            let _ = deliveries
                .mark_failed_with_backoff(delivery.id, delivery.attempts, None, e)
                .await;
        }
    }
    // A retry-loop attempt feeds the breaker too (atomically — the retry loop can
    // process many failed deliveries for the SAME endpoint in one tick, so a
    // read-fold-write here would otherwise undercount them all to a single +1).
    if let Err(e) = repo.apply_breaker_outcome(target.id, outcome_of(&result), now).await {
        warn!(error = ?e, hook = %target.id, "webhook breaker: persist failed");
    }
}

/// Background loop: subscribe to `im.room.*` and POST each new `Message` event to
/// its room's active outgoing webhooks (HMAC-signed). Mirrors
/// [`crate::ws::run_bus_listener`]'s subscribe/ack pattern but with an ephemeral
/// consumer name distinct from the WS listener so the two don't compete for the
/// same durable cursor. Started once per process at boot (see the bin spawn line
/// reported by the integrator).
///
/// Scope: only [`RoomEvent::Message`] is delivered today (the common "new
/// message" integration); the per-target filter still gates by kind, so adding
/// more kinds later is a one-line change to the match below.
///
/// # Errors
/// Returns the subscribe error if the bus subscription cannot be established.
pub async fn run_webhook_dispatcher(state: AppState) -> anyhow::Result<()> {
    use aero_bus::EventBus;
    let bus: Arc<dyn EventBus> = state.bus.clone();
    let repo = WebhookRepo::new(state.participants.pool().clone());
    let deliveries = WebhookDeliveryRepo::new(state.pg.clone());
    let sender = aero_storage::ReqwestSender::new();
    // Resubscribe across NATS reconnects so a dropped subscription stream never
    // permanently stops webhook delivery (the same fan-out black-hole fixed for the
    // WS listeners in `ws.rs`). The durable consumer "aero-webhooks" resumes from
    // its committed cursor, and every event is acked, so nothing is re-sent.
    loop {
        let mut stream = match bus.subscribe("im.room.*", Some("aero-webhooks")).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "webhook dispatcher subscribe failed; retrying");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        info!("webhook dispatcher started");
        while let Some(sub) = stream.next().await {
            if let Ok(event) = serde_json::from_slice::<RoomEvent>(sub.payload()) {
                // Only new messages are dispatched today (see the doc note above).
                if matches!(event, RoomEvent::Message(_)) {
                    let now = time::OffsetDateTime::now_utc().unix_timestamp();
                    dispatch_event(&repo, &deliveries, &sender, &event, now).await;
                }
            }
            // Broadcast-style consumer: always ack so the cursor advances regardless
            // of delivery outcome (webhook delivery is best-effort, not a work queue).
            let _ = sub.ack().await;
        }
        warn!("webhook dispatcher subscription stream ended; resubscribing");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// Background retry loop: every `interval_secs`, claim due `failed` deliveries
/// ([`WebhookDeliveryRepo::claim_due`]) and re-send each via [`redeliver`] — on
/// success they go `delivered`, otherwise they back off again or, at the cap,
/// land in the DLQ. Polling-based (no bus subscription) since retries are
/// time-driven, not event-driven. Started once per process at boot (see the bin
/// spawn line reported by the integrator); runs until the process exits.
pub async fn run_webhook_retry_loop(state: AppState, interval_secs: u64) {
    let repo = WebhookRepo::new(state.participants.pool().clone());
    let deliveries = WebhookDeliveryRepo::new(state.pg.clone());
    let sender = aero_storage::ReqwestSender::new();
    let period = std::time::Duration::from_secs(interval_secs.max(1));
    info!(interval_secs, "webhook retry loop started");
    loop {
        tokio::time::sleep(period).await;
        let now = time::OffsetDateTime::now_utc();
        // Claim a bounded batch of due deliveries (claim_due bumps attempts).
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
        for delivery in due {
            redeliver(&repo, &deliveries, &sender, &delivery, stamp).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::{Message, MessageEnvelope, MessageId, ParticipantId};
    use aero_storage::FakeSender;

    fn message_event(room: RoomId) -> RoomEvent {
        let msg = Message {
            id: MessageId::new(),
            room_id: room,
            sender_id: ParticipantId::new(),
            blocks: vec![Block::text("hi")],
            reply_to: None,
            metadata: serde_json::Value::Null,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            edited_at: None,
            deleted_at: None,
            expires_at: None,
        };
        RoomEvent::Message(MessageEnvelope { message: msg, recipients: Vec::new() })
    }

    // ----- blocks_from_body -----

    #[test]
    fn body_text_becomes_single_text_block() {
        let blocks = blocks_from_body(IncomingBody {
            text: Some("  hello  ".into()),
            blocks: None,
        })
        .unwrap();
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Text { content, .. } => assert_eq!(content, "hello"),
            other => panic!("expected text block, got {other:?}"),
        }
    }

    #[test]
    fn body_blocks_pass_through_and_text_wins_over_blocks() {
        // `text` takes precedence when both are present.
        let blocks = blocks_from_body(IncomingBody {
            text: Some("win".into()),
            blocks: Some(vec![Block::text("lose")]),
        })
        .unwrap();
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            Block::Text { content, .. } => assert_eq!(content, "win"),
            other => panic!("expected text block, got {other:?}"),
        }
        // Blocks-only path is returned verbatim.
        let only = blocks_from_body(IncomingBody {
            text: None,
            blocks: Some(vec![Block::text("a"), Block::text("b")]),
        })
        .unwrap();
        assert_eq!(only.len(), 2);
    }

    #[test]
    fn body_empty_is_rejected_as_invalid() {
        // No text + no blocks.
        let err = blocks_from_body(IncomingBody { text: None, blocks: None }).unwrap_err();
        assert_eq!(err.status_code(), 400);
        // Whitespace-only text + empty blocks also rejected.
        let err = blocks_from_body(IncomingBody {
            text: Some("   ".into()),
            blocks: Some(vec![]),
        })
        .unwrap_err();
        assert_eq!(err.status_code(), 400);
    }

    // ----- event_kind -----

    #[test]
    fn event_kind_matches_wire_discriminant() {
        assert_eq!(event_kind(&message_event(RoomId::new())), "message");
        let room = RoomId::new();
        assert_eq!(
            event_kind(&RoomEvent::Typing {
                room_id: room,
                participant: ParticipantId::new(),
                on: true,
            }),
            "typing"
        );
        // The kind string equals the serde tag emitted on the wire.
        let ev = message_event(room);
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["kind"], event_kind(&ev));
    }

    // ----- dispatch_event (no DB) -----

    #[tokio::test]
    async fn dispatch_skips_events_without_a_room() {
        // A roomless event (e.g. a directed call signal) has nothing to fan out to,
        // so the sender is never invoked and no DB query is attempted.
        use aero_common::{CallEvent, CallId};
        let sender = FakeSender::new(200);
        // Repos whose pools are never queried (we return before touching them).
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://u:p@localhost/db")
            .unwrap();
        let repo = WebhookRepo::new(pool.clone());
        let deliveries = WebhookDeliveryRepo::new(pool);
        let answer = RoomEvent::Call(CallEvent::Answer {
            call_id: CallId::new(),
            from: ParticipantId::new(),
            to: ParticipantId::new(),
            sdp: String::new(),
        });
        // `Answer` has no room_id ⇒ early return, sender + delivery log untouched.
        dispatch_event(&repo, &deliveries, &sender, &answer, 0).await;
        assert!(sender.calls().is_empty());
    }

    // ----- is_success -----

    #[test]
    fn is_success_only_for_2xx() {
        assert!(is_success(200));
        assert!(is_success(202));
        assert!(is_success(299));
        assert!(!is_success(199));
        assert!(!is_success(300));
        assert!(!is_success(404));
        assert!(!is_success(500));
    }

    // ----- event_correlation_id -----

    #[test]
    fn correlation_id_is_message_id_for_messages_else_none() {
        let ev = message_event(RoomId::new());
        let RoomEvent::Message(env) = &ev else { unreachable!() };
        assert_eq!(event_correlation_id(&ev), Some(env.message.id.to_string()));
        // A non-message event carries no natural correlation id.
        let typing = RoomEvent::Typing {
            room_id: RoomId::new(),
            participant: ParticipantId::new(),
            on: true,
        };
        assert_eq!(event_correlation_id(&typing), None);
    }
}
