//! VP9 keyframe detection from raw RTP payload bytes
//! (draft-ietf-payload-vp9).
//!
//! # Scope
//!
//! Parses **only** as much of the VP9 RTP payload descriptor as is needed to
//! decide whether the packet starts a keyframe and to verify the descriptor is
//! well-formed (not truncated). It is *not* a general depacketizer — frame
//! reassembly, SVC layer routing and picture-id tracking are out of scope.
//!
//! # Wire layout (draft-ietf-payload-vp9 §4.2)
//!
//! ```text
//!        0 1 2 3 4 5 6 7
//!       +-+-+-+-+-+-+-+-+
//!       |I|P|L|F|B|E|V|Z| (REQUIRED)  flags
//!       +-+-+-+-+-+-+-+-+
//!   I:  |M| PICTURE ID  | (OPTIONAL)  7-bit, or 15-bit over 2 bytes when M=1
//!       +-+-+-+-+-+-+-+-+
//!   L:  | TID |U| SID |D| (OPTIONAL)  layer indices
//!       +-+-+-+-+-+-+-+-+
//!       |   TL0PICIDX   | (OPTIONAL)  non-flexible mode (F=0) only
//!       +-+-+-+-+-+-+-+-+
//!  F&P: |    P_DIFF   |N| (OPTIONAL)  flexible mode, up to 3 times while N=1
//!       +-+-+-+-+-+-+-+-+
//!   V:  | SS .. scalability structure (see `skip_scalability_structure`)
//!       +-+-+-+-+-+-+-+-+
//! ```
//!
//! A packet starts a keyframe iff `P=0` (not inter-picture predicted) **and**
//! `B=1` (beginning of a frame). The optional fields are still walked so that
//! truncated descriptors and descriptor-only packets are rejected.

/// Flags byte bit masks.
const I_BIT: u8 = 0x80;
const P_BIT: u8 = 0x40;
const L_BIT: u8 = 0x20;
const F_BIT: u8 = 0x10;
const B_BIT: u8 = 0x08;
const V_BIT: u8 = 0x02;

/// `PictureID` extension flag: 15-bit picture id over two bytes.
const M_BIT: u8 = 0x80;

/// `P_DIFF` continuation flag (another `P_DIFF` byte follows).
const N_BIT: u8 = 0x01;

/// Flexible mode allows at most 3 reference `P_DIFF` entries.
const MAX_P_DIFF: usize = 3;

/// Returns `true` when the RTP payload `bytes` carries the start of a VP9
/// keyframe (`P=0` and `B=1`).
///
/// Returns `false` for:
/// - inter-picture predicted frames (`P=1`),
/// - packets that do not begin a frame (`B=0`),
/// - descriptor-only packets (no VP9 payload after the descriptor),
/// - empty, truncated, or otherwise malformed payloads (never panics).
///
/// # Codec assumption
///
/// Must only be called for VP9 payloads; dispatch via
/// [`crate::codec::payload_is_keyframe`].
#[must_use]
pub fn vp9_payload_is_keyframe(payload: &[u8]) -> bool {
    let Some(&flags) = payload.first() else {
        return false;
    };

    if flags & P_BIT != 0 || flags & B_BIT == 0 {
        return false;
    }

    let mut cursor = 1usize;

    if flags & I_BIT != 0 {
        let Some(&pid0) = payload.get(cursor) else {
            return false;
        };
        // M=1 → 15-bit PictureID spanning two bytes.
        cursor += if pid0 & M_BIT != 0 { 2 } else { 1 };
    }

    if flags & L_BIT != 0 {
        cursor += 1; // TID/U/SID/D layer indices
        if flags & F_BIT == 0 {
            cursor += 1; // TL0PICIDX, non-flexible mode only
        }
    }

    // Flexible-mode reference indices are only present when P=1, which we
    // already rejected — but walk them anyway so the skip logic is total.
    if flags & F_BIT != 0 && flags & P_BIT != 0 {
        for _ in 0..MAX_P_DIFF {
            let Some(&p_diff) = payload.get(cursor) else {
                return false;
            };
            cursor += 1;
            if p_diff & N_BIT == 0 {
                break;
            }
        }
    }

    if flags & V_BIT != 0 {
        let Some(after_ss) = skip_scalability_structure(payload, cursor) else {
            return false;
        };
        cursor = after_ss;
    }

    // Require at least one byte of actual VP9 payload after the descriptor;
    // a descriptor-only or truncated packet is not a usable keyframe start.
    cursor < payload.len()
}

/// Walks the scalability structure (SS) starting at `cursor`, returning the
/// index of the first byte after it, or `None` if truncated.
///
/// ```text
///       +-+-+-+-+-+-+-+-+
///   V:  | N_S |Y|G|-|-|-|
///       +-+-+-+-+-+-+-+-+              -|
///   Y:  |     WIDTH     | (2 bytes)     .
///       |     HEIGHT    | (2 bytes)     . N_S + 1 times
///       +-+-+-+-+-+-+-+-+              -|
///   G:  |      N_G      |
///       +-+-+-+-+-+-+-+-+                           -|
///  N_G: | TID |U| R |-|-|                            .
///       +-+-+-+-+-+-+-+-+              -|            . N_G times
///       |    P_DIFF     |               . R times    .
///       +-+-+-+-+-+-+-+-+              -|           -|
/// ```
fn skip_scalability_structure(payload: &[u8], mut cursor: usize) -> Option<usize> {
    const Y_BIT: u8 = 0x10;
    const G_BIT: u8 = 0x08;

    let &header = payload.get(cursor)?;
    cursor += 1;

    let n_s = usize::from(header >> 5) + 1;

    if header & Y_BIT != 0 {
        // WIDTH(2) + HEIGHT(2) per spatial layer.
        cursor = cursor.checked_add(n_s * 4)?;
        if cursor > payload.len() {
            return None;
        }
    }

    if header & G_BIT != 0 {
        let &n_g = payload.get(cursor)?;
        cursor += 1;
        for _ in 0..n_g {
            let &group = payload.get(cursor)?;
            cursor += 1;
            let r = usize::from((group >> 2) & 0x03);
            cursor = cursor.checked_add(r)?;
            if cursor > payload.len() {
                return None;
            }
        }
    }

    Some(cursor)
}

#[cfg(test)]
mod tests {
    use super::vp9_payload_is_keyframe;

    // ── Flags-only descriptors ───────────────────────────────────────────────

    #[test]
    fn begin_of_frame_not_predicted_is_keyframe() {
        // P=0, B=1, one payload byte
        let payload = [0x08u8, 0x86];
        assert!(vp9_payload_is_keyframe(&payload));
    }

    #[test]
    fn inter_predicted_frame_is_not_keyframe() {
        // P=1, B=1
        let payload = [0x48u8, 0x86];
        assert!(!vp9_payload_is_keyframe(&payload));
    }

    #[test]
    fn non_frame_start_is_not_keyframe() {
        // P=0 but B=0 — continuation packet
        let payload = [0x00u8, 0x86];
        assert!(!vp9_payload_is_keyframe(&payload));
    }

    #[test]
    fn end_and_z_bits_do_not_affect_detection() {
        // P=0, B=1, E=1, Z=1
        let payload = [0x0Du8, 0x86];
        assert!(vp9_payload_is_keyframe(&payload));
    }

    // ── PictureID ────────────────────────────────────────────────────────────

    #[test]
    fn with_7bit_picture_id_is_keyframe() {
        // I|B, pid=0x12 (M=0), payload
        let payload = [0x88u8, 0x12, 0x86];
        assert!(vp9_payload_is_keyframe(&payload));
    }

    #[test]
    fn with_15bit_picture_id_is_keyframe() {
        // I|B, pid=0x92 0x34 (M=1 → 2 bytes), payload
        let payload = [0x88u8, 0x92, 0x34, 0x86];
        assert!(vp9_payload_is_keyframe(&payload));
    }

    #[test]
    fn with_15bit_picture_id_inter_frame_is_not_keyframe() {
        // I|P|B
        let payload = [0xC8u8, 0x92, 0x34, 0x86];
        assert!(!vp9_payload_is_keyframe(&payload));
    }

    // ── Layer indices ────────────────────────────────────────────────────────

    #[test]
    fn non_flexible_with_layer_indices_is_keyframe() {
        // I|L|B (F=0): pid(1) + layer(1) + TL0PICIDX(1) + payload
        let payload = [0xA8u8, 0x12, 0x00, 0x2A, 0x86];
        assert!(vp9_payload_is_keyframe(&payload));
    }

    #[test]
    fn flexible_with_layer_indices_is_keyframe() {
        // I|L|F|B (F=1 → no TL0PICIDX): pid(1) + layer(1) + payload
        let payload = [0xB8u8, 0x12, 0x00, 0x86];
        assert!(vp9_payload_is_keyframe(&payload));
    }

    #[test]
    fn non_flexible_missing_tl0picidx_returns_false() {
        // I|L|B promises pid(1)+layer(1)+TL0PICIDX(1) before payload; the
        // last byte present is the TL0PICIDX, leaving no payload.
        let payload = [0xA8u8, 0x12, 0x00, 0x2A];
        assert!(!vp9_payload_is_keyframe(&payload));
    }

    // ── Scalability structure ────────────────────────────────────────────────

    #[test]
    fn keyframe_with_full_scalability_structure() {
        // B|V; SS: N_S=1 (2 layers), Y=1, G=1
        let payload = [
            0x0Au8, // flags: B|V
            0x38,   // SS header: N_S=1, Y=1, G=1
            0x02, 0x80, 0x01, 0x68, // layer 0: 640x360
            0x05, 0x00, 0x02, 0xD0, // layer 1: 1280x720
            0x02, // N_G = 2
            0x04, // group 0: TID=0 U=0 R=1
            0x01, // P_DIFF
            0x00, // group 1: TID=0 U=0 R=0
            0x86, // VP9 payload
        ];
        assert!(vp9_payload_is_keyframe(&payload));
    }

    #[test]
    fn keyframe_with_resolution_only_scalability_structure() {
        // B|V; SS: N_S=0 (1 layer), Y=1, G=0
        let payload = [0x0Au8, 0x10, 0x02, 0x80, 0x01, 0x68, 0x86];
        assert!(vp9_payload_is_keyframe(&payload));
    }

    #[test]
    fn truncated_scalability_resolutions_return_false() {
        // SS promises N_S=1 → 8 resolution bytes, only 3 present.
        let payload = [0x0Au8, 0x38, 0x02, 0x80, 0x01];
        assert!(!vp9_payload_is_keyframe(&payload));
    }

    #[test]
    fn truncated_picture_group_returns_false() {
        // SS: Y=0, G=1, N_G=1, group byte says R=2 but only 1 P_DIFF present.
        let payload = [0x0Au8, 0x08, 0x01, 0x08, 0x01];
        assert!(!vp9_payload_is_keyframe(&payload));
    }

    // ── Truncated / malformed ────────────────────────────────────────────────

    #[test]
    fn empty_payload_returns_false() {
        assert!(!vp9_payload_is_keyframe(&[]));
    }

    #[test]
    fn flags_only_descriptor_returns_false() {
        // P=0, B=1 but no payload after the descriptor.
        let payload = [0x08u8];
        assert!(!vp9_payload_is_keyframe(&payload));
    }

    #[test]
    fn i_set_but_picture_id_missing_returns_false() {
        let payload = [0x88u8];
        assert!(!vp9_payload_is_keyframe(&payload));
    }

    #[test]
    fn truncated_15bit_picture_id_returns_false() {
        // M=1 promises a second pid byte; nothing follows.
        let payload = [0x88u8, 0x92];
        assert!(!vp9_payload_is_keyframe(&payload));
    }

    #[test]
    fn descriptor_only_with_scalability_structure_returns_false() {
        // Well-formed SS but zero payload bytes after it.
        let payload = [0x0Au8, 0x10, 0x02, 0x80, 0x01, 0x68];
        assert!(!vp9_payload_is_keyframe(&payload));
    }
}
