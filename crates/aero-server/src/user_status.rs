//! User custom status + presence HTTP surface.
//!
//! A participant sets a custom status — an emoji shorthand (`:palm_tree:`), free
//! text ("On vacation"), an optional auto-expiry — and a coarse presence
//! preference (active/away). Other participants read it on a profile / member
//! list. Thin handlers over
//! [`UserStatusRepo`](aero_storage::UserStatusRepo); mounted via [`routes`] and
//! `.merge`d into the gateway router, mirroring [`crate::workspaces`].
//!
//! This is the DURABLE, user-chosen status — distinct from the ephemeral Redis
//! online tracking in [`aero_storage::PresenceStore`]. A status change is
//! fetched on demand (profile view / member-list render); there is no new
//! `RoomEvent` (status is not room-scoped). Realtime push is a possible
//! follow-up — see the crate notes.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, Presence};
use aero_storage::UserStatusRepo;
use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use time::OffsetDateTime;

use crate::error::ApiResult;
use crate::state::AppState;

/// All user-status routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/me/status",
            get(get_my_status)
                .put(set_my_status)
                .delete(clear_my_status),
        )
        .route("/api/participants/:id/status", get(get_participant_status))
}

/// Construct the repo from the shared pool. (This worktree's `AppState` has no
/// dedicated `pg` field, so we borrow the participant repo's pool — every repo is
/// a cheap clone over the same `PgPool`.)
fn repo(s: &AppState) -> UserStatusRepo {
    UserStatusRepo::new(s.participants.pool().clone())
}

/// Cap on how far in the future a custom status may be scheduled to expire
/// (1 year, in seconds). Guards against absurd / overflowing `expires_in_secs`.
const MAX_EXPIRES_IN_SECS: i64 = 365 * 24 * 60 * 60;

/// `GET /api/me/status` — the caller's own current (non-expired) status, or
/// `null` when they have never set one.
async fn get_my_status(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let status = repo(&s).get(auth.participant_id).await?;
    Ok(Json(serde_json::to_value(status).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct SetStatusReq {
    /// Emoji shorthand, e.g. `:palm_tree:`. Absent/null ⇒ no emoji.
    #[serde(default)]
    emoji: Option<String>,
    /// Free-form status text. Absent/null ⇒ no text.
    #[serde(default)]
    text: Option<String>,
    /// Coarse presence preference token ("active"/"away"/"offline"). Absent ⇒
    /// defaults to "active".
    #[serde(default)]
    presence: Option<String>,
    /// Auto-expire the custom status after this many seconds from now. Absent/null
    /// ⇒ never expires.
    #[serde(default)]
    expires_in_secs: Option<i64>,
}

impl SetStatusReq {
    /// Whether the request carries no status fields at all — an empty body, which
    /// we treat as "clear my status".
    fn is_empty(&self) -> bool {
        self.emoji.is_none()
            && self.text.is_none()
            && self.presence.is_none()
            && self.expires_in_secs.is_none()
    }
}

/// Trim a string field, mapping empty/whitespace-only to `None` so a cleared
/// field round-trips as absent rather than an empty string.
fn clean(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// `PUT /api/me/status` — set (upsert) the caller's status. An empty body (no
/// fields) clears it, matching `DELETE`. `presence` defaults to "active" when
/// other fields are present; unknown tokens normalize to "active" in the repo.
async fn set_my_status(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<SetStatusReq>,
) -> ApiResult<Json<serde_json::Value>> {
    // Empty body ⇒ clear (parity with DELETE / "nulls" clearing semantics).
    if req.is_empty() {
        repo(&s).clear(auth.participant_id).await?;
        return Ok(Json(serde_json::Value::Null));
    }

    let emoji = clean(req.emoji.as_deref());
    let text = clean(req.text.as_deref());
    // Bound free text so a huge status can't be stored.
    if let Some(t) = text.as_deref() {
        if t.len() > 256 {
            return Err(AeroError::Invalid("status text too long (max 256)".into()).into());
        }
    }
    if let Some(e) = emoji.as_deref() {
        if e.len() > 64 {
            return Err(AeroError::Invalid("status emoji too long (max 64)".into()).into());
        }
    }

    // Presence: default to active; normalize through the lenient parser so the
    // canonical lowercase token is what we persist.
    let presence = req
        .presence
        .as_deref()
        .map_or(Presence::Active, Presence::from_str_lenient);

    let expires_at = match req.expires_in_secs {
        None => None,
        Some(secs) => {
            if secs <= 0 {
                return Err(AeroError::Invalid("expires_in_secs must be positive".into()).into());
            }
            if secs > MAX_EXPIRES_IN_SECS {
                return Err(AeroError::Invalid("expires_in_secs too large".into()).into());
            }
            Some(OffsetDateTime::now_utc() + time::Duration::seconds(secs))
        }
    };

    let status = repo(&s)
        .set(
            auth.participant_id,
            emoji.as_deref(),
            text.as_deref(),
            presence.as_str(),
            expires_at,
        )
        .await?;
    Ok(Json(serde_json::to_value(status).map_err(AeroError::from)?))
}

/// `DELETE /api/me/status` — clear the caller's status entirely.
async fn clear_my_status(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let removed = repo(&s).clear(auth.participant_id).await?;
    Ok(Json(
        serde_json::json!({ "cleared": true, "removed": removed }),
    ))
}

/// `GET /api/participants/:id/status` — another participant's current
/// (non-expired) status, or `null` when unset. Any authenticated user may read a
/// status (it is profile-visible), mirroring `GET /api/participants/:id`.
async fn get_participant_status(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = ParticipantId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))?;
    let status = repo(&s).get(target).await?;
    Ok(Json(serde_json::to_value(status).map_err(AeroError::from)?))
}
