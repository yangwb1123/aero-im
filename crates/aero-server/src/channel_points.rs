//! Channel-points + custom-reward redemption HTTP API.
//!
//! Twitch-style Channel Points: a creator defines custom rewards (point cost +
//! title); a viewer spends accrued points to redeem one, landing a `pending`
//! redemption in the creator's queue to fulfill or reject. Thin handlers over
//! [`aero_storage::ChannelPointsRepo`].
//!
//! Routes:
//!   * `POST /api/streams/:id/rewards`           — creator defines a reward.
//!   * `GET  /api/streams/:id/rewards`           — list the creator's rewards.
//!   * `POST /api/rewards/:id/redeem`            — a viewer redeems (ATOMIC debit).
//!   * `GET  /api/streams/:id/redemption-queue`  — creator/mod reads the queue.
//!   * `POST /api/redemptions/:id/resolve`       — creator resolves a redemption.
//!
//! Creator-gating on the stream-scoped routes resolves the stream and checks
//! `stream.owner_id == caller` (the queue read additionally allows a stream MOD via
//! [`crate::stream_moderators::may_moderate`]). The reward/redemption are keyed by the
//! creator (the stream's owner); on redeem, a
//! [`StreamEvent::PointsRedeemed`](aero_common::StreamEvent) is broadcast on the
//! stream so watchers see the alert.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, RedemptionId, RewardId, StreamEvent};
use aero_storage::{ChannelPointsRepo, RedeemError, StreamRepo};
use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use ulid::Ulid;

use crate::error::ApiResult;
use crate::state::AppState;

/// Mount the channel-points routes, folded into the main router by
/// [`crate::routes::build`].
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/streams/:id/rewards",
            post(create_reward).get(list_rewards),
        )
        .route("/api/rewards/:id/redeem", post(redeem_reward))
        .route("/api/streams/:id/redemption-queue", get(redemption_queue))
        .route("/api/redemptions/:id/resolve", post(resolve_redemption))
}

fn parse_stream(s: &str) -> Result<Ulid, AeroError> {
    Ulid::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("stream id: {e}")))
}

fn parse_reward(s: &str) -> Result<RewardId, AeroError> {
    RewardId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("reward id: {e}")))
}

fn parse_redemption(s: &str) -> Result<RedemptionId, AeroError> {
    RedemptionId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("redemption id: {e}")))
}

fn repo(s: &AppState) -> ChannelPointsRepo {
    ChannelPointsRepo::new(s.pg.clone())
}

/// Map a storage [`RedeemError`] to a clean client-facing API error.
fn map_redeem_error(e: RedeemError) -> AeroError {
    match e {
        RedeemError::InsufficientPoints => AeroError::Conflict("insufficient points".into()),
        RedeemError::RewardUnavailable => AeroError::NotFound("reward not found or disabled".into()),
        RedeemError::Db(db) => AeroError::from(db),
    }
}

/// Resolve `stream` and return its owner; `NotFound` if unknown.
async fn stream_owner(s: &AppState, stream: Ulid) -> Result<ParticipantId, AeroError> {
    let row = StreamRepo::new(s.pg.clone())
        .get(stream)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("stream {stream}")))?;
    Ok(row.owner_id)
}

#[derive(Deserialize)]
struct CreateRewardReq {
    title: String,
    cost: i64,
    #[serde(default)]
    auto_fulfill: bool,
}

/// `POST /api/streams/:id/rewards` — the stream's creator defines a custom reward.
async fn create_reward(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<CreateRewardReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&id_str)?;
    let owner = stream_owner(&s, stream).await?;
    if owner != auth.participant_id {
        return Err(AeroError::Forbidden("only the creator may define rewards".into()).into());
    }
    let title = req.title.trim();
    if title.is_empty() {
        return Err(AeroError::Invalid("reward title is empty".into()).into());
    }
    if req.cost < 0 {
        return Err(AeroError::Invalid("reward cost must be >= 0".into()).into());
    }
    let id = repo(&s)
        .create_reward(owner, title, req.cost, req.auto_fulfill)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({
        "reward_id": id.to_string(),
        "creator_id": owner.to_string(),
        "title": title,
        "cost": req.cost,
        "auto_fulfill": req.auto_fulfill,
    })))
}

/// `GET /api/streams/:id/rewards` — the creator's reward catalog. Any authenticated
/// viewer may browse what is redeemable (the redeem UI needs it).
async fn list_rewards(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&id_str)?;
    let owner = stream_owner(&s, stream).await?;
    let rewards = repo(&s).list_rewards(owner).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "rewards": rewards })))
}

/// `POST /api/rewards/:id/redeem` — the authenticated viewer redeems a reward: an
/// ATOMIC point debit against their balance with the reward's creator (409 if
/// insufficient), enqueuing a pending redemption and broadcasting
/// [`StreamEvent::PointsRedeemed`] on the creator's live stream (best-effort) so
/// watchers see the alert.
async fn redeem_reward(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let reward_id = parse_reward(&id_str)?;
    let (redemption_id, reward, balance) = repo(&s)
        .redeem(reward_id, auth.participant_id)
        .await
        .map_err(map_redeem_error)?;

    // Broadcast on the creator's CURRENT live stream, if any, so its watchers see the
    // redemption alert. Best-effort: a creator with no live stream simply has no
    // watchers to notify. We look up the creator's live stream id from the streams
    // table (owner's most recent live stream).
    if let Some(stream_id) = creator_live_stream(&s, reward.creator_id).await {
        s.live
            .broadcast(&StreamEvent::PointsRedeemed {
                stream_id,
                viewer: auth.participant_id,
                reward_id,
            })
            .await;
    }

    Ok(Json(serde_json::json!({
        "redemption_id": redemption_id.to_string(),
        "reward_id": reward_id.to_string(),
        "status": if reward.auto_fulfill { "fulfilled" } else { "pending" },
        "balance": balance,
    })))
}

/// The creator's current live stream id, if they have one — used to target the
/// redemption-alert broadcast. Best-effort: `None` on any storage hiccup or when the
/// creator is not live. Scans the (typically small) set of currently-live streams for
/// one owned by `creator` (newest-started first, matching `list_live` ordering).
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
struct QueueFilter {
    status: Option<String>,
}

/// `GET /api/streams/:id/redemption-queue` — the creator (or a stream MOD) reads the
/// redemption queue for the stream's creator, optionally filtered by `?status=`.
async fn redemption_queue(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Query(filter): Query<QueueFilter>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&id_str)?;
    // Creator OR stream-moderator may read the queue.
    if !crate::stream_moderators::may_moderate(&s, stream, auth.participant_id).await? {
        return Err(
            AeroError::Forbidden("only the creator or a moderator may read the queue".into())
                .into(),
        );
    }
    let owner = stream_owner(&s, stream).await?;
    let status = filter.status.as_deref();
    let queue = repo(&s)
        .list_queue(owner, status)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "redemptions": queue })))
}

#[derive(Deserialize)]
struct ResolveReq {
    /// `fulfilled` or `rejected`.
    status: String,
}

/// `POST /api/redemptions/:id/resolve` — the reward's creator resolves a pending
/// redemption to `fulfilled` / `rejected`.
async fn resolve_redemption(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<ResolveReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let redemption_id = parse_redemption(&id_str)?;
    let status = req.status.trim();
    if !aero_storage::is_resolution_status(status) {
        return Err(AeroError::Invalid("status must be 'fulfilled' or 'rejected'".into()).into());
    }

    // Only the reward's creator may resolve. Resolve the redemption -> its reward ->
    // the reward's creator.
    let r = repo(&s);
    let redemption = r
        .get_redemption(redemption_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("redemption {redemption_id}")))?;
    let reward = r
        .get_reward(redemption.reward_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("reward".into()))?;
    if reward.creator_id != auth.participant_id {
        return Err(AeroError::Forbidden("only the creator may resolve redemptions".into()).into());
    }

    let resolved = r
        .resolve_redemption(redemption_id, status)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "resolved": resolved, "status": status })))
}
