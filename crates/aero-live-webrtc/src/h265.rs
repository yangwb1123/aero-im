//! H.265 / HEVC RTP keyframe detection from raw payload bytes (RFC 7798).
//!
//! The SFU forwards without decoding, so a keyframe (IRAP picture) is detected
//! from the 2-byte NAL unit header that prefixes every HEVC RTP payload:
//!
//! ```text
//!  0               1
//!  0 1 2 3 4 5 6 7 8 ...
//! +-+-+-+-+-+-+-+-+-...
//! |F|   Type    |LayerId| TID |
//! +-+-+-+-+-+-+-+-+-...
//! ```
//!
//! `Type` (bits 1–6 of byte 0) names the NAL unit. IRAP (Intra Random Access
//! Point) pictures — BLA / IDR / CRA, types 16–21 — are keyframes. Two RTP
//! packetization wrappers carry an inner NAL whose type must be read instead:
//!
//! | RFC 7798 type | Handled |
//! |---|---|
//! | Single NAL (0–47) | ✓ |
//! | Fragmentation Unit, FU (49) | ✓ — inner type from the FU header, START fragment only |
//! | Aggregation Packet, AP (48) | treated as non-keyframe (rare for IRAP) |

/// Lowest IRAP NAL type (`BLA_W_LP`).
const IRAP_LOW: u8 = 16;
/// Highest IRAP NAL type (`CRA_NUT`).
const IRAP_HIGH: u8 = 21;
/// RFC 7798 Fragmentation Unit packet type.
const FU: u8 = 49;

/// Returns `true` when `payload` (raw H.265 RTP payload, no RTP header) begins an
/// IRAP (keyframe) picture. Handles single NAL units and the START fragment of a
/// fragmentation unit; truncated payloads return `false` and never panic.
#[must_use]
pub fn h265_payload_is_keyframe(payload: &[u8]) -> bool {
    let Some(&b0) = payload.first() else {
        return false;
    };
    let nal_type = (b0 >> 1) & 0x3F;
    if nal_type == FU {
        // FU: the real NAL type is the FU header's low 6 bits (3rd byte), and only
        // a START fragment (high bit set) begins the picture.
        let Some(&fu_header) = payload.get(2) else {
            return false;
        };
        let start = fu_header & 0x80 != 0;
        let inner = fu_header & 0x3F;
        start && (IRAP_LOW..=IRAP_HIGH).contains(&inner)
    } else {
        (IRAP_LOW..=IRAP_HIGH).contains(&nal_type)
    }
}

#[cfg(test)]
mod tests {
    use super::h265_payload_is_keyframe;

    /// Build a single-NAL header byte 0 for `nal_type` (F=0, top of LayerId=0).
    fn nal0(nal_type: u8) -> u8 {
        (nal_type & 0x3F) << 1
    }

    #[test]
    fn single_nal_irap_types_are_keyframes() {
        // IDR_W_RADL=19, IDR_N_LP=20, CRA_NUT=21, BLA_W_LP=16 — all IRAP.
        for t in [16u8, 19, 20, 21] {
            assert!(h265_payload_is_keyframe(&[nal0(t), 0x01]), "type {t} is IRAP");
        }
    }

    #[test]
    fn single_nal_non_irap_is_not_keyframe() {
        // TRAIL_R=1, TRAIL_N=0 (inter), VPS=32, SPS=33, PPS=34 are not pictures.
        for t in [0u8, 1, 32, 33, 34] {
            assert!(!h265_payload_is_keyframe(&[nal0(t), 0x01]), "type {t} not IRAP");
        }
    }

    #[test]
    fn fu_start_of_irap_is_keyframe() {
        // FU packet (49): [hdr0=FU, hdr1, FU header]. START bit 0x80 + inner IDR(19).
        let fu_header = 0x80 | 19;
        assert!(h265_payload_is_keyframe(&[nal0(49), 0x01, fu_header]));
        // Non-START fragment of the same IRAP is NOT a frame start.
        assert!(!h265_payload_is_keyframe(&[nal0(49), 0x01, 19]));
        // START fragment of a non-IRAP NAL is not a keyframe.
        assert!(!h265_payload_is_keyframe(&[nal0(49), 0x01, 0x80 | 1]));
    }

    #[test]
    fn truncated_and_empty_are_not_keyframes() {
        assert!(!h265_payload_is_keyframe(&[]));
        // FU packet (type 49) missing its FU-header byte → can't confirm
        // START/inner type, so not a keyframe.
        assert!(!h265_payload_is_keyframe(&[nal0(49), 0x01]));
    }
}
