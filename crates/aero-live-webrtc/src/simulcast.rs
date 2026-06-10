//! Simulcast layer selection for the SFU.
//!
//! ## Overview
//!
//! A publisher may send the *same* track at multiple quality levels (layers),
//! each identified by a **RID** (e.g. `"low"`, `"mid"`, `"high"`). This module
//! models the available layers and lets the SFU pick which layer to forward to
//! each subscriber based on a target quality/bitrate, with two key guarantees:
//!
//! 1. **Keyframe-boundary switching** — when the target layer changes, the SFU
//!    keeps forwarding the *current* layer until a keyframe arrives from the
//!    target layer, then switches. This prevents decoder corruption that would
//!    result from starting in the middle of a non-intra-coded stream.
//!
//! 2. **Continuous seq/ts output** — the switch feeds through the existing
//!    [`RtpRemapper`](crate::remap::RtpRemapper), which handles the
//!    discontinuity in the new source's sequence space automatically.
//!
//! ## Public API
//!
//! - [`SimulcastLayer`] — a single spatial (and optional temporal) layer.
//! - [`LayerSet`] — the full set of layers a publisher offers, ordered
//!   lowest→highest quality.
//! - [`LayerSelector`] — per-subscriber state: tracks which layer is currently
//!   being forwarded, the pending target (if a layer switch is in progress),
//!   and whether a keyframe request has already been issued for that target.

use std::collections::HashMap;

use aero_common::ParticipantId;
use str0m::media::Rid;

/// Identifies the quality tier of a simulcast layer.
///
/// The ordering is intentional: `Low < Mid < High`. The [`LayerSet`] uses this
/// to find the highest available layer that does not exceed a given target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LayerKind {
    Low,
    Mid,
    High,
}

/// One simulcast spatial layer (and, optionally, its temporal sub-layer index).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulcastLayer {
    /// The RID label as negotiated in the SDP (e.g. `"low"`, `"mid"`, `"high"`).
    pub rid: Rid,
    /// Quality tier: used for ordering / selection.
    pub kind: LayerKind,
    /// Optional temporal layer index (0 = base, higher = more temporal layers).
    /// When `None`, the SFU treats the layer as a single temporal stream.
    pub temporal: Option<u8>,
}

impl SimulcastLayer {
    /// Create a layer without a temporal sub-layer.
    #[must_use]
    pub fn spatial(rid: Rid, kind: LayerKind) -> Self {
        Self {
            rid,
            kind,
            temporal: None,
        }
    }

    /// Create a layer with a specific temporal sub-layer index.
    #[must_use]
    pub fn with_temporal(rid: Rid, kind: LayerKind, temporal: u8) -> Self {
        Self {
            rid,
            kind,
            temporal: Some(temporal),
        }
    }
}

/// The complete set of simulcast layers a publisher offers, **sorted
/// lowest→highest quality** (by [`LayerKind`] ordinal then temporal index).
///
/// The SFU calls [`LayerSet::select`] to resolve a [`LayerKind`] target into
/// the concrete [`SimulcastLayer`] with the highest quality ≤ target that is
/// currently available.
#[derive(Debug, Clone, Default)]
pub struct LayerSet {
    /// Layers sorted by `(kind, temporal)` ascending — established once via
    /// [`LayerSet::new`] / [`LayerSet::from_layers`] and never mutated.
    layers: Vec<SimulcastLayer>,
}

impl LayerSet {
    /// Construct from an **unsorted** slice; the constructor sorts them.
    #[must_use]
    pub fn from_layers(mut layers: Vec<SimulcastLayer>) -> Self {
        layers.sort_by_key(|l| (l.kind, l.temporal));
        Self { layers }
    }

    /// The ordered layers (low→high).
    #[must_use]
    pub fn layers(&self) -> &[SimulcastLayer] {
        &self.layers
    }

    /// Highest layer whose [`LayerKind`] ≤ `target`.
    ///
    /// Returns `None` when the set is empty or when no layer is ≤ `target`
    /// (e.g. all available layers are higher quality than requested — callers
    /// should then fall back to the lowest available).
    #[must_use]
    pub fn select(&self, target: LayerKind) -> Option<&SimulcastLayer> {
        self.layers.iter().rfind(|l| l.kind <= target)
    }

    /// Lowest available layer (fallback when no layer meets the target).
    #[must_use]
    pub fn lowest(&self) -> Option<&SimulcastLayer> {
        self.layers.first()
    }

    /// Highest available layer.
    #[must_use]
    pub fn highest(&self) -> Option<&SimulcastLayer> {
        self.layers.last()
    }

    /// `true` when the set contains no layers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }
}

/// State of a pending layer switch for a single subscriber.
#[derive(Debug, Clone)]
struct PendingSwitch {
    /// The RID of the layer we want to switch *to*.
    target_rid: Rid,
    /// `true` after we have fired a keyframe request toward the publisher so we
    /// don't spam requests on every arriving packet.
    keyframe_requested: bool,
}

/// Per-subscriber layer-selection state.
///
/// Tracks:
/// - which layer is currently forwarded (`active_rid`),
/// - whether a layer switch is pending (waiting for a keyframe from the target),
/// - bookkeeping needed to fire exactly one keyframe request per switch attempt.
///
/// # Forwarding decision
///
/// Call [`LayerSelector::should_forward`] for every inbound RTP packet on a
/// simulcast track. It returns a [`ForwardDecision`] telling the SFU what to
/// do:
///
/// - [`ForwardDecision::Forward`] — deliver the packet to this subscriber.
/// - [`ForwardDecision::Drop`] — drop the packet (wrong layer, mid-switch).
/// - [`ForwardDecision::SwitchAndForward`] — keyframe arrived on the target
///   layer; commit the switch *and* forward this packet.
/// - [`ForwardDecision::RequestKeyframe`] — same as `Drop` but the SFU should
///   also issue a keyframe request toward the publisher.
#[derive(Debug, Default)]
pub struct LayerSelector {
    /// The RID currently being forwarded (set on first packet).
    active_rid: Option<Rid>,
    /// Pending layer switch, if any.
    pending: Option<PendingSwitch>,
}

/// What the SFU should do with the given inbound RTP packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardDecision {
    /// Deliver the packet to the subscriber as-is.
    Forward,
    /// Drop the packet; no other action needed.
    Drop,
    /// A keyframe arrived on the desired target layer — switch to it and
    /// deliver this packet.
    SwitchAndForward,
    /// Drop the packet *and* issue a keyframe request toward the publisher so
    /// the switch can happen promptly.
    RequestKeyframe,
}

impl LayerSelector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The RID currently being forwarded, if any.
    #[must_use]
    pub fn active_rid(&self) -> Option<Rid> {
        self.active_rid
    }

    /// The RID we are waiting to switch to (if a switch is pending).
    #[must_use]
    pub fn pending_rid(&self) -> Option<Rid> {
        self.pending.as_ref().map(|p| p.target_rid)
    }

    /// Request a switch to the layer identified by `target_rid`.
    ///
    /// If `target_rid` is already the active layer the call is a no-op.
    /// If a switch is already pending to the same target, the call is idempotent.
    /// Otherwise the pending switch is updated.
    ///
    /// After calling this, subsequent [`LayerSelector::should_forward`] calls
    /// will gate the switch on a keyframe from `target_rid`.
    pub fn request_switch(&mut self, target_rid: Rid) {
        // No-op if already on the target.
        if self.active_rid == Some(target_rid) {
            self.pending = None;
            return;
        }
        // No-op if already pending the same target.
        if matches!(&self.pending, Some(p) if p.target_rid == target_rid) {
            return;
        }
        self.pending = Some(PendingSwitch {
            target_rid,
            keyframe_requested: false,
        });
    }

    /// Evaluate one inbound RTP packet and return the forwarding decision.
    ///
    /// `rid` — the RID of this packet's layer.
    /// `is_keyframe` — whether this packet carries an intra frame (keyframe).
    ///
    /// This method mutates internal state (commits switches, records
    /// keyframe-request flags) as a side-effect.
    ///
    /// # Cold-start switch
    ///
    /// If a switch is requested *before* any packet has bootstrapped an active
    /// layer, the first non-target packet seen bootstraps the active layer (and
    /// is forwarded) so the subscriber renders video immediately instead of
    /// waiting on a black screen for the target layer's first keyframe. The
    /// pending switch remains in effect and commits later on the target keyframe.
    pub fn should_forward(&mut self, rid: Rid, is_keyframe: bool) -> ForwardDecision {
        match &mut self.pending {
            None => {
                // No switch pending: forward the active layer, drop others.
                match self.active_rid {
                    None => {
                        // First packet ever: accept any layer (bootstrap).
                        self.active_rid = Some(rid);
                        ForwardDecision::Forward
                    }
                    Some(active) if active == rid => ForwardDecision::Forward,
                    _ => ForwardDecision::Drop,
                }
            }
            Some(pending) => {
                let target = pending.target_rid;
                if rid == target {
                    if is_keyframe {
                        // Target keyframe arrived — commit the switch.
                        self.active_rid = Some(target);
                        self.pending = None;
                        ForwardDecision::SwitchAndForward
                    } else {
                        // Not a keyframe yet on target: drop and request one if
                        // we haven't already.
                        if pending.keyframe_requested {
                            ForwardDecision::Drop
                        } else {
                            pending.keyframe_requested = true;
                            ForwardDecision::RequestKeyframe
                        }
                    }
                } else if self.active_rid == Some(rid) {
                    // Still on the old active layer: keep forwarding until switch.
                    ForwardDecision::Forward
                } else if self.active_rid.is_none() {
                    // Cold start: a switch was requested before any packet
                    // bootstrapped an active layer (e.g. the subscriber asked for
                    // "high" quality up front). The target layer has not produced
                    // a keyframe yet, so committing to it now is impossible — but
                    // dropping every packet would leave the subscriber on a black
                    // screen until the target keyframe arrives (potentially
                    // seconds). Instead, bootstrap on the first non-target layer
                    // we see so the subscriber gets video immediately; the pending
                    // switch to the target stays in effect and is committed later
                    // on the target's keyframe, exactly as in the warm-start path.
                    self.active_rid = Some(rid);
                    ForwardDecision::Forward
                } else {
                    // Some third layer we don't care about.
                    ForwardDecision::Drop
                }
            }
        }
    }
}

/// Per-call table of layer selectors, one per `(subscriber, pub_mid)` pair.
///
/// The SFU calls [`LayerSelectorTable::decide`] in its RTP fan-out hot path to
/// determine which packets to forward for simulcast tracks.
#[derive(Debug, Default)]
pub struct LayerSelectorTable {
    /// `(subscriber, pub_mid) → selector`
    selectors: HashMap<(ParticipantId, String), LayerSelector>,
    /// `pub_mid → LayerSet` — the publisher's announced layer catalogue.
    layer_sets: HashMap<String, LayerSet>,
}

impl LayerSelectorTable {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a subscriber's layer selector for a specific published track.
    ///
    /// Idempotent — calling twice for the same pair is safe (existing state is
    /// preserved so an in-progress switch is not lost).
    pub fn register(&mut self, subscriber: ParticipantId, pub_mid: &str) {
        self.selectors
            .entry((subscriber, pub_mid.to_owned()))
            .or_default();
    }

    /// Remove all state for a subscriber (they left the call).
    pub fn unlink_subscriber(&mut self, subscriber: ParticipantId) {
        self.selectors.retain(|(sub, _), _| *sub != subscriber);
    }

    /// Register the [`LayerSet`] for a published track.
    pub fn set_layers(&mut self, pub_mid: &str, layers: LayerSet) {
        self.layer_sets.insert(pub_mid.to_owned(), layers);
    }

    /// The [`LayerSet`] for a published track, if registered.
    #[must_use]
    pub fn layer_set(&self, pub_mid: &str) -> Option<&LayerSet> {
        self.layer_sets.get(pub_mid)
    }

    /// Request a layer switch for `subscriber` on `pub_mid` to the best layer
    /// ≤ `target`. Returns the RID of the selected layer, or `None` if no
    /// layers are registered for the track.
    pub fn select_layer(
        &mut self,
        subscriber: ParticipantId,
        pub_mid: &str,
        target: LayerKind,
    ) -> Option<Rid> {
        let layer_set = self.layer_sets.get(pub_mid)?;
        let selected = layer_set
            .select(target)
            .or_else(|| layer_set.lowest())?;
        let rid = selected.rid;
        self.selectors
            .entry((subscriber, pub_mid.to_owned()))
            .or_default()
            .request_switch(rid);
        Some(rid)
    }

    /// The layer the subscriber is currently heading to: the pending switch
    /// target when one is in progress, otherwise the active layer. `None`
    /// when no selector is registered or nothing has bootstrapped yet.
    ///
    /// Bandwidth-driven adaptation compares against this (not just the active
    /// layer) so a decision made mid-switch doesn't re-request the same target.
    #[must_use]
    pub fn current_rid(&self, subscriber: ParticipantId, pub_mid: &str) -> Option<Rid> {
        self.selectors
            .get(&(subscriber, pub_mid.to_owned()))
            .and_then(|sel| sel.pending_rid().or_else(|| sel.active_rid()))
    }

    /// Evaluate a single inbound RTP packet for subscriber.
    ///
    /// Returns `None` when no selector is registered for this pair (non-simulcast
    /// track — forward unconditionally in that case).
    pub fn decide(
        &mut self,
        subscriber: ParticipantId,
        pub_mid: &str,
        rid: Rid,
        is_keyframe: bool,
    ) -> Option<ForwardDecision> {
        self.selectors
            .get_mut(&(subscriber, pub_mid.to_owned()))
            .map(|sel| sel.should_forward(rid, is_keyframe))
    }

    /// Test helper: immutable access to the [`LayerSelector`] for a given pair.
    /// Only compiled in `#[cfg(test)]` builds.
    #[cfg(test)]
    #[must_use]
    pub fn selector_for(
        &self,
        subscriber: ParticipantId,
        pub_mid: &str,
    ) -> Option<&LayerSelector> {
        self.selectors.get(&(subscriber, pub_mid.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rid(s: &str) -> Rid {
        Rid::from(s)
    }

    // ── LayerSet selection ────────────────────────────────────────────────────

    #[test]
    fn selects_highest_layer_at_or_below_target() {
        let set = LayerSet::from_layers(vec![
            SimulcastLayer::spatial(rid("low"), LayerKind::Low),
            SimulcastLayer::spatial(rid("mid"), LayerKind::Mid),
            SimulcastLayer::spatial(rid("high"), LayerKind::High),
        ]);

        // Target High → picks "high"
        assert_eq!(set.select(LayerKind::High).unwrap().kind, LayerKind::High);
        // Target Mid → picks "mid" (not "high")
        assert_eq!(set.select(LayerKind::Mid).unwrap().kind, LayerKind::Mid);
        // Target Low → picks "low"
        assert_eq!(set.select(LayerKind::Low).unwrap().kind, LayerKind::Low);
    }

    #[test]
    fn select_returns_none_when_no_layer_meets_target() {
        // Only high available, target is low → no layer ≤ low
        let set = LayerSet::from_layers(vec![SimulcastLayer::spatial(
            rid("high"),
            LayerKind::High,
        )]);
        assert!(set.select(LayerKind::Low).is_none());
    }

    #[test]
    fn lowest_and_highest_helpers() {
        let set = LayerSet::from_layers(vec![
            SimulcastLayer::spatial(rid("high"), LayerKind::High),
            SimulcastLayer::spatial(rid("low"), LayerKind::Low),
        ]);
        assert_eq!(set.lowest().unwrap().kind, LayerKind::Low);
        assert_eq!(set.highest().unwrap().kind, LayerKind::High);
    }

    // ── LayerSelector: forwarding decisions ───────────────────────────────────

    #[test]
    fn first_packet_bootstraps_active_layer() {
        let mut sel = LayerSelector::new();
        let decision = sel.should_forward(rid("low"), false);
        assert_eq!(decision, ForwardDecision::Forward);
        assert_eq!(sel.active_rid(), Some(rid("low")));
    }

    #[test]
    fn packets_on_non_active_layer_are_dropped() {
        let mut sel = LayerSelector::new();
        sel.should_forward(rid("low"), false); // bootstrap
        let d = sel.should_forward(rid("high"), false);
        assert_eq!(d, ForwardDecision::Drop);
    }

    #[test]
    fn switch_deferred_until_keyframe_on_target() {
        let mut sel = LayerSelector::new();
        // Bootstrap on "low"
        sel.should_forward(rid("low"), false);
        // Request switch to "high"
        sel.request_switch(rid("high"));

        // Non-keyframe on target → drop + request keyframe (first time)
        let d1 = sel.should_forward(rid("high"), false);
        assert_eq!(d1, ForwardDecision::RequestKeyframe);

        // Old layer packets keep forwarding during the wait
        let d2 = sel.should_forward(rid("low"), false);
        assert_eq!(d2, ForwardDecision::Forward);

        // Second non-keyframe on target → just drop (keyframe already requested)
        let d3 = sel.should_forward(rid("high"), false);
        assert_eq!(d3, ForwardDecision::Drop);

        // Keyframe on target → switch committed
        let d4 = sel.should_forward(rid("high"), true);
        assert_eq!(d4, ForwardDecision::SwitchAndForward);
        assert_eq!(sel.active_rid(), Some(rid("high")));
        assert_eq!(sel.pending_rid(), None);
    }

    #[test]
    fn switch_to_already_active_layer_is_noop() {
        let mut sel = LayerSelector::new();
        sel.should_forward(rid("high"), true); // bootstrap on high
        sel.request_switch(rid("high")); // switch to same layer
        assert_eq!(sel.pending_rid(), None, "no pending switch expected");
        let d = sel.should_forward(rid("high"), false);
        assert_eq!(d, ForwardDecision::Forward);
    }

    #[test]
    fn packets_from_third_layer_are_dropped_during_switch() {
        let mut sel = LayerSelector::new();
        sel.should_forward(rid("low"), false); // bootstrap
        sel.request_switch(rid("high"));

        // "mid" is neither active nor target
        let d = sel.should_forward(rid("mid"), false);
        assert_eq!(d, ForwardDecision::Drop);
    }

    #[test]
    fn cold_start_switch_bootstraps_on_first_non_target_layer() {
        // A switch is requested *before any packet has arrived* (e.g. the
        // subscriber asked for "high" up front). The first packet to arrive is on
        // "low" (which has a ready keyframe); the target "high" has not produced
        // one yet. We must bootstrap on "low" and forward it — NOT starve the
        // subscriber on a black screen until the target keyframe appears.
        let mut sel = LayerSelector::new();
        assert_eq!(sel.active_rid(), None, "no layer bootstrapped yet");

        // Request switch to "high" while still cold (no active layer).
        sel.request_switch(rid("high"));
        assert_eq!(sel.pending_rid(), Some(rid("high")));

        // First-ever packet arrives on "low" (non-target). Before the fix this
        // returned Drop, leaving the subscriber with nothing.
        let d = sel.should_forward(rid("low"), false);
        assert_eq!(
            d,
            ForwardDecision::Forward,
            "cold-start must bootstrap on the first non-target layer"
        );
        assert_eq!(sel.active_rid(), Some(rid("low")));
        // The pending switch to "high" must still be in effect.
        assert_eq!(sel.pending_rid(), Some(rid("high")));
    }

    #[test]
    fn cold_start_switch_still_commits_on_target_keyframe() {
        // After the cold-start bootstrap, the pending switch must still gate on a
        // keyframe from the target and commit normally (the upgrade is not lost).
        let mut sel = LayerSelector::new();
        sel.request_switch(rid("high"));

        // Bootstrap on "low".
        assert_eq!(sel.should_forward(rid("low"), false), ForwardDecision::Forward);

        // Subsequent "low" packets keep flowing while we wait for the target.
        assert_eq!(sel.should_forward(rid("low"), false), ForwardDecision::Forward);

        // Non-keyframe on target → request a keyframe (first time only).
        assert_eq!(
            sel.should_forward(rid("high"), false),
            ForwardDecision::RequestKeyframe
        );

        // Target keyframe → switch commits.
        assert_eq!(
            sel.should_forward(rid("high"), true),
            ForwardDecision::SwitchAndForward
        );
        assert_eq!(sel.active_rid(), Some(rid("high")));
        assert_eq!(sel.pending_rid(), None);

        // After commit, the old bootstrap layer "low" is dropped.
        assert_eq!(sel.should_forward(rid("low"), false), ForwardDecision::Drop);
    }

    #[test]
    fn cold_start_switch_target_keyframe_first_packet_commits_directly() {
        // Cold start where the very first packet is a keyframe on the target:
        // there is no need to bootstrap a lower layer — commit straight away.
        let mut sel = LayerSelector::new();
        sel.request_switch(rid("high"));

        let d = sel.should_forward(rid("high"), true);
        assert_eq!(d, ForwardDecision::SwitchAndForward);
        assert_eq!(sel.active_rid(), Some(rid("high")));
        assert_eq!(sel.pending_rid(), None);
    }

    #[test]
    fn cold_start_non_keyframe_on_target_does_not_bootstrap() {
        // If the cold-start packet happens to be on the *target* layer but is a
        // non-keyframe, we must NOT bootstrap-and-forward it (that would deliver a
        // mid-GOP frame on the target with no preceding keyframe → decoder
        // corruption). It must go through the keyframe-gated request path and the
        // active layer must stay unset.
        let mut sel = LayerSelector::new();
        sel.request_switch(rid("high"));

        let d = sel.should_forward(rid("high"), false);
        assert_eq!(
            d,
            ForwardDecision::RequestKeyframe,
            "non-keyframe on target must request a keyframe, not bootstrap-forward"
        );
        // active_rid stays None — we never bootstrapped on a mid-GOP target frame.
        assert_eq!(sel.active_rid(), None);
        assert_eq!(sel.pending_rid(), Some(rid("high")));
    }

    // ── Seq/ts continuity across switch (via RtpRemapper) ────────────────────

    #[test]
    fn seq_ts_continuous_across_layer_switch() {
        use crate::remap::{RtpKey, RtpRemapper};

        let mut sel = LayerSelector::new();
        let mut remapper = RtpRemapper::new();

        // Bootstrap: forward 3 packets on "low" (seq 10,11,12 / ts 1000,2000,3000)
        for (seq, ts) in [(10u64, 1000u64), (11, 2000), (12, 3000)] {
            let d = sel.should_forward(rid("low"), false);
            assert_eq!(d, ForwardDecision::Forward);
            remapper.remap(RtpKey::new(seq, ts));
        }
        // last outbound seq should be 2 (started at 0)
        assert_eq!(remapper.last_out_seq(), Some(2));

        // Request switch to "high"
        sel.request_switch(rid("high"));

        // One more "low" packet while waiting
        let d = sel.should_forward(rid("low"), false);
        assert_eq!(d, ForwardDecision::Forward);
        let r = remapper.remap(RtpKey::new(13, 4000));
        assert_eq!(r.seq, 3);

        // Non-keyframe on "high" → RequestKeyframe decision, DON'T remap
        let d = sel.should_forward(rid("high"), false);
        assert_eq!(d, ForwardDecision::RequestKeyframe);

        // Keyframe on "high". In reality simulcast layers have independent SSRCs
        // and therefore independent extended sequence spaces. Simulate this by
        // using a seq far enough from seq=13 to exceed MAX_CONTIGUOUS_FORWARD
        // (32768) so RtpRemapper detects a source switch. We start "high" at
        // seq 40_000 (delta from 13 = 39_987 > 32768 → source switch).
        let d = sel.should_forward(rid("high"), true);
        assert_eq!(d, ForwardDecision::SwitchAndForward);
        // Feed into remapper; it must detect source switch and continue at 4
        let last_ts_before_switch = 3000u64; // outbound ts for seq=3 (ts=4000 remapped)
        let r = remapper.remap(RtpKey::new(40_000, 4_000_000));
        assert_eq!(
            r.seq, 4,
            "outbound seq must continue monotonically after layer switch"
        );
        // Outbound ts must be strictly after the last emitted ts (monotonic).
        // The remapper anchors the switch at (last_out_ts + 1), so the new ts
        // is last_ts_before_switch + 1.
        assert!(
            r.ts > last_ts_before_switch,
            "outbound ts ({}) must be strictly after last emitted ts ({}) across switch",
            r.ts,
            last_ts_before_switch
        );

        // Next "high" packet (seq 40_001)
        let d = sel.should_forward(rid("high"), false);
        assert_eq!(d, ForwardDecision::Forward);
        let r2 = remapper.remap(RtpKey::new(40_001, 4_003_000));
        assert_eq!(r2.seq, 5, "continues cleanly on new layer");
    }

    // ── LayerSelectorTable ─────────────────────────────────────────────────────

    #[test]
    fn table_select_layer_picks_best_available() {
        let sub = ParticipantId::new();
        let mut table = LayerSelectorTable::new();
        table.set_layers(
            "v0",
            LayerSet::from_layers(vec![
                SimulcastLayer::spatial(rid("low"), LayerKind::Low),
                SimulcastLayer::spatial(rid("high"), LayerKind::High),
            ]),
        );
        table.register(sub, "v0");

        let selected = table.select_layer(sub, "v0", LayerKind::Mid);
        // Mid not available → falls back to Low (highest ≤ Mid)
        assert_eq!(selected, Some(rid("low")));
        assert_eq!(
            table.selectors[&(sub, "v0".to_owned())].pending_rid(),
            Some(rid("low"))
        );
    }

    #[test]
    fn table_decide_returns_none_for_unregistered_pair() {
        let sub = ParticipantId::new();
        let mut table = LayerSelectorTable::new();
        let d = table.decide(sub, "v0", rid("low"), false);
        assert!(d.is_none(), "no selector → forward unconditionally");
    }

    #[test]
    fn table_current_rid_reports_pending_then_active() {
        let sub = ParticipantId::new();
        let mut table = LayerSelectorTable::new();
        table.set_layers(
            "v0",
            LayerSet::from_layers(vec![
                SimulcastLayer::spatial(rid("low"), LayerKind::Low),
                SimulcastLayer::spatial(rid("high"), LayerKind::High),
            ]),
        );
        table.register(sub, "v0");
        assert_eq!(table.current_rid(sub, "v0"), None, "nothing bootstrapped");

        // Bootstrap on "low".
        table.decide(sub, "v0", rid("low"), false);
        assert_eq!(table.current_rid(sub, "v0"), Some(rid("low")));

        // Pending switch takes precedence over the active layer.
        table.select_layer(sub, "v0", LayerKind::High);
        assert_eq!(table.current_rid(sub, "v0"), Some(rid("high")));

        // Commit the switch → active is now "high", no pending.
        table.decide(sub, "v0", rid("high"), true);
        assert_eq!(table.current_rid(sub, "v0"), Some(rid("high")));

        // Unregistered pair → None.
        assert_eq!(table.current_rid(ParticipantId::new(), "v0"), None);
    }

    #[test]
    fn table_unlink_subscriber_removes_all_state() {
        let sub = ParticipantId::new();
        let mut table = LayerSelectorTable::new();
        table.register(sub, "v0");
        table.register(sub, "a0");
        table.unlink_subscriber(sub);
        assert!(table.selectors.is_empty());
    }
}
