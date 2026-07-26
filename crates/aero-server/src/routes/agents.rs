//! Agent routes — create bot/agent participants.
//! Extracted from the monolithic `routes.rs`.

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantKind};
use axum::{extract::State, routing::post, Json, Router};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/agents", post(create_agent))
}

pub(crate) fn validate_bot_name(raw: &str) -> Result<String, AeroError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AeroError::Invalid("name must not be empty".into()));
    }
    if trimmed.chars().count() > 64 {
        return Err(AeroError::Invalid("name too long (max 64 chars)".into()));
    }
    Ok(trimmed.to_owned())
}

#[derive(Deserialize)]
struct CreateAgentReq {
    display_name: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    avatar_url: Option<String>,
}

async fn create_agent(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateAgentReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let kind = match req.kind.as_deref().unwrap_or("bot") {
        "agent" => ParticipantKind::Agent,
        _ => ParticipantKind::Bot,
    };
    let display_name = validate_bot_name(&req.display_name)?;
    let bot = s.participants
        .create_bot(&display_name, kind, Some(auth.participant_id), req.avatar_url.as_deref())
        .await.map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(bot).map_err(AeroError::from)?))
}
