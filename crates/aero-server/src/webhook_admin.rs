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
//! Outbound webhooks are room-scoped; a delivery is gated on the **workspace**
//! that owns the webhook's room. Each handler resolves
//! `webhook → room → workspace` and asserts the caller is an Owner/Admin of that
//! workspace (the same `can_administer` bar as analytics / audit / the AI DLQ).
//! The privilege decision is the pure, DB-free [`authorize_admin`], unit-tested
//! offline; the async `assert_admin_for_webhook` does the resolution.
//!
//! Routes (all workspace-admin-gated):
//! * `GET  /api/webhooks/:id/deliveries?limit=`      — full delivery log for a hook
//! * `GET  /api/webhooks/:id/deliveries/dead?limit=` — the dead-letter subset
//! * `POST /api/webhook-deliveries/:id/requeue`      — requeue one dead delivery

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{
    Error as AeroError, ParticipantId, Result as AeroResult, WebhookDeliveryId, WebhookId,
    WorkspaceRole,
};
use aero_storage::{WebhookDeliveryRepo, WebhookRepo};
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

/// May `caller` view/manage a workspace's webhook deliveries? **Admin or owner
/// only** — delivery logs expose outbound integration targets and failures, the
/// same administrative bar as analytics / audit. Gates on
/// [`WorkspaceRole::can_administer`].
///
/// # Errors
/// [`AeroError::Forbidden`] for non-administrators (members / guests).
pub fn authorize_admin(caller: WorkspaceRole) -> AeroResult<()> {
    if caller.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden("webhook delivery admin requires admin".into()))
    }
}

/// Resolve the workspace that owns `webhook` (via its room) and assert `caller`
/// may administer it. Returns the resolved [`WebhookRepo`] so a handler can reuse
/// it. A webhook (or its room) that no longer exists is a `404`.
async fn assert_admin_for_webhook(
    s: &AppState,
    webhook: WebhookId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    let hooks = WebhookRepo::new(s.pg.clone());
    let room = hooks
        .outgoing_room(webhook)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("webhook".into()))?;
    let ws = s
        .rooms
        .room_workspace(room)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("workspace".into()))?;
    let role = s
        .workspaces
        .member_role(ws, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))?;
    authorize_admin(role)
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
    assert_admin_for_webhook(&s, webhook, auth.participant_id).await?;
    let rows = WebhookDeliveryRepo::new(s.pg.clone())
        .list_for_webhook(webhook, q.limit)
        .await
        .map_err(AeroError::from)?;
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
    assert_admin_for_webhook(&s, webhook, auth.participant_id).await?;
    let repo = WebhookDeliveryRepo::new(s.pg.clone());
    let rows = repo.list_dead(webhook, q.limit).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "dead": rows })))
}

/// `POST /api/webhook-deliveries/:id/requeue` — requeue one dead delivery for a
/// fresh attempt (resets attempts, marks it due now). Admin/owner of the owning
/// workspace only. The delivery is resolved first so its owning webhook can be
/// authorized; a non-dead/unknown id is a `404`.
async fn requeue(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<StatusCode> {
    let delivery_id = parse_delivery(&id_str)?;
    let repo = WebhookDeliveryRepo::new(s.pg.clone());
    // Resolve the delivery so we know which webhook (⇒ workspace) authorizes it.
    let delivery = repo
        .get(delivery_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("delivery".into()))?;
    assert_admin_for_webhook(&s, delivery.webhook_id, auth.participant_id).await?;
    let requeued = repo.requeue(delivery_id).await.map_err(AeroError::from)?;
    if requeued {
        Ok(StatusCode::NO_CONTENT)
    } else {
        // The delivery exists but is not in the `dead` state (already live).
        Err(AeroError::Invalid("delivery is not in the dead state".into()).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [WorkspaceRole; 4] = [
        WorkspaceRole::Guest,
        WorkspaceRole::Member,
        WorkspaceRole::Admin,
        WorkspaceRole::Owner,
    ];

    fn status_of(r: &AeroResult<()>) -> u16 {
        r.as_ref().err().map_or(200, AeroError::status_code)
    }

    #[test]
    fn webhook_admin_is_admin_and_owner_only() {
        for r in ALL {
            assert_eq!(
                authorize_admin(r).is_ok(),
                r.can_administer(),
                "delivery admin allowed only for admin/owner, role {r:?}"
            );
        }
        assert!(authorize_admin(WorkspaceRole::Owner).is_ok());
        assert!(authorize_admin(WorkspaceRole::Admin).is_ok());
        assert!(authorize_admin(WorkspaceRole::Member).is_err());
        assert!(authorize_admin(WorkspaceRole::Guest).is_err());
    }

    #[test]
    fn webhook_admin_denials_are_403() {
        for r in [WorkspaceRole::Guest, WorkspaceRole::Member] {
            assert_eq!(status_of(&authorize_admin(r)), 403, "role {r:?}");
        }
    }
}
