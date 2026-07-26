//! RTC config route — ICE server configuration for WebRTC.
//! Extracted from the monolithic `routes.rs`.

use aero_auth::AuthUser;
use axum::{routing::get, Json, Router};

use crate::error::ApiResult;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/rtc/config", get(rtc_config))
}

async fn rtc_config(_auth: AuthUser) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(rtc_config_payload()))
}

fn rtc_config_payload() -> serde_json::Value {
    let stun = std::env::var("AERO_STUN_URLS")
        .unwrap_or_else(|_| "stun:stun.l.google.com:19302".into());
    let urls: Vec<String> = stun.split(',').map(|s| s.trim().to_owned()).collect();
    let mut ice_servers = vec![serde_json::json!({"urls": urls})];
    if let (Ok(url), Ok(user), Ok(pass)) = (
        std::env::var("AERO_TURN_URL"),
        std::env::var("AERO_TURN_USERNAME"),
        std::env::var("AERO_TURN_PASSWORD"),
    ) {
        ice_servers.push(serde_json::json!({
            "urls": [url],
            "username": user,
            "credential": pass,
        }));
    }
    serde_json::json!({
        "ice_servers": ice_servers,
        "ice_transport_policy": "all",
    })
}
