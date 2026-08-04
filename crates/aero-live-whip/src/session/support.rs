//! SDP acceptance and construction details kept out of the media-loop module.

use std::borrow::Cow;
use std::net::{IpAddr, SocketAddr};
use std::time::Instant;

use str0m::change::{SdpAnswer, SdpOffer};
use str0m::media::Pt;
use str0m::{Candidate, Rtc, RtcError};

/// Maximum inbound UDP datagram buffered by the media loop.
pub(super) const RECV_BUF: usize = 2048;

/// Default jitter-buffer window for inbound RTP.
pub const DEFAULT_REORDER_WINDOW: u16 = 64;

/// Errors specific to driving a str0m session.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The SDP offer could not be parsed by str0m.
    #[error("parse offer: {0}")]
    Offer(String),
    /// str0m rejected the offer or failed to produce an answer.
    #[error("rtc: {0}")]
    Rtc(#[from] RtcError),
    /// The supplied ingest address was not a valid `SocketAddr`.
    #[error("invalid ingest address {0}:{1}")]
    Addr(String, u16),
    /// Building a local ICE candidate failed.
    #[error("candidate: {0}")]
    Candidate(String),
    /// Socket I/O error while running the loop.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub(super) struct AcceptedSession {
    pub(super) rtc: Rtc,
    pub(super) local_addr: SocketAddr,
    pub(super) video_pts: Vec<Pt>,
    pub(super) answer: SdpAnswer,
}

pub(super) fn accept(
    offer_sdp: &str,
    ingest_host: &str,
    ingest_port: u16,
) -> Result<AcceptedSession, SessionError> {
    aero_signaling::signaling::validate_sdp(offer_sdp)
        .map_err(|error| SessionError::Offer(error.to_string()))?;
    let ingest_ip: IpAddr = ingest_host
        .parse()
        .map_err(|_| SessionError::Addr(ingest_host.to_string(), ingest_port))?;
    let local_addr = SocketAddr::new(ingest_ip, ingest_port);

    let normalized_offer = normalize_descriptive_fields(offer_sdp);
    let offer = SdpOffer::from_sdp_string(normalized_offer.as_ref())
        .map_err(|error| SessionError::Offer(error.to_string()))?;

    // RTP mode surfaces raw packets so this crate's RFC 6184 depacketizer owns
    // NAL reassembly while str0m still owns ICE, DTLS, and SRTP.
    let mut rtc = Rtc::builder().set_rtp_mode(true).build(Instant::now());
    let candidate = Candidate::host(local_addr, "udp")
        .map_err(|error| SessionError::Candidate(error.to_string()))?;
    let _ = rtc.add_local_candidate(candidate);
    let answer = rtc.sdp_api().accept_offer(offer)?;

    let video_pts = rtc
        .codec_config()
        .params()
        .iter()
        .filter(|params| params.spec().codec.is_video())
        .map(str0m::format::PayloadParams::pt)
        .collect();

    Ok(AcceptedSession {
        rtc,
        local_addr,
        video_pts,
        answer,
    })
}

/// Normalize SDP fields that are descriptive rather than transport semantics.
///
/// str0m 0.19 accepts only its own or `-` values for the origin username and
/// session name, while interoperable WHIP publishers such as FFmpeg 8 emit
/// legal values like `o=FFmpeg …` and `s=FFmpegPublishSession`. Replacing only
/// those two labels preserves the origin id/version, ICE, DTLS, media, codec,
/// SSRC and direction attributes.
fn normalize_descriptive_fields(offer_sdp: &str) -> Cow<'_, str> {
    let needs_normalization = offer_sdp.lines().any(|line| {
        let line = line.trim_end_matches('\r');
        (line.starts_with("o=") && !line.starts_with("o=- ") && !line.starts_with("o=str0m-"))
            || (line.starts_with("s=") && line != "s=-")
    });
    if !needs_normalization {
        return Cow::Borrowed(offer_sdp);
    }

    let mut normalized = String::with_capacity(offer_sdp.len());
    for raw_line in offer_sdp.lines() {
        let line = raw_line.trim_end_matches('\r');
        if let Some(origin) = line.strip_prefix("o=") {
            if let Some((_, remainder)) = origin.split_once(' ') {
                normalized.push_str("o=- ");
                normalized.push_str(remainder);
            } else {
                normalized.push_str(line);
            }
        } else if line.starts_with("s=") {
            normalized.push_str("s=-");
        } else {
            normalized.push_str(line);
        }
        normalized.push_str("\r\n");
    }
    Cow::Owned(normalized)
}

/// Convert a str0m media timestamp to the MPEG-TS 90 kHz clock.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(super) fn media_time_to_90k(time: str0m::media::MediaTime) -> u64 {
    (time.as_seconds() * 90_000.0).max(0.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    const FFMPEG_8_OFFER: &str = "v=0\r\n\
o=FFmpeg 4489045141692799359 2 IN IP4 127.0.0.1\r\n\
s=FFmpegPublishSession\r\n\
t=0 0\r\n\
a=group:BUNDLE 1\r\n\
a=extmap-allow-mixed\r\n\
a=msid-semantic: WMS\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 106 105\r\n\
c=IN IP4 0.0.0.0\r\n\
a=ice-ufrag:3e21cd58\r\n\
a=ice-pwd:d5428b9ea63edb1bd83ca02a7fc41495\r\n\
a=fingerprint:sha-256 09:6F:1B:37:56:37:8E:4C:07:D8:56:29:B8:BA:9C:E7:6E:7C:E3:F8:F4:48:2E:C7:9D:40:A1:53:9B:C9:B5:7A\r\n\
a=setup:passive\r\n\
a=mid:1\r\n\
a=sendonly\r\n\
a=msid:FFmpeg video\r\n\
a=rtcp-mux\r\n\
a=rtcp-rsize\r\n\
a=rtpmap:106 H264/90000\r\n\
a=fmtp:106 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001e\r\n\
a=rtcp-fb:106 nack\r\n\
a=rtpmap:105 rtx/90000\r\n\
a=fmtp:105 apt=106\r\n\
a=ssrc-group:FID 1969524073 1969524074\r\n\
a=ssrc:1969524073 cname:FFmpeg\r\n\
a=ssrc:1969524073 msid:FFmpeg video\r\n";

    #[test]
    fn ffmpeg_descriptive_fields_are_normalized_without_touching_media() {
        let normalized = normalize_descriptive_fields(FFMPEG_8_OFFER);
        assert!(normalized.contains("o=- 4489045141692799359 2 IN IP4 127.0.0.1"));
        assert!(normalized.contains("s=-\r\n"));
        assert!(normalized.contains("a=setup:passive"));
        assert!(normalized.contains("a=rtpmap:106 H264/90000"));
        assert!(normalized.contains("a=ssrc-group:FID 1969524073 1969524074"));

        let accepted =
            accept(FFMPEG_8_OFFER, "127.0.0.1", 7002).expect("FFmpeg offer must interoperate");
        let answer = accepted.answer.to_sdp_string();
        assert!(answer.contains("a=setup:active"));
        assert!(answer.contains("a=recvonly"));
        assert!(accepted.video_pts.contains(&Pt::from(106)));
    }
}
