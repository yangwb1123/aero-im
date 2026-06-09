//! SRT ACK/NAK reliability state machine.
//!
//! A pure, drivable state machine — no real sockets, no `async`.  The caller
//! drives time forward by calling [`ReliabilityState::tick`] with the current
//! instant, and injects/extracts packets via a set of methods.  No I/O happens
//! inside this module; all network-side effects are returned as [`Action`]s for
//! the caller to execute.
//!
//! ## Architecture
//!
//! Two sides of a connection, each represented by its own half of
//! [`ReliabilityState`]:
//!
//! ### Sender side
//! - Keeps a **send buffer** keyed by 31-bit sequence number.
//! - Keeps a **loss list** of sequence numbers reported by NAK.
//! - On [`ReliabilityState::enqueue`]: assigns the next sequence number,
//!   stores the payload, returns the sequenced packet to send.
//! - On [`ReliabilityState::on_nak`]: adds the reported range to the loss
//!   list; the next [`ReliabilityState::drain_retransmits`] call returns the
//!   payloads that need retransmission.
//! - On [`ReliabilityState::on_ack`]: advances the send window — drops all
//!   buffered packets with `seq_no < ack_no` from the send buffer.
//!
//! ### Receiver side
//! - Tracks the highest sequence number seen (`rcv_seq`).
//! - Detects **gaps** (missing sequence numbers) and emits a NAK range.
//! - Emits a periodic **full ACK** (configurable interval, default 10 ms).
//! - Emits an **ACKACK** in response to each ACK it receives from the sender.
//!
//! ### RTT estimation
//! - Updated by the sender when an ACK is echoed back as an ACKACK.
//! - RTT = `ackack_arrival_time − ack_send_time`.
//! - RTT variance is updated as `rtt_var = |rtt_var * 3/4 + |rtt - old_rtt| / 4|`.
//!
//! ## Sequence-number arithmetic
//!
//! All sequence numbers are 31-bit (SRT data-packet header bit 30..0).
//! Wraparound is handled by [`seq_lt`] and [`seq_diff`] which use modular
//! arithmetic with the half-range trick.

// SRT-specific acronyms (SEQ, ACK, NAK, RTT, TSBPD, …) are standard in the
// domain; suppressing doc_markdown keeps them readable without backticks.
#![allow(clippy::doc_markdown)]

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

// ─────────────────────────────────────────────────────────────────────────────
// Sequence number helpers
// ─────────────────────────────────────────────────────────────────────────────

/// The SRT sequence space is 2^31.
const SEQ_SPACE: u64 = 1 << 31;
/// Maximum valid sequence number (inclusive): 2^31 − 1.
const SEQ_MAX: u32 = 0x7FFF_FFFF;

/// Upper bound on how many sequence numbers a single NAK loss range may span
/// before [`ReliabilityState::on_nak`] refuses to walk it.
///
/// A NAK reports packets the *receiver* missed, so a legitimate range can never
/// exceed the in-flight window. SRT's default flow window is 8192 packets; we
/// allow a generous 64Ki headroom over that and treat anything larger as a
/// malformed or malicious report. Without this bound a NAK whose `to` endpoint
/// sits *behind* `from` in the forward 31-bit direction (e.g. `from=10, to=5`)
/// would make the walk traverse almost the entire 2^31 sequence space — a
/// remote-triggerable CPU denial of service, since NAK ranges arrive from the
/// network via [`crate::control::decode_nak_loss_list`].
const MAX_NAK_RANGE_SPAN: i64 = 1 << 16;

/// Increment a 31-bit sequence number, wrapping at `SEQ_MAX`.
#[must_use]
pub fn seq_next(seq: u32) -> u32 {
    (seq.wrapping_add(1)) & SEQ_MAX
}

/// Return `true` if sequence number `a` comes strictly *before* `b` in the
/// modular 31-bit sequence space (i.e. `b - a` is a small positive number).
///
/// Uses the half-range trick: if `|b - a| < SEQ_SPACE / 2` and `b > a`, then
/// `a < b`; otherwise (the difference wraps around), `b` is actually earlier.
#[must_use]
pub fn seq_lt(a: u32, b: u32) -> bool {
    let av = u64::from(a) & u64::from(SEQ_MAX);
    let bv = u64::from(b) & u64::from(SEQ_MAX);
    if av == bv {
        return false;
    }
    let forward = (bv.wrapping_sub(av)) & (SEQ_SPACE - 1);
    forward < SEQ_SPACE / 2
}

/// Signed difference `b - a` in the modular 31-bit space (positive means `b`
/// is ahead of `a`).
#[must_use]
#[allow(clippy::cast_possible_wrap)] // SEQ_SPACE / 2 is 2^30, well within i64 range
pub fn seq_diff(a: u32, b: u32) -> i64 {
    let av = i64::from(a & SEQ_MAX);
    let bv = i64::from(b & SEQ_MAX);
    let diff = bv - av;
    // SEQ_SPACE is 2^31; half is 2^30.  Both fit in i64 without wrapping.
    let half = (SEQ_SPACE / 2) as i64;
    let space = SEQ_SPACE as i64;
    if diff > half {
        diff - space
    } else if diff < -half {
        diff + space
    } else {
        diff
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Action enum — effects the caller must execute
// ─────────────────────────────────────────────────────────────────────────────

/// An action that the reliability state machine asks the caller to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Send a data packet with the given sequence number and payload.
    ///
    /// Emitted by [`ReliabilityState::enqueue`] for new packets, and by
    /// [`ReliabilityState::drain_retransmits`] for retransmissions.
    SendData {
        seq_no: u32,
        payload: Vec<u8>,
        /// `true` if this is a retransmission (set in the SRT `R` flag).
        is_retransmit: bool,
    },
    /// Send an ACK control packet.
    ///
    /// `ack_seq_no` is the full-acknowledgement number (= highest contiguous
    /// received seq + 1, i.e. the next expected).  `ack_id` is a monotonic
    /// counter used to correlate the ACKACK.
    SendAck {
        ack_seq_no: u32,
        ack_id: u32,
    },
    /// Send a NAK (loss report) control packet listing the lost sequence range.
    ///
    /// `from` and `to` are inclusive endpoints of the lost range.
    SendNak {
        from: u32,
        to: u32,
    },
    /// Send an ACKACK in response to an ACK we received from the sender.
    SendAckAck {
        ack_id: u32,
    },
}

// ─────────────────────────────────────────────────────────────────────────────
// RTT estimator
// ─────────────────────────────────────────────────────────────────────────────

/// Exponential-moving-average RTT and variance estimator.
///
/// Updated on each ACK/ACKACK round-trip using the SRT-prescribed formula:
/// ```text
/// rtt_new  = (rtt * 7 + sample) / 8
/// rttvar   = (rttvar * 3 + |sample - rtt|) / 4
/// ```
#[derive(Debug, Clone)]
pub struct RttEstimator {
    /// Smoothed RTT in microseconds.
    rtt_us: u64,
    /// RTT variance in microseconds.
    rttvar_us: u64,
}

impl Default for RttEstimator {
    fn default() -> Self {
        Self {
            rtt_us: 100_000, // 100 ms initial guess
            rttvar_us: 50_000,
        }
    }
}

impl RttEstimator {
    /// Update the RTT estimate with a new sample (in microseconds).
    pub fn update(&mut self, sample_us: u64) {
        let diff = self.rtt_us.abs_diff(sample_us);
        self.rttvar_us = (self.rttvar_us * 3 + diff) / 4;
        self.rtt_us = (self.rtt_us * 7 + sample_us) / 8;
    }

    /// Smoothed RTT.
    #[must_use]
    pub fn rtt(&self) -> Duration {
        Duration::from_micros(self.rtt_us)
    }

    /// RTT variance.
    #[must_use]
    pub fn rttvar(&self) -> Duration {
        Duration::from_micros(self.rttvar_us)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Pending-ACK record (sender side)
// ─────────────────────────────────────────────────────────────────────────────

/// A sent ACK whose ACKACK has not yet been received; used for RTT sampling.
#[derive(Debug, Clone)]
struct PendingAck {
    ack_id: u32,
    sent_at: Instant,
}

// ─────────────────────────────────────────────────────────────────────────────
// ReliabilityState — the combined sender+receiver state machine
// ─────────────────────────────────────────────────────────────────────────────

/// How frequently the receiver emits a full ACK (default: 10 ms, matching the
/// SRT reference implementation's `ACK_INTERVAL`).
const DEFAULT_ACK_INTERVAL: Duration = Duration::from_millis(10);

/// Combined sender-side and receiver-side SRT reliability state for one
/// direction of a connection.
///
/// A real deployment would instantiate one per direction (publish → ingest and
/// ingest → publisher); tests drive a single instance end-to-end.
#[derive(Debug)]
pub struct ReliabilityState {
    // ── Sender side ──────────────────────────────────────────────────────────
    /// Next sequence number to assign to an outgoing packet.
    next_seq: u32,
    /// Buffered sent packets awaiting ACK, keyed by sequence number.
    send_buf: BTreeMap<u32, Vec<u8>>,
    /// Loss list: sequence numbers reported by NAK that need retransmission.
    loss_list: VecDeque<u32>,

    // ── Receiver side ────────────────────────────────────────────────────────
    /// Highest sequence number received so far (or `None` before any packet).
    rcv_seq: Option<u32>,
    /// Last sequence number that has been fully acknowledged (contiguous).
    ack_seq: Option<u32>,
    /// Monotonic ACK identifier counter.
    ack_id: u32,
    /// When the next periodic ACK should be emitted.
    next_ack_at: Option<Instant>,
    /// How often to emit a full ACK.
    ack_interval: Duration,

    // ── RTT ──────────────────────────────────────────────────────────────────
    rtt: RttEstimator,
    /// Outstanding ACKs we sent, waiting for ACKACK to sample RTT.
    pending_acks: Vec<PendingAck>,
}

impl ReliabilityState {
    /// Create a new reliability state starting from sequence number `initial_seq`.
    #[must_use]
    pub fn new(initial_seq: u32) -> Self {
        Self {
            next_seq: initial_seq & SEQ_MAX,
            send_buf: BTreeMap::new(),
            loss_list: VecDeque::new(),
            rcv_seq: None,
            ack_seq: None,
            ack_id: 1,
            next_ack_at: None,
            ack_interval: DEFAULT_ACK_INTERVAL,
            rtt: RttEstimator::default(),
            pending_acks: Vec::new(),
        }
    }

    /// Override the periodic ACK interval (useful in tests).
    pub fn set_ack_interval(&mut self, interval: Duration) {
        self.ack_interval = interval;
    }

    // ──────────────────────────── Sender API ─────────────────────────────────

    /// Assign the next sequence number to `payload`, buffer it, and return the
    /// [`Action::SendData`] the caller should transmit.
    pub fn enqueue(&mut self, payload: Vec<u8>) -> Action {
        let seq = self.next_seq;
        self.next_seq = seq_next(seq);
        self.send_buf.insert(seq, payload.clone());
        Action::SendData {
            seq_no: seq,
            payload,
            is_retransmit: false,
        }
    }

    /// Drain all pending retransmits (sequence numbers on the loss list that
    /// are still in the send buffer) and return them as [`Action::SendData`]
    /// with `is_retransmit = true`.
    pub fn drain_retransmits(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        while let Some(seq) = self.loss_list.pop_front() {
            if let Some(payload) = self.send_buf.get(&seq) {
                actions.push(Action::SendData {
                    seq_no: seq,
                    payload: payload.clone(),
                    is_retransmit: true,
                });
            }
            // If the packet has already been acked (removed from send_buf),
            // silently skip it.
        }
        actions
    }

    /// Process an incoming ACK from the receiver.
    ///
    /// Advances the send window: all buffered packets with `seq_no < ack_no`
    /// are freed from the send buffer (they have been received).  Also removes
    /// them from the loss list.
    ///
    /// Returns an optional [`Action::SendAckAck`] that the sender should echo
    /// back to the receiver so it can sample RTT.
    pub fn on_ack(&mut self, ack_no: u32, ack_id: u32, now: Instant) -> Option<Action> {
        // Free all buffered packets with seq_no < ack_no.
        let acked_keys: Vec<u32> = self
            .send_buf
            .range(..=SEQ_MAX)
            .filter(|(&k, _)| seq_lt(k, ack_no))
            .map(|(&k, _)| k)
            .collect();
        for k in acked_keys {
            self.send_buf.remove(&k);
        }
        // Prune matching entries from the loss list.
        self.loss_list.retain(|&seq| !seq_lt(seq, ack_no));
        // Record this ACK so we can emit ACKACK and time RTT.
        self.pending_acks.push(PendingAck { ack_id, sent_at: now });
        Some(Action::SendAckAck { ack_id })
    }

    /// Process an incoming NAK from the receiver.
    ///
    /// `from` and `to` are the inclusive endpoints of the reported loss range.
    /// Sequence numbers in `[from, to]` that are still in the send buffer are
    /// added to the loss list for retransmission.
    ///
    /// The range is walked forward (`from → to`) through the modular 31-bit
    /// sequence space. A NAK that arrives from an untrusted peer can carry a
    /// `to` endpoint that lies *behind* `from` in that forward direction
    /// (whether through corruption or malice); walking such a range unguarded
    /// would iterate almost the entire 2^31 space. To stay bounded we ignore
    /// any range that is not forward-ordered, or that spans more than
    /// [`MAX_NAK_RANGE_SPAN`] packets (far beyond any legitimate in-flight
    /// window). A single-packet NAK (`from == to`) always proceeds.
    pub fn on_nak(&mut self, from: u32, to: u32) {
        let from = from & SEQ_MAX;
        let to = to & SEQ_MAX;
        // `seq_diff` yields the signed forward distance `to - from`. A negative
        // value means `to` is behind `from` (an inverted / wrapped range we
        // refuse to walk); a value past the cap is implausibly large. Anything
        // outside `0..=MAX_NAK_RANGE_SPAN` is dropped.
        let span = seq_diff(from, to);
        if !(0..=MAX_NAK_RANGE_SPAN).contains(&span) {
            return;
        }
        let mut seq = from;
        loop {
            if self.send_buf.contains_key(&seq) {
                // Avoid duplicates in the loss list.
                if !self.loss_list.contains(&seq) {
                    self.loss_list.push_back(seq);
                }
            }
            if seq == to {
                break;
            }
            seq = seq_next(seq);
        }
    }

    // ──────────────────────────── Receiver API ───────────────────────────────

    /// Record an incoming data packet with the given `seq_no`.
    ///
    /// - Detects gaps (missing sequence numbers between the last received and
    ///   this one) and returns a [`Action::SendNak`] for the gap.
    /// - Emits a periodic full [`Action::SendAck`] when `now >= next_ack_at`.
    /// - Does **not** duplicate-detect; the caller is responsible for de-dup if
    ///   needed.
    pub fn on_data(&mut self, seq_no: u32, now: Instant) -> Vec<Action> {
        let seq_no = seq_no & SEQ_MAX;
        let mut actions = Vec::new();

        match self.rcv_seq {
            None => {
                // Very first packet — open the receive window.
                self.rcv_seq = Some(seq_no);
                self.ack_seq = Some(seq_no);
                self.next_ack_at = Some(now + self.ack_interval);
            }
            Some(prev) => {
                let diff = seq_diff(prev, seq_no);
                if diff <= 0 {
                    // Out-of-order or duplicate: accept but don't NAK.
                } else if diff == 1 {
                    // Consecutive — advance rcv_seq.
                    self.rcv_seq = Some(seq_no);
                    // Also advance ack_seq if it is now contiguous with rcv_seq.
                    if self.ack_seq.map_or(true, |a| seq_diff(a, seq_no) == 1) {
                        self.ack_seq = Some(seq_no);
                    }
                } else {
                    // Gap detected: [prev+1, seq_no-1] is missing.
                    let gap_from = seq_next(prev);
                    let gap_to = (seq_no.wrapping_sub(1)) & SEQ_MAX;
                    actions.push(Action::SendNak {
                        from: gap_from,
                        to: gap_to,
                    });
                    self.rcv_seq = Some(seq_no);
                }
            }
        }

        // Periodic full ACK.
        if self.next_ack_at.is_some_and(|t| now >= t) {
            if let Some(ack_seq) = self.ack_seq {
                let ack_id = self.ack_id;
                self.ack_id = self.ack_id.wrapping_add(1);
                self.next_ack_at = Some(now + self.ack_interval);
                actions.push(Action::SendAck {
                    ack_seq_no: seq_next(ack_seq), // next expected
                    ack_id,
                });
            }
        }

        actions
    }

    /// Force emission of a full ACK immediately (useful for end-of-stream and
    /// tests that want to trigger an ACK without waiting for the timer).
    pub fn force_ack(&mut self, now: Instant) -> Option<Action> {
        let ack_seq = self.ack_seq?;
        let ack_id = self.ack_id;
        self.ack_id = self.ack_id.wrapping_add(1);
        self.next_ack_at = Some(now + self.ack_interval);
        Some(Action::SendAck {
            ack_seq_no: seq_next(ack_seq),
            ack_id,
        })
    }

    /// Process an incoming ACKACK — the sender echoes back an ACK id so we can
    /// sample RTT.
    ///
    /// `ackack_id` is the ACK id echoed by the sender.  `now` is the current
    /// instant.  Updates the internal RTT estimator.
    pub fn on_ackack(&mut self, ackack_id: u32, now: Instant) {
        if let Some(pos) = self.pending_acks.iter().position(|p| p.ack_id == ackack_id) {
            let pa = self.pending_acks.remove(pos);
            let rtt_us = u64::try_from(now.duration_since(pa.sent_at).as_micros())
                .unwrap_or(u64::MAX);
            self.rtt.update(rtt_us);
        }
    }

    // ──────────────────────────── Accessors ──────────────────────────────────

    /// The current smoothed RTT estimate.
    #[must_use]
    pub fn rtt(&self) -> Duration {
        self.rtt.rtt()
    }

    /// The current RTT variance.
    #[must_use]
    pub fn rttvar(&self) -> Duration {
        self.rtt.rttvar()
    }

    /// Number of packets still in the send buffer (awaiting ACK).
    #[must_use]
    pub fn send_buffer_len(&self) -> usize {
        self.send_buf.len()
    }

    /// Number of sequence numbers in the loss list.
    #[must_use]
    pub fn loss_list_len(&self) -> usize {
        self.loss_list.len()
    }

    /// Highest sequence number received on the receiver side.
    #[must_use]
    pub fn rcv_seq(&self) -> Option<u32> {
        self.rcv_seq
    }

    /// Last contiguously acknowledged sequence number.
    #[must_use]
    pub fn ack_seq(&self) -> Option<u32> {
        self.ack_seq
    }

    /// The last-used ACK ID (monotonic counter).
    #[must_use]
    pub fn last_ack_id(&self) -> u32 {
        self.ack_id.wrapping_sub(1)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // ──────────── Sequence number arithmetic ─────────────

    #[test]
    fn seq_next_wraps_at_seq_max() {
        assert_eq!(seq_next(SEQ_MAX), 0, "SEQ_MAX + 1 wraps to 0");
        assert_eq!(seq_next(0), 1);
        assert_eq!(seq_next(5), 6);
    }

    #[test]
    fn seq_lt_normal_ordering() {
        assert!(seq_lt(0, 1));
        assert!(seq_lt(100, 200));
        assert!(!seq_lt(200, 100));
        assert!(!seq_lt(5, 5));
    }

    #[test]
    fn seq_lt_handles_wraparound() {
        // Near the wraparound boundary: SEQ_MAX → 0.
        assert!(seq_lt(SEQ_MAX, 0), "SEQ_MAX < 0 after wrap");
        assert!(seq_lt(SEQ_MAX - 10, SEQ_MAX));
        // 0 comes after SEQ_MAX (it wrapped), so SEQ_MAX - 0 is a large
        // positive forward distance.
        assert!(!seq_lt(0, SEQ_MAX), "0 > SEQ_MAX in the forward direction");
    }

    #[test]
    fn seq_diff_normal_and_wraparound() {
        assert_eq!(seq_diff(0, 5), 5);
        assert_eq!(seq_diff(5, 0), -5);
        // Wraparound: SEQ_MAX + 1 = 0, so diff(SEQ_MAX, 0) = +1.
        assert_eq!(seq_diff(SEQ_MAX, 0), 1, "wraparound diff is +1");
        assert_eq!(seq_diff(0, SEQ_MAX), -1, "reverse wraparound diff is -1");
    }

    // ──────────── Sender: enqueue + ACK advances window ─────────────

    #[test]
    fn enqueue_assigns_sequential_seq_nos_and_buffers() {
        let mut state = ReliabilityState::new(10);
        let a1 = state.enqueue(b"pkt0".to_vec());
        let a2 = state.enqueue(b"pkt1".to_vec());
        assert!(matches!(a1, Action::SendData { seq_no: 10, is_retransmit: false, .. }));
        assert!(matches!(a2, Action::SendData { seq_no: 11, is_retransmit: false, .. }));
        assert_eq!(state.send_buffer_len(), 2);
    }

    #[test]
    fn on_ack_advances_send_window_and_frees_buffer() {
        let now = Instant::now();
        let mut state = ReliabilityState::new(0);
        state.enqueue(b"pkt0".to_vec()); // seq 0
        state.enqueue(b"pkt1".to_vec()); // seq 1
        state.enqueue(b"pkt2".to_vec()); // seq 2
        assert_eq!(state.send_buffer_len(), 3);

        // ACK for seq 2 (ack_no = 2 means seqs 0 and 1 are acked).
        state.on_ack(2, 1, now);
        assert_eq!(state.send_buffer_len(), 1, "seqs 0 and 1 freed; seq 2 still buffered");
    }

    #[test]
    fn on_ack_returns_ackack() {
        let now = Instant::now();
        let mut state = ReliabilityState::new(0);
        state.enqueue(b"x".to_vec());
        let action = state.on_ack(1, 42, now);
        assert_eq!(action, Some(Action::SendAckAck { ack_id: 42 }));
    }

    // ──────────── Sender: NAK → retransmit ─────────────

    #[test]
    fn on_nak_adds_to_loss_list_and_drain_retransmits() {
        let mut state = ReliabilityState::new(0);
        state.enqueue(b"pkt0".to_vec()); // seq 0
        state.enqueue(b"pkt1".to_vec()); // seq 1
        state.enqueue(b"pkt2".to_vec()); // seq 2

        // Receiver reports seq 0 and 1 as lost.
        state.on_nak(0, 1);
        assert_eq!(state.loss_list_len(), 2);

        let retx = state.drain_retransmits();
        assert_eq!(retx.len(), 2, "two packets retransmitted");
        assert!(matches!(&retx[0], Action::SendData { seq_no: 0, is_retransmit: true, .. }));
        assert!(matches!(&retx[1], Action::SendData { seq_no: 1, is_retransmit: true, .. }));
        assert_eq!(state.loss_list_len(), 0, "loss list drained after retransmit");
    }

    #[test]
    fn on_nak_skips_already_acked_packets() {
        let now = Instant::now();
        let mut state = ReliabilityState::new(0);
        state.enqueue(b"pkt0".to_vec()); // seq 0
        state.enqueue(b"pkt1".to_vec()); // seq 1

        // ACK seq 0 (ack_no=1 → seq 0 freed).
        state.on_ack(1, 1, now);
        assert_eq!(state.send_buffer_len(), 1);

        // Now NAK both 0 and 1: seq 0 is no longer in the buffer.
        state.on_nak(0, 1);
        let retx = state.drain_retransmits();
        assert_eq!(retx.len(), 1, "only seq 1 retransmitted; seq 0 was already acked");
        assert!(matches!(&retx[0], Action::SendData { seq_no: 1, .. }));
    }

    #[test]
    fn on_nak_no_duplicates_in_loss_list() {
        let mut state = ReliabilityState::new(0);
        state.enqueue(b"x".to_vec()); // seq 0
        state.on_nak(0, 0);
        state.on_nak(0, 0); // duplicate NAK
        assert_eq!(state.loss_list_len(), 1, "no duplicate entries in loss list");
    }

    #[test]
    fn on_nak_ignores_inverted_range_without_walking_whole_seq_space() {
        // A malformed/hostile NAK with `to` *behind* `from` in the forward
        // direction must be rejected outright — never walked (which would take
        // ~2^31 iterations and hang the receiver). Because this test completes
        // promptly, it also acts as a liveness guard against the bound
        // regressing.
        let mut state = ReliabilityState::new(0);
        for _ in 0..16 {
            state.enqueue(vec![0u8; 4]); // seqs 0..=15 are buffered
        }
        // from=10, to=5: seq_diff(10, 5) = -5 (inverted), so it must be ignored.
        state.on_nak(10, 5);
        assert_eq!(
            state.loss_list_len(),
            0,
            "inverted NAK range must add nothing to the loss list"
        );
    }

    #[test]
    fn on_nak_ignores_oversized_range() {
        // A range spanning more than MAX_NAK_RANGE_SPAN packets is implausible
        // (no legitimate in-flight window is that large) and must be dropped so a
        // single bogus report can't enqueue a colossal walk.
        let mut state = ReliabilityState::new(0);
        state.enqueue(vec![0u8; 4]); // seq 0 buffered
        let over = u32::try_from(MAX_NAK_RANGE_SPAN + 1).unwrap();
        state.on_nak(0, over);
        assert_eq!(
            state.loss_list_len(),
            0,
            "oversized NAK range must be ignored"
        );
    }

    #[test]
    fn on_nak_accepts_max_span_boundary() {
        // Exactly MAX_NAK_RANGE_SPAN is still walked. We buffer only seq 0, so
        // the loss list ends up with just that one entry — proving the walk ran,
        // respected the send buffer, and stayed bounded at the boundary.
        let mut state = ReliabilityState::new(0);
        state.enqueue(vec![0u8; 4]); // only seq 0 is buffered
        let to = u32::try_from(MAX_NAK_RANGE_SPAN).unwrap();
        state.on_nak(0, to);
        assert_eq!(
            state.loss_list_len(),
            1,
            "boundary span is accepted; only the buffered seq 0 is enqueued"
        );
    }

    #[test]
    fn on_nak_single_packet_still_works() {
        // The bound must not regress the common single-packet NAK (from == to).
        let mut state = ReliabilityState::new(0);
        state.enqueue(vec![0u8; 4]); // seq 0
        state.on_nak(0, 0);
        assert_eq!(state.loss_list_len(), 1, "single-packet NAK still retransmits");
    }

    #[test]
    fn on_nak_inverted_range_across_wraparound_is_ignored() {
        // A small `from` with a large `to` near SEQ_MAX has a negative forward
        // distance (the range is inverted/wrapped), so it must be rejected
        // rather than walked the long way round.
        let mut state = ReliabilityState::new(0);
        state.enqueue(vec![0u8; 4]); // seq 0
        state.on_nak(5, SEQ_MAX - 5);
        assert_eq!(
            state.loss_list_len(),
            0,
            "inverted wrapped NAK range must be ignored"
        );
    }

    // ──────────── Receiver: gap detection → NAK ─────────────

    #[test]
    fn receiver_detects_gap_and_emits_nak() {
        let now = Instant::now();
        let mut state = ReliabilityState::new(0);

        // Receive seq 0 (no gap).
        let a0 = state.on_data(0, now);
        assert!(a0.iter().all(|a| !matches!(a, Action::SendNak { .. })), "no NAK for first packet");

        // Skip seq 1 and receive seq 2 → gap [1, 1].
        let a2 = state.on_data(2, now);
        let nak = a2.iter().find(|a| matches!(a, Action::SendNak { .. }));
        assert!(nak.is_some(), "NAK emitted for the gap");
        assert_eq!(nak.unwrap(), &Action::SendNak { from: 1, to: 1 });
    }

    #[test]
    fn receiver_detects_larger_gap() {
        let now = Instant::now();
        let mut state = ReliabilityState::new(0);
        state.on_data(0, now);
        // Jump to seq 5 → gap [1, 4].
        let actions = state.on_data(5, now);
        let nak = actions.iter().find(|a| matches!(a, Action::SendNak { .. }));
        assert_eq!(nak.unwrap(), &Action::SendNak { from: 1, to: 4 });
    }

    #[test]
    fn receiver_no_nak_for_consecutive_packets() {
        let now = Instant::now();
        let mut state = ReliabilityState::new(0);
        for seq in 0..10u32 {
            let actions = state.on_data(seq, now);
            assert!(
                actions.iter().all(|a| !matches!(a, Action::SendNak { .. })),
                "no NAK for consecutive packet {seq}"
            );
        }
    }

    // ──────────── Receiver: periodic ACK ─────────────

    #[test]
    fn receiver_emits_ack_after_interval() {
        let start = Instant::now();
        let mut state = ReliabilityState::new(0);
        state.set_ack_interval(Duration::from_millis(10));

        // Receive seq 0: sets the timer but timer hasn't fired yet.
        state.on_data(0, start);

        // Simulate 15 ms passing.
        let later = start + Duration::from_millis(15);
        let actions = state.on_data(1, later);
        let ack = actions.iter().find(|a| matches!(a, Action::SendAck { .. }));
        assert!(ack.is_some(), "ACK emitted after the interval");
    }

    #[test]
    fn force_ack_returns_ack_immediately() {
        let now = Instant::now();
        let mut state = ReliabilityState::new(0);
        state.on_data(0, now);
        state.on_data(1, now);
        let action = state.force_ack(now);
        assert!(action.is_some(), "force_ack must return an ACK");
        assert!(matches!(action.unwrap(), Action::SendAck { ack_seq_no: 2, .. }));
    }

    // ──────────── RTT estimation from ACK/ACKACK timing ─────────────

    #[test]
    fn rtt_estimated_from_ack_ackack_round_trip() {
        let start = Instant::now();
        let mut state = ReliabilityState::new(0);
        state.on_data(0, start);
        // Force an ACK.
        let ack = state.force_ack(start).unwrap();
        let Action::SendAck { ack_id, .. } = ack else {
            panic!("expected SendAck");
        };
        // on_ack records the pending ACK for RTT sampling.
        state.on_ack(1, ack_id, start);

        // 50 ms later the ACKACK arrives.
        let ackack_time = start + Duration::from_millis(50);
        state.on_ackack(ack_id, ackack_time);

        // RTT should be updated — it should now reflect roughly 50 ms.
        // The initial estimate is 100 ms; after one 50 ms sample:
        // rtt_new = (100_000 * 7 + 50_000) / 8 = 93_750 µs ≈ 93.75 ms.
        let rtt = state.rtt();
        assert!(
            rtt < Duration::from_millis(100),
            "RTT should decrease towards 50 ms after a 50 ms sample; got {rtt:?}"
        );
    }

    #[test]
    fn rtt_estimator_update_is_weighted() {
        let mut est = RttEstimator::default();
        // Default is 100 ms.
        assert_eq!(est.rtt(), Duration::from_micros(100_000));
        est.update(0); // sample of 0 µs
        // rtt_new = (100_000 * 7 + 0) / 8 = 87_500
        assert_eq!(est.rtt(), Duration::from_micros(87_500));
    }

    // ──────────── Sequence wraparound in the send/receive path ─────────────

    #[test]
    fn seq_wraparound_in_enqueue() {
        // Start at SEQ_MAX - 1: the 3rd packet should wrap around to seq 0.
        let mut state = ReliabilityState::new(SEQ_MAX - 1);
        let a0 = state.enqueue(b"a".to_vec());
        let a1 = state.enqueue(b"b".to_vec());
        let a2 = state.enqueue(b"c".to_vec());
        assert!(matches!(a0, Action::SendData { seq_no, .. } if seq_no == SEQ_MAX - 1));
        assert!(matches!(a1, Action::SendData { seq_no, .. } if seq_no == SEQ_MAX));
        assert!(matches!(a2, Action::SendData { seq_no: 0, .. }), "wrapped to 0");
    }

    #[test]
    fn ack_frees_wrapped_seqs() {
        let now = Instant::now();
        // Start at SEQ_MAX - 1 and enqueue 3 packets (wrapping to 0).
        let mut state = ReliabilityState::new(SEQ_MAX - 1);
        state.enqueue(b"a".to_vec()); // SEQ_MAX - 1
        state.enqueue(b"b".to_vec()); // SEQ_MAX
        state.enqueue(b"c".to_vec()); // 0
        assert_eq!(state.send_buffer_len(), 3);

        // ACK for seq 0 (exclusive), meaning SEQ_MAX-1 and SEQ_MAX are acked.
        state.on_ack(0, 1, now);
        assert_eq!(
            state.send_buffer_len(),
            1,
            "SEQ_MAX-1 and SEQ_MAX should be freed; seq 0 still buffered"
        );
    }

    #[test]
    fn receiver_detects_gap_near_wraparound() {
        let now = Instant::now();
        let mut state = ReliabilityState::new(0);
        state.on_data(SEQ_MAX - 1, now);
        // Skip SEQ_MAX, receive 0 — gap is [SEQ_MAX, SEQ_MAX].
        let actions = state.on_data(0, now);
        let nak = actions.iter().find(|a| matches!(a, Action::SendNak { .. }));
        assert!(nak.is_some(), "NAK emitted across the wraparound boundary");
        assert_eq!(
            nak.unwrap(),
            &Action::SendNak {
                from: SEQ_MAX,
                to: SEQ_MAX
            }
        );
    }

    // ──────────── Loss list / send buffer interaction ─────────────

    #[test]
    fn ack_prunes_loss_list() {
        let now = Instant::now();
        let mut state = ReliabilityState::new(0);
        state.enqueue(b"p0".to_vec()); // seq 0
        state.enqueue(b"p1".to_vec()); // seq 1
        state.enqueue(b"p2".to_vec()); // seq 2
        // NAK all three.
        state.on_nak(0, 2);
        assert_eq!(state.loss_list_len(), 3);
        // ACK up to seq 2 (seq 0 and 1 freed).
        state.on_ack(2, 1, now);
        assert_eq!(
            state.loss_list_len(),
            1,
            "only seq 2 remains in the loss list"
        );
    }
}
