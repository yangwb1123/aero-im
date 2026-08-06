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
    rtc_config_payload_from(&aero_signaling::default_rtc_config_from_env())
}

fn rtc_config_payload_from(config: &aero_signaling::RtcConfig) -> serde_json::Value {
    serde_json::json!({
        "ice_servers": config.ice_servers,
        "ice_transport_policy": config.ice_transport_policy,
    })
}

#[cfg(test)]
mod tests {
    use aero_signaling::{IceServer, RtcConfig};

    use super::rtc_config_payload_from;

    #[test]
    fn payload_preserves_relay_policy_and_turn_credentials() {
        let payload = rtc_config_payload_from(&RtcConfig {
            ice_servers: vec![IceServer::turn(
                "turn:127.0.0.1:3478",
                "local-user",
                "local-password",
            )],
            ice_transport_policy: "relay".into(),
        });

        assert_eq!(payload["ice_transport_policy"], "relay");
        assert_eq!(payload["ice_servers"][0]["urls"][0], "turn:127.0.0.1:3478");
        assert_eq!(payload["ice_servers"][0]["username"], "local-user");
        assert_eq!(payload["ice_servers"][0]["credential"], "local-password");
    }
}
