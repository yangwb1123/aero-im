//! Extended subscription tier levels (migration 0109).
//!
//! Distinct from the original `creator_tiers` (migration 0051, [`crate::subscriptions`]):
//! these tiers carry a display `position`, a freeform `benefits` JSONB bag, and
//! live in the `subscription_tiers` table via [`aero_storage::SubscriptionTierRepo`].
//!
//! Routes (all require authentication):
//! - `POST   /api/creators/:id/subscription-tiers`         — creator-only: create tier
//! - `GET    /api/creators/:id/subscription-tiers`         — public: list tiers
//! - `DELETE /api/creators/:id/subscription-tiers/:tid`    — creator-only: delete tier

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId};
use aero_storage::SubscriptionTierRepo;
use axum::{
    extract::{Path, State},
    routing::{delete, post},
    Json, Router,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::error::ApiResult;
use crate::state::AppState;

/// Max tier name length (chars).
const MAX_NAME: usize = 128;

/// All subscription-tier routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/creators/:id/subscription-tiers",
            post(create_tier).get(list_tiers),
        )
        .route(
            "/api/creators/:id/subscription-tiers/:tid",
            delete(delete_tier),
        )
}

fn repo(s: &AppState) -> SubscriptionTierRepo {
    SubscriptionTierRepo::new(s.pg.clone())
}

fn parse_participant(s: &str) -> Result<ParticipantId, AeroError> {
    ParticipantId::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("creator id: {e}")))
}

fn parse_tier_id(s: &str) -> Result<Uuid, AeroError> {
    Uuid::parse_str(s.trim()).map_err(|e| AeroError::Invalid(format!("tier id: {e}")))
}

#[derive(Deserialize)]
struct CreateTierReq {
    name: String,
    #[serde(default)]
    price_cents: i32,
    #[serde(default)]
    benefits: Option<serde_json::Value>,
}

/// `POST /api/creators/:id/subscription-tiers` — create a tier. Creator-only:
/// the path `:id` must equal the caller, else `403`.
async fn create_tier(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(creator_str): Path<String>,
    Json(req): Json<CreateTierReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let creator = parse_participant(&creator_str)?;
    if creator != auth.participant_id {
        return Err(
            AeroError::Forbidden("only the creator may define subscription tiers".into()).into(),
        );
    }
    let name = req.name.trim();
    if name.is_empty() {
        return Err(AeroError::Invalid("name must not be empty".into()).into());
    }
    if name.chars().count() > MAX_NAME {
        return Err(AeroError::Invalid("name too long".into()).into());
    }
    if req.price_cents < 0 {
        return Err(AeroError::Invalid("price_cents must not be negative".into()).into());
    }
    let benefits = req
        .benefits
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
    let tier = repo(&s)
        .create(creator, name, req.price_cents, benefits)
        .await?;
    Ok(Json(serde_json::to_value(tier).map_err(AeroError::from)?))
}

/// `GET /api/creators/:id/subscription-tiers` — list tiers, ordered by position
/// then name. Any authed caller may read them.
async fn list_tiers(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(creator_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let creator = parse_participant(&creator_str)?;
    let tiers = repo(&s).list_for_creator(creator).await?;
    Ok(Json(serde_json::json!({ "tiers": tiers })))
}

/// `DELETE /api/creators/:id/subscription-tiers/:tid` — delete one of the
/// caller's own tiers. Creator-only; returns `404` if the tier doesn't belong to
/// them or doesn't exist.
async fn delete_tier(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((creator_str, tid_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let creator = parse_participant(&creator_str)?;
    if creator != auth.participant_id {
        return Err(
            AeroError::Forbidden("only the creator may delete subscription tiers".into()).into(),
        );
    }
    let tid = parse_tier_id(&tid_str)?;
    let removed = repo(&s).delete(tid, creator).await?;
    if !removed {
        return Err(AeroError::NotFound("subscription tier".into()).into());
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}
