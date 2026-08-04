//! Admin — outbound-webhook delivery log + DLQ + requeue (operability).
//!
//! [`crate::webhooks`] now records every outbound delivery in
//! `webhook_delivery_log` (retry/backoff/DLQ — see
//! [`aero_storage::WebhookDeliveryRepo`]). These admin-gated routes surface that
//! log: the full per-hook delivery history, the dead-letter subset, and a manual
//! requeue of a dead delivery.
//!
//! ## Authorization
//!
//! Outbound webhooks are room-scoped; storage resolves and locks
//! `delivery → webhook → room → workspace`, then rechecks a current effective
//! Owner/Admin in the same transaction as every read or requeue. Cross-tenant
//! identifiers are opaque `404`s; same-tenant members below Admin receive `403`.
//!
//! Routes (all workspace-admin-gated):
//! * `GET  /api/webhooks/:id/deliveries?limit=`      — full delivery log for a hook
//! * `GET  /api/webhooks/:id/deliveries/dead?limit=` — the dead-letter subset
//! * `POST /api/webhook-deliveries/:id/requeue`      — requeue one dead delivery

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, WebhookDeliveryId, WebhookId};
use aero_storage::WebhookDeliveryRepo;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// Mount the webhook-delivery admin routes, ready to `.merge` into the gateway.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/webhooks/:id/deliveries", get(list_deliveries))
        .route("/api/webhooks/:id/deliveries/dead", get(list_dead))
        .route("/api/webhook-deliveries/:id/requeue", post(requeue))
}

fn parse_webhook(s: &str) -> Result<WebhookId, AeroError> {
    WebhookId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("webhook id: {e}")))
}

fn parse_delivery(s: &str) -> Result<WebhookDeliveryId, AeroError> {
    WebhookDeliveryId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("delivery id: {e}")))
}

#[derive(Deserialize)]
struct LimitQuery {
    /// Page size (clamped in the repo). Absent ⇒ the repo's default cap.
    #[serde(default)]
    limit: Option<i64>,
}

/// `GET /api/webhooks/:id/deliveries?limit=` — the full outbound-delivery log for
/// a webhook, newest first. Admin/owner of the owning workspace only.
async fn list_deliveries(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<LimitQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let webhook = parse_webhook(&id_str)?;
    let rows = WebhookDeliveryRepo::new(s.pg.clone())
        .list_for_webhook_authorized(webhook, auth.participant_id, q.limit)
        .await?;
    Ok(Json(serde_json::json!({ "deliveries": rows })))
}

/// `GET /api/webhooks/:id/deliveries/dead?limit=` — the dead-letter subset for a
/// webhook (deliveries that exhausted the retry budget). Admin/owner only.
async fn list_dead(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<LimitQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let webhook = parse_webhook(&id_str)?;
    let repo = WebhookDeliveryRepo::new(s.pg.clone());
    let rows = repo
        .list_dead_for_webhook_authorized(webhook, auth.participant_id, q.limit)
        .await?;
    Ok(Json(serde_json::json!({ "dead": rows })))
}

/// `POST /api/webhook-deliveries/:id/requeue` — requeue one dead delivery for a
/// fresh attempt (resets attempts, marks it due now). Admin/owner of the owning
/// workspace only. Storage resolves, authorizes, locks, requeues, rotates the
/// claim generation, and appends the audit row atomically. Unknown, non-dead,
/// revoked, and cross-tenant ids are opaque `404`s.
async fn requeue(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<StatusCode> {
    let delivery_id = parse_delivery(&id_str)?;
    let repo = WebhookDeliveryRepo::new(s.pg.clone());
    repo.requeue_authorized(delivery_id, auth.participant_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
