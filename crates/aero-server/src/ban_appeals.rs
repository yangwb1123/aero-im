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
//! Reading and review authority are checked transactionally by storage under
//! the canonical stream lock.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{BanAppealId, Error as AeroError};
use aero_storage::ban_appeals::{
    DEFAULT_APPEAL_PAGE_SIZE, MAX_APPEAL_DECISION_REASON_CHARS, MAX_APPEAL_REASON_CHARS,
};
use aero_storage::{AppealError, BanAppealRepo};
use axum::{
    extract::{Path, Query, State},
    routing::post,
    Json, Router,
};
use serde::Deserialize;
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
        AppealError::NotBanned => AeroError::Conflict("no active ban to appeal".into()),
        AppealError::Invalid(message) => AeroError::Invalid(message),
        AppealError::Access(error) => error,
        AppealError::Db(db) => AeroError::from(db),
    }
}

fn validate_reason<'a>(
    value: &'a str,
    max_chars: usize,
    field: &str,
) -> Result<&'a str, AeroError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(AeroError::Invalid(format!("{field} is empty")));
    }
    if value.chars().count() > max_chars {
        return Err(AeroError::Invalid(format!(
            "{field} exceeds {max_chars} characters"
        )));
    }
    Ok(value)
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
    let reason = validate_reason(&req.reason, MAX_APPEAL_REASON_CHARS, "appeal reason")?;
    let id = repo(&s)
        .submit_appeal(stream, auth.participant_id, reason)
        .await
        .map_err(map_appeal_error)?;
    Ok(Json(serde_json::json!({
        "appeal_id": id.to_string(),
        "stream_id": stream.to_string(),
        "status": "pending",
    })))
}

#[derive(Default, Deserialize)]
struct AppealPage {
    limit: Option<i64>,
    offset: Option<i64>,
}

/// `GET /api/streams/:id/appeals` — the creator/mod reads the stream's pending appeal
/// queue (oldest first).
async fn list_appeals(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Query(page): Query<AppealPage>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&id_str)?;
    let appeals = repo(&s)
        .list_pending_authorized(
            stream,
            auth.participant_id,
            page.limit.unwrap_or(DEFAULT_APPEAL_PAGE_SIZE),
            page.offset.unwrap_or(0),
        )
        .await?;
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
    let decision_reason = req
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|x| !x.is_empty());
    if let Some(reason) = decision_reason {
        validate_reason(
            reason,
            MAX_APPEAL_DECISION_REASON_CHARS,
            "appeal decision reason",
        )?;
    }
    let reviewed = r
        .review_authorized(appeal, auth.participant_id, req.approved, decision_reason)
        .await?;
    Ok(Json(serde_json::json!({
        "reviewed": reviewed,
        "status": if req.approved { "approved" } else { "denied" },
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appeal_reason_envelope_uses_unicode_scalars() {
        assert_eq!(
            validate_reason("  sorry  ", MAX_APPEAL_REASON_CHARS, "appeal reason").unwrap(),
            "sorry"
        );
        assert!(validate_reason("", MAX_APPEAL_REASON_CHARS, "appeal reason").is_err());
        assert!(validate_reason(
            &"界".repeat(MAX_APPEAL_REASON_CHARS + 1),
            MAX_APPEAL_REASON_CHARS,
            "appeal reason"
        )
        .is_err());
    }
}
