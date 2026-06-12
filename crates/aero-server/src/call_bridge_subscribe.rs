//! Internal cross-node call-bridge subscribe endpoint (ROADMAP 方向五).
//!
//! When a node needs a call's media that another node owns, its RTP puller
//! advertises the UDP address it listens on by POSTing here. The owning node
//! records it in [`BridgeSubscriberRegistry`](crate::call_bridge_supervisor::BridgeSubscriberRegistry),
//! and the call's egress then frames each locally-published packet to that
//! address (see `UdpRtpEgress`).
//!
//! This is a **node-to-node** endpoint, not a user one: it is gated by a shared
//! cluster secret (`AERO_INTERNAL_BRIDGE_SECRET`, sent as `Authorization:
//! Bearer …`). When the secret is unset the endpoint is **disabled** (404), so
//! it is never open by default.
//!
//! Route:
//!   POST /api/internal/call-bridge/subscribe  — body `{call_id, addr}`

use std::str::FromStr;

use aero_common::CallId;
use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use serde::Deserialize;

use crate::state::AppState;

/// Subscribe-request body: the call and the puller's UDP receive address.
#[derive(Debug, Deserialize)]
struct SubscribeReq {
    call_id: String,
    addr: String,
}

/// Env var holding the shared node-to-node bridge secret.
const SECRET_ENV: &str = "AERO_INTERNAL_BRIDGE_SECRET";

/// Mount the internal subscribe route.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/internal/call-bridge/subscribe", post(subscribe))
}

/// The configured cluster bridge secret, or `None` (endpoint disabled) when
/// unset / blank.
fn bridge_secret() -> Option<String> {
    std::env::var(SECRET_ENV).ok().filter(|s| !s.trim().is_empty())
}

/// Whether the request carries the expected `Authorization: Bearer <secret>`.
/// Length-checked byte equality (the secret is a deployment value, not a
/// password DB) — mirrors the metrics-token guard.
fn secret_matches(headers: &HeaderMap, expected: &str) -> bool {
    let Some(raw) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(tok) = raw.strip_prefix("Bearer ").or_else(|| raw.strip_prefix("bearer ")) else {
        return false;
    };
    let tok = tok.trim();
    tok.len() == expected.len() && tok.bytes().zip(expected.bytes()).all(|(a, b)| a == b)
}

async fn subscribe(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SubscribeReq>,
) -> StatusCode {
    // Disabled unless a cluster secret is configured — never open by default.
    let Some(expected) = bridge_secret() else {
        return StatusCode::NOT_FOUND;
    };
    if !secret_matches(&headers, &expected) {
        return StatusCode::UNAUTHORIZED;
    }
    let Ok(call) = CallId::from_str(req.call_id.trim()) else {
        return StatusCode::BAD_REQUEST;
    };
    let Ok(addr) = req.addr.trim().parse::<std::net::SocketAddr>() else {
        return StatusCode::BAD_REQUEST;
    };
    s.bridge_subscribers.subscribe(call, addr);
    StatusCode::NO_CONTENT
}

#[cfg(test)]
mod tests {
    use super::secret_matches;
    use axum::http::{header, HeaderMap, HeaderValue};

    fn auth(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        h
    }

    #[test]
    fn secret_matches_only_the_exact_bearer_token() {
        assert!(secret_matches(&auth("Bearer s3cr3t"), "s3cr3t"));
        assert!(secret_matches(&auth("bearer s3cr3t"), "s3cr3t")); // case-insensitive scheme
        // Wrong token, wrong length, missing scheme, and absent header all fail.
        assert!(!secret_matches(&auth("Bearer wrong"), "s3cr3t"));
        assert!(!secret_matches(&auth("Bearer s3cr3"), "s3cr3t"));
        assert!(!secret_matches(&auth("s3cr3t"), "s3cr3t"));
        assert!(!secret_matches(&HeaderMap::new(), "s3cr3t"));
    }
}
