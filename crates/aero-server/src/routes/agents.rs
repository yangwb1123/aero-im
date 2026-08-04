//! Agent routes — create bot/agent participants.
//! Extracted from the monolithic `routes.rs`.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantKind, RoomId};
use axum::{extract::State, routing::post, Json, Router};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/agents", post(create_agent))
}

const MAX_AGENT_AVATAR_URL_BYTES: usize = 2_048;

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

fn parse_agent_kind(raw: Option<&str>) -> Result<ParticipantKind, AeroError> {
    match raw.unwrap_or("bot") {
        "bot" => Ok(ParticipantKind::Bot),
        "agent" => Ok(ParticipantKind::Agent),
        _ => Err(AeroError::Invalid(
            "kind must be either 'bot' or 'agent'".into(),
        )),
    }
}

fn normalize_avatar_url(raw: Option<&str>) -> Result<Option<String>, AeroError> {
    let avatar_url = raw.map(str::trim).filter(|value| !value.is_empty());
    if avatar_url.is_some_and(|value| value.len() > MAX_AGENT_AVATAR_URL_BYTES) {
        return Err(AeroError::Invalid(format!(
            "avatar_url must be at most {MAX_AGENT_AVATAR_URL_BYTES} bytes"
        )));
    }
    Ok(avatar_url.map(str::to_owned))
}

#[derive(Deserialize)]
struct CreateAgentReq {
    room_id: String,
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
    let room = RoomId::from_str(req.room_id.trim())
        .map_err(|error| AeroError::Invalid(format!("room id: {error}")))?;
    // The route-level tenant guard is mandatory for every room-scoped API.
    // Storage repeats the manager check under lock before writing any row.
    s.im.assert_room_access(auth.participant_id, room).await?;
    let kind = parse_agent_kind(req.kind.as_deref())?;
    let display_name = validate_bot_name(&req.display_name)?;
    let avatar_url = normalize_avatar_url(req.avatar_url.as_deref())?;
    let participant =
        s.im.create_service_identity(
            auth.participant_id,
            room,
            &display_name,
            kind,
            avatar_url.as_deref(),
        )
        .await?;
    s.room_member_cache.invalidate(&room);
    Ok(Json(
        serde_json::to_value(participant).map_err(AeroError::from)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_kind_and_avatar_inputs_are_bounded() {
        assert_eq!(parse_agent_kind(None).unwrap(), ParticipantKind::Bot);
        assert_eq!(
            parse_agent_kind(Some("agent")).unwrap(),
            ParticipantKind::Agent
        );
        assert!(parse_agent_kind(Some("human")).is_err());
        assert_eq!(
            normalize_avatar_url(Some("  https://example.test/a.png  ")).unwrap(),
            Some("https://example.test/a.png".into())
        );
        assert_eq!(normalize_avatar_url(Some("   ")).unwrap(), None);
        assert!(normalize_avatar_url(Some(&"x".repeat(MAX_AGENT_AVATAR_URL_BYTES + 1))).is_err());
    }

    #[test]
    fn agent_creation_requires_an_explicit_room_scope() {
        assert!(serde_json::from_value::<CreateAgentReq>(serde_json::json!({
            "display_name": "Detached"
        }))
        .is_err());
        let request = serde_json::from_value::<CreateAgentReq>(serde_json::json!({
            "room_id": RoomId::new().to_string(),
            "display_name": "Room agent",
            "kind": "agent"
        }))
        .unwrap();
        assert_eq!(request.kind.as_deref(), Some("agent"));
    }
}
