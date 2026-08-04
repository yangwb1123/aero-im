//! SRT control-packet encoder for reliability actions.
//!
//! Turns the [`Action`] variants emitted by [`crate::reliability::ReliabilityState`]
//! into fully-formed SRT control packets (header + CIF) that the transport layer
//! can send verbatim over UDP.
//!
//! ## Packet formats (per SRT Internet-Draft §3.2)
//!
//! All control packets share the 16-byte [`SrtHeader`] with bit 31 of word 0
//! set.  The "type-specific" header word and the CIF content vary by type:
//!
//! ### ACK (control type 0x0002)
//! - Header word 1 (`type_specific`): the monotonic ACK ID (`ack_id`).
//! - CIF (28 bytes as encoded here): ack-sequence-number, RTT (µs), RTT-variance
//!   (µs), available buffer size, packets receiving rate (zero — not tracked),
//!   estimated link capacity (zero — not tracked), and receiving rate
//!   (zero — not tracked).
//!   Fields that are not tracked by this implementation are set to zero and
//!   documented as such inline.
//!
//! ### NAK / loss report (control type 0x0003)
//! - Header word 1 (`type_specific`): 0 (unused by spec).
//! - CIF: variable-length **loss list**, one or two 32-bit words per entry:
//!   - Single loss: `seq_no` with bit 31 clear.
//!   - Range start: `from | 0x8000_0000` (bit 31 set = "range start marker").
//!   - Range end: `to` with bit 31 clear.
//!
//!   A single-packet gap (from == to) is encoded as a single word (no bit 31).
//!
//! ### ACKACK (control type 0x0006)
//! - Header word 1 (`type_specific`): the ACK ID being acknowledged.
//! - CIF: empty (no body).

// SRT-specific acronyms (ACK, NAK, ACKACK, RTT, CIF, …) are standard in the
// domain; suppressing doc_markdown keeps them readable without backticks.
#![allow(clippy::doc_markdown)]

use bytes::{BufMut as _, BytesMut};

use crate::protocol::{ControlType, PacketKind, SrtHeader, SRT_HEADER_LEN};
use crate::reliability::Action;

/// Encode one reliability [`Action`] into a well-formed SRT control packet.
///
/// `dst_socket_id` is the destination SRT socket id (the peer's id, placed in
/// the SRT header).  `ts` is the packet timestamp in microseconds since
/// connection start (typically `now - connect_time`; use 0 when not tracked).
/// `rtt_us` and `rttvar_us` are the smoothed RTT and its variance in
/// microseconds, used to populate the ACK CIF. `available_buffer_packets` is
/// the receiver's currently available packet capacity negotiated during the
/// handshake; it is a flow-control value, not an optional statistic.
///
/// Returns `None` for [`Action::SendData`], which is a data-plane action and
/// does not produce a control packet via this function.
///
/// # ACK fields not yet tracked
///
/// The ACK CIF contains several fields this implementation does not track:
/// - **Packets receiving rate** (field 5): set to 0.
/// - **Estimated link capacity** (field 6): set to 0.
/// - **Receiving rate** (field 7): set to 0.
///
/// The available receiver buffer (field 4) must be non-zero: libsrt installs
/// this value as its current send flow window. Advertising zero stalls the
/// publisher after its initial congestion window and eventually trips the
/// peer-idle timeout. The ingest path consumes packets as they arrive, so it
/// advertises the receive window negotiated by this implementation.
#[must_use]
pub fn encode_control(
    action: &Action,
    dst_socket_id: u32,
    ts: u32,
    rtt_us: u32,
    rttvar_us: u32,
    available_buffer_packets: u32,
) -> Option<Vec<u8>> {
    match action {
        Action::SendAck { ack_seq_no, ack_id } => Some(encode_ack(
            *ack_seq_no,
            *ack_id,
            dst_socket_id,
            ts,
            rtt_us,
            rttvar_us,
            available_buffer_packets,
        )),
        Action::SendNak { from, to } => Some(encode_nak(*from, *to, dst_socket_id, ts)),
        Action::SendAckAck { ack_id } => Some(encode_ackack(*ack_id, dst_socket_id, ts)),
        Action::SendData { .. } => None,
    }
}

/// Encode an ACK control packet.
///
/// Wire layout (28-byte CIF after the 16-byte header):
/// ```text
/// [ack_seq_no: u32]  — next expected sequence number (highest contiguous + 1)
/// [rtt_us: u32]      — smoothed RTT in microseconds
/// [rttvar_us: u32]   — RTT variance in microseconds
/// [buf_avail: u32]   — available receiver buffer (packets)
/// [pkt_rate: u32]    — packets per second receiving rate;  0 = not tracked
/// [link_cap: u32]    — estimated link capacity (packets/s); 0 = not tracked
/// [recv_rate: u32]   — estimated receiving rate (bytes/s);  0 = not tracked
/// ```
fn encode_ack(
    ack_seq_no: u32,
    ack_id: u32,
    dst_socket_id: u32,
    ts: u32,
    rtt_us: u32,
    rttvar_us: u32,
    available_buffer_packets: u32,
) -> Vec<u8> {
    // The ACK ID lives in the "type_specific" header word (word 1 of the
    // 16-byte SRT header), addressed to the peer.
    let header = SrtHeader {
        kind: PacketKind::Control {
            control_type: ControlType::Ack,
            subtype: 0,
            type_specific: ack_id,
        },
        timestamp: ts,
        dest_socket_id: dst_socket_id,
    };

    let mut out = BytesMut::with_capacity(SRT_HEADER_LEN + 28);
    header.write_to(&mut out);

    // CIF — 7 × u32 big-endian:
    out.put_u32(ack_seq_no);
    out.put_u32(rtt_us);
    out.put_u32(rttvar_us);
    // This is flow control, not an optional statistic. libsrt assigns it to
    // `m_iFlowWindowSize`; zero therefore deadlocks a live publisher.
    out.put_u32(available_buffer_packets);
    out.put_u32(0); // packets receiving rate — not tracked
    out.put_u32(0); // estimated link capacity — not tracked
    out.put_u32(0); // receiving rate — not tracked

    out.to_vec()
}

/// Encode a NAK (loss-report) control packet.
///
/// The CIF is a variable-length **loss list** encoded per the SRT spec:
/// - If `from == to` (single missing packet): one 32-bit word, bit 31 clear.
/// - Otherwise (range):
///   - Word 0: `from | 0x8000_0000` (bit 31 set = "range start").
///   - Word 1: `to`  with bit 31 clear.
fn encode_nak(from: u32, to: u32, dst_socket_id: u32, ts: u32) -> Vec<u8> {
    let header = SrtHeader {
        kind: PacketKind::Control {
            control_type: ControlType::Nak,
            subtype: 0,
            type_specific: 0,
        },
        timestamp: ts,
        dest_socket_id: dst_socket_id,
    };

    // Strip bit 31 (mask to 31-bit seq space) before encoding.
    let from31 = from & 0x7FFF_FFFF;
    let to31 = to & 0x7FFF_FFFF;

    // Determine CIF capacity: 1 word for a single loss, 2 for a range.
    let words = if from31 == to31 { 1 } else { 2 };
    let mut out = BytesMut::with_capacity(SRT_HEADER_LEN + words * 4);
    header.write_to(&mut out);

    if from31 == to31 {
        // Single lost packet: one word, bit 31 clear.
        out.put_u32(from31);
    } else {
        // Range: first word has bit 31 set, second is the range end (bit 31 clear).
        out.put_u32(from31 | 0x8000_0000);
        out.put_u32(to31);
    }

    out.to_vec()
}

/// Encode an ACKACK control packet.
///
/// The CIF is empty; the `ack_id` being acknowledged lives in the
/// `type_specific` word of the SRT header (word 1).
fn encode_ackack(ack_id: u32, dst_socket_id: u32, ts: u32) -> Vec<u8> {
    let header = SrtHeader {
        kind: PacketKind::Control {
            control_type: ControlType::AckAck,
            subtype: 0,
            type_specific: ack_id,
        },
        timestamp: ts,
        dest_socket_id: dst_socket_id,
    };
    header.to_bytes().to_vec()
}

/// Decode the loss list from a NAK control-packet body (the bytes after the
/// 16-byte header).
///
/// Returns a list of `(from, to)` inclusive ranges.  Malformed entries (e.g. a
/// range-start word not followed by a range-end word) are silently skipped.
///
/// Useful in tests to verify that an encoded NAK packet contains the expected
/// loss ranges.
#[must_use]
pub fn decode_nak_loss_list(body: &[u8]) -> Vec<(u32, u32)> {
    let mut ranges = Vec::new();
    let mut i = 0;
    while i + 4 <= body.len() {
        let word = u32::from_be_bytes([body[i], body[i + 1], body[i + 2], body[i + 3]]);
        i += 4;
        if word & 0x8000_0000 != 0 {
            // Range start: bit 31 set.  The next word must be the range end.
            let from = word & 0x7FFF_FFFF;
            if i + 4 <= body.len() {
                let to = u32::from_be_bytes([body[i], body[i + 1], body[i + 2], body[i + 3]]);
                i += 4;
                ranges.push((from, to & 0x7FFF_FFFF));
            }
            // If truncated, skip this entry.
        } else {
            // Single loss: both from and to are this word.
            ranges.push((word, word));
        }
    }
    ranges
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ControlType, PacketKind, SrtHeader, SRT_HEADER_LEN};
    use crate::reliability::Action;

    const DST_SOCK: u32 = 0xDEAD_CAFE;
    const TS: u32 = 0x0001_2345;
    const BUFFER_AVAIL: u32 = 8192;

    // ─── ACK round-trip ─────────────────────────────────────────────────────

    #[test]
    fn ack_encodes_to_correct_control_type() {
        let action = Action::SendAck {
            ack_seq_no: 42,
            ack_id: 7,
        };
        let pkt = encode_control(&action, DST_SOCK, TS, 10_000, 2_500, BUFFER_AVAIL)
            .expect("ACK must produce a packet");

        let hdr = SrtHeader::parse(&pkt).expect("must be a valid SRT header");
        assert!(hdr.is_control());
        assert_eq!(
            hdr.kind,
            PacketKind::Control {
                control_type: ControlType::Ack,
                subtype: 0,
                type_specific: 7, // ack_id in type_specific
            }
        );
        assert_eq!(hdr.timestamp, TS);
        assert_eq!(hdr.dest_socket_id, DST_SOCK);
    }

    #[test]
    fn ack_cif_contains_ack_seq_rtt_and_rttvar() {
        let action = Action::SendAck {
            ack_seq_no: 99,
            ack_id: 1,
        };
        let rtt_us = 50_000u32;
        let rttvar_us = 12_500u32;
        let pkt = encode_control(&action, DST_SOCK, TS, rtt_us, rttvar_us, BUFFER_AVAIL).unwrap();

        // CIF starts right after the 16-byte header.
        let cif = &pkt[SRT_HEADER_LEN..];
        assert!(
            cif.len() >= 28,
            "ACK CIF must be at least 28 bytes (7 words)"
        );

        let ack_seq = u32::from_be_bytes([cif[0], cif[1], cif[2], cif[3]]);
        let rtt = u32::from_be_bytes([cif[4], cif[5], cif[6], cif[7]]);
        let rttvar = u32::from_be_bytes([cif[8], cif[9], cif[10], cif[11]]);

        assert_eq!(ack_seq, 99);
        assert_eq!(rtt, rtt_us);
        assert_eq!(rttvar, rttvar_us);
    }

    #[test]
    fn ack_advertises_nonzero_receiver_window_and_zeroes_optional_stats() {
        let pkt = encode_control(
            &Action::SendAck {
                ack_seq_no: 1,
                ack_id: 1,
            },
            0,
            0,
            0,
            0,
            BUFFER_AVAIL,
        )
        .unwrap();
        let cif = &pkt[SRT_HEADER_LEN..];

        let available_buffer = u32::from_be_bytes([cif[12], cif[13], cif[14], cif[15]]);
        assert_eq!(
            available_buffer, BUFFER_AVAIL,
            "a full ACK must keep the peer's send flow window open"
        );
        assert_ne!(
            available_buffer, 0,
            "zero ACKD_BUFFERLEFT stalls libsrt until peer-idle timeout"
        );

        // Words 4..6 (packet rate, link capacity, receiving byte rate) are
        // optional statistics that this implementation does not track.
        assert_eq!(
            &cif[16..28],
            &[0u8; 12],
            "untracked ACK statistics must be zero"
        );
    }

    // ─── NAK round-trip ─────────────────────────────────────────────────────

    #[test]
    fn nak_single_loss_encodes_as_one_word_without_range_bit() {
        let action = Action::SendNak { from: 5, to: 5 };
        let pkt = encode_control(&action, DST_SOCK, TS, 0, 0, BUFFER_AVAIL).unwrap();

        let hdr = SrtHeader::parse(&pkt).unwrap();
        assert_eq!(
            hdr.kind,
            PacketKind::Control {
                control_type: ControlType::Nak,
                subtype: 0,
                type_specific: 0,
            }
        );

        let body = &pkt[SRT_HEADER_LEN..];
        assert_eq!(body.len(), 4, "single loss = 1 word");
        let word = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
        assert_eq!(
            word & 0x8000_0000,
            0,
            "bit 31 must be clear for a single loss"
        );
        assert_eq!(word, 5);

        let ranges = decode_nak_loss_list(body);
        assert_eq!(ranges, vec![(5, 5)]);
    }

    #[test]
    fn nak_range_encodes_as_two_words_with_range_bit() {
        let action = Action::SendNak { from: 10, to: 20 };
        let pkt = encode_control(&action, DST_SOCK, TS, 0, 0, BUFFER_AVAIL).unwrap();

        let body = &pkt[SRT_HEADER_LEN..];
        assert_eq!(body.len(), 8, "range loss = 2 words");

        let w0 = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
        let w1 = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);

        assert_ne!(
            w0 & 0x8000_0000,
            0,
            "bit 31 must be set on the range-start word"
        );
        assert_eq!(w0 & 0x7FFF_FFFF, 10, "range start = from");
        assert_eq!(w1 & 0x7FFF_FFFF, 20, "range end = to");
        assert_eq!(w1 & 0x8000_0000, 0, "bit 31 clear on range-end word");

        let ranges = decode_nak_loss_list(body);
        assert_eq!(ranges, vec![(10, 20)]);
    }

    #[test]
    fn nak_with_gap_from_1_to_2() {
        // The session gap test exercises [1, 2]; verify encoding/decoding here.
        let action = Action::SendNak { from: 1, to: 2 };
        let pkt = encode_control(&action, DST_SOCK, TS, 0, 0, BUFFER_AVAIL).unwrap();
        let body = &pkt[SRT_HEADER_LEN..];
        let ranges = decode_nak_loss_list(body);
        assert_eq!(ranges, vec![(1, 2)]);
    }

    // ─── ACKACK round-trip ──────────────────────────────────────────────────

    #[test]
    fn ackack_encodes_to_correct_control_type_and_ack_id() {
        let action = Action::SendAckAck { ack_id: 99 };
        let pkt = encode_control(&action, DST_SOCK, TS, 0, 0, BUFFER_AVAIL).unwrap();

        assert_eq!(pkt.len(), SRT_HEADER_LEN, "ACKACK has no CIF body");

        let hdr = SrtHeader::parse(&pkt).unwrap();
        assert_eq!(
            hdr.kind,
            PacketKind::Control {
                control_type: ControlType::AckAck,
                subtype: 0,
                type_specific: 99, // ack_id
            }
        );
        assert_eq!(hdr.dest_socket_id, DST_SOCK);
        assert_eq!(hdr.timestamp, TS);
    }

    // ─── SendData returns None ───────────────────────────────────────────────

    #[test]
    fn send_data_action_returns_none() {
        let action = Action::SendData {
            seq_no: 1,
            payload: vec![0u8; 10],
            is_retransmit: false,
        };
        assert!(
            encode_control(&action, 0, 0, 0, 0, BUFFER_AVAIL).is_none(),
            "SendData is a data-plane action; encode_control must return None"
        );
    }

    // ─── decode_nak_loss_list edge cases ────────────────────────────────────

    #[test]
    fn decode_nak_loss_list_empty_body() {
        assert!(decode_nak_loss_list(&[]).is_empty());
    }

    #[test]
    fn decode_nak_loss_list_truncated_range_start_is_skipped() {
        // Range-start word with bit 31 set, but no following word.
        let body = (5u32 | 0x8000_0000).to_be_bytes();
        let ranges = decode_nak_loss_list(&body);
        assert!(
            ranges.is_empty(),
            "truncated range-start with no end word must be skipped"
        );
    }

    #[test]
    fn decode_nak_loss_list_multiple_ranges() {
        // Encode two ranges manually: [1,3] and [7,7].
        let mut body = Vec::new();
        body.extend_from_slice(&(1u32 | 0x8000_0000).to_be_bytes());
        body.extend_from_slice(&3u32.to_be_bytes());
        body.extend_from_slice(&7u32.to_be_bytes()); // single
        let ranges = decode_nak_loss_list(&body);
        assert_eq!(ranges, vec![(1, 3), (7, 7)]);
    }
}
