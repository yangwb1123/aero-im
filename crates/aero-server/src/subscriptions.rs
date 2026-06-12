//! Creator subscriptions / membership tiers — recurring creator support.
//!
//! Viewers subscribe to a creator (a participant) at a named tier — like Twitch
//! subs / `YouTube` memberships. A creator defines tiers (name + monthly price in
//! cents + optional perks); a viewer subscribes at one tier and may later
//! unsubscribe. This is the recurring complement to the existing one-off live
//! gifts.
//!
//! Tier mutation is creator-only: the path `:id` (a [`ParticipantId`]) must equal
//! the caller, else `403`. Reading tiers is open to any authed caller.
//! Subscribing/unsubscribing makes the caller the *subscriber* (a self-subscribe
//! is rejected `400`, and the chosen tier must belong to the target creator).
//! Listing a creator's subscribers is creator-only. Thin handlers over
//! [`SubscriptionRepo`](aero_storage::SubscriptionRepo); mounted via [`routes`]
//! and `.merge`d into the main router, mirroring [`crate::templates`].

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{CreatorTierId, Error as AeroError, ParticipantId};
use aero_storage::SubscriptionRepo;
use axum::{
    extract::{Path, State},
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// Max length (in chars) of a tier name.
const MAX_NAME_LEN: usize = 128;
/// Max length (in chars) of the perks text.
const MAX_PERKS_LEN: usize = 2_000;

/// All creator-subscription routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/creators/:id/tiers",
            post(create_tier).get(list_tiers),
        )
        .route("/api/creators/:id/tiers/:tid", delete(delete_tier))
        .route(
            "/api/creators/:id/subscribe",
            post(subscribe).delete(unsubscribe),
        )
        .route("/api/creators/:id/subscribers", get(list_subscribers))
        .route("/api/me/subscriptions", get(my_subscriptions))
        .route("/api/creators/:id/gift-subscription", post(gift_subscription))
        .route(
            "/api/creators/:id/gift-subscription/leaderboard",
            get(gift_leaderboard),
        )
}

/// Build a [`SubscriptionRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> SubscriptionRepo {
    SubscriptionRepo::new(s.pg.clone())
}

fn parse_participant(s: &str) -> Result<ParticipantId, AeroError> {
    ParticipantId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("creator id: {e}")))
}

fn parse_tier(s: &str) -> Result<CreatorTierId, AeroError> {
    CreatorTierId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("tier id: {e}")))
}

#[derive(Deserialize)]
struct CreateTierReq {
    /// Human-readable tier name.
    name: String,
    /// Monthly price in cents.
    price_cents: i32,
    /// Optional free-text description of the tier's perks.
    #[serde(default)]
    perks: Option<String>,
}

/// `POST /api/creators/:id/tiers` — define a membership tier. Creator-only: the
/// path `:id` must equal the caller, else `403`. A blank/over-long name, a
/// negative price, or over-long perks is rejected `400`. Returns the created row.
async fn create_tier(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(creator_str): Path<String>,
    Json(req): Json<CreateTierReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let creator = parse_participant(&creator_str)?;
    if creator != auth.participant_id {
        return Err(AeroError::Forbidden("only the creator may define tiers".into()).into());
    }

    let name = req.name.trim();
    if name.is_empty() {
        return Err(AeroError::Invalid("name must not be empty".into()).into());
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(AeroError::Invalid("name too long".into()).into());
    }
    if req.price_cents < 0 {
        return Err(AeroError::Invalid("price_cents must not be negative".into()).into());
    }
    let perks = req.perks.as_deref().map(str::trim).filter(|p| !p.is_empty());
    if let Some(p) = perks {
        if p.chars().count() > MAX_PERKS_LEN {
            return Err(AeroError::Invalid("perks too long".into()).into());
        }
    }

    let id = repo(&s)
        .create_tier(creator, name, req.price_cents, perks)
        .await
        .map_err(AeroError::from)?;
    // Re-read so the response carries the full, canonical row (created_at).
    let row = repo(&s)
        .get_tier(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("tier".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `GET /api/creators/:id/tiers` — a creator's tiers, cheapest first. Any authed
/// caller may read them.
async fn list_tiers(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(creator_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let creator = parse_participant(&creator_str)?;
    let tiers = repo(&s)
        .list_tiers(creator)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "tiers": tiers })))
}

/// `DELETE /api/creators/:id/tiers/:tid` — delete one of the caller's own tiers.
/// Creator-only: the path `:id` must equal the caller (`403`), and the tier must
/// be theirs (`404` otherwise — the delete is creator-scoped at the SQL layer).
async fn delete_tier(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((creator_str, tier_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let creator = parse_participant(&creator_str)?;
    if creator != auth.participant_id {
        return Err(AeroError::Forbidden("only the creator may delete tiers".into()).into());
    }
    let tier = parse_tier(&tier_str)?;
    let removed = repo(&s)
        .delete_tier(tier, creator)
        .await
        .map_err(AeroError::from)?;
    if !removed {
        return Err(AeroError::NotFound(format!("tier {tier}")).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}

#[derive(Deserialize)]
struct SubscribeReq {
    /// The tier to subscribe at — must belong to the target creator.
    tier_id: String,
}

/// `POST /api/creators/:id/subscribe` — the caller subscribes to the creator at a
/// tier. A self-subscribe is rejected `400`; the tier must exist and belong to
/// the creator (`404`/`400` otherwise). Re-subscribing upserts the tier and
/// re-activates the row. Returns the subscription row.
async fn subscribe(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(creator_str): Path<String>,
    Json(req): Json<SubscribeReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let creator = parse_participant(&creator_str)?;
    let subscriber = auth.participant_id;
    if creator == subscriber {
        return Err(AeroError::Invalid("cannot subscribe to yourself".into()).into());
    }
    let tier_id = parse_tier(&req.tier_id)?;

    // The tier must exist and belong to THIS creator — otherwise a viewer could
    // subscribe at another creator's (possibly cheaper) tier.
    let tier = repo(&s)
        .get_tier(tier_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("tier {tier_id}")))?;
    if tier.creator_id != creator {
        return Err(AeroError::Invalid("tier does not belong to this creator".into()).into());
    }

    let id = repo(&s)
        .subscribe(creator, subscriber, tier_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({
        "id": id,
        "creator_id": creator,
        "subscriber_id": subscriber,
        "tier_id": tier_id,
        "active": true,
    })))
}

/// `DELETE /api/creators/:id/subscribe` — the caller unsubscribes from the
/// creator (the row is kept but flagged inactive). `404` if the caller had no
/// active subscription.
async fn unsubscribe(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(creator_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let creator = parse_participant(&creator_str)?;
    let removed = repo(&s)
        .unsubscribe(creator, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    if !removed {
        return Err(AeroError::NotFound("subscription".into()).into());
    }
    Ok(Json(serde_json::json!({ "unsubscribed": true })))
}

/// `GET /api/me/subscriptions` — the caller's active subscriptions, newest first.
async fn my_subscriptions(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let subs = repo(&s)
        .subscriptions_of(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "subscriptions": subs })))
}

/// `GET /api/creators/:id/subscribers` — the creator's active subscribers.
/// Creator-only: the path `:id` must equal the caller, else `403`.
async fn list_subscribers(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(creator_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let creator = parse_participant(&creator_str)?;
    if creator != auth.participant_id {
        return Err(AeroError::Forbidden("only the creator may list subscribers".into()).into());
    }
    let subscribers = repo(&s)
        .subscribers_of(creator)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "subscribers": subscribers })))
}

#[derive(Deserialize)]
struct GiftSubscriptionReq {
    /// The participant who will receive the gift.
    recipient_id: String,
    /// The tier the recipient will be subscribed at.
    tier_id: String,
    /// Duration of the gift in months (converted to days server-side).
    #[serde(default = "default_months")]
    months: u32,
}

/// `GET /api/creators/:id/gift-subscription/leaderboard` — top gifters for this
/// creator's channel. Any authenticated user may view the leaderboard.
/// Returns `{leaderboard: [{gifter_id, count}]}` ordered by gift count desc.
async fn gift_leaderboard(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(creator_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let creator = parse_participant(&creator_str)?;
    let rows = repo(&s)
        .gift_leaderboard(creator, 100)
        .await
        .map_err(AeroError::from)?;
    let leaderboard: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(gifter_id, count)| {
            serde_json::json!({ "gifter_id": gifter_id, "count": count })
        })
        .collect();
    Ok(Json(serde_json::json!({ "leaderboard": leaderboard })))
}

fn default_months() -> u32 {
    1
}

/// `POST /api/creators/:id/gift-subscription` — the caller (gifter) gifts a
/// subscription to a recipient on this creator's channel. The tier must belong
/// to the creator (`400`/`404`). A self-gift (gifter == recipient) is rejected
/// `400`. No coin deduction is performed; this endpoint records the subscription
/// only.
async fn gift_subscription(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(creator_str): Path<String>,
    Json(req): Json<GiftSubscriptionReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let creator = parse_participant(&creator_str)?;
    let gifter = auth.participant_id;
    let recipient = parse_participant(&req.recipient_id)?;

    if gifter == recipient {
        return Err(AeroError::Invalid("cannot gift a subscription to yourself".into()).into());
    }

    let tier_id = parse_tier(&req.tier_id)?;

    // The tier must exist and belong to THIS creator.
    let tier = repo(&s)
        .get_tier(tier_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("tier {tier_id}")))?;
    if tier.creator_id != creator {
        return Err(AeroError::Invalid("tier does not belong to this creator".into()).into());
    }

    let months = req.months.max(1);
    let duration_days = i32::try_from(months * 30)
        .unwrap_or(i32::MAX);

    repo(&s)
        .gift_subscription(
            creator,
            tier_id.to_uuid(),
            recipient,
            gifter,
            duration_days,
        )
        .await
        .map_err(AeroError::from)?;

    Ok(Json(serde_json::json!({
        "creator_id": creator,
        "recipient_id": recipient,
        "gifter_id": gifter,
        "tier_id": tier_id,
        "months": months,
        "active": true,
    })))
}
