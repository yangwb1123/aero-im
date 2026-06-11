//! Ban / timeout appeal-workflow HTTP API.
//!
//! A viewer banned (or timed-out) from a stream's danmaku chat may APPEAL: submit a
//! reason for the creator/mods to review. Approval lifts the ban (the existing unban
//! path); denial keeps it. Thin handlers over [`aero_storage::BanAppealRepo`].
//!
//! Routes:
//!   * `POST /api/streams/:id/appeals`   — a banned viewer submits an appeal.
//!   * `GET  /api/streams/:id/appeals`   — the creator/mod reads the pending queue.
//!   * `POST /api/appeals/:id/review`    — the creator/mod approves / denies.
//!
//! Submission is open to the authenticated caller (the storage layer gates it to an
//! ACTIVE ban for `(stream, caller)` — a non-banned viewer gets a 404/conflict).
//! Reading + reviewing are creator-or-mod gated via
//! [`crate::stream_moderators::may_moderate`].

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{BanAppealId, Error as AeroError, ParticipantId};
use aero_storage::{AppealError, BanAppealRepo};
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use time::OffsetDateTime;
use ulid::Ulid;

use crate::error::ApiResult;
use crate::state::AppState;

/// Mount the ban-appeal routes, folded into the main router by
/// [`crate::routes::build`].
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/streams/:id/appeals",
            post(submit_appeal).get(list_appeals),
        )
        .route("/api/appeals/:id/review", post(review_appeal))
}

fn parse_stream(s: &str) -> Result<Ulid, AeroError> {
    Ulid::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("stream id: {e}")))
}

fn parse_appeal(s: &str) -> Result<BanAppealId, AeroError> {
    BanAppealId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("appeal id: {e}")))
}

fn repo(s: &AppState) -> BanAppealRepo {
    BanAppealRepo::new(s.pg.clone())
}

/// Map a storage [`AppealError`] to a clean client-facing API error.
fn map_appeal_error(e: AppealError) -> AeroError {
    match e {
        AppealError::NotBanned => {
            AeroError::Conflict("no active ban to appeal".into())
        }
        AppealError::Db(db) => AeroError::from(db),
    }
}

/// Assert `caller` may moderate `stream` (its creator or a stream moderator).
async fn require_moderate(
    s: &AppState,
    stream: Ulid,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    if !crate::stream_moderators::may_moderate(s, stream, caller).await? {
        return Err(AeroError::Forbidden(
            "only the creator or a moderator may review appeals".into(),
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
struct SubmitReq {
    reason: String,
}

/// `POST /api/streams/:id/appeals` — the authenticated (banned) viewer appeals their
/// ban. The storage layer rejects the appeal (409) unless an ACTIVE ban exists for
/// `(stream, caller)`.
async fn submit_appeal(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<SubmitReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&id_str)?;
    let reason = req.reason.trim();
    if reason.is_empty() {
        return Err(AeroError::Invalid("appeal reason is empty".into()).into());
    }
    let id = repo(&s)
        .submit_appeal(stream, auth.participant_id, reason, OffsetDateTime::now_utc())
        .await
        .map_err(map_appeal_error)?;
    Ok(Json(serde_json::json!({
        "appeal_id": id.to_string(),
        "stream_id": stream.to_string(),
        "status": "pending",
    })))
}

/// `GET /api/streams/:id/appeals` — the creator/mod reads the stream's pending appeal
/// queue (oldest first).
async fn list_appeals(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&id_str)?;
    require_moderate(&s, stream, auth.participant_id).await?;
    let appeals = repo(&s).list_pending(stream).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "appeals": appeals })))
}

#[derive(Deserialize)]
struct ReviewReq {
    approved: bool,
    #[serde(default)]
    reason: Option<String>,
}

/// `POST /api/appeals/:id/review` — the creator/mod approves (lifting the ban) or
/// denies (keeping it) a pending appeal.
async fn review_appeal(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<ReviewReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let appeal = parse_appeal(&id_str)?;
    let r = repo(&s);
    // Resolve the appeal to learn its stream, so we can gate on that stream's
    // creator/mod authority.
    let row = r
        .get(appeal)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("appeal {appeal}")))?;
    require_moderate(&s, row.stream_id, auth.participant_id).await?;

    let decision_reason = req.reason.as_deref().map(str::trim).filter(|x| !x.is_empty());
    let reviewed = r
        .review(appeal, auth.participant_id, req.approved, decision_reason)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({
        "reviewed": reviewed,
        "status": if req.approved { "approved" } else { "denied" },
    })))
}
