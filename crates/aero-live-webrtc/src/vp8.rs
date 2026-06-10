//! VP8 keyframe detection from raw RTP payload bytes (RFC 7741).
//!
//! # Scope
//!
//! Parses **only** as much of the VP8 RTP payload descriptor as is needed to
//! locate the VP8 payload header and read its inverse-key-frame flag. It is
//! *not* a general depacketizer — partition reassembly, picture-id tracking
//! and temporal-layer handling are out of scope.
//!
//! # Wire layout (RFC 7741 §4.2 / §4.3)
//!
//! ```text
//!        0 1 2 3 4 5 6 7
//!       +-+-+-+-+-+-+-+-+
//!       |X|R|N|S|R| PID | (REQUIRED)  payload descriptor, first byte
//!       +-+-+-+-+-+-+-+-+
//!   X:  |I|L|T|K|  RSV  | (OPTIONAL)  extended control bits
//!       +-+-+-+-+-+-+-+-+
//!   I:  |M| PictureID   | (OPTIONAL)  7-bit, or 15-bit over 2 bytes when M=1
//!       +-+-+-+-+-+-+-+-+
//!   L:  |   TL0PICIDX   | (OPTIONAL)
//!       +-+-+-+-+-+-+-+-+
//!  T/K: |TID|Y| KEYIDX  | (OPTIONAL)
//!       +-+-+-+-+-+-+-+-+
//!       |Size0|H| VER |P|             VP8 payload header, first byte
//!       +-+-+-+-+-+-+-+-+
//! ```
//!
//! A packet starts a keyframe iff the descriptor has `S=1` (start of
//! partition) and `PID=0` (first partition) — guaranteeing the payload header
//! is actually present at the start of the VP8 payload — **and** the payload
//! header's `P` bit (inverse key frame flag, RFC 6386 frame tag bit 0) is 0.

/// Descriptor byte 0 bit masks.
const X_BIT: u8 = 0x80;
const S_BIT: u8 = 0x10;
const PID_MASK: u8 = 0x07;

/// Extended control bits (present when `X` is set).
const I_BIT: u8 = 0x80;
const L_BIT: u8 = 0x40;
const T_BIT: u8 = 0x20;
const K_BIT: u8 = 0x10;

/// `PictureID` extension flag: 15-bit picture id over two bytes.
const M_BIT: u8 = 0x80;

/// VP8 payload header inverse key frame flag (0 = key frame).
const P_BIT: u8 = 0x01;

/// Returns `true` when the RTP payload `bytes` carries the start of a VP8
/// keyframe.
///
/// Returns `false` for:
/// - interframes (payload header `P=1`),
/// - non-start packets (`S=0`) and partitions other than the first (`PID≠0`),
/// - descriptor-only packets (no VP8 payload header byte after the descriptor),
/// - empty, truncated, or otherwise malformed payloads (never panics).
///
/// # Codec assumption
///
/// Must only be called for VP8 payloads; dispatch via
/// [`crate::codec::payload_is_keyframe`].
#[must_use]
pub fn vp8_payload_is_keyframe(payload: &[u8]) -> bool {
    let Some(&first) = payload.first() else {
        return false;
    };

    // Only the first packet of the first partition carries the payload header.
    if first & S_BIT == 0 || first & PID_MASK != 0 {
        return false;
    }

    let mut cursor = 1usize;

    if first & X_BIT != 0 {
        let Some(&ext) = payload.get(cursor) else {
            return false;
        };
        cursor += 1;

        if ext & I_BIT != 0 {
            let Some(&pid0) = payload.get(cursor) else {
                return false;
            };
            // M=1 → 15-bit PictureID spanning two bytes.
            cursor += if pid0 & M_BIT != 0 { 2 } else { 1 };
        }
        if ext & L_BIT != 0 {
            cursor += 1; // TL0PICIDX
        }
        if ext & (T_BIT | K_BIT) != 0 {
            cursor += 1; // TID/Y/KEYIDX — one byte shared by T and K
        }
    }

    // First byte of the VP8 payload header; absent (descriptor-only or
    // truncated extension fields) → not a detectable keyframe.
    let Some(&hdr) = payload.get(cursor) else {
        return false;
    };
    hdr & P_BIT == 0
}

#[cfg(test)]
mod tests {
    use super::vp8_payload_is_keyframe;

    // ── Minimal descriptor (no X) ────────────────────────────────────────────

    #[test]
    fn start_of_first_partition_keyframe() {
        // S=1, PID=0; payload header P=0 (key frame)
        let payload = [0x10u8, 0x00, 0x9D, 0x01, 0x2A];
        assert!(vp8_payload_is_keyframe(&payload));
    }

    #[test]
    fn start_of_first_partition_interframe() {
        // S=1, PID=0; payload header P=1 (interframe)
        let payload = [0x10u8, 0x01, 0x9D, 0x01];
        assert!(!vp8_payload_is_keyframe(&payload));
    }

    #[test]
    fn non_start_packet_is_not_keyframe() {
        // S=0 — continuation packet; the byte after the descriptor is frame
        // data, not the payload header, and must not be interpreted.
        let payload = [0x00u8, 0x00, 0xAB];
        assert!(!vp8_payload_is_keyframe(&payload));
    }

    #[test]
    fn non_zero_partition_index_is_not_keyframe() {
        // S=1 but PID=2 — start of a later partition, no payload header here.
        let payload = [0x12u8, 0x00, 0xAB];
        assert!(!vp8_payload_is_keyframe(&payload));
    }

    // ── Extended control bits ────────────────────────────────────────────────

    #[test]
    fn extension_with_7bit_picture_id_keyframe() {
        // X|S, ext=I, PictureID=0x05 (M=0 → 1 byte), payload header P=0
        let payload = [0x90u8, 0x80, 0x05, 0x00, 0x9D];
        assert!(vp8_payload_is_keyframe(&payload));
    }

    #[test]
    fn extension_with_15bit_picture_id_keyframe() {
        // X|S, ext=I, PictureID=0x85 0x67 (M=1 → 2 bytes), payload header P=0
        let payload = [0x90u8, 0x80, 0x85, 0x67, 0x00, 0x9D];
        assert!(vp8_payload_is_keyframe(&payload));
    }

    #[test]
    fn extension_with_15bit_picture_id_interframe() {
        let payload = [0x90u8, 0x80, 0x85, 0x67, 0x01, 0x9D];
        assert!(!vp8_payload_is_keyframe(&payload));
    }

    #[test]
    fn extension_with_all_fields_keyframe() {
        // X|S, ext=I|L|T|K, 15-bit pid (2) + TL0PICIDX (1) + TID/KEYIDX (1),
        // then payload header P=0.
        let payload = [0x90u8, 0xF0, 0x85, 0x67, 0x2A, 0x42, 0x00, 0x9D];
        assert!(vp8_payload_is_keyframe(&payload));
    }

    #[test]
    fn extension_with_l_and_t_only_keyframe() {
        // X|S, ext=L|T, TL0PICIDX (1) + TID/Y/KEYIDX (1), payload header P=0
        let payload = [0x90u8, 0x60, 0x2A, 0x40, 0x00];
        assert!(vp8_payload_is_keyframe(&payload));
    }

    // ── Truncated / malformed ────────────────────────────────────────────────

    #[test]
    fn empty_payload_returns_false() {
        assert!(!vp8_payload_is_keyframe(&[]));
    }

    #[test]
    fn descriptor_byte_only_returns_false() {
        // S=1, PID=0 but nothing after the mandatory descriptor byte.
        let payload = [0x10u8];
        assert!(!vp8_payload_is_keyframe(&payload));
    }

    #[test]
    fn x_set_but_extension_byte_missing_returns_false() {
        let payload = [0x90u8];
        assert!(!vp8_payload_is_keyframe(&payload));
    }

    #[test]
    fn i_set_but_picture_id_missing_returns_false() {
        let payload = [0x90u8, 0x80];
        assert!(!vp8_payload_is_keyframe(&payload));
    }

    #[test]
    fn descriptor_only_with_picture_id_returns_false() {
        // Full descriptor (X|S, I, 7-bit pid) but no payload header byte.
        let payload = [0x90u8, 0x80, 0x05];
        assert!(!vp8_payload_is_keyframe(&payload));
    }

    #[test]
    fn truncated_15bit_picture_id_returns_false() {
        // M=1 promises a second pid byte; only one byte follows and it would
        // otherwise read as a keyframe header — must still be false.
        let payload = [0x90u8, 0x80, 0x85];
        assert!(!vp8_payload_is_keyframe(&payload));
    }

    #[test]
    fn truncated_extension_fields_return_false() {
        // ext=I|L|T|K needs pid(1)+tl0(1)+tid(1) then header; cut mid-way.
        let payload = [0x90u8, 0xF0, 0x05, 0x2A];
        assert!(!vp8_payload_is_keyframe(&payload));
    }
}
