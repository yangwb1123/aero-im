//! Codec-aware keyframe detection seam.
//!
//! The SFU runs in RTP-forwarding mode and never decodes media, so keyframe
//! detection has to be done per-codec from the raw RTP payload. This module is
//! the single dispatch point: callers resolve the [`Codec`] once (from the
//! negotiated payload-type mapping, see [`crate::SfuPeer::codec_for_pt`]) and
//! pass every payload through [`payload_is_keyframe`].

use crate::av1::av1_payload_is_keyframe;
use crate::h264::h264_payload_is_keyframe;
use crate::h265::h265_payload_is_keyframe;
use crate::vp8::vp8_payload_is_keyframe;
use crate::vp9::vp9_payload_is_keyframe;

/// Video codec of an RTP payload, as far as keyframe detection is concerned.
///
/// `Unknown` covers everything the SFU has no payload-descriptor parser for
/// (audio codecs, RTX, …) — those payloads are never reported as keyframes,
/// preserving the pre-codec-seam behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Codec {
    /// H.264 / AVC (RFC 6184 payload format).
    H264,
    /// H.265 / HEVC (RFC 7798 payload format).
    H265,
    /// VP8 (RFC 7741 payload format).
    Vp8,
    /// VP9 (draft-ietf-payload-vp9 payload format).
    Vp9,
    /// AV1 (draft-ietf-payload-av1 aggregation header).
    Av1,
    /// Unrecognized or unsupported codec — keyframe detection always `false`.
    #[default]
    Unknown,
}

impl Codec {
    /// Whether packets for this codec need a decodable random-access point
    /// before a newly attached bridge may start forwarding them.
    ///
    /// Audio, RTX, and unknown codecs have no video keyframe concept. Treating
    /// them as keyframe-gated would permanently suppress Opus because
    /// [`payload_is_keyframe`] correctly returns `false` for it.
    #[must_use]
    pub const fn requires_keyframe(self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

impl From<str0m::format::Codec> for Codec {
    fn from(c: str0m::format::Codec) -> Self {
        match c {
            str0m::format::Codec::H264 => Self::H264,
            str0m::format::Codec::H265 => Self::H265,
            str0m::format::Codec::Vp8 => Self::Vp8,
            str0m::format::Codec::Vp9 => Self::Vp9,
            str0m::format::Codec::Av1 => Self::Av1,
            _ => Self::Unknown,
        }
    }
}

/// Returns `true` when `payload` (raw RTP payload bytes, no RTP header)
/// begins a keyframe for the given `codec`.
///
/// Dispatches to the per-codec detectors ([`h264_payload_is_keyframe`],
/// [`h265_payload_is_keyframe`], [`vp8_payload_is_keyframe`],
/// [`vp9_payload_is_keyframe`], [`av1_payload_is_keyframe`]); [`Codec::Unknown`]
/// is always `false`. Malformed or truncated payloads return `false` — never panic.
#[must_use]
pub fn payload_is_keyframe(codec: Codec, payload: &[u8]) -> bool {
    match codec {
        Codec::H264 => h264_payload_is_keyframe(payload),
        Codec::H265 => h265_payload_is_keyframe(payload),
        Codec::Vp8 => vp8_payload_is_keyframe(payload),
        Codec::Vp9 => vp9_payload_is_keyframe(payload),
        Codec::Av1 => av1_payload_is_keyframe(payload),
        Codec::Unknown => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{payload_is_keyframe, Codec};

    /// H.264 single-NAL IDR (type 5).
    const H264_IDR: [u8; 3] = [0x65, 0x88, 0x84];
    /// VP8 descriptor S=1/PID=0 + payload header P=0.
    const VP8_KEY: [u8; 3] = [0x10, 0x00, 0x9D];
    /// VP9 flags P=0/B=1 + one payload byte.
    const VP9_KEY: [u8; 2] = [0x08, 0x86];
    /// AV1 aggregation header with the N (new coded video sequence) bit set.
    const AV1_KEY: [u8; 2] = [0x08, 0xAA];
    /// H.265 single-NAL `IDR_W_RADL` (type 19) header byte 0 + a payload byte.
    const H265_KEY: [u8; 2] = [19 << 1, 0x01];

    #[test]
    fn dispatches_to_h264() {
        assert!(payload_is_keyframe(Codec::H264, &H264_IDR));
        // non-IDR slice
        assert!(!payload_is_keyframe(Codec::H264, &[0x41, 0x9A]));
    }

    #[test]
    fn dispatches_to_vp8() {
        assert!(payload_is_keyframe(Codec::Vp8, &VP8_KEY));
        // interframe (payload header P=1)
        assert!(!payload_is_keyframe(Codec::Vp8, &[0x10, 0x01, 0x9D]));
    }

    #[test]
    fn dispatches_to_vp9() {
        assert!(payload_is_keyframe(Codec::Vp9, &VP9_KEY));
        // inter-predicted (P=1)
        assert!(!payload_is_keyframe(Codec::Vp9, &[0x48, 0x86]));
    }

    #[test]
    fn dispatches_to_av1() {
        assert!(payload_is_keyframe(Codec::Av1, &AV1_KEY));
        // N bit clear → continuation / inter frame.
        assert!(!payload_is_keyframe(Codec::Av1, &[0x00, 0xAA]));
    }

    #[test]
    fn dispatches_to_h265() {
        assert!(payload_is_keyframe(Codec::H265, &H265_KEY));
        // TRAIL_R (type 1) → not an IRAP picture.
        assert!(!payload_is_keyframe(Codec::H265, &[1 << 1, 0x01]));
    }

    #[test]
    fn unknown_codec_is_never_keyframe() {
        for payload in [
            &H264_IDR[..],
            &VP8_KEY[..],
            &VP9_KEY[..],
            &AV1_KEY[..],
            &H265_KEY[..],
            &[][..],
        ] {
            assert!(!payload_is_keyframe(Codec::Unknown, payload));
        }
    }

    #[test]
    fn cross_codec_payloads_do_not_panic() {
        // Feeding the wrong codec's bytes must be safe (result is undefined
        // but must not panic); empty input is false for all codecs.
        let codecs = [
            Codec::H264,
            Codec::H265,
            Codec::Vp8,
            Codec::Vp9,
            Codec::Av1,
            Codec::Unknown,
        ];
        for codec in codecs {
            for payload in [
                &H264_IDR[..],
                &VP8_KEY[..],
                &VP9_KEY[..],
                &AV1_KEY[..],
                &H265_KEY[..],
                &[][..],
            ] {
                let _ = payload_is_keyframe(codec, payload);
            }
            assert!(!payload_is_keyframe(codec, &[]));
        }
    }

    #[test]
    fn maps_str0m_codecs() {
        use str0m::format::Codec as S;
        assert_eq!(Codec::from(S::H264), Codec::H264);
        assert_eq!(Codec::from(S::H265), Codec::H265);
        assert_eq!(Codec::from(S::Vp8), Codec::Vp8);
        assert_eq!(Codec::from(S::Vp9), Codec::Vp9);
        assert_eq!(Codec::from(S::Av1), Codec::Av1);
        assert_eq!(Codec::from(S::Opus), Codec::Unknown);
        assert_eq!(Codec::from(S::Rtx), Codec::Unknown);
    }

    #[test]
    fn default_is_unknown() {
        assert_eq!(Codec::default(), Codec::Unknown);
    }

    #[test]
    fn only_known_video_codecs_require_keyframes() {
        for codec in [Codec::H264, Codec::H265, Codec::Vp8, Codec::Vp9, Codec::Av1] {
            assert!(codec.requires_keyframe());
        }
        assert!(!Codec::Unknown.requires_keyframe());
    }
}
