//! Validators for SDP blobs and ICE candidates carried by `CallEvent`.
//!
//! The WS layer calls [`validate_call_event`] before relaying any signaling
//! event onto the bus, so malformed payloads never reach peers.

use aero_common::CallEvent;
use serde_json::Value;

use crate::errors::SignalingError;

/// Upper bound for SDP blobs we accept. Real-world SDP is a few KiB; 64 KiB
/// leaves generous headroom while preventing denial-of-service by oversized
/// strings being relayed unmodified to other peers.
pub const MAX_SDP_BYTES: usize = 64 * 1024;

/// Upper bound for a single ICE candidate JSON blob (8 KiB).
pub const MAX_CANDIDATE_BYTES: usize = 8 * 1024;

/// Validate an SDP offer/answer body.
///
/// Performs lightweight structural checks only — the full SDP grammar is
/// re-parsed by the browser. We just ensure it looks like an SDP document
/// (starts with `v=0`) and is within the size cap.
pub fn validate_sdp(sdp: &str) -> Result<(), SignalingError> {
    if sdp.is_empty() {
        return Err(SignalingError::InvalidSdp("empty"));
    }
    if sdp.len() > MAX_SDP_BYTES {
        return Err(SignalingError::InvalidSdp("exceeds 64 KiB"));
    }
    // SDP must start with version line `v=0`. Allow optional BOM/whitespace
    // trimming but require the very first non-whitespace token.
    let head = sdp.trim_start();
    if !head.starts_with("v=0") {
        return Err(SignalingError::InvalidSdp("missing v=0 line"));
    }
    Ok(())
}

/// Validate an ICE candidate JSON payload.
///
/// Expected shape (matches the browser `RTCIceCandidateInit`):
/// ```json
/// { "candidate": "candidate:...", "sdpMid": "0", "sdpMLineIndex": 0 }
/// ```
///
/// Beyond the size cap, this enforces the structural invariants the remote
/// browser's `new RTCIceCandidate(init)` constructor relies on, so a malformed
/// candidate is rejected *here* rather than silently relayed and then thrown
/// away (or throwing a `TypeError`) on the peer:
///
/// - `candidate` is a string and the serialized blob fits in
///   [`MAX_CANDIDATE_BYTES`].
/// - A **non-empty** `candidate` is an `a=candidate` attribute value, so it must
///   begin with the `candidate:` prefix (RFC 8839 §5.1). A leading `a=` — which
///   some clients erroneously include — is tolerated and stripped before the
///   check. (An *empty* `candidate` is the end-of-candidates sentinel; the IM
///   relay does not forward those, so it is still rejected here as it always
///   was.)
/// - A non-empty candidate must be addressable: at least one of `sdpMid` /
///   `sdpMLineIndex` must be present and non-null, otherwise the browser
///   constructor throws.
/// - When present and non-null, `sdpMid` must be a string and `sdpMLineIndex` a
///   non-negative integer in the `u16` m-line index range.
pub fn validate_ice_candidate(candidate: &Value) -> Result<(), SignalingError> {
    let obj = candidate
        .as_object()
        .ok_or(SignalingError::InvalidCandidate("not a JSON object"))?;

    let cand = obj
        .get("candidate")
        .ok_or(SignalingError::InvalidCandidate(
            "missing 'candidate' field",
        ))?;
    let cand_str = cand.as_str().ok_or(SignalingError::InvalidCandidate(
        "'candidate' is not a string",
    ))?;
    if cand_str.is_empty() {
        return Err(SignalingError::InvalidCandidate("'candidate' is empty"));
    }

    // A non-empty candidate is an `a=candidate:...` attribute value. Browsers
    // accept the value with or without a leading `a=`; normalize then require
    // the mandatory `candidate:` prefix so garbage strings never reach a peer.
    let normalized = cand_str.strip_prefix("a=").unwrap_or(cand_str);
    if !normalized.starts_with("candidate:") {
        return Err(SignalingError::InvalidCandidate(
            "'candidate' must start with 'candidate:'",
        ));
    }

    // The candidate must be addressable to an m-section. At least one of
    // `sdpMid`/`sdpMLineIndex` must be present and non-null, and each, when
    // given, must have the correct JSON type.
    let mid = obj.get("sdpMid").filter(|v| !v.is_null());
    let mline = obj.get("sdpMLineIndex").filter(|v| !v.is_null());
    if mid.is_none() && mline.is_none() {
        return Err(SignalingError::InvalidCandidate(
            "requires 'sdpMid' or 'sdpMLineIndex'",
        ));
    }
    if let Some(mid) = mid {
        if !mid.is_string() {
            return Err(SignalingError::InvalidCandidate("'sdpMid' is not a string"));
        }
    }
    if let Some(mline) = mline {
        // m-line indices are non-negative and bounded by the number of media
        // sections; `u16` is the relevant range (RFC 8829 / browser behavior).
        let ok = mline.as_u64().is_some_and(|n| u16::try_from(n).is_ok());
        if !ok {
            return Err(SignalingError::InvalidCandidate(
                "'sdpMLineIndex' must be a non-negative integer in u16 range",
            ));
        }
    }

    // Serialize once to check the overall envelope size.
    let serialized = serde_json::to_vec(candidate)
        .map_err(|_| SignalingError::InvalidCandidate("not serializable"))?;
    if serialized.len() > MAX_CANDIDATE_BYTES {
        return Err(SignalingError::InvalidCandidate("exceeds 8 KiB"));
    }

    Ok(())
}

/// Per-variant validation for a `CallEvent` before it is relayed.
pub fn validate_call_event(ev: &CallEvent) -> Result<(), SignalingError> {
    match ev {
        CallEvent::Invite { sdp, to, .. } => {
            if to.is_empty() {
                return Err(SignalingError::Protocol("invite has no recipients".into()));
            }
            validate_sdp(sdp)
        }
        CallEvent::Answer { sdp, .. } | CallEvent::Offer { sdp, .. } => validate_sdp(sdp),
        CallEvent::Ice { candidate, .. } => validate_ice_candidate(candidate),
        CallEvent::End { reason, .. } => {
            if reason.len() > 256 {
                return Err(SignalingError::Protocol(
                    "end reason exceeds 256 bytes".into(),
                ));
            }
            Ok(())
        }
        CallEvent::Caption { text, .. } => {
            if text.is_empty() {
                return Err(SignalingError::Protocol("caption text is empty".into()));
            }
            if text.len() > 2000 {
                return Err(SignalingError::Protocol(
                    "caption text exceeds 2000 bytes".into(),
                ));
            }
            Ok(())
        }
        // Group-call (P6 mesh) coordination events: membership signals carry no
        // SDP; the per-pair `Offer` does.
        CallEvent::Join { .. }
        | CallEvent::Leave { .. }
        | CallEvent::Roster { .. }
        | CallEvent::SfuPublisher { .. } => Ok(()),
    }
}
