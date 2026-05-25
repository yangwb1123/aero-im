//! Ingest RTP reorder / jitter buffer.
//!
//! Sits between raw inbound RTP and the [`H264Depacketizer`](crate::depacketize::H264Depacketizer).
//! Collects arriving packets (which UDP may deliver out of order) and releases
//! them in strict sequence-number order, subject to a bounded jitter window.
//!
//! ## Design
//!
//! A small `BTreeMap` keyed by a *normalized* sequence number holds packets
//! that have arrived but not yet been released. On every `push` the buffer:
//!
//! 1. Drops the packet if it is a **duplicate** (seq already seen or drained).
//! 2. Drops the packet if it is **too old** — i.e. its sequence number is
//!    earlier than `next_expected` adjusted for wrap-around.
//! 3. Otherwise inserts it. Then the buffer drains all contiguous packets
//!    starting at `next_expected` into the caller-supplied drain callback.
//!
//! Packets that are received but not contiguous (i.e. there is a gap) remain
//! buffered until either:
//! - The missing predecessor arrives (gap is filled), **or**
//! - A newer packet arrives that is *more than `window` sequence numbers ahead*
//!   of `next_expected` — the buffer then **skips** the missing packet(s) and
//!   drains what it can.  Skipped sequence numbers are flagged as lost.
//!
//! ## Sequence-number wraparound
//!
//! RTP sequence numbers are 16-bit. The buffer uses *serial arithmetic* (RFC
//! 1982) throughout: two sequence numbers are compared by checking whether their
//! unsigned 16-bit difference (mod 2^16) is less than 2^15.

use std::collections::BTreeMap;

// ---- Serial arithmetic helpers ----
//
// All helpers operate on `u16` and use the RFC 1982 half-space convention:
// a sequence number `a` is "before" `b` if `(b - a) mod 2^16 < 2^15`.

/// True if sequence number `a` is before or equal to `b`.
#[inline]
fn seq_before_or_eq(a: u16, b: u16) -> bool {
    b.wrapping_sub(a) <= 0x8000
}

/// Number of sequence numbers from `a` to `b` (mod 2^16, serial arithmetic).
/// Returns 0 when `a == b`.
#[inline]
fn seq_distance(a: u16, b: u16) -> u16 {
    b.wrapping_sub(a)
}

// ---- ReorderBuffer ----

/// An ingest RTP reorder / jitter buffer.
///
/// Generic over `T` — the caller may store any packet wrapper. The sequence
/// number is supplied separately to [`push`](Self::push).
///
/// Construct with [`ReorderBuffer::new`] (starts at seq 0) or
/// [`ReorderBuffer::with_start`] (starts at a specific sequence number).
#[derive(Debug)]
pub struct ReorderBuffer<T> {
    /// Packets waiting to be drained, keyed by a *normalized* sequence number
    /// (the raw `u16` offset from `base`, extended to `u32` to handle wrap).
    inner: BTreeMap<u32, T>,
    /// The sequence number of the next packet we want to drain.
    next_expected: u16,
    /// How many sequence numbers beyond `next_expected` we hold before
    /// declaring a packet lost and skipping over it.
    window: u16,
    /// Anchor for normalizing sequence numbers to avoid `BTreeMap` ordering
    /// glitches across the 65535→0 wrap.
    base: u16,
}

impl<T> ReorderBuffer<T> {
    /// Create a new reorder buffer expecting the stream to start at sequence
    /// number `0`.
    ///
    /// `window` is the maximum number of sequence-number slots we buffer
    /// before declaring a missing packet lost and skipping it. 64 is a
    /// reasonable default for typical network jitter.
    #[must_use]
    pub fn new(window: u16) -> Self {
        Self::with_start(0, window)
    }

    /// Create a new reorder buffer expecting the stream to start at `start_seq`.
    ///
    /// Packets whose sequence numbers precede `start_seq` (in serial arithmetic)
    /// are silently dropped as "too old".
    #[must_use]
    pub fn with_start(start_seq: u16, window: u16) -> Self {
        Self {
            inner: BTreeMap::new(),
            next_expected: start_seq,
            window,
            base: start_seq,
        }
    }

    /// Push a packet with the given RTP `seq` number into the buffer.
    ///
    /// `drain` is called synchronously (zero or more times) with each packet
    /// that is now ready to be released in order. Packets are drained in
    /// ascending sequence-number order; the `Option<T>` is `None` when the
    /// sequence number was **skipped** (declared lost because the jitter window
    /// was exceeded).
    ///
    /// Returns `false` (and does **not** call `drain`) if the packet is a
    /// duplicate or arrives behind the current drain cursor.
    pub fn push<F>(&mut self, seq: u16, packet: T, drain: &mut F) -> bool
    where
        F: FnMut(u16, Option<T>),
    {
        // Reject packets that precede next_expected (too old or duplicate).
        if !seq_before_or_eq(self.next_expected, seq) {
            return false;
        }
        // Reject in-buffer duplicates.
        if self.inner.contains_key(&self.normalize(seq)) {
            return false;
        }

        self.inner.insert(self.normalize(seq), packet);

        // Drain contiguous packets starting at next_expected.
        self.drain_ready(drain);

        // If the buffer now holds a packet that is more than `window` positions
        // ahead of `next_expected`, declare the missing packet(s) lost and skip.
        self.drain_skipping_lost(drain);

        true
    }

    /// Flush all buffered packets (releasing them in order, marking any gaps as
    /// lost). Call at end-of-stream or on a major discontinuity.
    pub fn flush<F>(&mut self, drain: &mut F)
    where
        F: FnMut(u16, Option<T>),
    {
        while !self.inner.is_empty() {
            self.drain_ready(drain);
            if self.inner.is_empty() {
                break;
            }
            // There is a gap; emit a loss marker for next_expected and advance.
            drain(self.next_expected, None);
            self.next_expected = self.next_expected.wrapping_add(1);
        }
    }

    /// Number of packets currently buffered (not yet drained).
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.inner.len()
    }

    /// The sequence number of the next packet expected to be drained.
    #[must_use]
    pub fn next_expected(&self) -> u16 {
        self.next_expected
    }

    // ---- internals ----

    /// Map a raw 16-bit sequence number to a monotonically increasing `u32`
    /// key anchored at `self.base`. This lets the `BTreeMap` maintain correct
    /// order across the 65535→0 wraparound.
    ///
    /// The signed distance from `base` is computed mod 2^16 (serial
    /// arithmetic), then widened to `u32`.
    #[inline]
    fn normalize(&self, seq: u16) -> u32 {
        u32::from(self.base) + u32::from(seq.wrapping_sub(self.base))
    }

    /// Convert a normalized `u32` key back to the raw 16-bit sequence number.
    #[inline]
    fn denormalize(&self, key: u32) -> u16 {
        // The offset from base, masked back to 16 bits, plus base.
        self.base.wrapping_add(
            u16::try_from(key.wrapping_sub(u32::from(self.base)) & 0xFFFF).unwrap_or(0),
        )
    }

    /// Drain all packets whose normalized key matches the current
    /// `next_expected`, advancing the cursor after each.
    fn drain_ready<F>(&mut self, drain: &mut F)
    where
        F: FnMut(u16, Option<T>),
    {
        loop {
            let key = self.normalize(self.next_expected);
            if let Some(pkt) = self.inner.remove(&key) {
                drain(self.next_expected, Some(pkt));
                self.next_expected = self.next_expected.wrapping_add(1);
            } else {
                break;
            }
        }
    }

    /// If the earliest buffered packet is `window` or more positions ahead of
    /// `next_expected`, declare all the missing positions lost (emit loss
    /// markers) until the buffered packet becomes the next one to drain, then
    /// drain it (and any further contiguous packets).
    fn drain_skipping_lost<F>(&mut self, drain: &mut F)
    where
        F: FnMut(u16, Option<T>),
    {
        // Check whether the earliest buffered packet exceeds the window.
        let trigger_gap = {
            let Some((&first_key, _)) = self.inner.iter().next() else {
                return;
            };
            let first_seq = self.denormalize(first_key);
            seq_distance(self.next_expected, first_seq)
        };
        if trigger_gap < self.window {
            return; // Still within the jitter window — keep waiting.
        }
        // The gap has exceeded the window: skip forward, declaring each
        // missing seq lost, until we reach the earliest buffered packet.
        loop {
            let Some((&first_key, _)) = self.inner.iter().next() else {
                break;
            };
            let first_seq = self.denormalize(first_key);
            if first_seq == self.next_expected {
                // We've caught up — drain the buffered packet (and any that
                // follow contiguously).
                self.drain_ready(drain);
                break;
            }
            // Still a gap — declare next_expected lost and advance.
            drain(self.next_expected, None);
            self.next_expected = self.next_expected.wrapping_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect all drained `(seq, Option<&'static str>)` into a vec.
    fn drain_all(buf: &mut ReorderBuffer<&'static str>) -> Vec<(u16, Option<&'static str>)> {
        let mut out = Vec::new();
        buf.flush(&mut |seq, pkt| out.push((seq, pkt)));
        out
    }

    /// Push a packet and collect everything drained as a result.
    fn push_drain(
        buf: &mut ReorderBuffer<&'static str>,
        seq: u16,
        pkt: &'static str,
    ) -> Vec<(u16, Option<&'static str>)> {
        let mut out = Vec::new();
        buf.push(seq, pkt, &mut |s, p| out.push((s, p)));
        out
    }

    // ---- in-order delivery ----

    #[test]
    fn in_order_packets_drain_immediately() {
        let mut buf: ReorderBuffer<&'static str> = ReorderBuffer::new(16);
        let r0 = push_drain(&mut buf, 0, "p0");
        let r1 = push_drain(&mut buf, 1, "p1");
        let r2 = push_drain(&mut buf, 2, "p2");
        assert_eq!(r0, vec![(0, Some("p0"))]);
        assert_eq!(r1, vec![(1, Some("p1"))]);
        assert_eq!(r2, vec![(2, Some("p2"))]);
        assert_eq!(buf.buffered(), 0);
    }

    // ---- out-of-order delivery ----

    #[test]
    fn out_of_order_held_then_released_in_order() {
        // Start expecting seq 0. Seq 1 arrives first → held. Seq 0 arrives
        // → both drain in order.
        let mut buf: ReorderBuffer<&'static str> = ReorderBuffer::new(16);
        let r1 = push_drain(&mut buf, 1, "p1"); // held — seq 0 not yet seen
        let r0 = push_drain(&mut buf, 0, "p0"); // fills gap → drains 0 then 1
        assert_eq!(r1, vec![], "pkt 1 arrives first — held, no drain");
        assert_eq!(r0, vec![(0, Some("p0")), (1, Some("p1"))]);
        assert_eq!(buf.buffered(), 0);
    }

    #[test]
    fn three_out_of_order_released_correctly() {
        let mut buf: ReorderBuffer<&'static str> = ReorderBuffer::new(16);
        push_drain(&mut buf, 2, "p2");
        push_drain(&mut buf, 0, "p0");
        let r = push_drain(&mut buf, 1, "p1");
        // 0 drained immediately on arrival; 1 fills gap → 1 and 2 also drain.
        assert_eq!(r, vec![(1, Some("p1")), (2, Some("p2"))]);
        assert_eq!(buf.buffered(), 0);
    }

    // ---- duplicate dropping ----

    #[test]
    fn duplicate_is_dropped() {
        let mut buf: ReorderBuffer<&'static str> = ReorderBuffer::new(16);
        let r0 = push_drain(&mut buf, 0, "first");
        assert_eq!(r0, vec![(0, Some("first"))]);

        // Push seq 0 again — already drained, must be rejected.
        let mut drained = Vec::new();
        let accepted = buf.push(0, "dup", &mut |s, p| drained.push((s, p)));
        assert!(!accepted, "duplicate must be rejected");
        assert_eq!(drained, vec![], "no drain on duplicate");
    }

    #[test]
    fn duplicate_in_buffer_is_dropped() {
        let mut buf: ReorderBuffer<&'static str> = ReorderBuffer::new(16);
        // Seq 1 arrives before seq 0 → buffered.
        let r1a = push_drain(&mut buf, 1, "p1-first");
        assert_eq!(r1a, vec![], "seq 1 buffered (seq 0 not yet seen)");
        // Push seq 1 again while it is still in the buffer.
        let mut drained = Vec::new();
        let accepted = buf.push(1, "p1-dup", &mut |s, p| drained.push((s, p)));
        assert!(!accepted, "duplicate in-buffer must be rejected");
        assert_eq!(drained, vec![]);
        assert_eq!(buf.buffered(), 1, "original still there");
    }

    // ---- window / lost packet ----

    #[test]
    fn packet_past_window_causes_skip_and_loss_marker() {
        // Window = 4. Seq 0 arrives, then seq 5 (gap of 5 > window=4).
        // next_expected becomes 1 after draining 0; seq 5 is 4 positions ahead
        // of seq 1, which exceeds the window → packets 1..=4 declared lost.
        let mut buf: ReorderBuffer<&'static str> = ReorderBuffer::new(4);
        push_drain(&mut buf, 0, "p0"); // drains immediately
        let mut drained = Vec::new();
        buf.push(5, "p5", &mut |s, p| drained.push((s, p)));

        // Expect 4 loss markers (seqs 1..=4) then seq 5.
        assert_eq!(drained.len(), 5, "4 lost + 1 received: {drained:?}");
        assert_eq!(drained[0], (1, None));
        assert_eq!(drained[1], (2, None));
        assert_eq!(drained[2], (3, None));
        assert_eq!(drained[3], (4, None));
        assert_eq!(drained[4], (5, Some("p5")));
    }

    #[test]
    fn packet_within_window_stays_buffered() {
        // Window = 8. seq 3 arrives before seq 0..2. It is within the window
        // so it must stay buffered, not cause a skip.
        let mut buf: ReorderBuffer<&'static str> = ReorderBuffer::new(8);
        let r = push_drain(&mut buf, 3, "p3");
        assert_eq!(r, vec![], "seq 3 buffered; gap is ≤ window");
        assert_eq!(buf.buffered(), 1);
    }

    // ---- sequence number wraparound ----

    #[test]
    fn wraparound_in_order() {
        let start = u16::MAX - 2;
        let mut buf: ReorderBuffer<&'static str> = ReorderBuffer::with_start(start, 16);
        push_drain(&mut buf, start, "a");
        push_drain(&mut buf, start.wrapping_add(1), "b");
        push_drain(&mut buf, start.wrapping_add(2), "c"); // wraps to 65535
        let r = push_drain(&mut buf, 0, "d"); // wraps past 65535
        assert_eq!(r, vec![(0, Some("d"))]);
        assert_eq!(buf.buffered(), 0);
    }

    #[test]
    fn wraparound_out_of_order() {
        // Stream starts at 65534. 65535 arrives first, then 65534 fills the gap.
        let start = u16::MAX - 1; // 65534
        let mut buf: ReorderBuffer<&'static str> = ReorderBuffer::with_start(start, 16);
        let rmax = push_drain(&mut buf, u16::MAX, "last");    // buffered
        let rprev = push_drain(&mut buf, u16::MAX - 1, "prev"); // fills gap
        assert_eq!(rmax, vec![], "65535 buffered; 65534 not yet seen");
        // 65534 fills the gap → both drain in order.
        assert_eq!(rprev[0], (u16::MAX - 1, Some("prev")));
        assert_eq!(rprev[1], (u16::MAX, Some("last")));
        assert_eq!(buf.buffered(), 0);
    }

    #[test]
    fn wraparound_lost_packet_triggers_skip() {
        // Window = 4; stream starts at 65532.
        // 65532 drains immediately (next_expected → 65533).
        // 65538 (= 65533 + 5) arrives → gap of 5 > window=4 → seqs 65533..65537 lost.
        let base: u16 = u16::MAX - 3; // 65532
        let mut buf: ReorderBuffer<&'static str> = ReorderBuffer::with_start(base, 4);
        push_drain(&mut buf, base, "p_base");
        let far = base.wrapping_add(5); // 65532 + 5 = 65537? No: 65532+5 = 65537 mod 65536 = 1
        let mut drained = Vec::new();
        buf.push(far, "p_far", &mut |s, p| drained.push((s, p)));
        // 4 loss markers then far.
        assert_eq!(drained.len(), 5, "{drained:?}");
        let last = drained.last().unwrap();
        assert_eq!(last.0, far);
        assert!(last.1.is_some());
    }

    // ---- flush ----

    #[test]
    fn flush_releases_buffered_with_loss_markers_for_gaps() {
        // Seq 0 and 2 arrive; 1 is never delivered. flush() must release:
        //   seq 0 (already drained inline), seq 1 (lost), seq 2 (received).
        let mut buf: ReorderBuffer<&'static str> = ReorderBuffer::new(16);
        push_drain(&mut buf, 0, "p0");
        push_drain(&mut buf, 2, "p2");
        assert_eq!(buf.buffered(), 1, "seq 2 still held");
        let out = drain_all(&mut buf);
        // seq 0 was already drained inline; flush picks up seq 1 (lost) + seq 2.
        assert_eq!(out, vec![(1, None), (2, Some("p2"))]);
    }

    // ---- with_start constructor ----

    #[test]
    fn with_start_drops_packets_before_start_seq() {
        // A packet arriving before the declared start should be dropped.
        let mut buf: ReorderBuffer<&'static str> = ReorderBuffer::with_start(10, 8);
        let mut drained = Vec::new();
        let accepted = buf.push(5, "old", &mut |s, p| drained.push((s, p)));
        assert!(!accepted, "packet before start_seq must be dropped");
        assert_eq!(drained, vec![]);
    }

    #[test]
    fn with_start_accepts_first_packet_at_start_seq() {
        let mut buf: ReorderBuffer<&'static str> = ReorderBuffer::with_start(100, 8);
        let r = push_drain(&mut buf, 100, "p100");
        assert_eq!(r, vec![(100, Some("p100"))]);
    }
}
