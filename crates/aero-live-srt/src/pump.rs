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
//!    control packet (ACK / NAK / ACKACK) through the sink (control packets
//!    are never paced — they are tiny and time-critical).
//! 2. Moves [`ReliabilityState::drain_retransmits`] output into the session's
//!    retransmit queue, then drains the data plane — retransmits first, then
//!    fresh data queued via [`SrtSession::send_data`] — gating **every** data
//!    packet through the session's [`Pacer`]. A [`Allowance::DeferUntil`]
//!    verdict holds that packet *and everything behind it* (order is
//!    preserved) until a later `pump` call whose `now` has reached the defer
//!    instant; the instant is surfaced via [`SrtSession::next_send_at`].
//!
//! Congestion signals reach the pacer through [`SrtSession::handle_control`]:
//! ACK CIFs carry the peer's RTT / RTT-variance samples, NAK loss lists carry
//! the retransmit-request counts.
//!
//! No I/O primitives, no async, no real socket — fully unit-testable.
//!
//! ## Limitations (intentional — see crate docs)
//!
//! - No real UDP socket is involved; `pump` talks only to the supplied sink.
//! - Send timing is caller-driven: the pacer only computes *when* the next
//!   packet may go; the caller must call `pump` again at (or after) that
//!   instant. The caller controls the clock via `now`, so tests fake time.
//! - The `peer_socket_id` parameter mirrors that of
//!   [`SrtSession::drain_control_packets`].

// SRT-specific acronyms (ACK, NAK, ACKACK, RTT, CIF, …) are domain-standard.
#![allow(clippy::doc_markdown)]

use std::time::Instant;

use bytes::BytesMut;

use crate::control::decode_nak_loss_list;
use crate::pacing::Allowance;
use crate::protocol::{ControlType, PacketKind, SrtHeader, SRT_HEADER_LEN};
use crate::reliability::{seq_diff, Action};
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

/// Convenience sink that collects packets into a `Vec` for later async dispatch.
///
/// The production receive loop uses this to gather all control packets from
/// [`SrtSession::pump`], then sends them asynchronously via the UDP socket.
impl SrtSink for Vec<Vec<u8>> {
    fn send(&mut self, bytes: &[u8]) {
        self.push(bytes.to_vec());
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// pump implementation
// ─────────────────────────────────────────────────────────────────────────────

/// A decoded SRT ACK CIF (the first three words — the fields this
/// implementation consumes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckCif {
    /// Next expected sequence number (highest contiguous received + 1).
    pub ack_seq_no: u32,
    /// Peer's smoothed RTT in microseconds.
    pub rtt_us: u32,
    /// Peer's RTT variance in microseconds.
    pub rttvar_us: u32,
}

/// Decode the leading words of an ACK CIF (the bytes after the 16-byte SRT
/// header). Returns `None` if the body is shorter than the three mandatory
/// words; trailing fields (buffer size, rates) are ignored.
#[must_use]
pub fn decode_ack_cif(body: &[u8]) -> Option<AckCif> {
    if body.len() < 12 {
        return None;
    }
    let word = |i: usize| u32::from_be_bytes([body[i], body[i + 1], body[i + 2], body[i + 3]]);
    Some(AckCif {
        ack_seq_no: word(0),
        rtt_us: word(4),
        rttvar_us: word(8),
    })
}

impl SrtSession {
    /// Queue a fresh outbound data packet: assigns the next sequence number,
    /// buffers the payload for retransmission, and holds it for the next
    /// [`SrtSession::pump`] (which sends it as soon as the pacer allows).
    ///
    /// Returns the assigned sequence number.
    pub fn send_data(&mut self, payload: Vec<u8>) -> u32 {
        let Action::SendData { seq_no, payload, .. } = self.reliability.enqueue(payload) else {
            unreachable!("enqueue always returns SendData");
        };
        self.fresh_data.push_back((seq_no, payload));
        seq_no
    }

    /// Process an inbound SRT control packet (ACK / NAK / ACKACK) from the
    /// peer, updating the sender-side reliability state and the congestion
    /// pacer:
    ///
    /// - **ACK** — advances the send window, queues the ACKACK reply, and
    ///   feeds the CIF's RTT / RTT-variance samples to the pacer.
    /// - **NAK** — schedules the listed packets for retransmission and feeds
    ///   the lost-packet count to the pacer.
    /// - **ACKACK** — samples RTT on the receiver-side estimator.
    ///
    /// Unknown or unparsable packets are ignored. `now` is injected so tests
    /// can fake time.
    pub fn handle_control(&mut self, datagram: &[u8], now: Instant) {
        let Some(header) = SrtHeader::parse(datagram) else {
            return;
        };
        let PacketKind::Control {
            control_type,
            type_specific,
            ..
        } = header.kind
        else {
            return;
        };
        let body = datagram.get(SRT_HEADER_LEN..).unwrap_or(&[]);
        match control_type {
            ControlType::Ack => {
                let Some(cif) = decode_ack_cif(body) else {
                    return;
                };
                self.pacer.on_ack(cif.rtt_us, cif.rttvar_us, now);
                // type_specific carries the ACK id to echo in the ACKACK.
                if let Some(action) = self.reliability.on_ack(cif.ack_seq_no, type_specific, now) {
                    self.pending_actions.push(action);
                }
            }
            ControlType::Nak => {
                let mut lost: usize = 0;
                for (from, to) in decode_nak_loss_list(body) {
                    self.reliability.on_nak(from, to);
                    // Forward span + 1 = packets requested; inverted ranges
                    // were already rejected by on_nak, count them as zero.
                    let span = seq_diff(from, to);
                    if span >= 0 {
                        lost = lost.saturating_add(usize::try_from(span).unwrap_or(usize::MAX))
                            .saturating_add(1);
                    }
                }
                self.pacer.on_nak(lost, now);
            }
            ControlType::AckAck => self.reliability.on_ackack(type_specific, now),
            _ => {}
        }
    }

    /// Drive all pending outgoing work through `sink`.
    ///
    /// Call this periodically (e.g. from a timer tick or immediately after
    /// feeding inbound packets) to flush:
    ///
    /// 1. **Control packets** — every ACK, NAK, or ACKACK that accumulated in
    ///    `pending_actions` since the last `pump` (or `drain_control_packets`).
    ///    Never paced.
    /// 2. **Data packets**, gated through the congestion [`Pacer`]:
    ///    retransmissions (NAK-triggered, from the loss list) first, then
    ///    fresh packets queued via [`SrtSession::send_data`]. The first
    ///    [`Allowance::DeferUntil`] verdict stops the data drain — held
    ///    packets keep their order and go out on a later `pump` whose `now`
    ///    has reached the deferral instant ([`SrtSession::next_send_at`]).
    ///
    /// `peer_socket_id` is written into the `dest_socket_id` field of every
    /// outgoing packet.  `now` is the current [`Instant`]; it feeds the pacer
    /// (the caller controls the clock, so tests can fake time).
    ///
    /// Returns the number of packets sent (useful in tests to confirm activity).
    pub fn pump(
        &mut self,
        sink: &mut impl SrtSink,
        now: Instant,
        peer_socket_id: u32,
    ) -> usize {
        let mut sent = 0usize;

        // ── 1. Control packets (ACK / NAK / ACKACK) ───────────────────────────
        //
        // drain_control_packets takes `pending_actions` and serialises each one
        // via encode_control.  SendData actions are silently dropped there (they
        // are not control-plane), which is correct: data is handled below.
        for pkt in self.drain_control_packets(peer_socket_id) {
            sink.send(&pkt);
            sent += 1;
        }

        // ── 2. Data packets, paced ────────────────────────────────────────────
        //
        // Newly NAK'd sequence numbers join the retransmit queue, which is
        // always drained ahead of fresh data (a receiver stalled on a gap
        // benefits more from the missing packet than from new ones) — but
        // both spend from the same pacer budget.
        for action in self.reliability.drain_retransmits() {
            if let Action::SendData { seq_no, payload, .. } = action {
                self.deferred_retransmits.push_back((seq_no, payload));
            }
        }

        self.next_send_at = None;
        loop {
            let is_retransmit = !self.deferred_retransmits.is_empty();
            let queue = if is_retransmit {
                &mut self.deferred_retransmits
            } else {
                &mut self.fresh_data
            };
            let Some((seq_no, payload)) = queue.front() else {
                break;
            };
            // Ask the pacer with the wire length (header + payload) so the
            // packet is only encoded — one allocation — when it may be sent.
            match self.pacer.allowance(now, SRT_HEADER_LEN + payload.len()) {
                Allowance::Allow => {
                    let pkt = encode_data_packet(*seq_no, is_retransmit, peer_socket_id, payload);
                    sink.send(&pkt);
                    sent += 1;
                    queue.pop_front();
                }
                Allowance::DeferUntil(at) => {
                    // Hold this packet and everything behind it: releasing
                    // later packets first would reorder the stream.
                    self.next_send_at = Some(at);
                    break;
                }
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

    // ─────────────────────────────────────────────────────────────────────────
    // Pacing tests: fake clock (caller-supplied `now`) + recording sink
    // ─────────────────────────────────────────────────────────────────────────

    /// Payload size that yields exactly one nominal 1500-byte wire packet
    /// after the 16-byte SRT header is prepended.
    const MTU_PAYLOAD: usize = 1500 - SRT_HEADER_LEN;

    /// Wire-encode an ACK control packet whose CIF carries the given RTT
    /// sample (the same bytes a real receiver would put on the wire).
    fn make_ack_pkt(ack_seq_no: u32, ack_id: u32, rtt_us: u32, rttvar_us: u32) -> Vec<u8> {
        crate::control::encode_control(
            &Action::SendAck { ack_seq_no, ack_id },
            1,
            0,
            rtt_us,
            rttvar_us,
        )
        .expect("SendAck encodes to a control packet")
    }

    /// Wire-encode a NAK control packet reporting the inclusive loss range.
    fn make_nak_pkt(from: u32, to: u32) -> Vec<u8> {
        crate::control::encode_control(&Action::SendNak { from, to }, 1, 0, 0, 0)
            .expect("SendNak encodes to a control packet")
    }

    /// Sequence numbers of the data packets in `sink`, in send order.
    fn data_seqs(sink: &RecordingSink) -> Vec<u32> {
        sink.packets
            .iter()
            .filter_map(|p| match SrtHeader::parse(p).map(|h| h.kind) {
                Some(PacketKind::Data { seq_no, .. }) => Some(seq_no),
                _ => None,
            })
            .collect()
    }

    // ─── decode_ack_cif ───────────────────────────────────────────────────────

    #[test]
    fn decode_ack_cif_parses_leading_words_and_rejects_short_bodies() {
        let pkt = make_ack_pkt(42, 7, 1_000, 250);
        let cif = decode_ack_cif(&pkt[SRT_HEADER_LEN..]).expect("full CIF must parse");
        assert_eq!(
            cif,
            AckCif {
                ack_seq_no: 42,
                rtt_us: 1_000,
                rttvar_us: 250,
            }
        );
        assert!(
            decode_ack_cif(&[0u8; 11]).is_none(),
            "11 bytes is short of the 3 mandatory CIF words"
        );
    }

    // ─── send_data bookkeeping ────────────────────────────────────────────────

    /// `send_data` assigns consecutive sequence numbers and keeps payloads in
    /// the retransmit buffer (so a later NAK can replay them).
    #[tokio::test]
    async fn send_data_assigns_consecutive_seqs_and_buffers_for_retransmit() {
        let (mut session, _dir) = fresh_session().await;
        assert_eq!(session.send_data(b"a".to_vec()), 0);
        assert_eq!(session.send_data(b"b".to_vec()), 1);
        assert_eq!(session.reliability.send_buffer_len(), 2);
    }

    // ─── steady-state spacing through pump ────────────────────────────────────

    /// At 1500 B/s with 1500-byte wire packets, `pump` must absorb the initial
    /// 4-MTU bucket as a burst, then release exactly one packet per second —
    /// in order, with the release instant surfaced via `next_send_at`.
    #[tokio::test]
    async fn pump_paces_fresh_data_and_releases_in_order() {
        let (session, _dir) = fresh_session().await;
        let mut session = session.with_max_bandwidth(1500);
        let t0 = Instant::now();
        for _ in 0..6 {
            session.send_data(vec![0xAA; MTU_PAYLOAD]);
        }

        // Bucket capacity is 4 MTUs: the first 4 packets pass as a burst.
        let mut sink = RecordingSink::default();
        session.pump(&mut sink, t0, 1);
        assert_eq!(data_seqs(&sink), vec![0, 1, 2, 3], "initial burst absorbed in order");
        assert_eq!(
            session.next_send_at(),
            Some(t0 + Duration::from_secs(1)),
            "1500-byte deficit at 1500 B/s = exactly 1 s"
        );

        // Pumping before the defer instant releases nothing and keeps the
        // (unchanged) release instant.
        let mut sink = RecordingSink::default();
        let sent = session.pump(&mut sink, t0 + Duration::from_millis(500), 1);
        assert_eq!(sent, 0, "no budget halfway through the deferral");
        assert_eq!(session.next_send_at(), Some(t0 + Duration::from_secs(1)));

        // At the defer instant exactly one packet's budget has accrued…
        let mut sink = RecordingSink::default();
        session.pump(&mut sink, t0 + Duration::from_secs(1), 1);
        assert_eq!(data_seqs(&sink), vec![4]);
        assert_eq!(session.next_send_at(), Some(t0 + Duration::from_secs(2)));

        // …and the last packet goes one second later: steady-state spacing.
        let mut sink = RecordingSink::default();
        session.pump(&mut sink, t0 + Duration::from_secs(2), 1);
        assert_eq!(data_seqs(&sink), vec![5]);
        assert_eq!(session.next_send_at(), None, "queue drained — nothing held back");
    }

    // ─── NAK-storm slowdown through handle_control ────────────────────────────

    /// A sustained NAK storm fed through `handle_control` must collapse the
    /// send rate multiplicatively down to the 5% floor and visibly widen the
    /// spacing `pump` enforces.
    #[tokio::test]
    async fn pump_nak_storm_slows_send_rate_and_widens_spacing() {
        let (session, _dir) = fresh_session().await;
        let mut session = session.with_max_bandwidth(1500);
        let t0 = Instant::now();
        assert_eq!(session.current_send_rate(), 1500);

        // One NAK reporting 10 lost packets (none buffered → no retransmits)
        // crosses the per-window threshold: one multiplicative decrease.
        session.handle_control(&make_nak_pkt(100, 109), t0);
        assert_eq!(session.current_send_rate(), 1275, "1500 × 85% after one NAK burst");

        // One storm per 100 ms rate window → one more decrease each window,
        // clamping at the 5% floor.
        for i in 1..40u64 {
            session.handle_control(&make_nak_pkt(100, 109), t0 + Duration::from_millis(100 * i));
        }
        assert_eq!(session.current_send_rate(), 75, "floor = 5% of 1500");

        // Spacing widens to match: probe at the instant of the last storm NAK
        // (waiting longer would let clean-window recovery lift the rate again
        // — that path is covered by the pacing unit tests). After the 4-MTU
        // burst, the next packet is 1500 B / 75 B/s = 20 s out instead of the
        // 1 s it would be at the configured rate.
        let t1 = t0 + Duration::from_millis(3900);
        for _ in 0..5 {
            session.send_data(vec![0xAA; MTU_PAYLOAD]);
        }
        let mut sink = RecordingSink::default();
        session.pump(&mut sink, t1, 1);
        assert_eq!(data_seqs(&sink), vec![0, 1, 2, 3]);
        assert_eq!(
            session.next_send_at(),
            Some(t1 + Duration::from_secs(20)),
            "backed-off rate must stretch the inter-packet gap"
        );
    }

    // ─── RTT inflation slowdown through handle_control ────────────────────────

    /// ACK CIFs fed through `handle_control` must drive the pacer (baseline,
    /// then back-off on inflation), advance the send window, and queue ACKACK
    /// echoes that the next `pump` delivers.
    #[tokio::test]
    async fn handle_control_ack_feeds_rtt_to_pacer_and_advances_the_window() {
        let (session, _dir) = fresh_session().await;
        let mut session = session.with_max_bandwidth(1000);
        let t0 = Instant::now();

        // Buffer two packets, then ACK both (ack_seq_no = next expected = 2).
        session.send_data(b"one".to_vec());
        session.send_data(b"two".to_vec());
        session.handle_control(&make_ack_pkt(2, 7, 20_000, 0), t0); // baseline 20 ms
        assert_eq!(
            session.reliability.send_buffer_len(),
            0,
            "ACK must free the retransmit buffer"
        );
        assert_eq!(
            session.current_send_rate(),
            1000,
            "the first RTT sample only sets the baseline"
        );

        // 40 ms > 20 ms × 1.25 → congestion: multiplicative decrease.
        session.handle_control(&make_ack_pkt(2, 8, 40_000, 0), t0 + Duration::from_millis(10));
        assert_eq!(session.current_send_rate(), 850);

        // Both ACKs queued an ACKACK echo; pump must deliver them in order.
        let mut sink = RecordingSink::default();
        session.pump(&mut sink, t0 + Duration::from_millis(10), 1);
        let ackacks: Vec<u32> = sink
            .packets
            .iter()
            .filter_map(|p| match SrtHeader::parse(p).map(|h| h.kind) {
                Some(PacketKind::Control {
                    control_type: ControlType::AckAck,
                    type_specific,
                    ..
                }) => Some(type_specific),
                _ => None,
            })
            .collect();
        assert_eq!(ackacks, vec![7, 8], "each ACK id must be echoed as an ACKACK");
    }

    // ─── retransmit priority under a shared budget ────────────────────────────

    /// NAK-triggered retransmissions go out ahead of queued fresh data but
    /// spend from the same pacer budget — with the bucket drained, both wait,
    /// and as budget accrues the retransmits (R bit set) are released first.
    #[tokio::test]
    async fn pump_retransmits_take_priority_but_spend_the_pacer_budget() {
        let (session, _dir) = fresh_session().await;
        let mut session = session.with_max_bandwidth(1500);
        let t0 = Instant::now();

        // Send 4 packets (seq 0..=3), draining the 4-MTU bucket exactly.
        for _ in 0..4 {
            session.send_data(vec![0xAA; MTU_PAYLOAD]);
        }
        let mut sink = RecordingSink::default();
        session.pump(&mut sink, t0, 1);
        assert_eq!(data_seqs(&sink), vec![0, 1, 2, 3]);

        // The peer NAKs seq 0–1 (2 packets: below the back-off threshold, so
        // the rate stays put) and a fifth fresh packet joins the queue.
        session.handle_control(&make_nak_pkt(0, 1), t0);
        session.send_data(vec![0xAA; MTU_PAYLOAD]); // seq 4
        assert_eq!(session.current_send_rate(), 1500, "2 NAKs must not back off");

        // Budget exhausted: nothing goes out — retransmissions included.
        let mut sink = RecordingSink::default();
        assert_eq!(
            session.pump(&mut sink, t0, 1),
            0,
            "retransmits still wait for pacer budget"
        );

        // As budget accrues (one packet per second) the retransmits go first,
        // each with the R bit set…
        for (step, want_seq) in [(1u64, 0u32), (2, 1)] {
            let mut sink = RecordingSink::default();
            session.pump(&mut sink, t0 + Duration::from_secs(step), 1);
            assert_eq!(data_seqs(&sink), vec![want_seq], "retransmits drain in NAK order");
            let msg_word = sink
                .packets
                .iter()
                .find_map(|p| match SrtHeader::parse(p).map(|h| h.kind) {
                    Some(PacketKind::Data { msg_word, .. }) => Some(msg_word),
                    _ => None,
                })
                .expect("a data packet was sent");
            assert_ne!(msg_word & (1 << 2), 0, "retransmit must carry the R bit");
        }

        // …then the fresh packet (R bit clear) brings up the rear.
        let mut sink = RecordingSink::default();
        session.pump(&mut sink, t0 + Duration::from_secs(3), 1);
        assert_eq!(data_seqs(&sink), vec![4], "fresh data must follow the retransmits");
        let msg_word = sink
            .packets
            .iter()
            .find_map(|p| match SrtHeader::parse(p).map(|h| h.kind) {
                Some(PacketKind::Data { msg_word, .. }) => Some(msg_word),
                _ => None,
            })
            .expect("a data packet was sent");
        assert_eq!(msg_word & (1 << 2), 0, "fresh data must not carry the R bit");
    }

    // ─── handle_control robustness ────────────────────────────────────────────

    /// Unparsable datagrams, data packets, and truncated CIFs must neither
    /// move the pacer nor queue control actions.
    #[tokio::test]
    async fn handle_control_ignores_malformed_and_irrelevant_packets() {
        let (session, _dir) = fresh_session().await;
        let mut session = session.with_max_bandwidth(1000);
        let t0 = Instant::now();

        session.handle_control(&[], t0); // unparsable
        session.handle_control(&make_data_pkt(0, KkFlag::Clear, b"x"), t0); // data plane
        let ack = make_ack_pkt(1, 1, 99_000, 0);
        session.handle_control(&ack[..SRT_HEADER_LEN + 8], t0); // CIF one word short

        assert_eq!(
            session.current_send_rate(),
            1000,
            "none of the packets above may move the pacer"
        );
        let mut sink = RecordingSink::default();
        assert_eq!(
            session.pump(&mut sink, t0, 1),
            0,
            "and none may queue control actions"
        );
    }
}
