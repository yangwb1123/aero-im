//! Receive-side reorder buffer for the SRT data path (ROADMAP 方向五).
//!
//! SRT can deliver data packets out of sequence — retransmits land after the
//! packets that followed the loss, and multipath/jitter can swap neighbours. The
//! [`MpegTsSegmenter`](crate::segmenter::MpegTsSegmenter) needs byte-stream
//! continuity, so payloads must reach it **in sequence order**. This buffer
//! reorders by 31-bit SRT sequence number:
//!
//! * an in-order packet is released immediately (zero added latency);
//! * a packet ahead of the expected sequence is held until the gap before it
//!   fills, then the whole now-contiguous run is released at once;
//! * an old/duplicate sequence (e.g. a retransmit for a gap already given up on)
//!   is dropped;
//! * **bounded**: once more than `max_depth` packets are held, the lowest
//!   buffered sequence is force-released — so a permanently-lost packet (one a
//!   NAK never recovers) can never stall the stream indefinitely.
//!
//! Sequence comparison is wrap-aware via [`seq_lt`]/[`seq_next`] so the buffer is
//! correct across the 31-bit wraparound.

use std::collections::BTreeMap;

use crate::reliability::{seq_lt, seq_next};

/// Default cap on out-of-order packets held before the lowest is force-released.
/// Bounds memory and worst-case added latency when a packet is permanently lost.
pub const DEFAULT_MAX_DEPTH: usize = 128;

/// Reorders SRT data-packet payloads into sequence order before the segmenter.
#[derive(Debug)]
pub struct ReorderBuffer {
    /// Next sequence number expected in order; `None` until the first packet
    /// seeds it.
    next: Option<u32>,
    /// Out-of-order packets held until the gap before them fills, keyed by seq.
    pending: BTreeMap<u32, Vec<u8>>,
    /// Force-release threshold (held packets).
    max_depth: usize,
}

impl Default for ReorderBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_DEPTH)
    }
}

impl ReorderBuffer {
    /// Create a buffer that force-releases once more than `max_depth` packets are
    /// held (`max_depth` is floored at 1).
    #[must_use]
    pub fn new(max_depth: usize) -> Self {
        Self {
            next: None,
            pending: BTreeMap::new(),
            max_depth: max_depth.max(1),
        }
    }

    /// Accept a data packet; return the payloads now releasable **in order**:
    /// empty when the packet opens or extends a gap, one for an in-order packet,
    /// several when it closes a gap. A duplicate / already-released sequence
    /// number yields an empty vec (dropped).
    pub fn accept(&mut self, seq: u32, payload: Vec<u8>) -> Vec<Vec<u8>> {
        let Some(next) = self.next else {
            // First packet seeds the sequence; release it and anything contiguous
            // that somehow arrived first.
            self.next = Some(seq_next(seq));
            let mut out = vec![payload];
            self.drain_contiguous(&mut out);
            return out;
        };
        if seq == next {
            self.next = Some(seq_next(next));
            let mut out = vec![payload];
            self.drain_contiguous(&mut out);
            out
        } else if seq_lt(seq, next) {
            // Old or duplicate (e.g. a retransmit for a gap already skipped) — drop.
            Vec::new()
        } else {
            // Ahead of `next`: a gap. Hold until it fills (or we overflow).
            self.pending.insert(seq, payload);
            if self.pending.len() > self.max_depth {
                self.force_release()
            } else {
                Vec::new()
            }
        }
    }

    /// Release pending packets that are now contiguous with `next`.
    fn drain_contiguous(&mut self, out: &mut Vec<Vec<u8>>) {
        while let Some(next) = self.next {
            if let Some(p) = self.pending.remove(&next) {
                out.push(p);
                self.next = Some(seq_next(next));
            } else {
                break;
            }
        }
    }

    /// Overflow: give up on the still-missing packets before the lowest buffered
    /// sequence (wrap-aware), jump `next` to it, and release the contiguous run.
    fn force_release(&mut self) -> Vec<Vec<u8>> {
        let Some(&lowest) = self.pending.keys().min_by(|a, b| {
            if seq_lt(**a, **b) {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        }) else {
            return Vec::new();
        };
        self.next = Some(lowest);
        let mut out = Vec::new();
        self.drain_contiguous(&mut out);
        out
    }

    /// Number of out-of-order packets currently held (diagnostics / tests).
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::ReorderBuffer;

    fn p(n: u8) -> Vec<u8> {
        vec![n]
    }

    #[test]
    fn in_order_packets_pass_through_immediately() {
        let mut b = ReorderBuffer::new(8);
        assert_eq!(b.accept(0, p(0)), vec![p(0)]);
        assert_eq!(b.accept(1, p(1)), vec![p(1)]);
        assert_eq!(b.accept(2, p(2)), vec![p(2)]);
        assert_eq!(b.pending_len(), 0);
    }

    #[test]
    fn out_of_order_is_buffered_then_released_when_gap_fills() {
        let mut b = ReorderBuffer::new(8);
        assert_eq!(b.accept(0, p(0)), vec![p(0)]);
        // 2 arrives before 1 → held.
        assert_eq!(b.accept(2, p(2)), Vec::<Vec<u8>>::new());
        assert_eq!(b.pending_len(), 1);
        // 1 arrives → releases 1 AND the buffered 2, in order.
        assert_eq!(b.accept(1, p(1)), vec![p(1), p(2)]);
        assert_eq!(b.pending_len(), 0);
    }

    #[test]
    fn duplicate_and_old_sequences_are_dropped() {
        let mut b = ReorderBuffer::new(8);
        assert_eq!(b.accept(0, p(0)), vec![p(0)]);
        assert_eq!(b.accept(1, p(1)), vec![p(1)]);
        // Re-delivery of 0 and 1 (already released) → dropped.
        assert_eq!(b.accept(0, p(9)), Vec::<Vec<u8>>::new());
        assert_eq!(b.accept(1, p(9)), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn overflow_force_releases_lowest_and_does_not_stall() {
        // Depth 3: packet 1 is permanently lost; 2,3,4,5 arrive.
        let mut b = ReorderBuffer::new(3);
        assert_eq!(b.accept(0, p(0)), vec![p(0)]); // next = 1
        assert_eq!(b.accept(2, p(2)), Vec::<Vec<u8>>::new());
        assert_eq!(b.accept(3, p(3)), Vec::<Vec<u8>>::new());
        assert_eq!(b.accept(4, p(4)), Vec::<Vec<u8>>::new());
        assert_eq!(b.pending_len(), 3);
        // 5th held packet exceeds depth 3 → force-release the lowest run (2,3,4,5),
        // skipping the lost 1 rather than stalling forever.
        assert_eq!(b.accept(5, p(5)), vec![p(2), p(3), p(4), p(5)]);
        assert_eq!(b.pending_len(), 0);
    }

    #[test]
    fn first_packet_seeds_from_any_sequence() {
        // The stream needn't start at 0; the first packet seen seeds `next`.
        let mut b = ReorderBuffer::new(8);
        assert_eq!(b.accept(100, p(1)), vec![p(1)]);
        assert_eq!(b.accept(101, p(2)), vec![p(2)]);
        assert_eq!(b.accept(103, p(4)), Vec::<Vec<u8>>::new()); // gap at 102
        assert_eq!(b.accept(102, p(3)), vec![p(3), p(4)]);
    }
}
