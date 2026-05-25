//! H.264 keyframe detection from raw RTP payload bytes (RFC 6184).
//!
//! # Scope
//!
//! This module operates **only** on H.264 RTP payloads. For any other codec
//! (VP8, VP9, AV1, Opus, …) pass `false` as `is_keyframe` — the SFU has no
//! codec-agnostic way to detect intra frames from raw RTP in RTP-forwarding
//! mode.
//!
//! # Packet types covered
//!
//! | RFC 6184 type | NAL-type byte | Handled |
//! |---|---|---|
//! | Single NAL | 1–23 | ✓ |
//! | STAP-A | 24 | ✓ (iterates all aggregated NALs) |
//! | FU-A | 28 | ✓ (reads FU header from START fragment only) |
//! | STAP-B / MTAP16 / MTAP24 / FU-B | 25–27, 29 | treated as non-keyframe |
//!
//! # Keyframe NAL types
//!
//! - **Type 5** (IDR slice) — the definitive keyframe marker.
//! - **Type 7** (SPS) / **Type 8** (PPS) — parameter sets that always precede
//!   an IDR in a complete keyframe access unit (STAP-A or back-to-back packets).
//!   Treating them as keyframe indicators lets the SFU commit a layer switch at
//!   the right boundary even when SPS/PPS arrive bundled with the IDR in a
//!   STAP-A.

/// NAL unit types that mark a keyframe access unit.
const IDR: u8 = 5;
const SPS: u8 = 7;
const PPS: u8 = 8;

/// RFC 6184 RTP payload type identifiers.
const STAP_A: u8 = 24;
const FU_A: u8 = 28;

/// FU header bit that marks the first fragment of a fragmented NAL unit.
const FU_START_BIT: u8 = 0x80;

/// Returns `true` when the RTP payload `bytes` carries an H.264 keyframe
/// access unit (IDR slice, or a STAP-A containing SPS/PPS/IDR).
///
/// Returns `false` for:
/// - non-IDR slices (NAL type 1),
/// - FU-A continuations and END fragments (not START),
/// - empty or malformed payloads,
/// - any packet type this function does not recognise.
///
/// # Codec assumption
///
/// This function **must only be called for H.264 payloads**. Calling it with
/// VP8/VP9/AV1/Opus data will produce incorrect results. For unknown / non-H.264
/// codecs, callers should keep `is_keyframe = false`.
#[must_use]
pub fn h264_payload_is_keyframe(payload: &[u8]) -> bool {
    let Some(&first) = payload.first() else {
        return false;
    };

    let nal_type = first & 0x1F;

    match nal_type {
        // ── Single NAL unit (types 1–23) ────────────────────────────────────
        1..=23 => is_keyframe_nal_type(nal_type),

        // ── STAP-A (type 24): one or more NALs aggregated in one RTP packet ─
        STAP_A => {
            // Payload layout after the 1-byte RTP header:
            //   [ 2-byte size | NAL data ] [ 2-byte size | NAL data ] …
            let mut cursor = 1usize; // skip the STAP-A NAL header byte
            while cursor + 2 <= payload.len() {
                let size = u16::from_be_bytes([payload[cursor], payload[cursor + 1]]) as usize;
                cursor += 2;
                if size == 0 || cursor + size > payload.len() {
                    break; // malformed — stop, return false
                }
                let nal_hdr = payload[cursor] & 0x1F;
                if is_keyframe_nal_type(nal_hdr) {
                    return true;
                }
                cursor += size;
            }
            false
        }

        // ── FU-A (type 28): fragmented NAL unit ──────────────────────────────
        FU_A => {
            // Payload layout:
            //   byte 0: FU indicator (the byte we already have as `first`)
            //   byte 1: FU header  [ S | E | R | nal_type (5 bits) ]
            let Some(&fu_header) = payload.get(1) else {
                return false;
            };
            // Only the START fragment carries the NAL type that matters.
            if fu_header & FU_START_BIT == 0 {
                return false; // continuation or END fragment → not a keyframe start
            }
            let nal_type = fu_header & 0x1F;
            is_keyframe_nal_type(nal_type)
        }

        // All other types (STAP-B=25, MTAP16=26, MTAP24=27, FU-B=29, …)
        // are not parsed; conservatively return false.
        _ => false,
    }
}

/// Whether a raw NAL unit type byte indicates an IDR / parameter-set NAL.
#[inline]
fn is_keyframe_nal_type(nal_type: u8) -> bool {
    matches!(nal_type, IDR | SPS | PPS)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::h264_payload_is_keyframe;

    // ── Single-NAL cases ─────────────────────────────────────────────────────

    #[test]
    fn single_nal_idr_is_keyframe() {
        // NAL type 5 = IDR (0x65: forbidden_zero=0, nal_ref_idc=3, nal_type=5)
        let payload = [0x65u8, 0x88, 0x84, 0x00, 0x33];
        assert!(h264_payload_is_keyframe(&payload));
    }

    #[test]
    fn single_nal_sps_is_keyframe() {
        // NAL type 7 = SPS (0x67)
        let payload = [0x67u8, 0x42, 0xC0, 0x1F];
        assert!(h264_payload_is_keyframe(&payload));
    }

    #[test]
    fn single_nal_pps_is_keyframe() {
        // NAL type 8 = PPS (0x68)
        let payload = [0x68u8, 0xCE, 0x38, 0x80];
        assert!(h264_payload_is_keyframe(&payload));
    }

    #[test]
    fn single_nal_non_idr_is_not_keyframe() {
        // NAL type 1 = non-IDR slice (0x41)
        let payload = [0x41u8, 0x9A, 0x24, 0x6C];
        assert!(!h264_payload_is_keyframe(&payload));
    }

    #[test]
    fn single_nal_sei_is_not_keyframe() {
        // NAL type 6 = SEI (0x06)
        let payload = [0x06u8, 0x05, 0x12];
        assert!(!h264_payload_is_keyframe(&payload));
    }

    // ── FU-A cases ───────────────────────────────────────────────────────────

    #[test]
    fn fua_start_idr_is_keyframe() {
        // FU indicator: forbidden=0, nri=3, type=28 → 0x7C
        // FU header: S=1 E=0 R=0 nal_type=5 → 0x85
        let payload = [0x7Cu8, 0x85, 0xAB, 0xCD];
        assert!(h264_payload_is_keyframe(&payload));
    }

    #[test]
    fn fua_continuation_is_not_keyframe() {
        // FU header: S=0 (not start) nal_type=5 → 0x05
        let payload = [0x7Cu8, 0x05, 0xAB, 0xCD];
        assert!(!h264_payload_is_keyframe(&payload));
    }

    #[test]
    fn fua_end_is_not_keyframe() {
        // FU header: S=0 E=1 nal_type=5 → 0x45
        let payload = [0x7Cu8, 0x45, 0xAB, 0xCD];
        assert!(!h264_payload_is_keyframe(&payload));
    }

    #[test]
    fn fua_start_non_idr_is_not_keyframe() {
        // FU header: S=1 nal_type=1 → 0x81
        let payload = [0x7Cu8, 0x81, 0x12, 0x34];
        assert!(!h264_payload_is_keyframe(&payload));
    }

    #[test]
    fn fua_too_short_returns_false() {
        // Only FU indicator byte, no FU header
        let payload = [0x7Cu8];
        assert!(!h264_payload_is_keyframe(&payload));
    }

    // ── STAP-A cases ─────────────────────────────────────────────────────────

    #[test]
    fn stap_a_containing_sps_pps_idr_is_keyframe() {
        // Build a STAP-A with three NALs: SPS (7), PPS (8), IDR (5)
        // STAP-A header byte: type=24 → 0x78
        let sps_nal = [0x67u8, 0x42, 0xC0, 0x1F]; // 4 bytes
        let pps_nal = [0x68u8, 0xCE, 0x38, 0x80]; // 4 bytes
        let idr_nal = [0x65u8, 0x88, 0x84, 0x00]; // 4 bytes

        let mut payload = vec![0x78u8]; // STAP-A header
        for nal in [&sps_nal[..], &pps_nal[..], &idr_nal[..]] {
            // NALs are tiny fixed-size test constants; the cast is safe.
            #[allow(clippy::cast_possible_truncation)]
            let len = nal.len() as u16;
            payload.extend_from_slice(&len.to_be_bytes());
            payload.extend_from_slice(nal);
        }
        assert!(h264_payload_is_keyframe(&payload));
    }

    #[test]
    fn stap_a_containing_only_non_idr_is_not_keyframe() {
        // STAP-A with two non-IDR slices (type 1)
        let slice1 = [0x41u8, 0x12, 0x34]; // 3 bytes, type=1
        let slice2 = [0x41u8, 0xAB, 0xCD]; // 3 bytes, type=1

        let mut payload = vec![0x78u8]; // STAP-A header
        for nal in [&slice1[..], &slice2[..]] {
            // NALs are tiny fixed-size test constants; the cast is safe.
            #[allow(clippy::cast_possible_truncation)]
            let len = nal.len() as u16;
            payload.extend_from_slice(&len.to_be_bytes());
            payload.extend_from_slice(nal);
        }
        assert!(!h264_payload_is_keyframe(&payload));
    }

    #[test]
    fn stap_a_empty_after_header_returns_false() {
        // STAP-A header only, no NALs
        let payload = [0x78u8];
        assert!(!h264_payload_is_keyframe(&payload));
    }

    #[test]
    fn stap_a_malformed_size_returns_false() {
        // STAP-A header + a size field claiming 100 bytes but only 2 follow
        let payload = [0x78u8, 0x00, 0x64, 0x65, 0x88]; // size=100, only 2 bytes
        assert!(!h264_payload_is_keyframe(&payload));
    }

    // ── Edge cases ───────────────────────────────────────────────────────────

    #[test]
    fn empty_payload_returns_false() {
        assert!(!h264_payload_is_keyframe(&[]));
    }

    #[test]
    fn unrecognised_nal_type_returns_false() {
        // NAL type 25 = STAP-B (not handled)
        let payload = [0x19u8, 0x00, 0x04, 0x65, 0x88];
        assert!(!h264_payload_is_keyframe(&payload));
    }
}
