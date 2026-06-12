//! AV1 RTP keyframe detection from the aggregation header (draft-ietf-payload-av1).
//!
//! The SFU forwards without decoding, so a keyframe must be detected from the
//! one-byte RTP **aggregation header** that prefixes every AV1 payload:
//!
//! ```text
//!  0 1 2 3 4 5 6 7
//! +-+-+-+-+-+-+-+-+
//! |Z|Y| W |N|-|-|-|
//! +-+-+-+-+-+-+-+-+
//! ```
//!
//! The `N` bit (0x08) is set on the **first packet of a new coded video
//! sequence** — a sequence header OBU followed by a key frame — which is exactly
//! the random-access point an SFU needs for layer-switch / PLI gating. This is
//! the standard forwarding-mode heuristic (the same signal Janus/mediasoup use);
//! it does not require parsing the OBU stream.

/// Returns `true` when `payload` (raw AV1 RTP payload, no RTP header) begins a
/// new coded video sequence — i.e. a keyframe — per the aggregation header's
/// `N` bit. An empty payload returns `false`; never panics.
#[must_use]
pub fn av1_payload_is_keyframe(payload: &[u8]) -> bool {
    // Aggregation header is the first byte; N (new coded video sequence) = 0x08.
    matches!(payload.first(), Some(b) if b & 0x08 != 0)
}

#[cfg(test)]
mod tests {
    use super::av1_payload_is_keyframe;

    #[test]
    fn n_bit_set_is_keyframe() {
        // N bit (0x08) set → start of a new coded video sequence.
        assert!(av1_payload_is_keyframe(&[0x08]));
        assert!(av1_payload_is_keyframe(&[0x08, 0x12, 0x34]));
        // N set alongside other aggregation-header bits (Z/Y/W).
        assert!(av1_payload_is_keyframe(&[0b1101_1000, 0xAA]));
    }

    #[test]
    fn n_bit_clear_is_not_keyframe() {
        // Continuation / inter frames: N clear.
        assert!(!av1_payload_is_keyframe(&[0x00, 0x12]));
        assert!(!av1_payload_is_keyframe(&[0b1101_0000, 0xAA]));
    }

    #[test]
    fn empty_payload_is_not_keyframe() {
        assert!(!av1_payload_is_keyframe(&[]));
    }
}
