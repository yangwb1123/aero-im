//! Transport-abstracted send pump for SRT control and retransmit packets.
//!
//! ## Overview
//!
//! [`SrtSink`] is the sole I/O abstraction: anything that can absorb a byte
//! slice counts as a sink — a real UDP socket, a `Vec<Vec<u8>>` accumulator, or
//! a test spy.
//!
//! [`SrtSession::pump`] is the "last mile" that turns the action queue and
//! retransmit buffer into wire bytes and hands them to the sink.  It:
//!
//! 1. Calls [`SrtSession::drain_control_packets`] and sends each resulting
//!    control packet (ACK / NAK / ACKACK) through the sink.
//! 2. Calls [`ReliabilityState::drain_retransmits`] and re-encodes each
//!    [`Action::SendData`] as a proper SRT data packet before sending it.
//!
//! No I/O primitives, no async, no real socket — fully unit-testable.
//!
//! ## Limitations (intentional — see crate docs)
//!
//! - No real UDP socket is involved; `pump` talks only to the supplied sink.
//! - No congestion control or RTT-based pacing.
//! - Retransmit timing is caller-driven: call `pump` as often as you want
//!   retransmits to go out (the caller controls the clock via `now`).
//! - The `now` and `peer_socket_id` parameters mirror those of
//!   [`SrtSession::drain_control_packets`] so the caller can fake time in tests.

// SRT-specific acronyms (ACK, NAK, ACKACK, RTT, …) are domain-standard.
#![allow(clippy::doc_markdown)]

use std::time::Instant;

use bytes::BytesMut;

use crate::protocol::{PacketKind, SrtHeader, SRT_HEADER_LEN};
use crate::reliability::Action;
use crate::SrtSession;

// ─────────────────────────────────────────────────────────────────────────────
// SrtSink — the only I/O abstraction
// ─────────────────────────────────────────────────────────────────────────────

/// Anything that can absorb a fully-framed SRT packet (control or data).
///
/// Implementations can be a real UDP socket wrapper, a vector accumulator for
/// tests, or a no-op sink for benchmarks.  The method is `&mut self` so
/// implementations can record state (e.g. collect sent packets in a `Vec`).
///
/// The bytes slice contains a complete, ready-to-transmit SRT packet including
/// the 16-byte header.
pub trait SrtSink {
    /// Consume one outgoing SRT packet.
    ///
    /// Called once per packet that `pump` needs to send.  The implementation
    /// should treat `bytes` as an opaque, complete UDP payload.
    fn send(&mut self, bytes: &[u8]);
}

// ─────────────────────────────────────────────────────────────────────────────
// pump implementation
// ─────────────────────────────────────────────────────────────────────────────

impl SrtSession {
    /// Drive all pending outgoing work through `sink`.
    ///
    /// Call this periodically (e.g. from a timer tick or immediately after
    /// feeding inbound packets) to flush:
    ///
    /// 1. **Control packets** — every ACK, NAK, or ACKACK that accumulated in
    ///    `pending_actions` since the last `pump` (or `drain_control_packets`).
    /// 2. **Retransmitted data packets** — every sequence number that appeared
    ///    in the loss list (populated by received NAK control packets via
    ///    [`ReliabilityState::on_nak`]) and is still in the send buffer.
    ///
    /// `peer_socket_id` is written into the `dest_socket_id` field of every
    /// outgoing packet.  `now` is the current [`Instant`]; it is passed through
    /// to [`SrtSession::drain_control_packets`] (timestamp is currently zero —
    /// production callers can subtract `connect_time` to get microseconds).
    ///
    /// Returns the number of packets sent (useful in tests to confirm activity).
    pub fn pump(
        &mut self,
        sink: &mut impl SrtSink,
        _now: Instant,
        peer_socket_id: u32,
    ) -> usize {
        let mut sent = 0usize;

        // ── 1. Control packets (ACK / NAK / ACKACK) ───────────────────────────
        //
        // drain_control_packets takes `pending_actions` and serialises each one
        // via encode_control.  SendData actions are silently dropped there (they
        // are not control-plane), which is correct: retransmits are handled
        // below.
        for pkt in self.drain_control_packets(peer_socket_id) {
            sink.send(&pkt);
            sent += 1;
        }

        // ── 2. Retransmitted data packets ─────────────────────────────────────
        //
        // drain_retransmits pops every sequence number from the loss list that
        // is still in the send buffer and returns Action::SendData{is_retransmit:
        // true, …}.  We re-encode each one as a minimal SRT data packet.
        for action in self.reliability.drain_retransmits() {
            if let Action::SendData {
                seq_no,
                payload,
                is_retransmit,
            } = action
            {
                let pkt = encode_data_packet(seq_no, is_retransmit, peer_socket_id, &payload);
                sink.send(&pkt);
                sent += 1;
            }
        }

        sent
    }
}

/// Encode an SRT data packet: 16-byte header followed by the raw payload.
///
/// - `seq_no` — 31-bit sequence number (bit 31 is the control flag; it is
///   always clear for data packets).
/// - `is_retransmit` — sets the `R` bit (bit 2) in the message-word.
/// - `dst_socket_id` — placed in the header's destination socket id field.
/// - `payload` — raw bytes (MPEG-TS, decrypted, etc.).
///
/// Fields not tracked by this implementation (PP, O, KK, message number) are
/// left at zero.
fn encode_data_packet(seq_no: u32, is_retransmit: bool, dst_socket_id: u32, payload: &[u8]) -> Vec<u8> {
    // msg_word: bit 2 (0-indexed from the spec's numbering after the seq field)
    // is the `R` (retransmit) flag in the SRT data header word 1.
    // Per SRT spec §3.1: word 1 = PP(2) | O(1) | KK(2) | R(1) | MsgNo(26).
    // R is bit 26 of word 1 (bit 2 of the high byte).
    let r_bit: u32 = if is_retransmit { 1 << 2 } else { 0 };
    let msg_word: u32 = r_bit;

    let header = SrtHeader {
        kind: PacketKind::Data { seq_no, msg_word },
        timestamp: 0,
        dest_socket_id: dst_socket_id,
    };

    let mut out = BytesMut::with_capacity(SRT_HEADER_LEN + payload.len());
    header.write_to(&mut out);
    out.extend_from_slice(payload);
    out.to_vec()
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use aero_live_hls::HlsWriter;

    use super::*;
    use crate::control::decode_nak_loss_list;
    use crate::protocol::{ControlType, PacketKind, SrtHeader, SRT_HEADER_LEN};
    use crate::reliability::{Action, ReliabilityState};
    use crate::{KkFlag, SrtSession, SEGMENT_DURATION_SECS};

    // ─── Mock sink ────────────────────────────────────────────────────────────

    /// Records every packet handed to it.
    #[derive(Default)]
    struct RecordingSink {
        packets: Vec<Vec<u8>>,
    }

    impl SrtSink for RecordingSink {
        fn send(&mut self, bytes: &[u8]) {
            self.packets.push(bytes.to_vec());
        }
    }

    // ─── Helper: make a full SRT data packet (header + payload) ──────────────

    fn make_data_pkt(seq_no: u32, kk: KkFlag, payload: &[u8]) -> Vec<u8> {
        let msg_word = kk.set_in_msg_word(0);
        let header = SrtHeader {
            kind: PacketKind::Data { seq_no, msg_word },
            timestamp: 0,
            dest_socket_id: 1,
        };
        let mut pkt = bytes::BytesMut::new();
        header.write_to(&mut pkt);
        pkt.extend_from_slice(payload);
        pkt.to_vec()
    }

    /// Open a fresh in-memory SrtSession backed by a temp HLS dir.
    async fn fresh_session() -> (SrtSession, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let hls = HlsWriter::new(dir.path().to_path_buf(), SEGMENT_DURATION_SECS)
            .await
            .unwrap();
        (SrtSession::new(hls), dir)
    }

    // ─── Test: sequence gap → NAK through sink ────────────────────────────────

    /// When the receiver observes a gap, `pump` must deliver a NAK control
    /// packet through the sink whose loss list matches the gap exactly.
    #[tokio::test]
    async fn pump_sequence_gap_delivers_nak_to_sink() {
        let (mut session, _dir) = fresh_session().await;
        let peer_socket_id = 0xDEAD_BEEF_u32;
        let now = Instant::now();

        // 188-byte dummy payload (one TS packet worth of bytes).
        let payload = vec![0x47u8; 188];

        // Feed packet 0: no gap yet.
        session
            .feed_packet(&make_data_pkt(0, KkFlag::Clear, &payload))
            .await
            .unwrap();
        // Pump once to clear pending actions from the first packet.
        let mut sink = RecordingSink::default();
        session.pump(&mut sink, now, peer_socket_id);
        sink.packets.clear(); // discard setup traffic

        // Feed packet 5, skipping 1..4 → gap [1, 4].
        session
            .feed_packet(&make_data_pkt(5, KkFlag::Clear, &payload))
            .await
            .unwrap();

        let mut sink = RecordingSink::default();
        session.pump(&mut sink, now, peer_socket_id);

        // Find the NAK in what was sent.
        let nak_pkt = sink.packets.iter().find(|p| {
            SrtHeader::parse(p).is_some_and(|h| {
                matches!(
                    h.kind,
                    PacketKind::Control {
                        control_type: ControlType::Nak,
                        ..
                    }
                )
            })
        });
        assert!(
            nak_pkt.is_some(),
            "pump must send a NAK packet through the sink for a sequence gap; \
             got packets: {:?}",
            sink.packets
                .iter()
                .filter_map(|p| SrtHeader::parse(p))
                .collect::<Vec<_>>()
        );

        // Decode the loss list and confirm it covers [1, 4].
        let body = &nak_pkt.unwrap()[SRT_HEADER_LEN..];
        let ranges = decode_nak_loss_list(body);
        assert_eq!(
            ranges,
            vec![(1, 4)],
            "NAK loss list must exactly match the gap [1, 4]"
        );

        // Every packet must be addressed to our peer.
        for pkt in &sink.packets {
            if let Some(h) = SrtHeader::parse(pkt) {
                assert_eq!(
                    h.dest_socket_id, peer_socket_id,
                    "all packets must target the peer socket id"
                );
            }
        }
    }

    // ─── Test: periodic ACK on clock advance ─────────────────────────────────

    /// When the clock is advanced past the ACK interval, `pump` must deliver
    /// at least one ACK control packet through the sink.
    #[tokio::test]
    async fn pump_delivers_periodic_ack_after_interval() {
        let (mut session, _dir) = fresh_session().await;
        let peer_socket_id = 0xCAFE_1234_u32;

        // Set a very short ACK interval so it fires immediately.
        session
            .reliability
            .set_ack_interval(Duration::from_nanos(1));

        let t0 = Instant::now();
        let payload = vec![0x47u8; 188];

        // Feed packet 0 to initialise the receive window and set the ACK timer.
        session
            .feed_packet(&make_data_pkt(0, KkFlag::Clear, &payload))
            .await
            .unwrap();

        // Advance time past the 1 ns interval: feed a second packet at t0+1ms.
        let t1 = t0 + Duration::from_millis(1);
        // We inject `t1` by calling on_data directly so we control the clock.
        let actions = session.reliability.on_data(1, t1);
        session.pending_actions.extend(actions);

        let mut sink = RecordingSink::default();
        session.pump(&mut sink, t1, peer_socket_id);

        let has_ack = sink.packets.iter().any(|p| {
            SrtHeader::parse(p).is_some_and(|h| {
                matches!(
                    h.kind,
                    PacketKind::Control {
                        control_type: ControlType::Ack,
                        ..
                    }
                )
            })
        });
        assert!(
            has_ack,
            "pump must send an ACK after the ACK interval has elapsed; \
             got: {:?}",
            sink.packets
                .iter()
                .filter_map(|p| SrtHeader::parse(p))
                .collect::<Vec<_>>()
        );
    }

    // ─── Test: received NAK drives sender retransmission ─────────────────────

    /// On the sender side: enqueue packets, simulate a NAK, then confirm that
    /// `pump` retransmits exactly the right data packets through the sink.
    #[tokio::test]
    async fn pump_sender_nak_drives_retransmission_of_lost_packets() {
        let (mut session, _dir) = fresh_session().await;
        let peer_socket_id = 0xBEEF_CAFE_u32;
        let now = Instant::now();

        // Enqueue three packets on the sender side.
        let payloads = [b"packet-0".as_ref(), b"packet-1".as_ref(), b"packet-2".as_ref()];
        for p in &payloads {
            session.reliability.enqueue(p.to_vec());
        }
        // seq 0 → "packet-0", seq 1 → "packet-1", seq 2 → "packet-2".

        // Simulate a NAK from the receiver for seq 0 and 1.
        session.reliability.on_nak(0, 1);

        let mut sink = RecordingSink::default();
        session.pump(&mut sink, now, peer_socket_id);

        // Collect the data packets the sink received (bit 31 = 0 → data).
        let data_pkts: Vec<_> = sink
            .packets
            .iter()
            .filter(|p| {
                SrtHeader::parse(p)
                    .is_some_and(|h| matches!(h.kind, PacketKind::Data { .. }))
            })
            .collect();

        assert_eq!(
            data_pkts.len(),
            2,
            "pump must retransmit exactly the 2 NAK'd packets; \
             got data pkts: {}",
            data_pkts.len()
        );

        // Verify seq numbers and payloads.
        let mut seq_nos: Vec<u32> = data_pkts
            .iter()
            .filter_map(|p| {
                SrtHeader::parse(p).and_then(|h| match h.kind {
                    PacketKind::Data { seq_no, .. } => Some(seq_no),
                    PacketKind::Control { .. } => None,
                })
            })
            .collect();
        seq_nos.sort_unstable();
        assert_eq!(seq_nos, vec![0, 1], "retransmitted seq numbers must be 0 and 1");

        for pkt in &data_pkts {
            let body = &pkt[SRT_HEADER_LEN..];
            assert!(
                body == b"packet-0" || body == b"packet-1",
                "retransmitted payload must match the original; got: {body:?}"
            );
        }
    }

    // ─── Test: ACKACK sent in response to an ACK ──────────────────────────────

    /// On the sender side, receiving an ACK must cause `pump` to send an
    /// ACKACK back through the sink (so the receiver can sample RTT).
    #[tokio::test]
    async fn pump_sends_ackack_in_response_to_ack() {
        let (mut session, _dir) = fresh_session().await;
        let peer_socket_id = 0xABCD_1234_u32;
        let now = Instant::now();

        // Enqueue one packet so the send buffer is non-empty, then receive an ACK.
        session.reliability.enqueue(b"data".to_vec());
        // on_ack returns an Action::SendAckAck which must be queued.
        let maybe_ackack = session.reliability.on_ack(1, 42, now);
        // The action goes into pending_actions (mirroring feed_packet flow).
        if let Some(action) = maybe_ackack {
            session.pending_actions.push(action);
        }

        let mut sink = RecordingSink::default();
        session.pump(&mut sink, now, peer_socket_id);

        let has_ackack = sink.packets.iter().any(|p| {
            SrtHeader::parse(p).is_some_and(|h| {
                matches!(
                    h.kind,
                    PacketKind::Control {
                        control_type: ControlType::AckAck,
                        type_specific: 42, // echoes the ack_id
                        ..
                    }
                )
            })
        });
        assert!(
            has_ackack,
            "pump must send an ACKACK with ack_id=42 after receiving an ACK; \
             got packets: {:?}",
            sink.packets
                .iter()
                .filter_map(|p| SrtHeader::parse(p))
                .collect::<Vec<_>>()
        );
    }

    // ─── Test: pump is consuming (no double-send) ─────────────────────────────

    /// A second call to `pump` with no new packets fed must produce nothing.
    #[tokio::test]
    async fn pump_is_consuming_no_double_send() {
        let (mut session, _dir) = fresh_session().await;
        let now = Instant::now();
        let payload = vec![0x47u8; 188];

        // Feed one packet to produce some actions.
        session
            .feed_packet(&make_data_pkt(0, KkFlag::Clear, &payload))
            .await
            .unwrap();

        let mut sink1 = RecordingSink::default();
        session.pump(&mut sink1, now, 1);
        // The first pump may or may not produce packets (depends on timer), but
        // the critical invariant is that the *second* pump sees nothing new.

        let mut sink2 = RecordingSink::default();
        let sent = session.pump(&mut sink2, now, 1);
        assert_eq!(
            sent, 0,
            "second pump with no new actions must send 0 packets"
        );
    }

    // ─── Test: encode_data_packet sets R bit for retransmits ─────────────────

    #[test]
    fn encode_data_packet_sets_r_bit_for_retransmit() {
        let pkt = encode_data_packet(5, true, 0xDEAD_BEEF, b"hello");
        let hdr = SrtHeader::parse(&pkt).unwrap();
        let PacketKind::Data { seq_no, msg_word } = hdr.kind else {
            panic!("expected data packet");
        };
        assert_eq!(seq_no, 5);
        // R bit is bit 2 of msg_word.
        assert_ne!(msg_word & (1 << 2), 0, "R bit must be set for retransmit");

        let body = &pkt[SRT_HEADER_LEN..];
        assert_eq!(body, b"hello");
    }

    #[test]
    fn encode_data_packet_clears_r_bit_for_new_packet() {
        let pkt = encode_data_packet(3, false, 1, b"world");
        let hdr = SrtHeader::parse(&pkt).unwrap();
        let PacketKind::Data { msg_word, .. } = hdr.kind else {
            panic!("expected data packet");
        };
        assert_eq!(msg_word & (1 << 2), 0, "R bit must be clear for a new packet");
    }

    // ─── Test: SrtSink blanket impl for closures (ergonomics check) ───────────

    /// A closure-based sink works correctly, confirming the trait is usable
    /// without a named struct in simple scenarios.
    #[tokio::test]
    async fn sink_implemented_as_closure_wrapper() {
        // We test via a trivial wrapper because Rust trait objects can't be
        // satisfied by raw closures without a wrapper.  This test confirms the
        // trait is object-safe and the bound `impl SrtSink` accepts any struct.
        struct ClosureSink<F: FnMut(&[u8])>(F);
        impl<F: FnMut(&[u8])> SrtSink for ClosureSink<F> {
            fn send(&mut self, bytes: &[u8]) {
                (self.0)(bytes);
            }
        }

        let (mut session, _dir) = fresh_session().await;
        let now = Instant::now();

        let mut count = 0usize;
        let mut sink = ClosureSink(|_bytes: &[u8]| {
            count += 1;
        });

        // Feed a packet that produces a NAK (seq 0 then seq 5).
        let payload = vec![0x47u8; 188];
        session
            .feed_packet(&make_data_pkt(0, KkFlag::Clear, &payload))
            .await
            .unwrap();
        session.pump(&mut sink, now, 1);
        session
            .feed_packet(&make_data_pkt(5, KkFlag::Clear, &payload))
            .await
            .unwrap();
        session.pump(&mut sink, now, 1);
        // At least the NAK packet must have been sent.
        assert!(count > 0, "closure sink must receive at least one packet");
    }

    // ─── Test: standalone ReliabilityState + pump round-trip ─────────────────

    /// Verify the full loop without an HLS writer: enqueue data on sender,
    /// simulate a gap on the receiver (NAK), and confirm retransmission.
    #[test]
    fn reliability_state_nak_retransmit_round_trip() {
        let now = Instant::now();
        let mut sender = ReliabilityState::new(0);

        // Sender enqueues three packets.
        sender.enqueue(b"A".to_vec()); // seq 0
        sender.enqueue(b"B".to_vec()); // seq 1
        sender.enqueue(b"C".to_vec()); // seq 2

        // Receiver reports seq 1 as lost.
        sender.on_nak(1, 1);

        // drain_retransmits must yield exactly seq 1.
        let retx = sender.drain_retransmits();
        assert_eq!(retx.len(), 1);
        assert!(matches!(
            &retx[0],
            Action::SendData {
                seq_no: 1,
                is_retransmit: true,
                ..
            }
        ));

        // Sender receives ACK for all three → send buffer clears.
        sender.on_ack(3, 1, now);
        assert_eq!(sender.send_buffer_len(), 0);
    }
}
