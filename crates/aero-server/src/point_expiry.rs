//! Channel-points expiry info endpoint (migration 0113).
//!
//! Returns the `earn_expires_at` timestamp for the authenticated viewer's ledger
//! row with a creator, so clients can display "your points expire in N days".

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId};
use aero_storage::ChannelPointsRepo;
use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};

use crate::error::ApiResult;
use crate::state::AppState;

/// Point-expiry routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/creators/:id/points/expiry", get(get_expiry))
}

fn repo(s: &AppState) -> ChannelPointsRepo {
    ChannelPointsRepo::new(s.pg.clone())
}

/// `GET /api/creators/:id/points/expiry` — returns the `earn_expires_at`
/// timestamp for the authenticated viewer's ledger row with this creator, or
/// `null` when the balance never expires or no row exists. Lets clients display
/// "your points expire in N days".
async fn get_expiry(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(creator_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let creator = ParticipantId::from_str(creator_str.trim())
        .map_err(|e| AeroError::Invalid(format!("creator id: {e}")))?;
    let expiry = repo(&s)
        .get_expiry(auth.participant_id, creator)
        .await
        .map_err(AeroError::from)?;
    let expiry_json = expiry.map(|ts| {
        ts.format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default()
    });
    Ok(Json(serde_json::json!({ "earn_expires_at": expiry_json })))
}
