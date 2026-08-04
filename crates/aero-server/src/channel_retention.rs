//! Per-channel message-retention override.
//!
//! Retention was workspace-only ([`WorkspaceRepo::sweep_expired_messages`](aero_storage::WorkspaceRepo::sweep_expired_messages)
//! keys off `workspaces.retention_days`). This module lets a channel set its OWN
//! `retention_days` (migration 0058), which takes precedence over the workspace
//! default in the sweep — so e.g. #legal can keep messages forever while #random
//! purges after 30 days. A room with no override (`NULL`) inherits the workspace
//! default, so the effective window is `COALESCE(room, workspace)`.
//!
//! Thin handlers over [`RoomRepo`](aero_storage::RoomRepo) (`set_retention_days` /
//! `retention_days`). Reading requires room access (the shared tenant + membership
//! guard [`assert_room_access`](aero_im_core::ImService::assert_room_access));
//! writing requires current channel Owner/Admin authority, or current workspace
//! Admin/Owner authority while still a channel member. The authorization check
//! and update share one storage transaction. The accepted-range check
//! ([`validate_override`]) is pure + DB-free so it is unit-tested offline.
//! Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, RoomId};
use aero_storage::{validate_retention_days, MIN_RETENTION_DAYS};
use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// Largest accepted per-channel retention override, in days (~10 years). The
/// floor is the shared [`MIN_RETENTION_DAYS`]; the ceiling bounds a per-channel
/// override so a typo can't set an absurd window.
const MAX_RETENTION_DAYS: i32 = 3650;

/// All per-channel retention routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/rooms/:id/retention",
        get(get_retention).put(set_retention),
    )
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

/// Validate a per-channel retention override before it is persisted.
///
/// `None` (inherit the workspace default) is always valid; a `Some(n)` is valid
/// only when `MIN_RETENTION_DAYS <= n <= MAX_RETENTION_DAYS` (i.e. `1..=3650`).
/// Reuses the shared floor check ([`validate_retention_days`]) then applies the
/// per-channel ceiling. Pure + DB-free so the rule is unit-tested offline.
///
/// # Errors
/// Returns `Err(n)` echoing the offending value when out of `1..=3650`.
fn validate_override(days: Option<i32>) -> Result<(), i32> {
    validate_retention_days(days)?;
    match days {
        Some(n) if n > MAX_RETENTION_DAYS => Err(n),
        _ => Ok(()),
    }
}

/// Build the `{ effective, room, workspace }` retention view for `room`. The room
/// override takes precedence; `effective = COALESCE(room, workspace)` mirrors the
/// sweep's per-room cutoff exactly.
async fn retention_view(s: &AppState, room: RoomId) -> Result<serde_json::Value, AeroError> {
    let room_override = s
        .rooms
        .retention_days(room)
        .await
        .map_err(AeroError::from)?;
    let ws = s
        .rooms
        .room_workspace(room)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("room".into()))?;
    let workspace_default = s
        .workspaces
        .retention_days(ws)
        .await
        .map_err(AeroError::from)?;
    Ok(serde_json::json!({
        "effective": room_override.or(workspace_default),
        "room": room_override,
        "workspace": workspace_default,
    }))
}

/// `GET /api/rooms/:id/retention` — the room's retention override, the workspace
/// default, and the effective window. Requires room access (the standard tenant +
/// membership guard); any member may view. Shape:
/// `{ "effective": <days|null>, "room": <days|null>, "workspace": <days|null> }`,
/// where `effective = COALESCE(room, workspace)`.
async fn get_retention(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    Ok(Json(retention_view(&s, room).await?))
}

#[derive(Deserialize)]
struct SetRetentionReq {
    /// New retention window in whole days (`1..=3650`), or `null` (the default
    /// when omitted) to clear the override and inherit the workspace policy.
    #[serde(default)]
    days: Option<i32>,
}

/// `PUT /api/rooms/:id/retention` — set (or clear, with `null`) the room's
/// retention override. Current channel Owner/Admin or workspace Admin/Owner
/// authority is required (`403` otherwise). `days` must be `1..=3650` or `null`
/// (`400` otherwise). Returns the updated `{ effective, room, workspace }` view.
async fn set_retention(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<SetRetentionReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    // Keep the route-local tenant guard explicit for authz lint/readability.
    // The storage write below repeats management authorization under locks.
    s.im.assert_room_access(auth.participant_id, room).await?;
    validate_override(req.days).map_err(|n| {
        AeroError::Invalid(format!(
            "retention days must be between {MIN_RETENTION_DAYS} and {MAX_RETENTION_DAYS} or null, got {n}"
        ))
    })?;
    s.im.set_channel_retention(auth.participant_id, room, req.days)
        .await?;
    Ok(Json(retention_view(&s, room).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_accepts_none_and_in_range() {
        // null = inherit, always valid; the floor, a typical value, and the ceiling.
        assert!(validate_override(None).is_ok());
        assert!(validate_override(Some(MIN_RETENTION_DAYS)).is_ok());
        assert!(validate_override(Some(1)).is_ok());
        assert!(validate_override(Some(30)).is_ok());
        assert!(validate_override(Some(MAX_RETENTION_DAYS)).is_ok());
    }

    #[test]
    fn override_rejects_out_of_range_and_echoes_value() {
        // Below the floor (0 / negative) and above the ceiling are rejected; the
        // bad value is echoed back so the route can surface it.
        assert_eq!(validate_override(Some(0)), Err(0));
        assert_eq!(validate_override(Some(-1)), Err(-1));
        assert_eq!(
            validate_override(Some(MAX_RETENTION_DAYS + 1)),
            Err(MAX_RETENTION_DAYS + 1)
        );
        assert_eq!(validate_override(Some(i32::MAX)), Err(i32::MAX));
    }
}
