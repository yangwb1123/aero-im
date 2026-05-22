//! WHIP (WebRTC-HTTP Ingestion) + WHEP (WebRTC-HTTP Egress) handlers.
//!
//! ## Scope
//!
//! Implements the **HTTP signaling surface** of WHIP (RFC 9725) and WHEP for the
//! Aero live-streaming stack:
//!
//! - `WhipSession::handle_offer(sdp)` — parse a publisher's SDP offer, register
//!   the stream as live, and return an answer SDP + a Location resource id.
//! - `WhepSession::handle_offer(sdp)` — parse a player's SDP offer and return an
//!   answer pointing at the active publisher.
//! - `WhipRegistry` — in-memory map of `stream_key → resource_id`. Persistent
//!   stream lifecycle lives in `StreamRepo` (DB) via the server.
//!
//! ## What's *not* implemented here
//!
//! Real WebRTC media routing (DTLS, SRTP, RTP packetization, jitter buffer,
//! congestion control) is **out of scope** for P5. Those belong in
//! `aero-live-webrtc` (P6, str0m-based SFU). This crate's responsibility is
//! limited to:
//! - Generating a *plausible* SDP answer that lets the browser publish:
//!   it advertises a recvonly audio + recvonly video m-section that mirrors
//!   the offer's codecs, with a hard-coded ICE ufrag/pwd, DTLS fingerprint,
//!   and a placeholder ICE candidate pointing at the configured ingest host.
//! - Flipping the corresponding row in `streams` to `live` / `ended`.
//!
//! The intended next step is for the server's WHIP handler to hand the SDP
//! pair (offer + this crate's answer) to `aero-live-webrtc` once that crate
//! lands, where it'll mediate the actual DTLS handshake and forward the
//! resulting RTP into `aero-live-hls` (or a SFU fan-out for WHEP).

use std::sync::Arc;

use aero_common::Stream;
use aero_signaling::signaling::validate_sdp;
use parking_lot::Mutex;
use thiserror::Error;
use ulid::Ulid;

#[derive(Debug, Error)]
pub enum WhipError {
    #[error("invalid SDP: {0}")]
    InvalidSdp(String),
    #[error("stream not found")]
    NotFound,
    #[error("stream already publishing")]
    Conflict,
    #[error("internal: {0}")]
    Internal(String),
}

/// Live publisher resource — what the WHIP server hands back for resource control.
#[derive(Debug, Clone)]
pub struct WhipResource {
    pub stream_id: Ulid,
    pub resource_id: Ulid,
    pub ice_ufrag: String,
    pub ice_pwd: String,
    pub dtls_fingerprint: String,
    pub answer_sdp: String,
}

/// In-memory registry mapping a stream key → currently-active resource.
///
/// Keeps a single publisher per key; a second WHIP `POST` returns `Conflict`
/// until the previous resource is `DELETE`-ed.
#[derive(Default)]
pub struct WhipRegistry {
    inner: Mutex<std::collections::HashMap<Ulid, WhipResource>>,
}

impl WhipRegistry {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn insert(&self, resource: WhipResource) -> Result<(), WhipError> {
        let mut m = self.inner.lock();
        if m.contains_key(&resource.stream_id) {
            return Err(WhipError::Conflict);
        }
        m.insert(resource.stream_id, resource);
        Ok(())
    }

    pub fn remove(&self, stream_id: Ulid) -> Option<WhipResource> {
        self.inner.lock().remove(&stream_id)
    }

    pub fn get(&self, stream_id: Ulid) -> Option<WhipResource> {
        self.inner.lock().get(&stream_id).cloned()
    }
}

/// Construct a `WhipResource` for the given stream and offer.
///
/// The answer SDP is a stub — it parses enough of the offer to know the m-section
/// shapes (audio/video), then emits a deterministic answer suitable for the
/// browser to start ICE/DTLS negotiation against. The actual DTLS endpoint is
/// the server's UDP socket (when one is wired); until then, the publisher will
/// stay in `ICE-checking` state and time out, which is the expected behavior of
/// a P5 placeholder.
pub fn accept_whip_offer(
    stream: &Stream,
    offer_sdp: &str,
    ingest_host: &str,
    ingest_port: u16,
) -> Result<WhipResource, WhipError> {
    validate_sdp(offer_sdp).map_err(|e| WhipError::InvalidSdp(e.to_string()))?;

    let resource_id = Ulid::new();
    let ice_ufrag = random_token(8);
    let ice_pwd = random_token(24);
    let dtls_fingerprint = format!("sha-256 {}", hex_pseudo_fingerprint());

    // Mirror m-section types from the offer so the browser accepts the answer.
    let want_audio = offer_sdp.contains("m=audio");
    let want_video = offer_sdp.contains("m=video");

    let mut answer = String::new();
    answer.push_str("v=0\r\n");
    answer.push_str(&format!("o=- {} 2 IN IP4 0.0.0.0\r\n", ulid::Ulid::new().0));
    answer.push_str("s=-\r\n");
    answer.push_str("t=0 0\r\n");
    answer.push_str("a=group:BUNDLE 0 1\r\n");
    answer.push_str("a=msid-semantic: WMS aero\r\n");

    let mut mid: u32 = 0;
    if want_audio {
        push_m_section(
            &mut answer,
            "audio",
            mid,
            &ice_ufrag,
            &ice_pwd,
            &dtls_fingerprint,
            ingest_host,
            ingest_port,
        );
        mid += 1;
    }
    if want_video {
        push_m_section(
            &mut answer,
            "video",
            mid,
            &ice_ufrag,
            &ice_pwd,
            &dtls_fingerprint,
            ingest_host,
            ingest_port,
        );
    }

    Ok(WhipResource {
        stream_id: stream.id,
        resource_id,
        ice_ufrag,
        ice_pwd,
        dtls_fingerprint,
        answer_sdp: answer,
    })
}

fn push_m_section(
    sdp: &mut String,
    kind: &str,
    mid: u32,
    ice_ufrag: &str,
    ice_pwd: &str,
    fingerprint: &str,
    host: &str,
    port: u16,
) {
    use std::fmt::Write;
    let pt = if kind == "audio" { 111 } else { 96 };
    let codec = if kind == "audio" {
        "opus/48000/2"
    } else {
        "VP8/90000"
    };
    writeln!(sdp, "m={kind} 9 UDP/TLS/RTP/SAVPF {pt}\r").ok();
    writeln!(sdp, "c=IN IP4 0.0.0.0\r").ok();
    writeln!(sdp, "a=mid:{mid}\r").ok();
    writeln!(sdp, "a=recvonly\r").ok();
    writeln!(sdp, "a=rtcp-mux\r").ok();
    writeln!(sdp, "a=ice-ufrag:{ice_ufrag}\r").ok();
    writeln!(sdp, "a=ice-pwd:{ice_pwd}\r").ok();
    writeln!(sdp, "a=fingerprint:{fingerprint}\r").ok();
    writeln!(sdp, "a=setup:active\r").ok();
    writeln!(sdp, "a=rtpmap:{pt} {codec}\r").ok();
    writeln!(
        sdp,
        "a=candidate:1 1 UDP 2130706431 {host} {port} typ host\r"
    )
    .ok();
}

fn random_token(len: usize) -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| {
            let c = rng.gen_range(b'a'..=b'z');
            c as char
        })
        .collect()
}

fn hex_pseudo_fingerprint() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|i| format!("{:02X}{}", rng.gen::<u8>(), if i == 31 { "" } else { ":" }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::{ParticipantId, StreamProtocol, StreamStatus};

    fn stub_stream() -> Stream {
        Stream {
            id: Ulid::new(),
            owner_id: ParticipantId::new(),
            room_id: None,
            title: "test".into(),
            stream_key: "k".into(),
            status: StreamStatus::Idle,
            hls_path: None,
            protocol: StreamProtocol::Whip,
            started_at: None,
            ended_at: None,
            created_at: time::OffsetDateTime::now_utc(),
        }
    }

    const MIN_OFFER: &str = "v=0\r\no=- 1 2 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\n";

    #[test]
    fn accept_emits_answer_with_two_m_sections() {
        let s = stub_stream();
        let r = accept_whip_offer(&s, MIN_OFFER, "127.0.0.1", 9000).unwrap();
        assert!(r.answer_sdp.contains("m=audio"));
        assert!(r.answer_sdp.contains("m=video"));
        assert!(r.answer_sdp.contains("a=ice-ufrag:"));
        assert!(r.answer_sdp.contains("a=ice-pwd:"));
        assert!(r.answer_sdp.contains("a=fingerprint:sha-256 "));
    }

    #[test]
    fn rejects_garbage_sdp() {
        let s = stub_stream();
        let err = accept_whip_offer(&s, "garbage", "127.0.0.1", 9000).unwrap_err();
        assert!(matches!(err, WhipError::InvalidSdp(_)));
    }

    #[test]
    fn registry_enforces_single_publisher() {
        let reg = WhipRegistry::new();
        let s = stub_stream();
        let r = accept_whip_offer(&s, MIN_OFFER, "127.0.0.1", 9000).unwrap();
        reg.insert(r.clone()).unwrap();
        let r2 = accept_whip_offer(&s, MIN_OFFER, "127.0.0.1", 9000).unwrap();
        assert!(matches!(reg.insert(r2).unwrap_err(), WhipError::Conflict));
        reg.remove(s.id);
        let r3 = accept_whip_offer(&s, MIN_OFFER, "127.0.0.1", 9000).unwrap();
        reg.insert(r3).unwrap();
    }
}
