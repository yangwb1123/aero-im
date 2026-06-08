//! Per-conversation call log — read the call sessions held in a room.
//!
//! Every IM shows a per-conversation call history (who called, audio/video,
//! when it started/ended). Call sessions are ALREADY persisted by
//! [`CallRepo`](aero_storage::CallRepo) (it writes `call_sessions` rows on
//! start/end); this module simply exposes the existing, read-only
//! [`CallRepo::list_for_room`](aero_storage::CallRepo::list_for_room) — there is
//! NO table, NO id and NO migration.
//!
//! The single handler gates on the room exactly like [`crate::reaction_detail`]:
//! [`assert_room_access`] → `403` for a non-member / cross-tenant caller (and
//! `404` for an unknown room), so a room's call log can never leak to someone who
//! isn't in it. Mounted via [`routes`] and `.merge`d into the gateway router.
//!
//! [`assert_room_access`]: aero_im_core::ImService::assert_room_access

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, RoomId};
use axum::{
    extract::{Path, Query, State},
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// Mount the call-history route, folded into the gateway router by
/// [`crate::routes::build`].
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/rooms/:id/calls", get(list_calls))
}

/// Default number of call sessions returned when the client omits `?limit=`.
const DEFAULT_LIMIT: i64 = 50;
/// Hard ceiling on a call-log page (mirrors the clamp
/// [`CallRepo::list_for_room`](aero_storage::CallRepo::list_for_room) applies, so
/// the cap is also validated at the edge and unit-testable without a database).
const MAX_LIMIT: i64 = 200;

/// Resolve the effective page size: default when absent, clamped into
/// `[1, MAX_LIMIT]`. Pure, so the cap/floor is unit-tested offline.
#[must_use]
fn call_log_limit(requested: Option<i64>) -> i64 {
    requested.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

#[derive(Deserialize)]
struct ListCallsQuery {
    /// Optional page size; defaults to [`DEFAULT_LIMIT`], capped at [`MAX_LIMIT`].
    #[serde(default)]
    limit: Option<i64>,
}

/// `GET /api/rooms/:id/calls?limit=` — the room's call log, newest first.
///
/// Authorizes the caller against the room (`assert_room_access` → `403` for a
/// non-member / cross-tenant caller, `404` for an unknown room), then returns the
/// persisted call sessions. The response shape is `{ "calls": [ CallSession, ... ] }`,
/// ordered by `started_at` descending.
///
/// # Errors
/// - [`AeroError::Invalid`] when the path id fails to decode as a [`RoomId`].
/// - `403` / `404` propagated from [`assert_room_access`](aero_im_core::ImService::assert_room_access).
/// - [`AeroError::Internal`] on a storage failure.
async fn list_calls(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Query(q): Query<ListCallsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = RoomId::from_str(room_str.trim())
        .map_err(|e| AeroError::Invalid(format!("room id: {e}")))?;
    // Tenant + room-membership guard before exposing the call log.
    s.im.assert_room_access(auth.participant_id, room).await?;

    let limit = call_log_limit(q.limit);
    let calls = s.calls.list_for_room(room, limit).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "calls": calls })))
}

#[cfg(test)]
mod tests {
    use super::call_log_limit;

    #[test]
    fn limit_defaults_when_absent() {
        assert_eq!(call_log_limit(None), 50);
    }

    #[test]
    fn limit_is_clamped_into_range() {
        assert_eq!(call_log_limit(Some(0)), 1);
        assert_eq!(call_log_limit(Some(-5)), 1);
        assert_eq!(call_log_limit(Some(10)), 10);
        assert_eq!(call_log_limit(Some(10_000)), 200);
    }
}
