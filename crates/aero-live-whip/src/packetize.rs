//! H.264 RTP packetizer for WHEP egress (RFC 6184).
//!
//! The inverse of [`crate::depacketize::H264Depacketizer`]: takes H.264 NAL units
//! and produces RTP packets suitable for sending to a WHEP subscriber.
//!
//! ## Supported packetization modes (RFC 6184 §5.6–§5.8)
//!
//! - **Single NAL unit** (types 1–23): the entire NAL fits within `mtu - RTP_HDR`
//!   bytes, emitted as a single RTP packet.
//! - **FU-A** (type 28): NAL units that exceed the MTU are fragmented across
//!   multiple RTP packets, each carrying the correct Start/End bits in the FU
//!   header.
//! - **STAP-A** (type 24): consecutive small NAL units whose combined size falls
//!   within the MTU are aggregated into one RTP packet to reduce overhead. STAP-A
//!   aggregation is opt-in via [`WhepPacketizer::new`].
//!
//! ## RTP header fields
//!
//! Every emitted packet has:
//! - A 12-byte fixed header (version=2, no padding, no CSRC).
//! - The configurable SSRC and payload type.
//! - A monotonically incrementing 16-bit sequence number (wraps on overflow per
//!   the spec).
//! - A 90 kHz timestamp (caller-supplied; same across all packets in one AU, per
//!   RFC 6184 §5.1).
//! - The **marker bit** set on the *last* packet of each access unit (RFC 6184
//!   §5.1).

use bytes::{BufMut, Bytes, BytesMut};

/// RTP fixed-header size in bytes (no CSRC, no extension).
const RTP_HDR_SIZE: usize = 12;

/// NAL unit type for STAP-A aggregation packets (RFC 6184 §5.7.1).
const NAL_TYPE_STAP_A: u8 = 24;
/// NAL unit type for FU-A fragmentation packets (RFC 6184 §5.8).
const NAL_TYPE_FU_A: u8 = 28;

/// FU header bit: first fragment in a fragmented NAL unit.
const FU_START: u8 = 0x80;
/// FU header bit: last fragment in a fragmented NAL unit.
const FU_END: u8 = 0x40;

/// A single packetized RTP packet produced by [`WhepPacketizer`].
///
/// The `payload` is the complete on-wire RTP packet (header + payload). The
/// boolean `marker` mirrors the RTP marker bit for callers that need it without
/// parsing the header.
#[derive(Debug, Clone)]
pub struct RtpPacket {
    /// Complete RTP wire bytes (12-byte header + RFC 6184 payload).
    pub bytes: Bytes,
    /// True on the last RTP packet of an access unit (marker bit).
    pub marker: bool,
}

/// WHEP egress H.264 → RTP packetizer (RFC 6184).
///
/// Construct with [`WhepPacketizer::new`], then call [`WhepPacketizer::packetize`]
/// for each access unit.
///
/// ```
/// # use aero_live_whip::packetize::WhepPacketizer;
/// let mut p = WhepPacketizer::new(
///     0xDEAD_BEEF,   // ssrc
///     96,            // payload type
///     1400,          // mtu in bytes
///     true,          // enable STAP-A aggregation for small NALs
/// );
/// let sps = [0x67u8, 0x42, 0x00, 0x1F];
/// let pps = [0x68u8, 0xCE];
/// let pkts = p.packetize(&[&sps, &pps], 0);
/// assert_eq!(pkts[pkts.len() - 1].marker, true);
/// ```
#[derive(Debug)]
pub struct WhepPacketizer {
    /// Synchronization source identifier (placed in every RTP header).
    ssrc: u32,
    /// RTP payload type (7 bits; caller must supply a value ≤ 127).
    payload_type: u8,
    /// Maximum transmission unit for the *entire* RTP packet (header + payload).
    /// NALs are fragmented when they exceed `mtu - RTP_HDR_SIZE` bytes.
    mtu: usize,
    /// Enable STAP-A aggregation for small NAL units.
    stap_a: bool,
    /// Next RTP sequence number (wraps modulo 2^16, per spec).
    seq: u16,
}

impl WhepPacketizer {
    /// Create a new packetizer.
    ///
    /// - `ssrc` — RTP SSRC placed in every header.
    /// - `payload_type` — RTP payload type (must be ≤ 127).
    /// - `mtu` — max on-wire packet size in bytes (must be > `RTP_HDR_SIZE`).
    /// - `stap_a` — when `true`, consecutive small NAL units are aggregated
    ///   into STAP-A packets.
    ///
    /// # Panics
    ///
    /// Panics in debug mode if `payload_type > 127` or `mtu <= RTP_HDR_SIZE`.
    #[must_use]
    pub fn new(ssrc: u32, payload_type: u8, mtu: usize, stap_a: bool) -> Self {
        debug_assert!(payload_type <= 127, "payload_type must fit in 7 bits");
        debug_assert!(mtu > RTP_HDR_SIZE, "mtu must exceed the RTP header size");
        Self {
            ssrc,
            payload_type,
            mtu,
            stap_a,
            seq: 0,
        }
    }

    /// Packetize `nals` (raw bytes starting at the NAL header byte, **no**
    /// Annex-B start code or AVCC length prefix) into RTP packets.
    ///
    /// All packets share `rtp_ts` (the 90 kHz timestamp). The marker bit is set
    /// on the *last* packet. Returns an empty `Vec` if `nals` is empty.
    ///
    /// NAL input can also be in **Annex-B** form (with `00 00 00 01` or `00 00 01`
    /// prefixes); use [`WhepPacketizer::packetize_annex_b`] in that case.
    pub fn packetize(&mut self, nals: &[&[u8]], rtp_ts: u32) -> Vec<RtpPacket> {
        if nals.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<RtpPacket> = Vec::new();

        if self.stap_a {
            self.packetize_with_stap_a(nals, rtp_ts, &mut out);
        } else {
            for nal in nals {
                self.packetize_single_nal(nal, rtp_ts, false, &mut out);
            }
        }

        // Set the marker bit on the last packet of the access unit.
        if let Some(last) = out.last_mut() {
            if !last.marker {
                set_marker_bit(last);
            }
        }

        out
    }

    /// Packetize an Annex-B access unit (one or more NAL units each preceded by
    /// a 4-byte or 3-byte start code `00 00 00 01` / `00 00 01`).
    ///
    /// This is a convenience wrapper over [`packetize`](Self::packetize) that
    /// strips start codes before handing NALs off to the core packetizer.
    pub fn packetize_annex_b(&mut self, annex_b: &[u8], rtp_ts: u32) -> Vec<RtpPacket> {
        let nals: Vec<&[u8]> = split_annex_b_refs(annex_b);
        self.packetize(&nals, rtp_ts)
    }

    // ---- internals ----

    /// Max RTP payload size (bytes available after the 12-byte fixed header).
    fn max_payload(&self) -> usize {
        self.mtu.saturating_sub(RTP_HDR_SIZE)
    }

    /// Emit packets for `nals` using STAP-A aggregation where possible.
    ///
    /// Small NALs are gathered until the aggregate would exceed the MTU, then
    /// flushed as a STAP-A. A NAL that already exceeds the MTU is fragmented
    /// with FU-A regardless.
    fn packetize_with_stap_a(&mut self, nals: &[&[u8]], rtp_ts: u32, out: &mut Vec<RtpPacket>) {
        let max_payload = self.max_payload();
        // STAP-A header (1 byte) + per-NAL [2-byte size + NAL].
        // Minimum STAP-A overhead per NAL: 2 bytes (size field).
        let stap_hdr = 1usize;

        // Buffer small NALs until the aggregate would spill.
        let mut pending: Vec<&[u8]> = Vec::new();
        // Running byte count for the current pending group: 1 (STAP-A hdr) + sum(2 + len).
        let mut pending_size: usize = stap_hdr;

        let flush_pending = |pending: &mut Vec<&[u8]>,
                             pending_size: &mut usize,
                             packetizer: &mut Self,
                             rtp_ts: u32,
                             out: &mut Vec<RtpPacket>| {
            match pending.len() {
                0 => {}
                1 => {
                    // Single pending NAL → emit as single-NAL packet.
                    packetizer.packetize_single_nal(pending[0], rtp_ts, false, out);
                }
                _ => {
                    // Build a STAP-A packet.
                    let mut buf = BytesMut::with_capacity(*pending_size);
                    // STAP-A header: F=0 NRI=max(NRI of contained NALs), type=24.
                    let nri = pending.iter().map(|n| n[0] & 0x60).max().unwrap_or(0);
                    buf.put_u8(nri | NAL_TYPE_STAP_A);
                    for n in pending.iter() {
                        let len = u16::try_from(n.len()).unwrap_or(u16::MAX);
                        buf.put_u16(len);
                        buf.extend_from_slice(n);
                    }
                    out.push(packetizer.build_packet(&buf.freeze(), rtp_ts, false));
                }
            }
            pending.clear();
            *pending_size = stap_hdr;
        };

        for &nal in nals {
            let nal_contribution = 2 + nal.len(); // 2-byte size field + NAL bytes
            if nal.len() + RTP_HDR_SIZE > self.mtu {
                // This NAL is too large even on its own → fragment with FU-A.
                // First flush any buffered small NALs.
                flush_pending(&mut pending, &mut pending_size, self, rtp_ts, out);
                self.packetize_fu_a(nal, rtp_ts, out);
            } else if pending_size + nal_contribution > max_payload {
                // Adding this NAL would overflow the current STAP-A → flush first.
                flush_pending(&mut pending, &mut pending_size, self, rtp_ts, out);
                pending.push(nal);
                pending_size += nal_contribution;
            } else {
                pending.push(nal);
                pending_size += nal_contribution;
            }
        }
        // Flush any remaining buffered NALs.
        flush_pending(&mut pending, &mut pending_size, self, rtp_ts, out);
    }

    /// Emit RTP packet(s) for a single NAL: one single-NAL packet if it fits the
    /// MTU, or a sequence of FU-A packets if it does not.
    fn packetize_single_nal(
        &mut self,
        nal: &[u8],
        rtp_ts: u32,
        marker: bool,
        out: &mut Vec<RtpPacket>,
    ) {
        if nal.len() + RTP_HDR_SIZE <= self.mtu {
            // Fits entirely in one RTP packet.
            let payload = Bytes::copy_from_slice(nal);
            out.push(self.build_packet(&payload, rtp_ts, marker));
        } else {
            // Too large for a single packet → FU-A fragmentation.
            self.packetize_fu_a(nal, rtp_ts, out);
        }
    }

    /// Fragment `nal` into FU-A RTP packets (RFC 6184 §5.8).
    ///
    /// The FU indicator carries the original F/NRI bits; the FU header carries
    /// the Start/End/Reserved bits and the original NAL type.
    fn packetize_fu_a(&mut self, nal: &[u8], rtp_ts: u32, out: &mut Vec<RtpPacket>) {
        if nal.is_empty() {
            return;
        }
        let nal_header = nal[0];
        let body = &nal[1..];
        // FU indicator: copy F and NRI from the original header, set type to 28.
        let fu_indicator = (nal_header & 0xE0) | NAL_TYPE_FU_A;
        // FU header base: low 5 bits = original NAL type.
        let fu_type = nal_header & 0x1F;

        // Maximum data bytes per FU-A packet (= MTU - RTP header - 2 FU bytes).
        let max_frag = self.mtu.saturating_sub(RTP_HDR_SIZE + 2);
        if max_frag == 0 {
            // MTU too small to carry even a single byte of NAL data; skip.
            return;
        }

        let mut offset = 0usize;
        while offset < body.len() || (offset == 0 && body.is_empty()) {
            let end = (offset + max_frag).min(body.len());
            let is_first = offset == 0;
            let is_last = end >= body.len();

            let mut fu_header = fu_type;
            if is_first {
                fu_header |= FU_START;
            }
            if is_last {
                fu_header |= FU_END;
            }

            let frag_len = end - offset;
            let mut buf = BytesMut::with_capacity(2 + frag_len);
            buf.put_u8(fu_indicator);
            buf.put_u8(fu_header);
            buf.extend_from_slice(&body[offset..end]);

            out.push(self.build_packet(&buf.freeze(), rtp_ts, false));

            if is_last {
                break;
            }
            offset = end;
        }
    }

    /// Build one RTP packet with the 12-byte fixed header, incrementing the
    /// internal sequence counter. The marker bit in the header is set according
    /// to `marker`.
    fn build_packet(&mut self, payload: &Bytes, timestamp: u32, marker: bool) -> RtpPacket {
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);

        let mut hdr = BytesMut::with_capacity(RTP_HDR_SIZE + payload.len());
        // Octet 0: V=2, P=0, X=0, CC=0
        hdr.put_u8(0x80);
        // Octet 1: M bit + PT
        let m_pt = if marker {
            0x80 | (self.payload_type & 0x7F)
        } else {
            self.payload_type & 0x7F
        };
        hdr.put_u8(m_pt);
        hdr.put_u16(seq);
        hdr.put_u32(timestamp);
        hdr.put_u32(self.ssrc);
        hdr.extend_from_slice(payload);

        RtpPacket {
            bytes: hdr.freeze(),
            marker,
        }
    }
}

/// Set the marker bit (bit 7 of octet 1) in the serialized RTP bytes of `pkt`
/// and update `pkt.marker`.
fn set_marker_bit(pkt: &mut RtpPacket) {
    let bytes = pkt.bytes.clone();
    // The marker bit is bit 7 of octet 1 (after the 1-byte version/flags octet).
    let mut buf = BytesMut::with_capacity(bytes.len());
    buf.extend_from_slice(&bytes);
    buf[1] |= 0x80;
    pkt.bytes = buf.freeze();
    pkt.marker = true;
}

/// Split an Annex-B byte stream into NAL unit slices (each starting at the NAL
/// header byte, no start code included). Handles both 3-byte (`00 00 01`) and
/// 4-byte (`00 00 00 01`) start codes.
fn split_annex_b_refs(data: &[u8]) -> Vec<&[u8]> {
    let mut nals: Vec<&[u8]> = Vec::new();
    let Some((pos, len)) = find_start_code(data, 0) else {
        return nals;
    };
    let mut start = pos + len;
    let mut i = start;
    while let Some((next_pos, next_len)) = find_start_code(data, i) {
        if next_pos > start {
            nals.push(&data[start..next_pos]);
        }
        start = next_pos + next_len;
        i = start;
    }
    if data.len() > start {
        nals.push(&data[start..]);
    }
    nals
}

/// Returns `(position, length)` of the next Annex-B start code at or after
/// `from`. Prefers the 4-byte form over the 3-byte form.
fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                return Some((i, 4));
            }
            if data[i + 2] == 1 {
                return Some((i, 3));
            }
        }
        i += 1;
    }
    None
}

// ---- parse helpers (public for round-trip tests) ----

/// Strip the 12-byte RTP header from `pkt` and return `(seq, ts, marker, payload)`.
///
/// Returns `None` if the packet is too short to contain a full header.
#[must_use]
pub fn rtp_parse(pkt: &[u8]) -> Option<(u16, u32, bool, &[u8])> {
    if pkt.len() < RTP_HDR_SIZE {
        return None;
    }
    let marker = pkt[1] & 0x80 != 0;
    let seq = u16::from_be_bytes([pkt[2], pkt[3]]);
    let ts = u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]);
    Some((seq, ts, marker, &pkt[RTP_HDR_SIZE..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::depacketize::{DepacketizeError, H264Depacketizer, ANNEX_B_START_CODE};
    use bytes::BytesMut;

    // ---- helpers ----

    /// Make a NAL unit from a header byte and body slice.
    fn make_nal(header: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![header];
        v.extend_from_slice(body);
        v
    }

    /// Build Annex-B from (header, body) pairs.
    fn annex_b(nals: &[(u8, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (h, b) in nals {
            out.extend_from_slice(&ANNEX_B_START_CODE);
            out.push(*h);
            out.extend_from_slice(b);
        }
        out
    }

    /// Feed a slice of `RtpPacket`s through `H264Depacketizer` and collect the
    /// Annex-B output.
    fn depacketize_packets(pkts: &[RtpPacket]) -> Result<BytesMut, DepacketizeError> {
        let mut d = H264Depacketizer::new();
        let mut out = BytesMut::new();
        for pkt in pkts {
            let (_, _, marker, payload) = rtp_parse(&pkt.bytes).expect("valid RTP header");
            d.push(payload, marker, &mut out)?;
        }
        Ok(out)
    }

    // ---- round-trip tests ----

    #[test]
    fn single_nal_round_trip() {
        // A NAL small enough to fit in one packet.
        let nal = make_nal(0x65, &[0x11, 0x22, 0x33, 0x44]);
        let mut p = WhepPacketizer::new(1, 96, 1400, false);
        let pkts = p.packetize(&[&nal], 9000);
        assert_eq!(pkts.len(), 1, "fits in one packet");
        assert!(pkts[0].marker, "marker bit on last packet");

        let out = depacketize_packets(&pkts).unwrap();
        let expected = annex_b(&[(0x65, &[0x11, 0x22, 0x33, 0x44])]);
        assert_eq!(&out[..], &expected[..]);
    }

    #[test]
    fn fu_a_round_trip_large_nal() {
        // Build a NAL larger than the MTU payload capacity so FU-A kicks in.
        // With mtu=20 the payload capacity = 20 - 12 = 8 bytes; FU-A eats 2 of
        // those, leaving 6 per fragment. A 30-byte body needs at least 5 packets.
        let body: Vec<u8> = (0u8..30).collect();
        let nal = make_nal(0x65, &body);
        let mut p = WhepPacketizer::new(0xABCD, 96, 20, false);
        let pkts = p.packetize(&[&nal], 1_800_000);

        // Must produce multiple packets.
        assert!(
            pkts.len() > 1,
            "FU-A must fragment; got {} packets",
            pkts.len()
        );

        // Marker only on the last.
        let n = pkts.len();
        for (i, pkt) in pkts.iter().enumerate() {
            assert_eq!(pkt.marker, i == n - 1, "marker on pkt {i}/{n}");
        }

        // Verify round-trip: depacketize must recover the original NAL.
        let out = depacketize_packets(&pkts).unwrap();
        let expected = annex_b(&[(0x65, &body)]);
        assert_eq!(&out[..], &expected[..]);
    }

    #[test]
    fn stap_a_round_trip_aggregation() {
        // SPS + PPS small enough to aggregate into one STAP-A packet.
        let sps = make_nal(0x67, &[0x42, 0x00, 0x1F]);
        let pps = make_nal(0x68, &[0xCE]);
        let mut p = WhepPacketizer::new(2, 97, 1400, true);
        let pkts = p.packetize(&[&sps, &pps], 0);

        assert_eq!(pkts.len(), 1, "SPS+PPS aggregated into one STAP-A");
        assert!(pkts[0].marker, "marker on sole packet");

        // Depacketizer must recover both NALs.
        let out = depacketize_packets(&pkts).unwrap();
        let expected = annex_b(&[(0x67, &[0x42, 0x00, 0x1F]), (0x68, &[0xCE])]);
        assert_eq!(&out[..], &expected[..]);
    }

    #[test]
    fn mixed_nals_stap_a_plus_fu_a_round_trip() {
        // SPS+PPS (small) aggregated, then a large IDR fragmented.
        let sps = make_nal(0x67, &[0x42, 0x00, 0x1F]);
        let pps = make_nal(0x68, &[0xCE]);
        let idr_body: Vec<u8> = (0u8..200).collect();
        let idr = make_nal(0x65, &idr_body);
        let mut p = WhepPacketizer::new(0, 96, 100, true);
        let nals: &[&[u8]] = &[&sps, &pps, &idr];
        let pkts = p.packetize(nals, 3_000);

        assert!(pkts.len() >= 2, "at least STAP-A and one FU-A fragment");
        assert!(pkts.last().unwrap().marker);

        let out = depacketize_packets(&pkts).unwrap();
        let expected = annex_b(&[
            (0x67, &[0x42, 0x00, 0x1F]),
            (0x68, &[0xCE]),
            (0x65, &idr_body),
        ]);
        assert_eq!(&out[..], &expected[..]);
    }

    // ---- FU-A correctness ----

    #[test]
    fn fu_a_start_end_bits_correct() {
        // mtu = 20 → payload cap = 8, FU-A data cap = 6.
        // body = 13 bytes → fragment sizes [6, 6, 1] → 3 packets.
        let body: Vec<u8> = (0u8..13).collect();
        let nal = make_nal(0x41, &body); // type 1, NRI=2
        let mut p = WhepPacketizer::new(0, 96, 20, false);
        let pkts = p.packetize(&[&nal], 0);
        assert_eq!(pkts.len(), 3, "expected exactly 3 FU-A packets");

        let check = |pkt: &RtpPacket, want_start: bool, want_end: bool| {
            let (_, _, _, payload) = rtp_parse(&pkt.bytes).unwrap();
            assert!(payload.len() >= 2, "FU-A needs at least 2 header bytes");
            let fu_header = payload[1];
            assert_eq!(fu_header & 0x80 != 0, want_start, "Start bit");
            assert_eq!(fu_header & 0x40 != 0, want_end, "End bit");
            // Reserved bit (bit 5) must be 0.
            assert_eq!(fu_header & 0x20, 0, "Reserved bit must be 0");
        };

        check(&pkts[0], true, false); // Start=1, End=0
        check(&pkts[1], false, false); // Start=0, End=0
        check(&pkts[2], false, true); // Start=0, End=1
    }

    #[test]
    fn fu_a_indicator_carries_f_nri_bits() {
        // NAL header 0x65: F=0, NRI=3 (0x60), type=5.
        let nal = make_nal(0x65, &[0xAA; 100]);
        let mut p = WhepPacketizer::new(0, 96, 30, false);
        let pkts = p.packetize(&[&nal], 0);
        assert!(pkts.len() >= 2);
        for pkt in &pkts {
            let (_, _, _, payload) = rtp_parse(&pkt.bytes).unwrap();
            let fu_indicator = payload[0];
            // F=0, NRI bits (0x60) preserved, type=28.
            assert_eq!(fu_indicator & 0xE0, 0x60, "NRI bits preserved");
            assert_eq!(fu_indicator & 0x1F, 28, "FU-A type=28");
        }
    }

    #[test]
    fn fu_a_reassembles_to_original() {
        // Explicit byte-level check that reassembly equals the original NAL.
        let body: Vec<u8> = (0u8..50).collect();
        let nal = make_nal(0x65, &body);
        let mut p = WhepPacketizer::new(0, 96, 25, false);
        let pkts = p.packetize(&[&nal], 0);
        let out = depacketize_packets(&pkts).unwrap();
        let expected = annex_b(&[(0x65, &body)]);
        assert_eq!(
            &out[..],
            &expected[..],
            "reassembly must equal original NAL"
        );
    }

    // ---- marker bit ----

    #[test]
    fn marker_bit_on_last_packet_of_each_au() {
        let nal_a = make_nal(0x67, &[0x01, 0x02]);
        let nal_b = make_nal(0x68, &[0x03]);
        let mut p = WhepPacketizer::new(0, 96, 1400, false);

        // First access unit.
        let pkts1 = p.packetize(&[&nal_a, &nal_b], 0);
        for (i, pkt) in pkts1.iter().enumerate() {
            let is_last = i == pkts1.len() - 1;
            assert_eq!(pkt.marker, is_last, "AU1 packet {i}: marker={}", pkt.marker);
        }

        // Second access unit.
        let pkts2 = p.packetize(&[&nal_a], 3000);
        for (i, pkt) in pkts2.iter().enumerate() {
            let is_last = i == pkts2.len() - 1;
            assert_eq!(pkt.marker, is_last, "AU2 packet {i}: marker={}", pkt.marker);
        }
    }

    #[test]
    fn marker_bit_set_in_rtp_header_bytes() {
        let nal = make_nal(0x65, &[0xAA; 200]);
        let mut p = WhepPacketizer::new(0, 96, 50, false);
        let pkts = p.packetize(&[&nal], 0);
        assert!(pkts.len() > 1);
        let n = pkts.len();
        for (i, pkt) in pkts.iter().enumerate() {
            // Octet 1 of RTP header: bit 7 = marker.
            let marker_in_wire = pkt.bytes[1] & 0x80 != 0;
            assert_eq!(marker_in_wire, i == n - 1, "wire marker bit at packet {i}");
        }
    }

    // ---- sequence numbers ----

    #[test]
    fn sequence_numbers_increment() {
        let mut p = WhepPacketizer::new(0, 96, 1400, false);
        let nals = [
            make_nal(0x67, &[0x01]),
            make_nal(0x68, &[0x02]),
            make_nal(0x65, &[0x03]),
        ];
        let nals_refs: Vec<&[u8]> = nals.iter().map(Vec::as_slice).collect();
        let pkts = p.packetize(&nals_refs, 0);
        let mut last_seq: Option<u16> = None;
        for pkt in &pkts {
            let (seq, _, _, _) = rtp_parse(&pkt.bytes).unwrap();
            if let Some(prev) = last_seq {
                assert_eq!(seq, prev.wrapping_add(1), "seq must increment");
            }
            last_seq = Some(seq);
        }
    }

    #[test]
    fn sequence_number_wraparound() {
        let mut p = WhepPacketizer::new(0, 96, 1400, false);
        // Pre-advance the sequence to near-overflow.
        p.seq = u16::MAX;
        let nal = make_nal(0x65, &[0xAA]);
        let pkts = p.packetize(&[&nal], 0);
        let (seq, _, _, _) = rtp_parse(&pkts[0].bytes).unwrap();
        assert_eq!(seq, u16::MAX);
        // Next packet starts at 0 (wraps).
        let pkts2 = p.packetize(&[&nal], 0);
        let (seq2, _, _, _) = rtp_parse(&pkts2[0].bytes).unwrap();
        assert_eq!(seq2, 0);
    }

    // ---- SSRC and payload type ----

    #[test]
    fn ssrc_in_rtp_header() {
        let ssrc = 0xDEAD_BEEFu32;
        let mut p = WhepPacketizer::new(ssrc, 96, 1400, false);
        let pkts = p.packetize(&[&make_nal(0x65, &[0xAA])], 0);
        let wire_ssrc = u32::from_be_bytes([
            pkts[0].bytes[8],
            pkts[0].bytes[9],
            pkts[0].bytes[10],
            pkts[0].bytes[11],
        ]);
        assert_eq!(wire_ssrc, ssrc);
    }

    #[test]
    fn payload_type_in_rtp_header() {
        let pt = 97u8;
        let mut p = WhepPacketizer::new(0, pt, 1400, false);
        let pkts = p.packetize(&[&make_nal(0x65, &[0xAA])], 0);
        // Octet 1: M bit (may be set) + PT.
        assert_eq!(pkts[0].bytes[1] & 0x7F, pt);
    }

    // ---- Annex-B convenience wrapper ----

    #[test]
    fn packetize_annex_b_produces_same_output_as_packetize() {
        let sps = make_nal(0x67, &[0x42, 0x00, 0x1F]);
        let pps = make_nal(0x68, &[0xCE]);
        let ab = annex_b(&[(0x67, &[0x42, 0x00, 0x1F]), (0x68, &[0xCE])]);

        let mut p1 = WhepPacketizer::new(0, 96, 1400, false);
        let pkts1 = p1.packetize(&[&sps, &pps], 0);

        let mut p2 = WhepPacketizer::new(0, 96, 1400, false);
        let pkts2 = p2.packetize_annex_b(&ab, 0);

        assert_eq!(pkts1.len(), pkts2.len());
        for (a, b) in pkts1.iter().zip(pkts2.iter()) {
            // Payloads must be identical; seq numbers also match since both start at 0.
            assert_eq!(a.bytes, b.bytes);
        }
    }

    #[test]
    fn empty_nals_returns_empty() {
        let mut p = WhepPacketizer::new(0, 96, 1400, false);
        assert!(p.packetize(&[], 0).is_empty());
        assert!(p.packetize_annex_b(&[], 0).is_empty());
    }
}
