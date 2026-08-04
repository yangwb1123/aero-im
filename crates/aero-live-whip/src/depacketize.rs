//! H.264 RTP depacketizer (RFC 6184).
//!
//! Reassembles Annex-B NAL units from the RTP payloads that a WebRTC publisher
//! sends. str0m surfaces *frame* data via [`crate::session`], but the
//! packetization mode chosen by browsers is RTP-level, so this module owns the
//! RFC 6184 reassembly rules independently of str0m's media plumbing. Keeping it
//! a free-standing, deeply unit-tested module is deliberate: it is the most
//! valuable verifiable piece of the WHIP media plane (no live browser needed).
//!
//! Supported packetization modes (RFC 6184 §5.6–§5.8):
//! - **Single NAL unit** packets (NAL types 1–23): the whole RTP payload is one
//!   NAL unit.
//! - **STAP-A** (type 24): an aggregation packet carrying several whole NAL
//!   units, each prefixed by a 16-bit size.
//! - **FU-A** (type 28): a single NAL unit fragmented across multiple RTP
//!   packets, marked with Start/End bits.
//!
//! Not handled (browsers do not emit these for H.264): STAP-B (25), MTAP16/24
//! (26/27), and FU-B (29). They are surfaced as [`DepacketizeError::Unsupported`]
//! rather than silently dropped.
//!
//! ## Bounded reassembly
//!
//! FU-A reassembly accumulates fragment bytes across many RTP packets. Since the
//! depacketizer is fed directly from untrusted network input, every reassembled
//! NAL is bounded by [`DEFAULT_MAX_NAL_SIZE`] (configurable via
//! [`H264Depacketizer::with_max_nal_size`]); a peer that withholds the End
//! fragment cannot grow the buffer without limit — the reassembly is dropped with
//! [`DepacketizeError::Oversized`] once the cap is exceeded.
//!
//! Output is **Annex-B**: each emitted NAL unit is prefixed with the 4-byte
//! start code `00 00 00 01`, matching what [`aero_live_hls`]'s TS muxer expects
//! after its own AVCC→Annex-B conversion (see `aero-live-hls/src/ts.rs`).

use bytes::{BufMut, BytesMut};

/// 4-byte Annex-B start code prepended to every reassembled NAL unit.
pub const ANNEX_B_START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// Default upper bound on a single reassembled NAL unit, in bytes (2 MiB).
///
/// FU-A reassembly accumulates fragment bytes across many RTP packets before an
/// End fragment closes the NAL. Without a cap, a peer that sends a Start
/// fragment followed by an unbounded run of Middle fragments (and never an End)
/// would grow the reassembly buffer without limit — a memory-exhaustion denial
/// of service, since the depacketizer is fed directly from untrusted network
/// input. 2 MiB
/// comfortably exceeds any legitimate single H.264 NAL (even a 4K IDR slice is
/// well under 1 MiB) while bounding the damage a hostile or buggy peer can do.
pub const DEFAULT_MAX_NAL_SIZE: usize = 2 * 1024 * 1024;

/// NAL unit type carried in the low 5 bits of the first NAL byte.
const NAL_TYPE_STAP_A: u8 = 24;
const NAL_TYPE_STAP_B: u8 = 25;
const NAL_TYPE_MTAP16: u8 = 26;
const NAL_TYPE_MTAP24: u8 = 27;
const NAL_TYPE_FU_A: u8 = 28;
const NAL_TYPE_FU_B: u8 = 29;

/// Errors raised while depacketizing an RTP H.264 payload.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DepacketizeError {
    /// Payload was empty or too short to contain the required headers.
    #[error("rtp payload too short: {0}")]
    Truncated(&'static str),
    /// A fragmentation/aggregation packet type we do not implement.
    #[error("unsupported NAL packetization type: {0}")]
    Unsupported(u8),
    /// An FU-A continuation/end arrived without a preceding start fragment.
    #[error("FU-A fragment without start")]
    DanglingFragment,
    /// The FU-A start changed NAL type mid-fragment, or a new start arrived
    /// before the previous fragment ended.
    #[error("FU-A reassembly desync")]
    FragmentDesync,
    /// A reassembled NAL unit exceeded the configured maximum size. Carries the
    /// byte count that triggered the limit. The in-progress fragment is dropped
    /// (the depacketizer resyncs) rather than buffering unboundedly.
    #[error("reassembled NAL unit exceeds {limit} bytes (reached {reached})")]
    Oversized { reached: usize, limit: usize },
}

/// Reassembles Annex-B NAL units from a sequence of RTP H.264 payloads.
///
/// Feed each packet's *payload* (RTP header already stripped — str0m hands us
/// the payload directly) to [`push`](Self::push) along with the RTP marker bit.
/// Completed NAL units are appended to the provided sink as Annex-B byte runs.
///
/// The depacketizer is stateful only across FU-A fragments; single-NAL and
/// STAP-A packets are self-contained.
#[derive(Debug)]
pub struct H264Depacketizer {
    /// In-progress FU-A reassembly buffer (NAL header byte already written).
    fu_buf: Option<FuReassembly>,
    /// Upper bound on any single reassembled NAL unit, in bytes. Guards FU-A
    /// reassembly (and, defensively, single-NAL / STAP-A units) against
    /// unbounded growth from a hostile or buggy peer.
    max_nal_size: usize,
}

impl Default for H264Depacketizer {
    fn default() -> Self {
        Self {
            fu_buf: None,
            max_nal_size: DEFAULT_MAX_NAL_SIZE,
        }
    }
}

#[derive(Debug)]
struct FuReassembly {
    /// Reconstructed NAL header byte (F|NRI from FU indicator, type from FU header).
    nal_header: u8,
    /// Accumulated NAL payload bytes (excluding the header byte).
    data: BytesMut,
}

impl H264Depacketizer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a depacketizer with an explicit maximum reassembled-NAL size.
    ///
    /// Any reassembled NAL unit (FU-A, STAP-A aggregated unit, or single-NAL
    /// packet) that would exceed `max_nal_size` bytes is rejected with
    /// [`DepacketizeError::Oversized`] and the in-progress fragment is dropped,
    /// so a peer cannot exhaust memory by withholding the End fragment. A value
    /// of `0` is treated as [`DEFAULT_MAX_NAL_SIZE`] (the cap is never disabled).
    #[must_use]
    pub fn with_max_nal_size(max_nal_size: usize) -> Self {
        Self {
            fu_buf: None,
            max_nal_size: if max_nal_size == 0 {
                DEFAULT_MAX_NAL_SIZE
            } else {
                max_nal_size
            },
        }
    }

    /// The configured maximum reassembled-NAL size, in bytes.
    #[must_use]
    pub fn max_nal_size(&self) -> usize {
        self.max_nal_size
    }

    /// Process one RTP payload. Appends zero or more complete Annex-B NAL units
    /// to `out`.
    ///
    /// `_marker` is the RTP marker bit; for H.264 it signals the last packet of
    /// an access unit. We accept it for API completeness and future AU-boundary
    /// detection but reassembly itself is driven by NAL/FU headers.
    pub fn push(
        &mut self,
        payload: &[u8],
        _marker: bool,
        out: &mut BytesMut,
    ) -> Result<(), DepacketizeError> {
        let first = *payload
            .first()
            .ok_or(DepacketizeError::Truncated("empty payload"))?;
        // RFC 6184 §1.3: the NAL unit type is the low 5 bits of the header byte.
        let nal_type = first & 0x1F;

        match nal_type {
            1..=23 => {
                // Single NAL unit packet — the entire payload is one NAL unit.
                self.reject_dangling_fragment()?;
                self.check_nal_size(payload.len())?;
                append_nal(out, payload);
                Ok(())
            }
            NAL_TYPE_STAP_A => {
                self.reject_dangling_fragment()?;
                self.depacketize_stap_a(payload, out)
            }
            NAL_TYPE_FU_A => self.depacketize_fu_a(payload, out),
            NAL_TYPE_STAP_B | NAL_TYPE_MTAP16 | NAL_TYPE_MTAP24 | NAL_TYPE_FU_B => {
                Err(DepacketizeError::Unsupported(nal_type))
            }
            // 0 and 30/31 are reserved/invalid per RFC 6184.
            other => Err(DepacketizeError::Unsupported(other)),
        }
    }

    /// True while an FU-A reassembly is in progress (start seen, end not yet).
    #[must_use]
    pub fn is_reassembling(&self) -> bool {
        self.fu_buf.is_some()
    }

    /// Drop any partially-reassembled fragment. Call on stream discontinuity
    /// (e.g. a gap in RTP sequence numbers) so a stale prefix never gets
    /// glued onto the next fragment.
    pub fn reset(&mut self) {
        self.fu_buf = None;
    }

    // ---- internals ----

    fn reject_dangling_fragment(&mut self) -> Result<(), DepacketizeError> {
        if self.fu_buf.is_some() {
            // A non-FU packet arrived mid-fragment: the previous FU-A never
            // ended. Discard it and report desync rather than corrupt output.
            self.fu_buf = None;
            return Err(DepacketizeError::FragmentDesync);
        }
        Ok(())
    }

    /// Reject a NAL unit whose size exceeds the configured cap.
    #[inline]
    fn check_nal_size(&self, reached: usize) -> Result<(), DepacketizeError> {
        if reached > self.max_nal_size {
            return Err(DepacketizeError::Oversized {
                reached,
                limit: self.max_nal_size,
            });
        }
        Ok(())
    }

    fn depacketize_stap_a(
        &self,
        payload: &[u8],
        out: &mut BytesMut,
    ) -> Result<(), DepacketizeError> {
        // RFC 6184 §5.7.1: STAP-A = [STAP-A NAL hdr][ (16-bit size)(NAL unit) ]+
        let mut p = 1usize; // skip the STAP-A header byte
        while p < payload.len() {
            if p + 2 > payload.len() {
                return Err(DepacketizeError::Truncated("stap-a size field"));
            }
            let size = u16::from_be_bytes([payload[p], payload[p + 1]]) as usize;
            p += 2;
            if size == 0 {
                return Err(DepacketizeError::Truncated("stap-a zero-size unit"));
            }
            if p + size > payload.len() {
                return Err(DepacketizeError::Truncated("stap-a aggregated unit"));
            }
            self.check_nal_size(size)?;
            append_nal(out, &payload[p..p + size]);
            p += size;
        }
        Ok(())
    }

    fn depacketize_fu_a(
        &mut self,
        payload: &[u8],
        out: &mut BytesMut,
    ) -> Result<(), DepacketizeError> {
        // RFC 6184 §5.8: FU-A = [FU indicator][FU header][FU payload]
        if payload.len() < 2 {
            return Err(DepacketizeError::Truncated("fu-a header"));
        }
        let fu_indicator = payload[0];
        let fu_header = payload[1];
        let start = fu_header & 0x80 != 0;
        let end = fu_header & 0x40 != 0;
        let fragment_type = fu_header & 0x1F;
        // Reconstruct the original NAL header: F|NRI from the indicator's top
        // 3 bits, type from the FU header's low 5 bits.
        let nal_header = (fu_indicator & 0xE0) | fragment_type;
        let frag = &payload[2..];

        if start {
            if self.fu_buf.is_some() {
                // New start before the previous fragment ended: desync.
                self.fu_buf = None;
                return Err(DepacketizeError::FragmentDesync);
            }
            // The reassembled NAL is the header byte plus this fragment so far.
            self.check_nal_size(frag.len() + 1)?;
            let mut data = BytesMut::with_capacity(frag.len() + 1);
            data.extend_from_slice(frag);
            self.fu_buf = Some(FuReassembly { nal_header, data });
        } else {
            let buf = self
                .fu_buf
                .as_mut()
                .ok_or(DepacketizeError::DanglingFragment)?;
            if buf.nal_header != nal_header {
                self.fu_buf = None;
                return Err(DepacketizeError::FragmentDesync);
            }
            // Bound the accumulation *before* extending: a peer that keeps
            // sending Middle fragments without an End must not grow this buffer
            // without limit. The +1 accounts for the reconstructed header byte.
            let reached = buf.data.len() + frag.len() + 1;
            if reached > self.max_nal_size {
                let limit = self.max_nal_size;
                self.fu_buf = None;
                return Err(DepacketizeError::Oversized { reached, limit });
            }
            buf.data.extend_from_slice(frag);
        }

        if end {
            let buf = self
                .fu_buf
                .take()
                .ok_or(DepacketizeError::DanglingFragment)?;
            out.put_slice(&ANNEX_B_START_CODE);
            out.put_u8(buf.nal_header);
            out.extend_from_slice(&buf.data);
        }
        Ok(())
    }
}

/// Append a whole NAL unit (header byte + body) to `out` in Annex-B form.
fn append_nal(out: &mut BytesMut, nal: &[u8]) {
    out.put_slice(&ANNEX_B_START_CODE);
    out.extend_from_slice(nal);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extract the NAL units (type + length) from an Annex-B byte run for
    /// assertions. Returns `(nal_type, body_len_including_header)` per unit.
    fn parse_annex_b(buf: &[u8]) -> Vec<(u8, usize)> {
        let mut units = Vec::new();
        let mut i = 0;
        while i + 4 <= buf.len() {
            assert_eq!(
                &buf[i..i + 4],
                &ANNEX_B_START_CODE,
                "missing start code at {i}"
            );
            i += 4;
            let nal_start = i;
            // Scan to the next start code (or end).
            while i + 4 <= buf.len() && buf[i..i + 4] != ANNEX_B_START_CODE {
                i += 1;
            }
            let end = if i + 4 <= buf.len() { i } else { buf.len() };
            let nal_type = buf[nal_start] & 0x1F;
            units.push((nal_type, end - nal_start));
        }
        units
    }

    #[test]
    fn single_nal_unit_is_prefixed_with_start_code() {
        let mut d = H264Depacketizer::new();
        let mut out = BytesMut::new();
        // NAL header 0x65 = F=0, NRI=3, type=5 (IDR slice), then 3 body bytes.
        let payload = [0x65, 0xAA, 0xBB, 0xCC];
        d.push(&payload, true, &mut out).unwrap();
        assert_eq!(&out[..4], &ANNEX_B_START_CODE);
        assert_eq!(&out[4..], &payload);
        let units = parse_annex_b(&out);
        assert_eq!(units, vec![(5u8, 4usize)]);
        assert!(!d.is_reassembling());
    }

    #[test]
    fn single_nal_sps_pps_types() {
        let mut d = H264Depacketizer::new();
        let mut out = BytesMut::new();
        d.push(&[0x67, 0x42, 0x00], false, &mut out).unwrap(); // SPS (type 7)
        d.push(&[0x68, 0xCE], false, &mut out).unwrap(); // PPS (type 8)
        let units = parse_annex_b(&out);
        assert_eq!(units, vec![(7, 3), (8, 2)]);
    }

    #[test]
    fn fu_a_two_fragments_reassemble_into_one_nal() {
        let mut d = H264Depacketizer::new();
        let mut out = BytesMut::new();

        // Original NAL: header 0x65 (IDR, NRI=3), body = [0x01,0x02,0x03,0x04,0x05].
        // FU indicator: keep F|NRI from 0x65 (0x60), type=28 → 0x60|28 = 0x7C.
        // Fragment 1: start=1,end=0,type=5 → FU header 0x85, body [0x01,0x02,0x03].
        let pkt1 = [0x7C, 0x85, 0x01, 0x02, 0x03];
        // Fragment 2: start=0,end=1,type=5 → FU header 0x45, body [0x04,0x05].
        let pkt2 = [0x7C, 0x45, 0x04, 0x05];

        d.push(&pkt1, false, &mut out).unwrap();
        assert!(d.is_reassembling(), "should be mid-reassembly after start");
        assert!(out.is_empty(), "nothing emitted until the end fragment");

        d.push(&pkt2, true, &mut out).unwrap();
        assert!(!d.is_reassembling());

        // Reassembled NAL = reconstructed header 0x65 + concatenated body.
        let expected: Vec<u8> = [
            &ANNEX_B_START_CODE[..],
            &[0x65, 0x01, 0x02, 0x03, 0x04, 0x05],
        ]
        .concat();
        assert_eq!(&out[..], &expected[..]);
        let units = parse_annex_b(&out);
        assert_eq!(units, vec![(5u8, 6usize)]); // type 5, header + 5 body bytes
    }

    #[test]
    fn fu_a_three_fragments_with_middle() {
        let mut d = H264Depacketizer::new();
        let mut out = BytesMut::new();
        // start
        d.push(&[0x7C, 0x81, 0xAA], false, &mut out).unwrap();
        // middle (start=0,end=0)
        d.push(&[0x7C, 0x01, 0xBB], false, &mut out).unwrap();
        assert!(out.is_empty());
        // end
        d.push(&[0x7C, 0x41, 0xCC], true, &mut out).unwrap();
        let expected: Vec<u8> = [&ANNEX_B_START_CODE[..], &[0x61, 0xAA, 0xBB, 0xCC]].concat();
        assert_eq!(&out[..], &expected[..]);
    }

    #[test]
    fn stap_a_with_two_nals_emits_both() {
        let mut d = H264Depacketizer::new();
        let mut out = BytesMut::new();
        // STAP-A header 0x78 (F=0,NRI=3,type=24), then:
        //   size=4, NAL = [0x67,0x42,0x00,0x1E] (SPS, type 7)
        //   size=2, NAL = [0x68,0xCE]           (PPS, type 8)
        let mut payload = vec![0x78];
        payload.extend_from_slice(&4u16.to_be_bytes());
        payload.extend_from_slice(&[0x67, 0x42, 0x00, 0x1E]);
        payload.extend_from_slice(&2u16.to_be_bytes());
        payload.extend_from_slice(&[0x68, 0xCE]);

        d.push(&payload, false, &mut out).unwrap();
        let units = parse_annex_b(&out);
        assert_eq!(units, vec![(7u8, 4usize), (8u8, 2usize)]);
        // Verify both start codes are present.
        assert_eq!(&out[0..4], &ANNEX_B_START_CODE);
    }

    #[test]
    fn stap_a_truncated_size_errors() {
        let mut d = H264Depacketizer::new();
        let mut out = BytesMut::new();
        // header + claims size 8 but only 2 bytes follow.
        let mut payload = vec![0x78];
        payload.extend_from_slice(&8u16.to_be_bytes());
        payload.extend_from_slice(&[0x67, 0x42]);
        let err = d.push(&payload, false, &mut out).unwrap_err();
        assert_eq!(err, DepacketizeError::Truncated("stap-a aggregated unit"));
    }

    #[test]
    fn empty_payload_errors() {
        let mut d = H264Depacketizer::new();
        let mut out = BytesMut::new();
        assert_eq!(
            d.push(&[], false, &mut out).unwrap_err(),
            DepacketizeError::Truncated("empty payload")
        );
    }

    #[test]
    fn fu_a_continuation_without_start_is_dangling() {
        let mut d = H264Depacketizer::new();
        let mut out = BytesMut::new();
        // end fragment with no prior start.
        let err = d.push(&[0x7C, 0x45, 0x01], true, &mut out).unwrap_err();
        assert_eq!(err, DepacketizeError::DanglingFragment);
        assert!(out.is_empty());
    }

    #[test]
    fn fu_a_double_start_is_desync() {
        let mut d = H264Depacketizer::new();
        let mut out = BytesMut::new();
        d.push(&[0x7C, 0x85, 0x01], false, &mut out).unwrap(); // start
        let err = d.push(&[0x7C, 0x85, 0x02], false, &mut out).unwrap_err(); // start again
        assert_eq!(err, DepacketizeError::FragmentDesync);
        assert!(!d.is_reassembling(), "buffer dropped on desync");
    }

    #[test]
    fn fu_a_type_change_mid_fragment_is_desync() {
        let mut d = H264Depacketizer::new();
        let mut out = BytesMut::new();
        d.push(&[0x7C, 0x85, 0x01], false, &mut out).unwrap(); // start type 5
                                                               // continuation but type 1 → reconstructed header differs → desync.
        let err = d.push(&[0x7C, 0x41, 0x02], true, &mut out).unwrap_err();
        assert_eq!(err, DepacketizeError::FragmentDesync);
    }

    #[test]
    fn single_nal_mid_fragment_aborts_fragment() {
        let mut d = H264Depacketizer::new();
        let mut out = BytesMut::new();
        d.push(&[0x7C, 0x85, 0x01], false, &mut out).unwrap(); // FU-A start
                                                               // A single-NAL packet now arrives before the FU-A ended.
        let err = d.push(&[0x65, 0xAA], true, &mut out).unwrap_err();
        assert_eq!(err, DepacketizeError::FragmentDesync);
        assert!(!d.is_reassembling());
    }

    #[test]
    fn unsupported_types_are_reported() {
        let mut d = H264Depacketizer::new();
        let mut out = BytesMut::new();
        // STAP-B (25), MTAP16 (26), MTAP24 (27), FU-B (29) and reserved 0.
        for t in [25u8, 26, 27, 29, 0] {
            let err = d.push(&[t, 0x00], false, &mut out).unwrap_err();
            assert_eq!(err, DepacketizeError::Unsupported(t));
        }
    }

    #[test]
    fn reset_clears_in_progress_fragment() {
        let mut d = H264Depacketizer::new();
        let mut out = BytesMut::new();
        d.push(&[0x7C, 0x85, 0x01], false, &mut out).unwrap();
        assert!(d.is_reassembling());
        d.reset();
        assert!(!d.is_reassembling());
        // After reset a fresh start works cleanly.
        d.push(&[0x7C, 0x85, 0xAA], false, &mut out).unwrap();
        d.push(&[0x7C, 0x45, 0xBB], true, &mut out).unwrap();
        let units = parse_annex_b(&out);
        assert_eq!(units, vec![(5u8, 3usize)]);
    }

    // ---- FU-A reassembly size cap (memory-exhaustion guard) ----

    #[test]
    fn default_max_nal_size_is_two_mib() {
        let d = H264Depacketizer::new();
        assert_eq!(d.max_nal_size(), DEFAULT_MAX_NAL_SIZE);
        assert_eq!(DEFAULT_MAX_NAL_SIZE, 2 * 1024 * 1024);
    }

    #[test]
    fn with_max_nal_size_zero_falls_back_to_default() {
        let d = H264Depacketizer::with_max_nal_size(0);
        assert_eq!(
            d.max_nal_size(),
            DEFAULT_MAX_NAL_SIZE,
            "a 0 cap must clamp to the default, never disable the guard"
        );
    }

    #[test]
    fn fu_a_unbounded_middle_fragments_are_capped() {
        // A peer sends a Start fragment then an endless run of Middle fragments
        // and never an End. Without the cap, fu_buf.data would grow without
        // limit. With it, the reassembly is rejected once it exceeds max_nal_size
        // and the buffer is dropped (resync), bounding memory use.
        let mut d = H264Depacketizer::with_max_nal_size(16);
        let mut out = BytesMut::new();

        // Start fragment: FU indicator 0x7C, FU header 0x85 (S=1,E=0,type=5),
        // body = 5 bytes. Reassembled NAL so far = 1 (header) + 5 = 6 ≤ 16.
        d.push(&[0x7C, 0x85, 1, 2, 3, 4, 5], false, &mut out)
            .unwrap();
        assert!(d.is_reassembling());

        // Middle fragment: FU header 0x05 (S=0,E=0,type=5), body = 8 bytes.
        // Reassembled = 6 + 8 = 14 ≤ 16, still accepted.
        d.push(&[0x7C, 0x05, 6, 7, 8, 9, 10, 11, 12, 13], false, &mut out)
            .unwrap();
        assert!(d.is_reassembling());

        // Another Middle fragment of 8 bytes: would reach 14 + 8 = 22 > 16.
        let err = d
            .push(
                &[0x7C, 0x05, 20, 21, 22, 23, 24, 25, 26, 27],
                false,
                &mut out,
            )
            .unwrap_err();
        assert_eq!(
            err,
            DepacketizeError::Oversized {
                reached: 22,
                limit: 16
            }
        );
        // The over-limit reassembly buffer is dropped (resynced), not retained.
        assert!(!d.is_reassembling(), "buffer dropped on oversize");
        // Nothing partial was emitted to the output sink.
        assert!(out.is_empty(), "no partial NAL emitted on overflow");
    }

    #[test]
    fn fu_a_oversize_start_fragment_is_rejected() {
        // Even the very first (Start) fragment is bounded: a single huge Start
        // fragment must not be buffered if it already exceeds the cap.
        let mut d = H264Depacketizer::with_max_nal_size(4);
        let mut out = BytesMut::new();
        // header + 5-byte body = 6 > 4.
        let err = d
            .push(&[0x7C, 0x85, 1, 2, 3, 4, 5], false, &mut out)
            .unwrap_err();
        assert_eq!(
            err,
            DepacketizeError::Oversized {
                reached: 6,
                limit: 4
            }
        );
        assert!(
            !d.is_reassembling(),
            "oversize start fragment is not buffered"
        );
    }

    #[test]
    fn fu_a_after_oversize_drop_a_fresh_nal_reassembles_cleanly() {
        // After an oversize abort the depacketizer must be usable again: a new
        // Start/End pair within the cap reassembles normally.
        let mut d = H264Depacketizer::with_max_nal_size(8);
        let mut out = BytesMut::new();
        // Overflow and drop.
        let _ = d
            .push(&[0x7C, 0x85, 1, 2, 3, 4, 5, 6, 7, 8, 9], false, &mut out)
            .unwrap_err();
        assert!(!d.is_reassembling());
        out.clear();

        // A small, in-cap NAL reassembles fine.
        d.push(&[0x7C, 0x85, 0xAA], false, &mut out).unwrap();
        d.push(&[0x7C, 0x45, 0xBB], true, &mut out).unwrap();
        let units = parse_annex_b(&out);
        assert_eq!(units, vec![(5u8, 3usize)], "fresh NAL after overflow");
    }

    #[test]
    fn single_nal_over_cap_is_rejected() {
        // A single-NAL packet larger than the cap is rejected too.
        let mut d = H264Depacketizer::with_max_nal_size(4);
        let mut out = BytesMut::new();
        let err = d
            .push(&[0x65, 0xAA, 0xBB, 0xCC, 0xDD], true, &mut out)
            .unwrap_err();
        assert_eq!(
            err,
            DepacketizeError::Oversized {
                reached: 5,
                limit: 4
            }
        );
        assert!(out.is_empty());
    }

    #[test]
    fn stap_a_aggregated_unit_over_cap_is_rejected() {
        // A STAP-A whose aggregated unit exceeds the cap is rejected.
        let mut d = H264Depacketizer::with_max_nal_size(4);
        let mut out = BytesMut::new();
        // STAP-A header 0x78, then size=5 + 5-byte NAL.
        let mut payload = vec![0x78];
        payload.extend_from_slice(&5u16.to_be_bytes());
        payload.extend_from_slice(&[0x67, 0x42, 0x00, 0x1E, 0xAB]);
        let err = d.push(&payload, false, &mut out).unwrap_err();
        assert_eq!(
            err,
            DepacketizeError::Oversized {
                reached: 5,
                limit: 4
            }
        );
    }

    #[test]
    fn nal_exactly_at_cap_is_accepted() {
        // Boundary: a NAL whose size equals the cap is fine; only strictly
        // larger is rejected.
        let mut d = H264Depacketizer::with_max_nal_size(4);
        let mut out = BytesMut::new();
        // header + 3 body = 4 == cap.
        d.push(&[0x65, 0xAA, 0xBB, 0xCC], true, &mut out).unwrap();
        let units = parse_annex_b(&out);
        assert_eq!(units, vec![(5u8, 4usize)]);
    }
}
