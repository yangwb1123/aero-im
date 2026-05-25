//! RTCP PLI / FIR feedback — parse, generate, route, and coalesce.
//!
//! ## Responsibilities
//!
//! 1. **Parse** raw RTCP packets from a subscriber's inbound datagram stream
//!    and identify PLI (Payload Loss Indication, FMT=1, PT=206) and FIR (Full
//!    Intra Request, FMT=4, PT=206).
//! 2. **Generate** outbound PLI/FIR bytes that can be sent toward a publisher
//!    to request a fresh keyframe.
//! 3. **Coalesce** bursts: once a keyframe has been requested for a given
//!    `(publisher, mid)` pair, suppress duplicate requests arriving within a
//!    configurable window (default: 200 ms). This prevents the publisher from
//!    being flooded when multiple subscribers send PLIs simultaneously.
//! 4. **Route** correctly: a subscriber PLI on `sub_mid` is translated to a
//!    request on the *publisher's* `pub_mid` for the correct SSRC.
//!
//! ## Wire format constants (RFC 4585)
//!
//! ```text
//! RTCP header (4 bytes):
//!   byte 0: V(2) P(1) FMT(5)
//!   byte 1: PT (payload type: 206 = PSFB)
//!   bytes 2-3: length in 32-bit words - 1
//!
//! PLI  (FMT=1, PT=206): 4-byte header + 4-byte sender SSRC + 4-byte media SSRC = 12 bytes
//! FIR  (FMT=4, PT=206): 4-byte header + 4 sender + 4 media(0) + N×8 FIR entries
//! ```
//!
//! ## Integration with the SFU
//!
//! The [`KeyframeGate`] is the central stateful object. The SFU calls:
//!
//! - [`KeyframeGate::on_subscriber_pli`] / [`KeyframeGate::on_subscriber_fir`]
//!   when a subscriber's RTCP is parsed.
//! - [`KeyframeGate::new_subscriber`] when a subscriber joins a track (always
//!   needs a keyframe to start decoding).
//! - [`KeyframeGate::on_layer_switch`] when simulcast layer selection requests
//!   a switch.
//! - [`KeyframeGate::poll_requests`] to drain pending keyframe requests that
//!   the SFU should relay to publishers.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

pub use str0m::rtp::rtcp::{FirEntry, Pli};
use str0m::rtp::rtcp::Fir;
pub use str0m::rtp::Ssrc;

use aero_common::ParticipantId;

// ── Constants ─────────────────────────────────────────────────────────────────

/// After a keyframe is requested for a `(publisher, mid)` pair, suppress
/// further requests for this window. 200 ms is a common SFU practice.
pub const COALESCE_WINDOW: Duration = Duration::from_millis(200);

/// RTCP PT for Payload-Specific Feedback (RFC 4585 §6.1).
const PT_PSFB: u8 = 206;
/// FMT field value for PLI within PSFB.
const FMT_PLI: u8 = 1;
/// FMT field value for FIR within PSFB.
const FMT_FIR: u8 = 4;

// ── RTCP serialization helpers ────────────────────────────────────────────────

/// Encode a PLI packet into `buf`. Returns the number of bytes written (12).
///
/// Wire layout (RFC 4585 §6.3.1):
/// ```text
/// 0x81 0xCE 0x00 0x02  [V=2, P=0, FMT=1, PT=206, length=2]
/// <sender_ssrc 4 bytes big-endian>
/// <media_ssrc  4 bytes big-endian>
/// ```
///
/// # Panics
/// Panics if `buf` is shorter than 12 bytes.
pub fn encode_pli(sender_ssrc: Ssrc, media_ssrc: Ssrc, buf: &mut [u8]) -> usize {
    // Header: V=2(0x80) | P=0 | FMT=1 → byte0=0x81; PT=206=0xCE; length-words-1=2
    buf[0] = 0x81;
    buf[1] = PT_PSFB;
    buf[2] = 0x00;
    buf[3] = 0x02;
    buf[4..8].copy_from_slice(&sender_ssrc.to_be_bytes());
    buf[8..12].copy_from_slice(&media_ssrc.to_be_bytes());
    12
}

/// Encode a single-entry FIR packet into `buf`. Returns bytes written (20).
///
/// Wire layout (RFC 5104 §4.3.1):
/// ```text
/// 0x84 0xCE 0x00 0x04  [V=2, P=0, FMT=4, PT=206, length=4]
/// <sender_ssrc 4 bytes big-endian>
/// 0x00 0x00 0x00 0x00  [media source SSRC = 0 in FIR]
/// <media_ssrc  4 bytes big-endian>  [FIR entry SSRC]
/// <seq_no> 0x00 0x00 0x00           [seq_no + 3 reserved bytes]
/// ```
///
/// # Panics
/// Panics if `buf` is shorter than 20 bytes.
pub fn encode_fir(sender_ssrc: Ssrc, media_ssrc: Ssrc, seq_no: u8, buf: &mut [u8]) -> usize {
    // Header: V=2 | P=0 | FMT=4 → 0x84; PT=206; length-words-1 = 4 (20 bytes = 5 words)
    buf[0] = 0x84;
    buf[1] = PT_PSFB;
    buf[2] = 0x00;
    buf[3] = 0x04;
    buf[4..8].copy_from_slice(&sender_ssrc.to_be_bytes());
    // media SSRC field in the common feedback header is 0 for FIR
    buf[8..12].copy_from_slice(&0u32.to_be_bytes());
    // FIR entry: SSRC + seq_no + 3 reserved bytes
    buf[12..16].copy_from_slice(&media_ssrc.to_be_bytes());
    buf[16] = seq_no;
    buf[17] = 0;
    buf[18] = 0;
    buf[19] = 0;
    20
}

/// Parse an RTCP PLI from the bytes **after** the 4-byte RTCP header
/// (i.e. the FCI / feedback control information).
pub fn parse_pli(payload: &[u8]) -> Option<Pli> {
    Pli::try_from(payload).ok()
}

/// Parse an RTCP FIR from the bytes **after** the 4-byte RTCP header.
pub fn parse_fir(payload: &[u8]) -> Option<Fir> {
    Fir::try_from(payload).ok()
}

/// Walk a compound RTCP buffer and return every PLI and FIR found.
///
/// This is a minimal compound-packet walker: it reads the length field from
/// each RTCP sub-packet header, slices the appropriate bytes, then delegates
/// to [`parse_pli`] / [`parse_fir`] for the payload-specific feedback packets
/// it recognises (PT=206, FMT=1 or FMT=4). All other sub-packets are skipped.
pub fn parse_keyframe_requests(buf: &[u8]) -> Vec<ParsedFeedback> {
    let mut out = Vec::new();
    let mut remaining = buf;
    loop {
        // Every RTCP packet is at least 4 bytes (the fixed header).
        if remaining.len() < 4 {
            break;
        }
        // RTCP header byte 0: V(2) P(1) FMT(5)
        let version = (remaining[0] & 0b1100_0000) >> 6;
        if version != 2 {
            break; // malformed
        }
        let fmt = remaining[0] & 0b0001_1111;
        let pt = remaining[1];
        // bytes 2-3: length in 32-bit words − 1
        let words_less_one = u16::from_be_bytes([remaining[2], remaining[3]]) as usize;
        let pkt_bytes = (words_less_one + 1) * 4;

        if pkt_bytes > remaining.len() {
            break; // truncated
        }

        let pkt = &remaining[..pkt_bytes];

        if pt == PT_PSFB {
            let fci = &pkt[4..]; // skip 4-byte common header; FCI starts at byte 4
            match fmt {
                FMT_PLI => {
                    if let Some(pli) = parse_pli(fci) {
                        out.push(ParsedFeedback::Pli(pli));
                    }
                }
                FMT_FIR => {
                    if let Some(fir) = parse_fir(fci) {
                        out.push(ParsedFeedback::Fir(Box::new(fir)));
                    }
                }
                _ => {}
            }
        }

        remaining = &remaining[pkt_bytes..];
    }
    out
}

/// A keyframe request parsed from a subscriber's inbound RTCP stream.
///
/// `Fir` is boxed because the `Fir` struct is large (it contains a
/// fixed-size report list) and the `Pli` variant is small.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedFeedback {
    Pli(Pli),
    Fir(Box<Fir>),
}

impl ParsedFeedback {
    /// The media SSRC targeted by this feedback. Returns `None` for a FIR
    /// with an empty report list (malformed packet).
    #[must_use]
    pub fn media_ssrc(&self) -> Option<Ssrc> {
        match self {
            Self::Pli(p) => Some(p.ssrc),
            Self::Fir(f) => f.reports.get(0).map(|e| e.ssrc),
        }
    }

    /// Whether this is a FIR (more severe than PLI).
    #[must_use]
    pub fn is_fir(&self) -> bool {
        matches!(self, Self::Fir(_))
    }
}

// ── Keyframe routing ──────────────────────────────────────────────────────────

/// A keyframe request the SFU must relay to a publisher.
#[derive(Debug, Clone)]
pub struct PendingKeyframeRequest {
    /// Publisher participant.
    pub publisher: ParticipantId,
    /// Publisher's track `mid`.
    pub pub_mid: String,
    /// Whether to use FIR (full intra request) or PLI.
    pub use_fir: bool,
}

/// Per-publisher/mid coalescing state.
#[derive(Debug, Default)]
struct CoalesceState {
    /// When the last request was sent (`None` = never sent).
    last_request: Option<Instant>,
    /// FIR sequence counter (incremented each time a FIR is actually sent).
    fir_seq: u8,
}

impl CoalesceState {
    /// Returns `true` and records a new request if we are past the coalesce
    /// window; returns `false` (suppress) if still within the window.
    fn gate(&mut self, now: Instant) -> bool {
        match self.last_request {
            Some(last) if now.duration_since(last) < COALESCE_WINDOW => false,
            _ => {
                self.last_request = Some(now);
                true
            }
        }
    }
}

/// Central coordinator for keyframe requests in a single SFU call.
///
/// Tracks coalescing state per `(publisher, mid)` and queues
/// [`PendingKeyframeRequest`]s for the SFU to drain via
/// [`KeyframeGate::poll_requests`].
#[derive(Debug, Default)]
pub struct KeyframeGate {
    /// `(publisher, pub_mid) → coalescing state`
    coalesce: HashMap<(ParticipantId, String), CoalesceState>,
    /// Queued requests ready for the SFU to relay to publishers.
    queue: VecDeque<PendingKeyframeRequest>,
}

impl KeyframeGate {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Called when a new subscriber joins a track: always issues a keyframe
    /// request so they can start decoding immediately.
    ///
    /// Bypasses the coalesce gate so the new subscriber always gets a prompt
    /// keyframe even if one was recently sent.
    pub fn new_subscriber(&mut self, publisher: ParticipantId, pub_mid: &str, now: Instant) {
        // Force past the coalesce window by clearing the last-request timestamp.
        let state = self
            .coalesce
            .entry((publisher, pub_mid.to_owned()))
            .or_default();
        state.last_request = None; // force gate open
        self.enqueue_inner(publisher, pub_mid, false, now);
    }

    /// Called when a simulcast layer switch is requested (keyframe needed on
    /// the target layer before the switch can complete).
    pub fn on_layer_switch(&mut self, publisher: ParticipantId, pub_mid: &str, now: Instant) {
        self.enqueue_inner(publisher, pub_mid, false, now);
    }

    /// Called when a subscriber sends a PLI toward a publisher track.
    pub fn on_subscriber_pli(
        &mut self,
        publisher: ParticipantId,
        pub_mid: &str,
        now: Instant,
    ) {
        self.enqueue_inner(publisher, pub_mid, false, now);
    }

    /// Called when a subscriber sends a FIR toward a publisher track.
    pub fn on_subscriber_fir(
        &mut self,
        publisher: ParticipantId,
        pub_mid: &str,
        now: Instant,
    ) {
        self.enqueue_inner(publisher, pub_mid, true, now);
    }

    /// Drain all pending keyframe requests accumulated since the last poll.
    /// Caller should relay each request to the named publisher.
    pub fn poll_requests(&mut self) -> impl Iterator<Item = PendingKeyframeRequest> + '_ {
        self.queue.drain(..)
    }

    /// Number of requests currently queued (not yet drained).
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.queue.len()
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    fn enqueue_inner(
        &mut self,
        publisher: ParticipantId,
        pub_mid: &str,
        use_fir: bool,
        now: Instant,
    ) {
        let state = self
            .coalesce
            .entry((publisher, pub_mid.to_owned()))
            .or_default();
        if !state.gate(now) {
            return; // suppressed within coalesce window
        }
        if use_fir {
            state.fir_seq = state.fir_seq.wrapping_add(1);
        }
        self.queue.push_back(PendingKeyframeRequest {
            publisher,
            pub_mid: pub_mid.to_owned(),
            use_fir,
        });
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Encode / decode round-trip ─────────────────────────────────────────────

    #[test]
    fn pli_encode_decode_round_trip() {
        let sender: Ssrc = 0xAABB_CCDDu32.into();
        let media: Ssrc = 0x1122_3344u32.into();
        let mut buf = [0u8; 12];
        let written = encode_pli(sender, media, &mut buf);
        assert_eq!(written, 12);

        // Verify RTCP header bytes:
        // byte 0: V=2 (0x80) | P=0 | FMT=1 (PLI) = 0x81
        assert_eq!(buf[0], 0x81, "V=2, P=0, FMT=1 (PLI)");
        // byte 1: PT=206 (PayloadSpecificFeedback)
        assert_eq!(buf[1], 206, "PT=206");
        // bytes 2-3: length in words - 1 = 2 (total 3 words = 12 bytes)
        assert_eq!(u16::from_be_bytes([buf[2], buf[3]]), 2, "length words-1 = 2");

        // Parse back the FCI (bytes after the 4-byte common header)
        let parsed = parse_pli(&buf[4..]).expect("PLI parse must succeed");
        assert_eq!(parsed.sender_ssrc, sender);
        assert_eq!(parsed.ssrc, media);
    }

    #[test]
    fn fir_encode_decode_round_trip() {
        let sender: Ssrc = 0xDEAD_BEEFu32.into();
        let media: Ssrc = 0x0102_0304u32.into();
        let seq = 42u8;
        let mut buf = [0u8; 20];
        let written = encode_fir(sender, media, seq, &mut buf);
        assert_eq!(written, 20);

        // byte 0: V=2, P=0, FMT=4 (FIR) → 0x84
        assert_eq!(buf[0], 0x84, "V=2, P=0, FMT=4 (FIR)");
        // byte 1: PT=206
        assert_eq!(buf[1], 206, "PT=206");

        // Parse back
        let parsed = parse_fir(&buf[4..]).expect("FIR parse must succeed");
        assert_eq!(parsed.sender_ssrc, sender);
        let entry = parsed.reports.get(0).expect("FIR must have one entry");
        assert_eq!(entry.ssrc, media);
        assert_eq!(entry.seq_no, seq);
    }

    #[test]
    fn parse_keyframe_requests_extracts_pli_and_fir() {
        // Build a compound RTCP buffer: PLI then FIR.
        let sender: Ssrc = 1u32.into();
        let media: Ssrc = 2u32.into();
        let mut buf = [0u8; 32];
        let n1 = encode_pli(sender, media, &mut buf[..12]);
        assert_eq!(n1, 12);
        let n2 = encode_fir(sender, media, 7, &mut buf[12..]);
        assert_eq!(n2, 20);

        let requests = parse_keyframe_requests(&buf[..n1 + n2]);
        assert_eq!(requests.len(), 2);
        assert!(!requests[0].is_fir(), "first should be PLI");
        assert!(requests[1].is_fir(), "second should be FIR");
    }

    #[test]
    fn parse_keyframe_requests_ignores_non_feedback_packets() {
        // A 4-byte truncated buffer with PT=200 (SenderReport) — not a keyframe req.
        // Build a minimal SR-like header: V=2, P=0, RC=0, PT=200, length=0
        let buf = [0x80u8, 200, 0x00, 0x00];
        let requests = parse_keyframe_requests(&buf);
        assert!(requests.is_empty());
    }

    // ── KeyframeGate: routing + coalescing ────────────────────────────────────

    #[test]
    fn new_subscriber_triggers_keyframe_request() {
        let mut gate = KeyframeGate::new();
        let pub_ = ParticipantId::new();
        let now = Instant::now();
        gate.new_subscriber(pub_, "v0", now);
        let reqs: Vec<_> = gate.poll_requests().collect();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].publisher, pub_);
        assert_eq!(reqs[0].pub_mid, "v0");
        assert!(!reqs[0].use_fir);
    }

    #[test]
    fn duplicate_plis_within_window_are_coalesced() {
        let mut gate = KeyframeGate::new();
        let pub_ = ParticipantId::new();
        let now = Instant::now();

        // First PLI passes gate
        gate.on_subscriber_pli(pub_, "v0", now);
        // Second PLI immediately (within window) — must be suppressed
        gate.on_subscriber_pli(pub_, "v0", now);
        // Third PLI still within window
        gate.on_subscriber_pli(pub_, "v0", now + Duration::from_millis(50));

        let reqs: Vec<_> = gate.poll_requests().collect();
        assert_eq!(reqs.len(), 1, "only first PLI should pass the gate");
    }

    #[test]
    fn pli_after_coalesce_window_passes_gate() {
        let mut gate = KeyframeGate::new();
        let pub_ = ParticipantId::new();
        let now = Instant::now();

        gate.on_subscriber_pli(pub_, "v0", now);
        // Drain queue
        let _ = gate.poll_requests().count();

        // Send PLI after the coalesce window
        gate.on_subscriber_pli(pub_, "v0", now + COALESCE_WINDOW + Duration::from_millis(1));
        let reqs: Vec<_> = gate.poll_requests().collect();
        assert_eq!(reqs.len(), 1, "PLI after window must pass");
    }

    #[test]
    fn subscriber_pli_routed_to_correct_publisher() {
        let mut gate = KeyframeGate::new();
        let pub_a = ParticipantId::new();
        let pub_b = ParticipantId::new();
        let now = Instant::now();

        gate.on_subscriber_pli(pub_a, "v0", now);
        gate.on_subscriber_pli(pub_b, "v1", now);

        let reqs: Vec<_> = gate.poll_requests().collect();
        assert_eq!(reqs.len(), 2);
        assert!(reqs.iter().any(|r| r.publisher == pub_a && r.pub_mid == "v0"));
        assert!(reqs.iter().any(|r| r.publisher == pub_b && r.pub_mid == "v1"));
    }

    #[test]
    fn fir_request_sets_use_fir_flag() {
        let mut gate = KeyframeGate::new();
        let pub_ = ParticipantId::new();
        gate.on_subscriber_fir(pub_, "v0", Instant::now());
        let reqs: Vec<_> = gate.poll_requests().collect();
        assert_eq!(reqs.len(), 1);
        assert!(reqs[0].use_fir, "FIR request must set use_fir=true");
    }

    #[test]
    fn layer_switch_triggers_keyframe_request() {
        let mut gate = KeyframeGate::new();
        let pub_ = ParticipantId::new();
        gate.on_layer_switch(pub_, "v0", Instant::now());
        let reqs: Vec<_> = gate.poll_requests().collect();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].pub_mid, "v0");
    }

    #[test]
    fn new_subscriber_bypasses_coalesce_gate() {
        // Even if a PLI was just sent, a new subscriber must always get a keyframe.
        let mut gate = KeyframeGate::new();
        let pub_ = ParticipantId::new();
        let now = Instant::now();

        // Trigger a PLI to prime the coalesce window
        gate.on_subscriber_pli(pub_, "v0", now);
        let _ = gate.poll_requests().count(); // drain

        // New subscriber joins immediately (still within coalesce window)
        gate.new_subscriber(pub_, "v0", now + Duration::from_millis(10));
        let reqs: Vec<_> = gate.poll_requests().collect();
        assert_eq!(reqs.len(), 1, "new_subscriber must bypass coalesce gate");
    }

    #[test]
    fn poll_requests_is_idempotent_when_empty() {
        let mut gate = KeyframeGate::new();
        assert_eq!(gate.poll_requests().count(), 0);
        assert_eq!(gate.pending_count(), 0);
    }
}
