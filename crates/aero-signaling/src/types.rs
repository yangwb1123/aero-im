//! Shared WebRTC configuration types.
//!
//! These mirror the [`RTCIceServer`] / [`RTCConfiguration`] JSON shapes consumed
//! by the browser `RTCPeerConnection` constructor, so the WS layer can ship
//! them verbatim to clients on call setup.
//!
//! [`RTCIceServer`]: https://www.w3.org/TR/webrtc/#rtciceserver-dictionary
//! [`RTCConfiguration`]: https://www.w3.org/TR/webrtc/#rtcconfiguration-dictionary

use std::env;

use serde::{Deserialize, Serialize};

/// A single STUN / TURN endpoint exposed to the browser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceServer {
    /// One or more `stun:`/`turn:`/`turns:` URLs.
    pub urls: Vec<String>,
    /// TURN username (omitted for plain STUN).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// TURN long-term credential (omitted for plain STUN).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

impl IceServer {
    /// Convenience constructor for a STUN-only entry.
    #[must_use]
    pub fn stun<S: Into<String>>(url: S) -> Self {
        Self { urls: vec![url.into()], username: None, credential: None }
    }

    /// Convenience constructor for a TURN entry with credentials.
    #[must_use]
    pub fn turn<S: Into<String>>(url: S, username: S, credential: S) -> Self {
        Self {
            urls: vec![url.into()],
            username: Some(username.into()),
            credential: Some(credential.into()),
        }
    }
}

/// The subset of [`RTCConfiguration`] the server hands to the browser.
///
/// `ice_transport_policy` is `"all"` (the default) or `"relay"` — the latter
/// forces every candidate through TURN, useful in symmetric-NAT environments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RtcConfig {
    pub ice_servers: Vec<IceServer>,
    pub ice_transport_policy: String,
}

impl RtcConfig {
    /// Builds a config with the supplied ICE servers and `"all"` policy.
    #[must_use]
    pub fn new(ice_servers: Vec<IceServer>) -> Self {
        Self { ice_servers, ice_transport_policy: "all".into() }
    }
}

/// Default STUN URL used when `AERO_STUN_URLS` is not set.
pub const DEFAULT_STUN_URL: &str = "stun:stun.l.google.com:19302";

/// Build the default `RtcConfig` from environment variables.
///
/// Reads:
/// - `AERO_STUN_URLS` — comma-separated list of STUN URLs.
///   Defaults to [`DEFAULT_STUN_URL`] when missing/empty.
/// - `AERO_TURN_URL`, `AERO_TURN_USERNAME`, `AERO_TURN_PASSWORD` —
///   optional TURN credentials. All three are required together;
///   if any is missing TURN is skipped.
/// - `AERO_ICE_TRANSPORT_POLICY` — `"all"` (default) or `"relay"`.
#[must_use]
pub fn default_rtc_config_from_env() -> RtcConfig {
    let stun_urls = env::var("AERO_STUN_URLS")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            s.split(',')
                .map(|p| p.trim().to_owned())
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec![DEFAULT_STUN_URL.to_owned()]);

    let mut ice_servers = vec![IceServer { urls: stun_urls, username: None, credential: None }];

    if let (Ok(url), Ok(user), Ok(pass)) = (
        env::var("AERO_TURN_URL"),
        env::var("AERO_TURN_USERNAME"),
        env::var("AERO_TURN_PASSWORD"),
    ) {
        let url = url.trim();
        if !url.is_empty() {
            ice_servers.push(IceServer {
                urls: vec![url.to_owned()],
                username: Some(user),
                credential: Some(pass),
            });
        }
    }

    let policy = env::var("AERO_ICE_TRANSPORT_POLICY")
        .ok()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| s == "all" || s == "relay")
        .unwrap_or_else(|| "all".into());

    RtcConfig { ice_servers, ice_transport_policy: policy }
}
