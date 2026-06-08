//! One-off "pause notifications until <time>" HTTP surface (Slack "Pause
//! notifications" / Teams quiet-time).
//!
//! Silences ALL of the caller's notifications ONCE until a chosen instant, then
//! lapses on its own — distinct from the RECURRING daily DND window in
//! [`crate::notif_prefs`]. Both controls compose in the suppression seam
//! (`ImService::dispatch_notifications` via `should_notify`): a notification is
//! held back while the snooze is active OR inside the DND window OR the room is
//! muted. Thin handlers over
//! [`NotificationPrefsRepo`](aero_storage::NotificationPrefsRepo)'s `set_snooze`
//! / `clear_snooze` / `get_snooze`; per-user (no workspace scope), authenticated
//! by [`AuthUser`]. Mounted via [`routes`] and `.merge`d into the gateway
//! router, mirroring [`crate::notif_prefs`].

use aero_auth::AuthUser;
use aero_common::Error as AeroError;
use aero_storage::NotificationPrefsRepo;
use axum::{
    extract::State,
    routing::{delete, get, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::error::ApiResult;
use crate::state::AppState;

/// All snooze routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/notifications/snooze", put(set_snooze))
        .route("/api/notifications/snooze", delete(clear_snooze))
        .route("/api/notifications/snooze", get(get_snooze))
}

/// Build a [`NotificationPrefsRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> NotificationPrefsRepo {
    NotificationPrefsRepo::new(s.pg.clone())
}

/// Request body for arming the snooze.
#[derive(Deserialize)]
struct SnoozeReq {
    /// When the one-off pause should lapse, RFC 3339 (e.g.
    /// `2026-06-10T18:30:00Z`). A malformed value is a 400 at body-deserialization
    /// time; a past instant is rejected 400 by the handler.
    #[serde(with = "time::serde::rfc3339")]
    until: time::OffsetDateTime,
}

/// The caller's current snooze state, returned by `GET` (and echoed by `PUT`).
#[derive(Serialize)]
struct SnoozeView {
    /// The active snooze instant (RFC 3339), or `null` when not snoozed.
    #[serde(with = "time::serde::rfc3339::option")]
    snooze_until: Option<time::OffsetDateTime>,
}

/// `PUT /api/notifications/snooze` — pause ALL of the caller's notifications once
/// until `until` (RFC 3339). A past or current instant is rejected `400`.
/// Idempotent: re-arming overwrites the previous instant.
async fn set_snooze(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<SnoozeReq>,
) -> ApiResult<Json<SnoozeView>> {
    if req.until <= time::OffsetDateTime::now_utc() {
        return Err(AeroError::Invalid("until must be in the future".into()).into());
    }
    repo(&s)
        .set_snooze(auth.participant_id, Some(req.until))
        .await
        .map_err(AeroError::from)?;
    Ok(Json(SnoozeView { snooze_until: Some(req.until) }))
}

/// `DELETE /api/notifications/snooze` — clear the caller's one-off snooze (the
/// daily DND window, if any, is left intact). Idempotent.
async fn clear_snooze(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<SnoozeView>> {
    repo(&s)
        .clear_snooze(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(SnoozeView { snooze_until: None }))
}

/// `GET /api/notifications/snooze` — the caller's current snooze instant, or
/// `null` when not snoozed. A value in the past may be returned verbatim (it
/// simply reads as no-longer-active).
async fn get_snooze(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<SnoozeView>> {
    let snooze_until = repo(&s)
        .get_snooze(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(SnoozeView { snooze_until }))
}
