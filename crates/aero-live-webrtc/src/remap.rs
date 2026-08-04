//! Pure RTP routing + header-remap bookkeeping for the SFU fan-out.
//!
//! This module is deliberately **free of any `str0m` / IO dependency** so the
//! interesting bits — *which* subscriber tracks receive a packet, and *how* the
//! RTP sequence number / timestamp are rewritten per subscriber — are plain
//! arithmetic that can be unit-tested with crafted values.
//!
//! ## Why remap at all?
//!
//! In a selective-forwarding unit the publisher's encoder owns one continuous
//! `(sequence_number, timestamp, ssrc)` space. Each *subscriber* track, however,
//! is its own outbound RTP stream with its own SSRC and must present a
//! **gap-free, monotonic** sequence space to the receiver's de-packetizer — even
//! when the SFU switches the active source (simulcast layer change, active
//! speaker switch) or drops packets for congestion control. [`RtpRemapper`]
//! keeps the bookkeeping: it maps each inbound `(seq, ts)` to a per-subscriber
//! outbound `(seq, ts)` by tracking an offset, transparently handling source
//! switches and 16-bit sequence-number / 32-bit timestamp wraparound.

use std::collections::HashMap;

use aero_common::ParticipantId;

/// A single inbound RTP packet's header values, post-`str0m`-parse, reduced to
/// just the fields the forwarder rewrites. (`str0m` extends the wire `u16`
/// sequence number into a 64-bit [`SeqNo`](str0m::rtp::SeqNo) and the `u32`
/// timestamp into a 64-bit media time to sidestep roll-over; we mirror that by
/// remapping in the extended domain and only truncating at the wire boundary.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpKey {
    /// Extended (roll-over-counted) sequence number from the publisher.
    pub seq: u64,
    /// Extended RTP timestamp in the codec clock rate.
    pub ts: u64,
}

impl RtpKey {
    #[must_use]
    pub fn new(seq: u64, ts: u64) -> Self {
        Self { seq, ts }
    }
}

/// The rewritten header values handed to a single subscriber's outbound stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemappedRtp {
    /// Sequence number in the subscriber's own outbound space.
    pub seq: u64,
    /// Timestamp in the subscriber's own outbound space.
    pub ts: u64,
}

/// Per-subscriber-track remap state for one forwarded source.
///
/// Holds the running offset between the publisher's `(seq, ts)` and the
/// subscriber's outbound `(seq, ts)`. The first packet establishes the offset
/// so the subscriber's stream starts exactly where its previous source left
/// off (or at zero for a brand-new track); subsequent packets carry the same
/// offset, so a contiguous publisher stream stays contiguous downstream.
#[derive(Debug, Clone, Default)]
pub struct RtpRemapper {
    /// `outbound_seq = inbound_seq.wrapping_add(seq_offset)`. Stored in the
    /// unsigned domain so the two's-complement wraparound is explicit and there
    /// are no signed casts; a "negative" shift is simply a large `u64` addend.
    seq_offset: u64,
    /// `outbound_ts = inbound_ts.wrapping_add(ts_offset)` (same scheme).
    ts_offset: u64,
    /// Highest outbound sequence number emitted so far (for source switches).
    last_out_seq: Option<u64>,
    /// Last inbound key seen, used to detect a source switch / discontinuity.
    last_in: Option<RtpKey>,
    /// Whether the offset has been initialised by the first packet.
    primed: bool,
}

impl RtpRemapper {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Remap one inbound packet to this subscriber's outbound space.
    ///
    /// On the very first packet the outbound stream starts at `0`. When the
    /// inbound source switches (detected as a non-monotonic jump relative to the
    /// previously seen inbound key), the offset is recomputed so the outbound
    /// sequence continues one past the last emitted value — keeping the
    /// subscriber's stream gap-free and monotonic across source changes.
    pub fn remap(&mut self, key: RtpKey) -> RemappedRtp {
        if !self.primed {
            // First ever packet: anchor outbound at 0 for both axes, i.e.
            // offset = 0.wrapping_sub(inbound) so inbound + offset == 0.
            self.seq_offset = 0u64.wrapping_sub(key.seq);
            self.ts_offset = 0u64.wrapping_sub(key.ts);
            self.primed = true;
        } else if self.is_source_switch(key) {
            // Continue one past the last emitted outbound sequence number.
            let next_out_seq = self.last_out_seq.map_or(0, |s| s.wrapping_add(1));
            self.seq_offset = next_out_seq.wrapping_sub(key.seq);
            // Keep timestamps moving forward: anchor at the previous outbound
            // timestamp + 1 codec tick so the receiver never sees time go back.
            let prev_out_ts = self
                .last_in
                .map_or(0, |prev| self.apply_ts(prev.ts))
                .wrapping_add(1);
            self.ts_offset = prev_out_ts.wrapping_sub(key.ts);
        }

        let out = RemappedRtp {
            seq: self.apply_seq(key.seq),
            ts: self.apply_ts(key.ts),
        };
        self.last_out_seq = Some(out.seq);
        self.last_in = Some(key);
        out
    }

    fn apply_seq(&self, seq: u64) -> u64 {
        seq.wrapping_add(self.seq_offset)
    }

    fn apply_ts(&self, ts: u64) -> u64 {
        ts.wrapping_add(self.ts_offset)
    }

    /// A source switch is any inbound key that isn't a forward step from the
    /// last one (the new source's sequence space is unrelated to the old one).
    fn is_source_switch(&self, key: RtpKey) -> bool {
        match self.last_in {
            None => false,
            // Same packet repeated (RTX / probe) is not a switch.
            Some(prev) if prev == key => false,
            // A backwards or wildly-forward sequence jump signals a new source.
            // We treat a forward delta of 1..=MAX_FORWARD as "same source".
            Some(prev) => {
                let delta = key.seq.wrapping_sub(prev.seq);
                delta == 0 || delta > MAX_CONTIGUOUS_FORWARD
            }
        }
    }

    /// Highest outbound sequence number emitted, if any (for diagnostics/tests).
    #[must_use]
    pub fn last_out_seq(&self) -> Option<u64> {
        self.last_out_seq
    }
}

/// A forward jump larger than this many packets is assumed to be a new source
/// rather than packet loss within the same source. 16-bit RTP wraps every
/// 65 536 packets; half that window is the usual "is this newer?" threshold.
const MAX_CONTIGUOUS_FORWARD: u64 = 1 << 15;

/// Identifies one outbound subscriber track that should receive a forwarded
/// packet: the subscriber peer plus the `mid` of *their* matching outbound
/// transceiver. (Publisher mid → subscriber mid can differ, which is why this
/// is computed rather than assumed equal.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardTarget {
    pub subscriber: ParticipantId,
    pub out_mid: String,
}

/// Per-call mapping from a published `mid` to each subscriber's outbound `mid`.
///
/// Pure data structure (no IO). The forwarder consults this to know, for an
/// inbound packet on publisher track `mid`, the exact set of outbound tracks
/// (and their remappers) that must receive the rewritten packet.
#[derive(Debug, Default)]
pub struct ForwardTable {
    /// `published_mid -> [(subscriber, out_mid)]`
    routes: HashMap<String, Vec<ForwardTarget>>,
    /// `(subscriber, out_mid) -> remapper`
    remappers: HashMap<(ParticipantId, String), RtpRemapper>,
}

impl ForwardTable {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register that `subscriber` receives publisher track `pub_mid` on their
    /// own outbound transceiver `out_mid`. Idempotent on the `(pub_mid,
    /// subscriber, out_mid)` triple.
    pub fn link(&mut self, pub_mid: &str, subscriber: ParticipantId, out_mid: &str) {
        let target = ForwardTarget {
            subscriber,
            out_mid: out_mid.to_owned(),
        };
        let entry = self.routes.entry(pub_mid.to_owned()).or_default();
        if !entry.contains(&target) {
            entry.push(target);
        }
        self.remappers
            .entry((subscriber, out_mid.to_owned()))
            .or_default();
    }

    /// Remove a subscriber entirely (e.g. they left the call).
    pub fn unlink_subscriber(&mut self, subscriber: ParticipantId) {
        for targets in self.routes.values_mut() {
            targets.retain(|t| t.subscriber != subscriber);
        }
        self.routes.retain(|_, t| !t.is_empty());
        self.remappers.retain(|(sub, _), _| *sub != subscriber);
    }

    /// Remove a published track (e.g. publisher unpublished / left).
    pub fn unlink_publisher_track(&mut self, pub_mid: &str) {
        self.routes.remove(pub_mid);
    }

    /// The outbound targets for an inbound packet on `pub_mid`. Empty slice if
    /// nobody subscribes — the forwarder then drops the packet cheaply.
    #[must_use]
    pub fn targets(&self, pub_mid: &str) -> &[ForwardTarget] {
        self.routes.get(pub_mid).map_or(&[], Vec::as_slice)
    }

    /// Remap an inbound key for a specific outbound target, mutating that
    /// target's per-stream offset state. Returns `None` if the target was never
    /// linked (so the caller can skip the write).
    pub fn remap_for(
        &mut self,
        subscriber: ParticipantId,
        out_mid: &str,
        key: RtpKey,
    ) -> Option<RemappedRtp> {
        self.remappers
            .get_mut(&(subscriber, out_mid.to_owned()))
            .map(|r| r.remap(key))
    }

    /// Number of distinct published tracks with at least one subscriber.
    #[must_use]
    pub fn live_route_count(&self) -> usize {
        self.routes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid() -> ParticipantId {
        ParticipantId::new()
    }

    // ---- RtpRemapper: the seq/timestamp remap arithmetic --------------------

    #[test]
    fn first_packet_anchors_outbound_at_zero() {
        let mut r = RtpRemapper::new();
        // Publisher's stream starts at an arbitrary high seq/ts.
        let out = r.remap(RtpKey::new(40_000, 1_000_000));
        assert_eq!(out.seq, 0, "outbound seq must start at 0");
        assert_eq!(out.ts, 0, "outbound ts must start at 0");
    }

    #[test]
    fn contiguous_stream_preserves_deltas() {
        let mut r = RtpRemapper::new();
        let base_seq = 1_000u64;
        let base_ts = 90_000u64;
        let a = r.remap(RtpKey::new(base_seq, base_ts));
        let b = r.remap(RtpKey::new(base_seq + 1, base_ts + 3000));
        let c = r.remap(RtpKey::new(base_seq + 2, base_ts + 6000));
        // Outbound deltas equal inbound deltas (just shifted to start at 0).
        assert_eq!(a.seq, 0);
        assert_eq!(b.seq, 1);
        assert_eq!(c.seq, 2);
        assert_eq!(a.ts, 0);
        assert_eq!(b.ts, 3000);
        assert_eq!(c.ts, 6000);
    }

    #[test]
    fn packet_loss_gap_is_preserved_within_source() {
        // A small forward gap (lost packets) is NOT a source switch: the
        // outbound sequence must reflect the same gap so the receiver's NACK
        // logic sees the loss.
        let mut r = RtpRemapper::new();
        r.remap(RtpKey::new(500, 0));
        let after_gap = r.remap(RtpKey::new(505, 9000)); // 4 packets lost
        assert_eq!(after_gap.seq, 5, "gap of 5 preserved in outbound space");
        assert_eq!(after_gap.ts, 9000);
    }

    #[test]
    fn source_switch_continues_monotonically() {
        // Forward source A, then switch to source B with an unrelated (much
        // lower) sequence space. Outbound seq must keep climbing, never reset.
        let mut r = RtpRemapper::new();
        let a0 = r.remap(RtpKey::new(10_000, 500_000));
        let a1 = r.remap(RtpKey::new(10_001, 503_000));
        assert_eq!(a0.seq, 0);
        assert_eq!(a1.seq, 1);

        // New source B starts at seq 7 (a backwards jump → switch).
        let b0 = r.remap(RtpKey::new(7, 12));
        assert_eq!(b0.seq, 2, "must continue one past last outbound seq (1)");
        let b1 = r.remap(RtpKey::new(8, 3012));
        assert_eq!(b1.seq, 3, "source B stays contiguous after switch");
        // Timestamp must not go backwards across the switch.
        assert!(
            b0.ts > a1.ts,
            "outbound ts strictly increases across switch"
        );
        assert!(b1.ts > b0.ts);
    }

    #[test]
    fn duplicate_packet_is_not_treated_as_switch() {
        let mut r = RtpRemapper::new();
        let first = r.remap(RtpKey::new(100, 1000));
        let dup = r.remap(RtpKey::new(100, 1000));
        assert_eq!(first.seq, 0);
        assert_eq!(dup.seq, 0, "re-sent same packet keeps same outbound seq");
    }

    #[test]
    fn seq_offset_handles_wraparound_arithmetic() {
        // Inbound seq near the top of the extended space; offset math must wrap
        // cleanly without panicking and produce contiguous outbound values.
        let mut r = RtpRemapper::new();
        let near_max = u64::MAX - 1;
        let a = r.remap(RtpKey::new(near_max, 100));
        let b = r.remap(RtpKey::new(near_max.wrapping_add(1), 3100)); // wraps to MAX
        let c = r.remap(RtpKey::new(0, 6100)); // inbound wrapped past u64::MAX → 0
        assert_eq!(a.seq, 0);
        assert_eq!(b.seq, 1);
        // c is a contiguous forward step from b (delta 1), so still same source.
        assert_eq!(c.seq, 2);
    }

    #[test]
    fn last_out_seq_tracks_emitted_value() {
        let mut r = RtpRemapper::new();
        assert_eq!(r.last_out_seq(), None);
        r.remap(RtpKey::new(50, 0));
        r.remap(RtpKey::new(51, 90));
        assert_eq!(r.last_out_seq(), Some(1));
    }

    // ---- ForwardTable: the pure subscription routing ------------------------

    #[test]
    fn targets_empty_for_unsubscribed_track() {
        let t = ForwardTable::new();
        assert!(t.targets("0").is_empty());
        assert_eq!(t.live_route_count(), 0);
    }

    #[test]
    fn link_routes_publisher_track_to_each_subscriber() {
        let mut t = ForwardTable::new();
        let s1 = pid();
        let s2 = pid();
        // Publisher's video track "v0" fanned out to two subscribers on their
        // own outbound mids.
        t.link("v0", s1, "100");
        t.link("v0", s2, "200");

        let targets = t.targets("v0");
        assert_eq!(targets.len(), 2);
        assert!(targets
            .iter()
            .any(|x| x.subscriber == s1 && x.out_mid == "100"));
        assert!(targets
            .iter()
            .any(|x| x.subscriber == s2 && x.out_mid == "200"));
        // A different publisher track has no subscribers.
        assert!(t.targets("a0").is_empty());
    }

    #[test]
    fn link_is_idempotent() {
        let mut t = ForwardTable::new();
        let s1 = pid();
        t.link("v0", s1, "100");
        t.link("v0", s1, "100");
        assert_eq!(
            t.targets("v0").len(),
            1,
            "duplicate link must not duplicate target"
        );
    }

    #[test]
    fn distinct_subscribers_get_independent_remappers() {
        // The same inbound packet remapped for two subscribers yields the same
        // first outbound value (both start at 0) but they are independent state
        // machines thereafter.
        let mut t = ForwardTable::new();
        let s1 = pid();
        let s2 = pid();
        t.link("v0", s1, "100");
        t.link("v0", s2, "200");

        let k0 = RtpKey::new(900, 7000);
        let r1 = t.remap_for(s1, "100", k0).unwrap();
        let r2 = t.remap_for(s2, "200", k0).unwrap();
        assert_eq!(r1, RemappedRtp { seq: 0, ts: 0 });
        assert_eq!(r2, RemappedRtp { seq: 0, ts: 0 });

        // Advance only s1; s2 stays anchored, proving independence.
        let r1b = t.remap_for(s1, "100", RtpKey::new(901, 10_000)).unwrap();
        assert_eq!(r1b, RemappedRtp { seq: 1, ts: 3000 });
        let r2b = t.remap_for(s2, "200", RtpKey::new(950, 50_000)).unwrap();
        // s2 only ever saw seq 900 then 950: gap of 50 within same source.
        assert_eq!(
            r2b,
            RemappedRtp {
                seq: 50,
                ts: 43_000
            }
        );
    }

    #[test]
    fn remap_for_unknown_target_returns_none() {
        let mut t = ForwardTable::new();
        let ghost = pid();
        assert!(t.remap_for(ghost, "999", RtpKey::new(1, 1)).is_none());
    }

    #[test]
    fn unlink_subscriber_removes_all_their_targets_and_state() {
        let mut t = ForwardTable::new();
        let s1 = pid();
        let s2 = pid();
        t.link("v0", s1, "100");
        t.link("v0", s2, "200");
        t.link("a0", s1, "101");

        // s1 had remap state on v0; prime it then unlink.
        t.remap_for(s1, "100", RtpKey::new(5, 5));
        t.unlink_subscriber(s1);

        let v0 = t.targets("v0");
        assert_eq!(v0.len(), 1);
        assert_eq!(v0[0].subscriber, s2);
        // "a0" route had only s1 → route pruned entirely.
        assert!(t.targets("a0").is_empty());
        // remap state for s1 is gone.
        assert!(t.remap_for(s1, "100", RtpKey::new(6, 6)).is_none());
        // s2's state survives.
        assert!(t.remap_for(s2, "200", RtpKey::new(6, 6)).is_some());
    }

    #[test]
    fn unlink_publisher_track_drops_route_only() {
        let mut t = ForwardTable::new();
        let s1 = pid();
        t.link("v0", s1, "100");
        t.link("a0", s1, "101");
        t.unlink_publisher_track("v0");
        assert!(t.targets("v0").is_empty());
        assert_eq!(t.targets("a0").len(), 1, "other publisher track unaffected");
    }
}
