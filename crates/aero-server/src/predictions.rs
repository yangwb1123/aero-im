//! Community-predictions / channel-betting HTTP API.
//!
//! Twitch-style Channel Prediction: a creator opens a prediction (a question + 2+
//! outcomes), viewers STAKE channel points on one outcome, the creator LOCKS staking
//! then RESOLVES to a winning outcome (paying winners proportionally from the pool),
//! or CANCELS (refunding everyone). Built on the channel-points ledger (0089): a stake
//! debits the viewer's balance with the prediction's creator; a payout/refund credits
//! it. Distinct from polls (plain voting, no stakes). Thin handlers over
//! [`aero_storage::PredictionRepo`].
//!
//! Routes:
//!   * `POST /api/streams/:id/predictions`     — creator opens a prediction.
//!   * `GET  /api/streams/:id/predictions`     — list a stream's OPEN predictions.
//!   * `POST /api/predictions/:id/stake`       — a viewer stakes (ATOMIC debit).
//!   * `POST /api/predictions/:id/lock`        — creator locks staking.
//!   * `POST /api/predictions/:id/resolve`     — creator resolves to a winning outcome.
//!   * `POST /api/predictions/:id/cancel`      — creator cancels (refund all).
//!
//! Creator-gating on the create route resolves the stream and checks
//! `stream.owner_id == caller`; the lifecycle routes (lock/resolve/cancel) resolve the
//! prediction's creator and check that against the caller. On open/lock/resolve a
//! [`StreamEvent`](aero_common::StreamEvent) is broadcast on the creator's current live
//! stream (best-effort), mirroring `channel_points.rs`.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, PredictionId, StreamEvent};
use aero_storage::{PredictionRepo, ResolveError, StakeError, StreamRepo};
use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use ulid::Ulid;

use crate::error::ApiResult;
use crate::state::AppState;

/// Mount the prediction routes, folded into the main router by
/// [`crate::routes::build`].
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/streams/:id/predictions",
            post(create_prediction).get(list_predictions),
        )
        .route("/api/predictions/:id/stake", post(stake))
        .route("/api/predictions/:id/lock", post(lock))
        .route("/api/predictions/:id/resolve", post(resolve))
        .route("/api/predictions/:id/cancel", post(cancel))
        .route("/api/me/prediction-history", get(prediction_history))
        .route(
            "/api/streams/:id/prediction-analytics",
            get(prediction_analytics),
        )
}

fn parse_stream(s: &str) -> Result<Ulid, AeroError> {
    Ulid::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("stream id: {e}")))
}

fn parse_prediction(s: &str) -> Result<PredictionId, AeroError> {
    PredictionId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("prediction id: {e}")))
}

fn repo(s: &AppState) -> PredictionRepo {
    PredictionRepo::new(s.pg.clone())
}

/// Map a storage [`StakeError`] to a clean client-facing API error (409 / 400 / 404).
fn map_stake_error(e: StakeError) -> AeroError {
    match e {
        StakeError::NotFound => AeroError::NotFound("prediction not found".into()),
        StakeError::BadState => AeroError::Conflict("prediction is not open for staking".into()),
        StakeError::BadOutcome => AeroError::Invalid("no such outcome".into()),
        StakeError::InsufficientPoints => AeroError::Conflict("insufficient points".into()),
        StakeError::AlreadyStaked => {
            AeroError::Conflict("already staked on this prediction".into())
        }
        StakeError::Db(db) => AeroError::from(db),
    }
}

/// Map a storage [`ResolveError`] to a clean client-facing API error.
fn map_resolve_error(e: ResolveError) -> AeroError {
    match e {
        ResolveError::NotFound => AeroError::NotFound("prediction not found".into()),
        ResolveError::BadState => AeroError::Conflict("prediction is not resolvable".into()),
        ResolveError::BadOutcome => AeroError::Invalid("no such outcome".into()),
        ResolveError::Db(db) => AeroError::from(db),
    }
}

/// Resolve `stream` and assert `caller` owns it. `NotFound` if unknown, `Forbidden`
/// if the caller is not the owner.
async fn require_owner(s: &AppState, stream: Ulid, caller: ParticipantId) -> Result<(), AeroError> {
    let row = StreamRepo::new(s.pg.clone())
        .get(stream)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("stream {stream}")))?;
    if row.owner_id != caller {
        return Err(AeroError::Forbidden(
            "only the stream owner may manage predictions".into(),
        ));
    }
    Ok(())
}

/// Resolve a prediction and assert `caller` is its creator. `NotFound` if unknown,
/// `Forbidden` if not the creator. Returns the loaded prediction's `(creator, stream)`.
async fn require_prediction_creator(
    s: &AppState,
    prediction: PredictionId,
    caller: ParticipantId,
) -> Result<(ParticipantId, Ulid), AeroError> {
    let pred = repo(s)
        .get(prediction)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("prediction {prediction}")))?;
    if pred.creator_id != caller {
        return Err(AeroError::Forbidden(
            "only the prediction's creator may manage it".into(),
        ));
    }
    Ok((pred.creator_id, pred.stream_id))
}

/// The creator's current live stream id, if they have one — used to target the
/// prediction broadcast. Best-effort: `None` on any storage hiccup or when the creator
/// is not live (matching `channel_points.rs::creator_live_stream`).
async fn creator_live_stream(s: &AppState, creator: ParticipantId) -> Option<Ulid> {
    StreamRepo::new(s.pg.clone())
        .list_live()
        .await
        .ok()?
        .into_iter()
        .find(|st| st.owner_id == creator)
        .map(|st| st.id)
}

#[derive(Deserialize)]
struct CreatePredictionReq {
    question: String,
    outcomes: Vec<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    expires_at: Option<time::OffsetDateTime>,
}

/// `POST /api/streams/:id/predictions` — the stream's owner opens a prediction with
/// 2+ outcomes, broadcasting [`StreamEvent::PredictionOpened`] on their live stream.
async fn create_prediction(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<CreatePredictionReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&id_str)?;
    require_owner(&s, stream, auth.participant_id).await?;

    let question = req.question.trim();
    if question.is_empty() {
        return Err(AeroError::Invalid("prediction question is empty".into()).into());
    }
    // Trim + drop blank outcomes, then require at least two distinct entries.
    let outcomes: Vec<String> = req
        .outcomes
        .iter()
        .map(|o| o.trim().to_owned())
        .filter(|o| !o.is_empty())
        .collect();
    if outcomes.len() < 2 {
        return Err(AeroError::Invalid("a prediction requires at least 2 outcomes".into()).into());
    }

    let pred = repo(&s)
        .create_prediction(stream, auth.participant_id, question, &outcomes, req.expires_at)
        .await
        .map_err(AeroError::from)?;

    // Broadcast on the creator's current live stream (best-effort) so watchers see
    // the betting card open.
    if let Some(stream_id) = creator_live_stream(&s, auth.participant_id).await {
        s.live
            .broadcast(&StreamEvent::PredictionOpened {
                stream_id,
                prediction_id: pred.id,
            })
            .await;
    }

    Ok(Json(serde_json::to_value(pred).map_err(AeroError::from)?))
}

/// `GET /api/streams/:id/predictions` — a stream's OPEN predictions. Any authenticated
/// viewer may read (the betting cards are public broadcast UI).
async fn list_predictions(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&id_str)?;
    let predictions = repo(&s).list_open(stream).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "predictions": predictions })))
}

#[derive(Deserialize)]
struct StakeReq {
    outcome_idx: i32,
    points: i64,
}

/// `POST /api/predictions/:id/stake` — the authenticated viewer stakes channel points
/// on an outcome: an ATOMIC point debit against their balance with the prediction's
/// creator (409 if insufficient / already staked / closed, 400 on a bad outcome).
async fn stake(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<StakeReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let prediction = parse_prediction(&id_str)?;
    if req.points <= 0 {
        return Err(AeroError::Invalid("stake points must be > 0".into()).into());
    }
    repo(&s)
        .stake(prediction, auth.participant_id, req.outcome_idx, req.points)
        .await
        .map_err(map_stake_error)?;
    Ok(Json(serde_json::json!({
        "prediction_id": prediction.to_string(),
        "outcome_idx": req.outcome_idx,
        "points": req.points,
        "staked": true,
    })))
}

/// `POST /api/predictions/:id/lock` — the creator closes staking (`open` -> `locked`),
/// broadcasting [`StreamEvent::PredictionLocked`].
async fn lock(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let prediction = parse_prediction(&id_str)?;
    let (creator, _stream) = require_prediction_creator(&s, prediction, auth.participant_id).await?;
    let locked = repo(&s).lock(prediction).await.map_err(AeroError::from)?;

    if locked {
        if let Some(stream_id) = creator_live_stream(&s, creator).await {
            s.live
                .broadcast(&StreamEvent::PredictionLocked {
                    stream_id,
                    prediction_id: prediction,
                })
                .await;
        }
    }
    Ok(Json(serde_json::json!({ "locked": locked })))
}

#[derive(Deserialize)]
struct ResolveReq {
    winning_outcome_idx: i32,
}

/// `POST /api/predictions/:id/resolve` — the creator resolves to a winning outcome,
/// paying winners proportionally from the pool (or refunding all if nobody picked the
/// winner), broadcasting [`StreamEvent::PredictionResolved`].
async fn resolve(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<ResolveReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let prediction = parse_prediction(&id_str)?;
    let (creator, _stream) = require_prediction_creator(&s, prediction, auth.participant_id).await?;
    let payouts = repo(&s)
        .resolve(prediction, req.winning_outcome_idx)
        .await
        .map_err(map_resolve_error)?;

    if let Some(stream_id) = creator_live_stream(&s, creator).await {
        s.live
            .broadcast(&StreamEvent::PredictionResolved {
                stream_id,
                prediction_id: prediction,
                winning_outcome_idx: req.winning_outcome_idx,
            })
            .await;
    }

    let payouts_json: Vec<serde_json::Value> = payouts
        .into_iter()
        .map(|(viewer, payout)| {
            serde_json::json!({ "viewer": viewer.to_string(), "payout": payout })
        })
        .collect();
    Ok(Json(serde_json::json!({
        "prediction_id": prediction.to_string(),
        "winning_outcome_idx": req.winning_outcome_idx,
        "payouts": payouts_json,
    })))
}

/// `POST /api/predictions/:id/cancel` — the creator cancels the prediction, refunding
/// every staker their stake.
async fn cancel(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let prediction = parse_prediction(&id_str)?;
    require_prediction_creator(&s, prediction, auth.participant_id).await?;
    let cancelled = repo(&s).cancel(prediction).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "cancelled": cancelled })))
}

#[derive(serde::Deserialize)]
struct Pagination {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

fn default_limit() -> i64 {
    20
}

/// `GET /api/me/prediction-history?limit=20&offset=0` — the authenticated viewer's
/// prediction stake history, newest first.
async fn prediction_history(
    State(s): State<AppState>,
    auth: AuthUser,
    Query(pg): Query<Pagination>,
) -> ApiResult<Json<serde_json::Value>> {
    let history = repo(&s)
        .list_for_viewer(auth.participant_id, pg.limit, pg.offset)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "predictions": history })))
}

/// `GET /api/streams/:id/prediction-analytics` — aggregate analytics for the
/// stream's predictions (owner-only). Returns total/resolved count, total
/// channel-points staked, and average participants per prediction.
async fn prediction_analytics(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&id_str)?;
    // Owner-gating: only the stream owner reads their prediction analytics.
    let row = StreamRepo::new(s.pg.clone())
        .get(stream)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("stream {stream}")))?;
    if row.owner_id != auth.participant_id {
        return Err(
            AeroError::Forbidden("only the stream owner may view prediction analytics".into())
                .into(),
        );
    }
    let analytics = repo(&s)
        .creator_analytics(stream)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(analytics).map_err(AeroError::from)?))
}
