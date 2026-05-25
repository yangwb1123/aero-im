//! WHIP (WebRTC-HTTP Ingestion, RFC 9725) ingest with a **real** str0m media
//! plane.
//!
//! ## Scope (P5)
//!
//! This crate terminates a browser WHIP publisher's WebRTC session using the
//! [`str0m`] sans-IO WebRTC implementation and routes its H.264 media toward
//! HLS:
//!
//! - [`accept_whip_offer`] — parse the publisher's SDP **offer** with str0m and
//!   return a [`WhipResource`] carrying a **real** SDP answer (str0m-generated
//!   DTLS fingerprint, ICE ufrag/pwd, and a host ICE candidate at the ingest
//!   address). This replaces the previous hand-rolled answer string while
//!   keeping the same signature the server's `POST /whip/:stream_key` route
//!   already calls.
//! - [`WhipSession`] ([`session`]) — owns the `Rtc` and runs the standard
//!   str0m event loop (`poll_output` → UDP `Transmit`/`Timeout`,
//!   `handle_input` ← UDP `Receive`/`Timeout`). Spawning it is the server's job
//!   (out of scope here), so it is provided as a clean, tested-where-possible
//!   API rather than auto-started.
//! - [`H264Depacketizer`](depacketize::H264Depacketizer) ([`depacketize`]) — an
//!   RFC 6184 H.264 depacketizer (single-NAL, FU-A, STAP-A) that reassembles
//!   Annex-B access units from received RTP. Heavily unit-tested.
//! - [`MediaSink`](hls_sink::MediaSink) ([`hls_sink`]) — the access-unit → HLS
//!   boundary, including the Annex-B↔AVCC repackaging that lets the existing
//!   `aero_live_hls::FlvToTsConverter` be reused, plus the concrete
//!   [`HlsSink`](hls_sink::HlsSink) that synthesizes an avcC from the first
//!   keyframe's SPS/PPS, muxes MPEG-TS, cuts segments on IDR keyframes, and
//!   drives [`aero_live_hls::HlsWriter`] (via an async
//!   [`HlsSegmentWriter`](hls_sink::HlsSegmentWriter)) to persist `.ts` +
//!   `index.m3u8`.
//!
//! - [`WhipRegistry`] — in-memory map of `stream_id → resource`, enforcing a
//!   single live publisher per stream.
//!
//! ## Verified vs. pending
//!
//! Compiles and is tested: SDP offer→answer (str0m), the H.264 depacketizer, the
//! Annex-B/AVCC bridge, and — new in P5 — the **complete RTP→HLS media path** at
//! the byte level: an integration test ([`session`] tests) feeds synthetic H.264
//! RTP packets (single-NAL, STAP-A, FU-A) through the real depacketizer into
//! [`HlsSink`](hls_sink::HlsSink) and asserts real `.ts` segments + a well-formed
//! `index.m3u8` land on disk. **Still pending live validation** (no browser
//! publisher available in CI): only the ICE/DTLS/SRTP handshake that delivers
//! those RTP packets — i.e. [`WhipSession::run`]'s transport — everything
//! downstream of a received RTP packet is now exercised.

pub mod depacketize;
pub mod hls_sink;
pub mod packetize;
pub mod reorder;
pub mod session;
pub mod whep;

#[cfg(test)]
pub(crate) mod testutil;

use std::sync::Arc;

use aero_common::Stream;
use aero_signaling::signaling::validate_sdp;
use parking_lot::Mutex;
use thiserror::Error;
use ulid::Ulid;

pub use hls_sink::{hls_sink, HlsSegment, HlsSegmentWriter, HlsSink, MediaSink};
pub use session::{SessionError, WhipSession};
pub use whep::{accept_whep_offer, NalSource, WhepSession};

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

impl From<SessionError> for WhipError {
    fn from(e: SessionError) -> Self {
        match e {
            SessionError::Offer(m) => WhipError::InvalidSdp(m),
            other => WhipError::Internal(other.to_string()),
        }
    }
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

/// Accept a WHIP SDP offer and produce a [`WhipResource`] with a **real**
/// str0m-generated SDP answer.
///
/// Signature is unchanged from the P5 placeholder so the server's
/// `POST /whip/:stream_key` route keeps compiling against it. Internally it now
/// builds a [`WhipSession`] (str0m `Rtc` + host candidate) and returns the
/// negotiated answer.
///
/// The session itself is *discarded* by this convenience entry point because
/// the route only needs the answer to reply over HTTP. To actually terminate
/// media, the server should instead call [`WhipSession::accept`] directly,
/// keep the returned session, bind a UDP socket at the advertised ingest
/// address, and spawn [`WhipSession::run`]. This split keeps the HTTP-signaling
/// surface and the media-plane lifetime decoupled (the latter is out of scope
/// to wire from this crate).
pub fn accept_whip_offer(
    stream: &Stream,
    offer_sdp: &str,
    ingest_host: &str,
    ingest_port: u16,
) -> Result<WhipResource, WhipError> {
    // Cheap structural gate first (size cap, `v=0`), then hand to str0m which
    // does the full SDP parse + answer generation.
    validate_sdp(offer_sdp).map_err(|e| WhipError::InvalidSdp(e.to_string()))?;

    let (_session, answer) = WhipSession::accept(offer_sdp, ingest_host, ingest_port)?;
    let answer_sdp = answer.to_sdp_string();

    // Surface the negotiated ICE/DTLS params on the resource for callers that
    // log or proxy them. They are parsed out of the answer str0m produced.
    let ice_ufrag = sdp_value(&answer_sdp, "a=ice-ufrag:").unwrap_or_default();
    let ice_pwd = sdp_value(&answer_sdp, "a=ice-pwd:").unwrap_or_default();
    let dtls_fingerprint = sdp_value(&answer_sdp, "a=fingerprint:").unwrap_or_default();

    Ok(WhipResource {
        stream_id: stream.id,
        resource_id: Ulid::new(),
        ice_ufrag,
        ice_pwd,
        dtls_fingerprint,
        answer_sdp,
    })
}

/// Pull the value following the first `prefix` SDP attribute line (trimmed of
/// trailing CR). Returns `None` if absent.
fn sdp_value(sdp: &str, prefix: &str) -> Option<String> {
    sdp.lines()
        .find_map(|l| l.strip_prefix(prefix))
        .map(|v| v.trim_end_matches('\r').trim().to_string())
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

    /// A real WHIP publisher offer str0m can parse (mirrors a browser's
    /// sendonly audio+video with ICE/DTLS attributes).
    const OFFER: &str = "v=0\r\n\
o=- 4611731400430051336 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0 1\r\n\
a=msid-semantic: WMS\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:abcd\r\n\
a=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
a=fingerprint:sha-256 11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=sendonly\r\n\
a=rtcp-mux\r\n\
a=rtpmap:111 opus/48000/2\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:abcd\r\n\
a=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
a=fingerprint:sha-256 11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00\r\n\
a=setup:actpass\r\n\
a=mid:1\r\n\
a=sendonly\r\n\
a=rtcp-mux\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n";

    #[test]
    fn accept_emits_real_str0m_answer() {
        let s = stub_stream();
        let r = accept_whip_offer(&s, OFFER, "127.0.0.1", 9000).unwrap();
        assert_eq!(r.stream_id, s.id);
        assert!(r.answer_sdp.starts_with("v=0"));
        assert!(r.answer_sdp.contains("m=audio"));
        assert!(r.answer_sdp.contains("m=video"));
        // str0m populates these; our parser lifts them onto the resource.
        assert!(!r.ice_ufrag.is_empty(), "ice-ufrag should be extracted");
        assert!(!r.ice_pwd.is_empty(), "ice-pwd should be extracted");
        assert!(
            r.dtls_fingerprint.starts_with("sha-256"),
            "fingerprint: {}",
            r.dtls_fingerprint
        );
    }

    #[test]
    fn rejects_garbage_sdp() {
        let s = stub_stream();
        let err = accept_whip_offer(&s, "garbage", "127.0.0.1", 9000).unwrap_err();
        assert!(matches!(err, WhipError::InvalidSdp(_)));
    }

    #[test]
    fn rejects_structurally_valid_but_unparseable_offer() {
        let s = stub_stream();
        // Passes the `v=0` gate but is not a complete SDP str0m can accept.
        let err = accept_whip_offer(&s, "v=0\r\nbroken", "127.0.0.1", 9000).unwrap_err();
        assert!(matches!(err, WhipError::InvalidSdp(_)));
    }

    #[test]
    fn registry_enforces_single_publisher() {
        let reg = WhipRegistry::new();
        let s = stub_stream();
        let r = accept_whip_offer(&s, OFFER, "127.0.0.1", 9000).unwrap();
        reg.insert(r.clone()).unwrap();
        let r2 = accept_whip_offer(&s, OFFER, "127.0.0.1", 9000).unwrap();
        assert!(matches!(reg.insert(r2).unwrap_err(), WhipError::Conflict));
        reg.remove(s.id);
        let r3 = accept_whip_offer(&s, OFFER, "127.0.0.1", 9000).unwrap();
        reg.insert(r3).unwrap();
    }

    #[test]
    fn sdp_value_extracts_and_trims() {
        let sdp = "v=0\r\na=ice-ufrag:XYZ\r\na=ice-pwd:secretvalue\r\n";
        assert_eq!(sdp_value(sdp, "a=ice-ufrag:").as_deref(), Some("XYZ"));
        assert_eq!(sdp_value(sdp, "a=ice-pwd:").as_deref(), Some("secretvalue"));
        assert_eq!(sdp_value(sdp, "a=missing:"), None);
    }
}
