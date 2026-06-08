//! Mobile push token registration (ROADMAP 方向二).
//!
//! Participants register their device's FCM/APNs token here so the server can
//! push notifications when they are offline. Tokens are owned by the
//! authenticated participant; registering a token that belongs to a different
//! participant moves it (handles device hand-offs without orphan rows).
//!
//! Routes:
//!
//! * `POST /api/me/push-token` — register a token. Idempotent: re-registering
//!   the same token for the same participant refreshes `registered_at`.
//! * `DELETE /api/me/push-token` — unregister a specific token (body `{token}`).
//!   Used when the user signs out on a device so it stops receiving pushes.
//! * `GET /api/me/push-token` — list all registered devices (token redacted to
//!   first 8 chars for display; the full token is never returned).

use aero_auth::AuthUser;
use aero_common::Error as AeroError;
use aero_storage::{PushPlatform, PushTokenRepo};
use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/me/push-token", get(list_tokens).post(register_token).delete(unregister_token))
}

#[derive(Deserialize)]
struct RegisterReq {
    /// `"fcm"` or `"apns"`.
    platform: String,
    /// The raw device token string.
    token: String,
}

#[derive(Deserialize)]
struct UnregisterReq {
    token: String,
}

/// `POST /api/me/push-token` — register a device push token.
async fn register_token(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<RegisterReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let token = req.token.trim();
    if token.is_empty() {
        return Err(AeroError::Invalid("token must not be empty".into()).into());
    }
    let platform = PushPlatform::parse(&req.platform)
        .ok_or_else(|| AeroError::Invalid("platform must be 'fcm' or 'apns'".into()))?;

    let repo = PushTokenRepo::new(s.pg.clone());
    let pt = repo
        .register(auth.participant_id, platform, token)
        .await
        .map_err(AeroError::from)?;

    Ok(Json(serde_json::json!({
        "id": pt.id,
        "platform": pt.platform,
        "registered_at": pt.registered_at,
    })))
}

/// `DELETE /api/me/push-token` — unregister a specific device token.
async fn unregister_token(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<UnregisterReq>,
) -> ApiResult<StatusCode> {
    let token = req.token.trim();
    if token.is_empty() {
        return Err(AeroError::Invalid("token must not be empty".into()).into());
    }
    PushTokenRepo::new(s.pg.clone())
        .unregister(auth.participant_id, token)
        .await
        .map_err(AeroError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/me/push-token` — list registered devices (token is truncated for
/// display; do not expose full tokens via the API).
async fn list_tokens(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let tokens = PushTokenRepo::new(s.pg.clone())
        .list_for_participant(auth.participant_id)
        .await
        .map_err(AeroError::from)?;

    let items: Vec<_> = tokens
        .iter()
        .map(|t| {
            let preview = if t.token.len() > 8 {
                format!("{}…", &t.token[..8])
            } else {
                t.token.clone()
            };
            serde_json::json!({
                "id": t.id,
                "platform": t.platform,
                "token_preview": preview,
                "registered_at": t.registered_at,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({ "tokens": items })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_platform_is_rejected_at_parse() {
        assert!(PushPlatform::parse("gcm").is_none());
        assert!(PushPlatform::parse("webpush").is_none());
        assert!(PushPlatform::parse("FCM").is_none()); // case-sensitive
        assert!(PushPlatform::parse("fcm").is_some());
        assert!(PushPlatform::parse("apns").is_some());
    }

    #[test]
    fn token_preview_truncates_long_tokens() {
        let token = "abcdefgh12345678".to_string();
        let preview = if token.len() > 8 {
            format!("{}…", &token[..8])
        } else {
            token.clone()
        };
        assert_eq!(preview, "abcdefgh…");
    }

    #[test]
    fn short_token_is_not_truncated() {
        let token = "short".to_string();
        let preview = if token.len() > 8 { format!("{}…", &token[..8]) } else { token.clone() };
        assert_eq!(preview, "short");
    }
}
