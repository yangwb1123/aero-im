//! Internal cross-node call-bridge control endpoints (ROADMAP 方向五).
//!
//! When a node needs a call's media that another node owns, its RTP puller
//! advertises the UDP address it listens on with an HTTP `POST`. The owning node
//! records it in [`BridgeSubscriberRegistry`](crate::call_bridge_supervisor::BridgeSubscriberRegistry),
//! and the call's egress then frames each locally-published packet to that
//! address (see `UdpRtpEgress`).
//!
//! This is a **node-to-node** endpoint, not a user one: it is gated by a shared
//! cluster secret (`AERO_INTERNAL_BRIDGE_SECRET`, sent as `Authorization:
//! Bearer …`). When the secret is unset the endpoint is **disabled** (404), so
//! it is never open by default.
//!
//! Routes:
//!   POST /api/internal/call-bridge/subscribe  — body `{call_id, addr}`
//!   POST /api/internal/call-bridge/unsubscribe — same body, immediate cleanup
//!   POST /api/internal/call-bridge/feedback   — PLI/FIR/REMB to a publisher

use std::str::FromStr;
use std::time::Duration;

use aero_common::{CallId, ParticipantId};
use aero_live_webrtc::{
    KeyframeRequestKind, Rid, BOUND_BRIDGE_COMPAT_VERSION, BOUND_BRIDGE_VERSION,
};
use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use serde::Deserialize;

use crate::sfu_media::SfuMediaError;
use crate::state::AppState;

/// Subscribe-request body: the call and the puller's UDP receive address.
#[derive(Debug, Deserialize)]
struct SubscribeReq {
    call_id: String,
    addr: String,
    /// Present on lease-aware pullers. Missing means a rolling-upgrade legacy
    /// peer which cannot safely be expired while its call remains active.
    #[serde(default)]
    lease_secs: Option<u64>,
    /// Pull incarnation used to fence delayed unsubscribe from a replaced UDP
    /// socket that happens to reuse the same address.
    #[serde(default)]
    subscription_id: Option<uuid::Uuid>,
    /// Generation-bound bridge envelope advertised by upgraded pullers.
    /// Missing on an otherwise generation-aware request means v3.
    #[serde(default)]
    wire_version: Option<u8>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FeedbackKind {
    Pli,
    Fir,
    Remb,
}

#[derive(Debug, Deserialize)]
struct FeedbackReq {
    call_id: CallId,
    publisher: ParticipantId,
    pub_mid: String,
    #[serde(default)]
    pub_rid: Option<String>,
    feedback: FeedbackKind,
    #[serde(default)]
    bitrate_bps: Option<u64>,
}

/// Env var holding the shared node-to-node bridge secret.
const SECRET_ENV: &str = "AERO_INTERNAL_BRIDGE_SECRET";
const MIN_LEASE_SECS: u64 = 30;
const MAX_LEASE_SECS: u64 = 300;

fn negotiated_wire_version(requested: Option<u8>) -> Option<u8> {
    let version = requested.unwrap_or(BOUND_BRIDGE_COMPAT_VERSION);
    matches!(version, BOUND_BRIDGE_COMPAT_VERSION | BOUND_BRIDGE_VERSION).then_some(version)
}

/// Mount the internal subscribe route.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/internal/call-bridge/subscribe", post(subscribe))
        .route("/api/internal/call-bridge/unsubscribe", post(unsubscribe))
        .route("/api/internal/call-bridge/feedback", post(feedback))
}

/// The configured cluster bridge secret, or `None` (endpoint disabled) when
/// unset / blank.
fn bridge_secret() -> Option<String> {
    std::env::var(SECRET_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// Whether the request carries the expected `Authorization: Bearer <secret>`.
/// Length-checked byte equality (the secret is a deployment value, not a
/// password DB) — mirrors the metrics-token guard.
fn secret_matches(headers: &HeaderMap, expected: &str) -> bool {
    let Some(raw) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(tok) = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
    else {
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
    let accepted = match (req.lease_secs, req.subscription_id, req.wire_version) {
        (Some(secs), Some(generation), wire_version)
            if (MIN_LEASE_SECS..=MAX_LEASE_SECS).contains(&secs) =>
        {
            let Some(wire_version) = negotiated_wire_version(wire_version) else {
                return StatusCode::BAD_REQUEST;
            };
            let _ = s.call_supervisor.ensure_current_egress(call).await;
            s.call_supervisor.subscribe_egress_generation(
                call,
                addr,
                generation,
                Duration::from_secs(secs),
                wire_version,
            )
        }
        (None, None, None) => {
            let _ = s.call_supervisor.ensure_current_egress(call).await;
            s.call_supervisor.subscribe_egress_legacy(call, addr)
        }
        _ => return StatusCode::BAD_REQUEST,
    };
    match accepted {
        None => StatusCode::NOT_FOUND,
        Some(false) => StatusCode::TOO_MANY_REQUESTS,
        Some(true) => {
            tracing::debug!(
                %call,
                %addr,
                generation_bound = req.subscription_id.is_some(),
                wire_version = ?req
                    .subscription_id
                    .and_then(|_| negotiated_wire_version(req.wire_version)),
                "call-bridge: egress subscription accepted"
            );
            StatusCode::NO_CONTENT
        }
    }
}

async fn unsubscribe(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SubscribeReq>,
) -> StatusCode {
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
    if s.call_supervisor
        .unsubscribe_egress_generation(call, addr, req.subscription_id)
    {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn feedback(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<FeedbackReq>,
) -> StatusCode {
    let Some(expected) = bridge_secret() else {
        return StatusCode::NOT_FOUND;
    };
    if !secret_matches(&headers, &expected) {
        return StatusCode::UNAUTHORIZED;
    }
    let track_mid = req.pub_mid.trim();
    if track_mid.is_empty() || track_mid.len() > 64 {
        return StatusCode::BAD_REQUEST;
    }
    let encoding_rid = match req.pub_rid.as_deref() {
        None => None,
        Some(value) => {
            let value = value.trim();
            if value.is_empty() || value.len() > 8 {
                return StatusCode::BAD_REQUEST;
            }
            Some(Rid::from(value))
        }
    };
    let result = match req.feedback {
        FeedbackKind::Pli => s.call_supervisor.request_sfu_keyframe(
            req.call_id,
            req.publisher,
            track_mid,
            encoding_rid,
            KeyframeRequestKind::Pli,
        ),
        FeedbackKind::Fir => s.call_supervisor.request_sfu_keyframe(
            req.call_id,
            req.publisher,
            track_mid,
            encoding_rid,
            KeyframeRequestKind::Fir,
        ),
        FeedbackKind::Remb => {
            let Some(bitrate_bps) = req
                .bitrate_bps
                .filter(|value| (1..=100_000_000).contains(value))
            else {
                return StatusCode::BAD_REQUEST;
            };
            s.call_supervisor
                .request_sfu_remb(req.call_id, req.publisher, track_mid, bitrate_bps)
        }
    };
    match result {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(SfuMediaError::NotFound) => StatusCode::NOT_FOUND,
        Err(SfuMediaError::QueueFull) => StatusCode::TOO_MANY_REQUESTS,
        Err(SfuMediaError::Closed) => StatusCode::SERVICE_UNAVAILABLE,
        Err(_) => StatusCode::BAD_REQUEST,
    }
}

#[cfg(test)]
mod tests {
    use super::{negotiated_wire_version, secret_matches};
    use aero_live_webrtc::{BOUND_BRIDGE_COMPAT_VERSION, BOUND_BRIDGE_VERSION};
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

    #[test]
    fn generation_request_negotiates_v3_default_or_explicit_v4() {
        assert_eq!(
            negotiated_wire_version(None),
            Some(BOUND_BRIDGE_COMPAT_VERSION)
        );
        assert_eq!(
            negotiated_wire_version(Some(BOUND_BRIDGE_COMPAT_VERSION)),
            Some(BOUND_BRIDGE_COMPAT_VERSION)
        );
        assert_eq!(
            negotiated_wire_version(Some(BOUND_BRIDGE_VERSION)),
            Some(BOUND_BRIDGE_VERSION)
        );
        assert_eq!(negotiated_wire_version(Some(2)), None);
        assert_eq!(negotiated_wire_version(Some(5)), None);
    }
}
