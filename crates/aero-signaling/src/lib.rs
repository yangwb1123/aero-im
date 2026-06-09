//! WebRTC signaling primitives (offer/answer/ICE), shared by IM call (P3) and
//! the future SFU group calls (P6).
//!
//! This crate is a *helper* layer — it does not own the bus relay. The IM
//! service in `aero-im-core` continues to publish `RoomEvent::Call(...)`; the
//! WS layer above uses this crate to:
//!
//! 1. Validate incoming SDP / ICE payloads before relaying them
//!    ([`signaling::validate_call_event`]).
//! 2. Hand the browser the right ICE/TURN configuration on call setup
//!    ([`types::default_rtc_config_from_env`]).
//! 3. Track the small in-memory roster of joined participants per active call
//!    ([`roster::CallRoster`]).

pub mod errors;
pub mod roster;
pub mod signaling;
pub mod types;

pub use errors::SignalingError;
pub use roster::CallRoster;
pub use signaling::{
    validate_call_event, validate_ice_candidate, validate_sdp, MAX_CANDIDATE_BYTES, MAX_SDP_BYTES,
};
pub use types::{default_rtc_config_from_env, IceServer, RtcConfig, DEFAULT_STUN_URL};

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::{CallEvent, CallId, CallKind, ParticipantId, RoomId};

    fn basic_sdp() -> String {
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n".to_owned()
    }

    fn invite(sdp: String) -> CallEvent {
        CallEvent::Invite {
            call_id: CallId::new(),
            room_id: RoomId::new(),
            from: ParticipantId::new(),
            to: vec![ParticipantId::new()],
            kind: CallKind::Audio,
            sdp,
        }
    }

    // -------- validate_call_event --------

    #[test]
    fn validate_call_event_accepts_basic_invite() {
        let ev = invite(basic_sdp());
        validate_call_event(&ev).expect("basic invite should validate");
    }

    #[test]
    fn validate_call_event_rejects_empty_sdp() {
        let ev = invite(String::new());
        let err = validate_call_event(&ev).expect_err("empty SDP must be rejected");
        assert!(matches!(err, SignalingError::InvalidSdp(_)));
    }

    #[test]
    fn validate_call_event_rejects_missing_version_line() {
        let ev = invite("s=session\r\nt=0 0\r\n".to_owned());
        let err = validate_call_event(&ev).expect_err("no v=0 line");
        assert!(matches!(err, SignalingError::InvalidSdp(_)));
    }

    #[test]
    fn validate_call_event_rejects_oversize_sdp() {
        let huge = format!("v=0\r\n{}", "a".repeat(MAX_SDP_BYTES));
        let err = validate_call_event(&invite(huge)).expect_err("must reject oversize");
        assert!(matches!(err, SignalingError::InvalidSdp(_)));
    }

    #[test]
    fn validate_call_event_invite_requires_recipients() {
        let mut ev = invite(basic_sdp());
        if let CallEvent::Invite { ref mut to, .. } = ev {
            to.clear();
        }
        let err = validate_call_event(&ev).expect_err("empty recipients");
        assert!(matches!(err, SignalingError::Protocol(_)));
    }

    #[test]
    fn validate_call_event_validates_ice_candidate() {
        let ev = CallEvent::Ice {
            call_id: CallId::new(),
            from: ParticipantId::new(),
            to: ParticipantId::new(),
            candidate: serde_json::json!({
                "candidate": "candidate:1 1 udp 2113937151 192.0.2.1 50000 typ host",
                "sdpMid": "0",
                "sdpMLineIndex": 0,
            }),
        };
        validate_call_event(&ev).expect("well-formed ICE candidate");
    }

    #[test]
    fn validate_call_event_rejects_bad_ice_candidate() {
        let ev = CallEvent::Ice {
            call_id: CallId::new(),
            from: ParticipantId::new(),
            to: ParticipantId::new(),
            candidate: serde_json::json!({ "candidate": "" }),
        };
        let err = validate_call_event(&ev).expect_err("empty candidate");
        assert!(matches!(err, SignalingError::InvalidCandidate(_)));
    }

    #[test]
    fn validate_call_event_rejects_non_object_ice() {
        let ev = CallEvent::Ice {
            call_id: CallId::new(),
            from: ParticipantId::new(),
            to: ParticipantId::new(),
            candidate: serde_json::json!("candidate:foo"),
        };
        let err = validate_call_event(&ev).expect_err("non-object candidate");
        assert!(matches!(err, SignalingError::InvalidCandidate(_)));
    }

    fn ice(candidate: serde_json::Value) -> CallEvent {
        CallEvent::Ice {
            call_id: CallId::new(),
            from: ParticipantId::new(),
            to: ParticipantId::new(),
            candidate,
        }
    }

    #[test]
    fn ice_rejects_candidate_without_prefix() {
        // A non-empty string that is not an `a=candidate` attribute value would
        // make the peer's RTCIceCandidate constructor throw — reject it here.
        let err = validate_call_event(&ice(serde_json::json!({
            "candidate": "garbage 1 udp 2113 192.0.2.1 5000 typ host",
            "sdpMLineIndex": 0,
        })))
        .expect_err("candidate without 'candidate:' prefix must be rejected");
        assert!(matches!(err, SignalingError::InvalidCandidate(_)));
    }

    #[test]
    fn ice_accepts_leading_a_equals_prefix() {
        // Some clients send the full `a=candidate:...` attribute line; tolerate it.
        validate_call_event(&ice(serde_json::json!({
            "candidate": "a=candidate:1 1 udp 2113937151 192.0.2.1 50000 typ host",
            "sdpMid": "0",
        })))
        .expect("a= prefixed candidate should be accepted");
    }

    #[test]
    fn ice_rejects_missing_mid_and_mline() {
        // Per the WebRTC spec a non-empty candidate needs at least one of
        // sdpMid / sdpMLineIndex; both absent → browser TypeError.
        let err = validate_call_event(&ice(serde_json::json!({
            "candidate": "candidate:1 1 udp 2113937151 192.0.2.1 50000 typ host",
        })))
        .expect_err("missing both sdpMid and sdpMLineIndex must be rejected");
        assert!(matches!(err, SignalingError::InvalidCandidate(_)));
    }

    #[test]
    fn ice_rejects_both_mid_and_mline_null() {
        // Explicit nulls are equivalent to absent — still must be rejected.
        let err = validate_call_event(&ice(serde_json::json!({
            "candidate": "candidate:1 1 udp 2113937151 192.0.2.1 50000 typ host",
            "sdpMid": serde_json::Value::Null,
            "sdpMLineIndex": serde_json::Value::Null,
        })))
        .expect_err("both null must be rejected");
        assert!(matches!(err, SignalingError::InvalidCandidate(_)));
    }

    #[test]
    fn ice_accepts_mline_only_with_null_mid() {
        // A null sdpMid alongside a valid sdpMLineIndex is the common Firefox
        // shape and must pass.
        validate_call_event(&ice(serde_json::json!({
            "candidate": "candidate:1 1 udp 2113937151 192.0.2.1 50000 typ host",
            "sdpMid": serde_json::Value::Null,
            "sdpMLineIndex": 0,
        })))
        .expect("null mid + valid mline should be accepted");
    }

    #[test]
    fn ice_rejects_wrong_typed_mid() {
        let err = validate_call_event(&ice(serde_json::json!({
            "candidate": "candidate:1 1 udp 2113937151 192.0.2.1 50000 typ host",
            "sdpMid": 5,
        })))
        .expect_err("numeric sdpMid must be rejected");
        assert!(matches!(err, SignalingError::InvalidCandidate(_)));
    }

    #[test]
    fn ice_rejects_out_of_range_mline() {
        // Negative and >u16 indices are not valid m-line positions.
        for bad in [serde_json::json!(-1), serde_json::json!(70000), serde_json::json!(1.5)] {
            let err = validate_call_event(&ice(serde_json::json!({
                "candidate": "candidate:1 1 udp 2113937151 192.0.2.1 50000 typ host",
                "sdpMLineIndex": bad,
            })))
            .expect_err("out-of-range sdpMLineIndex must be rejected");
            assert!(matches!(err, SignalingError::InvalidCandidate(_)));
        }
    }

    #[test]
    fn ice_accepts_mid_string_only() {
        validate_call_event(&ice(serde_json::json!({
            "candidate": "candidate:1 1 udp 2113937151 192.0.2.1 50000 typ host",
            "sdpMid": "audio",
        })))
        .expect("string sdpMid alone should be accepted");
    }

    #[test]
    fn validate_call_event_accepts_end() {
        let ev = CallEvent::End {
            call_id: CallId::new(),
            room_id: RoomId::new(),
            by: ParticipantId::new(),
            reason: "hangup".into(),
        };
        validate_call_event(&ev).expect("end is valid");
    }

    // -------- CallRoster --------

    #[test]
    fn roster_add_and_remove() {
        let mut r = CallRoster::new(CallId::new());
        let a = ParticipantId::new();
        let b = ParticipantId::new();
        assert!(r.is_empty());
        r.add(a).unwrap();
        r.add(b).unwrap();
        assert_eq!(r.len(), 2);
        assert!(r.contains(a));
        assert!(!r.remove(a)); // still has b → not empty
        assert!(r.remove(b)); // last out → empty
        assert!(r.is_empty());
    }

    #[test]
    fn roster_rejects_duplicate_join() {
        let mut r = CallRoster::new(CallId::new());
        let a = ParticipantId::new();
        r.add(a).unwrap();
        let err = r.add(a).expect_err("dup must fail");
        assert!(matches!(err, SignalingError::AlreadyJoined));
    }

    #[test]
    fn roster_remove_unknown_is_noop() {
        let mut r = CallRoster::new(CallId::new());
        let ghost = ParticipantId::new();
        // Removing from an empty roster reports empty=true.
        assert!(r.remove(ghost));
        // After adding someone else, removing the ghost still leaves the roster non-empty.
        let real = ParticipantId::new();
        r.add(real).unwrap();
        assert!(!r.remove(ghost));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn roster_tracks_join_time() {
        let mut r = CallRoster::new(CallId::new());
        let a = ParticipantId::new();
        r.add(a).unwrap();
        assert!(r.joined_at(a).is_some());
        r.remove(a);
        assert!(r.joined_at(a).is_none());
    }

    // -------- default_rtc_config_from_env --------
    //
    // These tests mutate process-wide env vars, so they live in a single
    // `#[test]` and run serially.

    #[test]
    fn rtc_config_env_handling() {
        // Lock so concurrent test threads can't trample each other's env.
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

        let keys = [
            "AERO_STUN_URLS",
            "AERO_TURN_URL",
            "AERO_TURN_USERNAME",
            "AERO_TURN_PASSWORD",
            "AERO_ICE_TRANSPORT_POLICY",
        ];
        let saved: Vec<_> = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        for k in &keys {
            std::env::remove_var(k);
        }

        // 1. Fallback — no env → default STUN, policy "all".
        let cfg = default_rtc_config_from_env();
        assert_eq!(cfg.ice_servers.len(), 1);
        assert_eq!(cfg.ice_servers[0].urls, vec![DEFAULT_STUN_URL.to_owned()]);
        assert!(cfg.ice_servers[0].username.is_none());
        assert_eq!(cfg.ice_transport_policy, "all");

        // 2. Custom STUN list + TURN credentials → both present.
        std::env::set_var("AERO_STUN_URLS", "stun:a.example:3478, stun:b.example:3478");
        std::env::set_var("AERO_TURN_URL", "turn:turn.example:3478");
        std::env::set_var("AERO_TURN_USERNAME", "u");
        std::env::set_var("AERO_TURN_PASSWORD", "p");
        std::env::set_var("AERO_ICE_TRANSPORT_POLICY", "relay");
        let cfg = default_rtc_config_from_env();
        assert_eq!(cfg.ice_servers.len(), 2);
        assert_eq!(
            cfg.ice_servers[0].urls,
            vec!["stun:a.example:3478".to_owned(), "stun:b.example:3478".to_owned()]
        );
        assert_eq!(cfg.ice_servers[1].urls, vec!["turn:turn.example:3478".to_owned()]);
        assert_eq!(cfg.ice_servers[1].username.as_deref(), Some("u"));
        assert_eq!(cfg.ice_servers[1].credential.as_deref(), Some("p"));
        assert_eq!(cfg.ice_transport_policy, "relay");

        // 3. Invalid policy is ignored and falls back to "all".
        std::env::set_var("AERO_ICE_TRANSPORT_POLICY", "bogus");
        let cfg = default_rtc_config_from_env();
        assert_eq!(cfg.ice_transport_policy, "all");

        // restore
        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    fn rtc_config_serializes_camel_case() {
        let cfg = RtcConfig::new(vec![IceServer::stun(DEFAULT_STUN_URL)]);
        let json = serde_json::to_value(&cfg).unwrap();
        assert!(json.get("iceServers").is_some());
        assert_eq!(json.get("iceTransportPolicy").and_then(|v| v.as_str()), Some("all"));
    }
}
