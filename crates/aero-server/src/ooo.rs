//! Out-of-office / auto-responder — self-service status management.
//!
//! A user sets an out-of-office (OOO) status — a free-text message plus an
//! optional active window — so that while active an automated reply is posted
//! once per sender into a 1:1 DM when someone messages them. The actual
//! auto-reply is delivered OUT-OF-BAND by [`crate::ooo_bot`] (a bus listener),
//! never on the message hot path; these handlers only own the status CRUD.
//!
//! Thin handlers over [`OutOfOfficeRepo`](aero_storage::OutOfOfficeRepo): the
//! self routes (`/api/me/ooo`) key on the authenticated caller, so a user can
//! only ever set/read/clear their own status. The public read
//! (`/api/participants/:id/ooo`) returns another user's OOO *only while active*,
//! so a client can surface an "out of office" badge. Mounted via [`routes`] and
//! `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId};
use aero_storage::OutOfOfficeRepo;
use axum::{
    extract::{Path, State},
    routing::{get, put},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All out-of-office routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/me/ooo",
            put(set_ooo).get(get_my_ooo).delete(clear_ooo),
        )
        .route("/api/participants/:id/ooo", get(get_participant_ooo))
}

/// Build an [`OutOfOfficeRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> OutOfOfficeRepo {
    OutOfOfficeRepo::new(s.pg.clone())
}

fn parse_participant(s: &str) -> Result<ParticipantId, AeroError> {
    ParticipantId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
}

#[derive(Deserialize)]
struct SetOooReq {
    /// The auto-reply message. Rejected when blank.
    message: String,
    /// Optional RFC 3339 start of the active window (`null`/absent ⇒ active now).
    /// Parsed via serde's rfc3339, so a malformed value is a `400` at
    /// body-deserialization time.
    #[serde(default, with = "time::serde::rfc3339::option")]
    starts_at: Option<time::OffsetDateTime>,
    /// Optional RFC 3339 end of the active window (`null`/absent ⇒ no end).
    #[serde(default, with = "time::serde::rfc3339::option")]
    ends_at: Option<time::OffsetDateTime>,
}

/// `PUT /api/me/ooo` — set (upsert) the caller's out-of-office status. A blank
/// message is rejected `400`; an `ends_at` before `starts_at` is rejected `400`.
/// Returns the stored row.
async fn set_ooo(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<SetOooReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let message = req.message.trim();
    if message.is_empty() {
        return Err(AeroError::Invalid("message must not be empty".into()).into());
    }
    if let (Some(start), Some(end)) = (req.starts_at, req.ends_at) {
        if end < start {
            return Err(AeroError::Invalid("ends_at must not be before starts_at".into()).into());
        }
    }

    repo(&s)
        .set(auth.participant_id, message, req.starts_at, req.ends_at)
        .await
        .map_err(AeroError::from)?;
    // Re-read so the response carries the full, canonical row (created_at).
    let row = repo(&s)
        .get(auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("out-of-office".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `GET /api/me/ooo` — the caller's own out-of-office status, or `null` if none
/// is set. Returns the raw row regardless of its window (so the caller can see a
/// status they scheduled for the future).
async fn get_my_ooo(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let row = repo(&s)
        .get(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `DELETE /api/me/ooo` — clear the caller's out-of-office status. Idempotent:
/// `deleted` is `false` when there was nothing to clear.
async fn clear_ooo(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let removed = repo(&s)
        .clear(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "deleted": removed })))
}

/// `GET /api/participants/:id/ooo` — another user's *active* out-of-office status
/// (so a client can show an "out of office" badge), or `null` when they have
/// none set or it is not currently within its window. Any authenticated caller
/// may read this.
async fn get_participant_ooo(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let target = parse_participant(&id_str)?;
    let now = time::OffsetDateTime::now_utc();
    let row = repo(&s).get(target).await.map_err(AeroError::from)?;
    let active = row.filter(|ooo| aero_storage::within_window(now, ooo.starts_at, ooo.ends_at));
    Ok(Json(serde_json::to_value(active).map_err(AeroError::from)?))
}
