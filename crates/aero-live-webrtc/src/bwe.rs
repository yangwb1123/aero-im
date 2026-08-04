//! Per-subscriber bandwidth estimation + adaptive simulcast layer policy.
//!
//! Pure, deterministic logic — time is injected as a `now_ms` parameter so
//! every component is unit-testable without clocks:
//!
//! - [`BandwidthEstimator`] — fuses two feedback signals into one estimate:
//!   - **REMB** is a direct receiver-side estimate; it is EWMA-smoothed and
//!     acts as a hard clamp on the final estimate.
//!   - **TWCC**-derived loss fraction and one-way-delay trend drive a simple
//!     AIMD loop: loss above ~5% or a rising delay trend multiplies the
//!     estimate by 0.85; clean feedback adds a time-proportional increment
//!     (capped per step).
//! - [`ThroughputEwma`] — per-simulcast-layer measured send rate (EWMA of
//!   bytes/sec over fixed windows), fed from forwarded RTP.
//! - [`LayerSwitchPolicy`] — chooses the highest layer whose measured rate
//!   fits `estimate × 0.85`, with hysteresis: down-switches apply immediately,
//!   up-switches require ~2 s of stable headroom. The policy only *picks* a
//!   target [`LayerKind`]; the actual switch goes through the existing
//!   keyframe-gated mechanics in [`crate::simulcast`].

use crate::simulcast::LayerKind;

// ── Estimator configuration ───────────────────────────────────────────────────

/// Tunables for [`BandwidthEstimator`]. [`BweConfig::default`] matches common
/// SFU practice and is what the forwarder uses.
#[derive(Debug, Clone)]
pub struct BweConfig {
    /// Floor for the estimate (bps). Keeps the lowest simulcast layer viable.
    pub min_bps: u64,
    /// Ceiling for the estimate (bps).
    pub max_bps: u64,
    /// Starting estimate before any feedback arrives (bps).
    pub initial_bps: u64,
    /// EWMA weight given to each new REMB sample (0..1].
    pub remb_alpha: f64,
    /// Loss fraction above which the AIMD loop backs off.
    pub loss_threshold: f64,
    /// Delay trend (µs, from [`crate::rtcp_fb::TwccSummary`]) above which the
    /// AIMD loop backs off even without loss.
    pub delay_trend_threshold_us: i64,
    /// Multiplicative decrease factor applied on congestion.
    pub decrease_factor: f64,
    /// Additive increase rate on clean feedback (bps per second of elapsed
    /// feedback time).
    pub increase_bps_per_sec: u64,
    /// Per-feedback cap on the additive increase (bps) so sparse feedback
    /// cannot produce one huge jump.
    pub max_increase_step_bps: u64,
}

impl Default for BweConfig {
    fn default() -> Self {
        Self {
            min_bps: 50_000,
            max_bps: 8_000_000,
            initial_bps: 600_000,
            remb_alpha: 0.5,
            loss_threshold: 0.05,
            delay_trend_threshold_us: 1_000,
            decrease_factor: 0.85,
            increase_bps_per_sec: 200_000,
            max_increase_step_bps: 100_000,
        }
    }
}

// ── Bandwidth estimator ───────────────────────────────────────────────────────

/// Per-subscriber bandwidth estimator (REMB clamp + TWCC-driven AIMD).
#[derive(Debug, Clone)]
pub struct BandwidthEstimator {
    cfg: BweConfig,
    /// AIMD-controlled estimate (bps).
    aimd_bps: f64,
    /// EWMA-smoothed REMB (bps); `None` until the first REMB arrives.
    remb_ewma_bps: Option<f64>,
    /// Time of the last TWCC feedback, for time-proportional increase.
    last_twcc_ms: Option<u64>,
}

impl Default for BandwidthEstimator {
    fn default() -> Self {
        Self::new(BweConfig::default())
    }
}

impl BandwidthEstimator {
    /// Create an estimator with the given tunables.
    #[must_use]
    pub fn new(cfg: BweConfig) -> Self {
        #[allow(clippy::cast_precision_loss)]
        let aimd_bps = cfg.initial_bps as f64;
        Self {
            cfg,
            aimd_bps,
            remb_ewma_bps: None,
            last_twcc_ms: None,
        }
    }

    /// Feed one REMB sample (receiver's direct estimate, bps).
    #[allow(clippy::cast_precision_loss)]
    pub fn on_remb(&mut self, bitrate_bps: u64, _now_ms: u64) {
        let sample = bitrate_bps as f64;
        self.remb_ewma_bps = Some(match self.remb_ewma_bps {
            None => sample,
            Some(prev) => self.cfg.remb_alpha * sample + (1.0 - self.cfg.remb_alpha) * prev,
        });
    }

    /// Feed one TWCC feedback summary: `received`/`lost` packet counts and the
    /// delay trend in µs (positive = inter-arrival spacing growing).
    #[allow(clippy::cast_precision_loss)]
    pub fn on_twcc(&mut self, received: u32, lost: u32, delay_trend_us: i64, now_ms: u64) {
        let total = received + lost;
        let loss_fraction = if total == 0 {
            0.0
        } else {
            f64::from(lost) / f64::from(total)
        };

        let congested = loss_fraction > self.cfg.loss_threshold
            || delay_trend_us > self.cfg.delay_trend_threshold_us;
        if congested {
            self.aimd_bps *= self.cfg.decrease_factor;
        } else if total > 0 {
            // Additive increase proportional to elapsed feedback time; the
            // first feedback gets no increase (no elapsed interval to scale).
            if let Some(last) = self.last_twcc_ms {
                let dt_ms = now_ms.saturating_sub(last);
                let step = (self.cfg.increase_bps_per_sec.saturating_mul(dt_ms) / 1000)
                    .min(self.cfg.max_increase_step_bps);
                self.aimd_bps += step as f64;
            }
        }
        self.aimd_bps = self
            .aimd_bps
            .clamp(self.cfg.min_bps as f64, self.cfg.max_bps as f64);
        self.last_twcc_ms = Some(now_ms);
    }

    /// The current estimate in bps: AIMD value clamped by the smoothed REMB
    /// (when present) and the configured `[min, max]` bounds.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn estimate_bps(&self) -> u64 {
        let mut est = self.aimd_bps;
        if let Some(remb) = self.remb_ewma_bps {
            est = est.min(remb);
        }
        est.clamp(self.cfg.min_bps as f64, self.cfg.max_bps as f64) as u64
    }

    /// The smoothed REMB value, if any REMB has arrived (mostly for tests /
    /// introspection).
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn remb_bps(&self) -> Option<u64> {
        self.remb_ewma_bps.map(|v| v.max(0.0) as u64)
    }
}

// ── Per-layer throughput meter ────────────────────────────────────────────────

/// Window length over which instantaneous layer throughput is sampled before
/// being folded into the EWMA.
pub const THROUGHPUT_WINDOW_MS: u64 = 500;

/// EWMA weight given to each new throughput window sample.
const THROUGHPUT_ALPHA: f64 = 0.3;

/// Measures a stream's throughput as an EWMA of bytes/sec, fed one packet at
/// a time with injected time. Used per simulcast layer in the forwarder.
#[derive(Debug, Clone, Default)]
pub struct ThroughputEwma {
    rate_bps: f64,
    initialized: bool,
    window_start_ms: Option<u64>,
    window_bytes: u64,
}

impl ThroughputEwma {
    /// Create an empty meter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `bytes` arriving at `now_ms`. Folds the current window into the
    /// EWMA once [`THROUGHPUT_WINDOW_MS`] has elapsed.
    #[allow(clippy::cast_precision_loss)]
    pub fn on_bytes(&mut self, bytes: usize, now_ms: u64) {
        match self.window_start_ms {
            None => {
                self.window_start_ms = Some(now_ms);
                self.window_bytes = bytes as u64;
            }
            Some(start) if now_ms.saturating_sub(start) >= THROUGHPUT_WINDOW_MS => {
                let elapsed_ms = now_ms - start;
                let inst_bps = (self.window_bytes as f64 * 8.0 * 1000.0) / elapsed_ms as f64;
                self.rate_bps = if self.initialized {
                    THROUGHPUT_ALPHA * inst_bps + (1.0 - THROUGHPUT_ALPHA) * self.rate_bps
                } else {
                    inst_bps
                };
                self.initialized = true;
                self.window_start_ms = Some(now_ms);
                self.window_bytes = bytes as u64;
            }
            Some(_) => {
                self.window_bytes += bytes as u64;
            }
        }
    }

    /// The smoothed rate in bps; `None` until the first full window has been
    /// folded (no meaningful sample yet).
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn rate_bps(&self) -> Option<u64> {
        self.initialized.then_some(self.rate_bps.max(0.0) as u64)
    }
}

// ── Layer switch policy (hysteresis) ──────────────────────────────────────────

/// Fraction of the bandwidth estimate a layer's measured rate must fit into.
pub const LAYER_FIT_SAFETY: f64 = 0.85;

/// How long headroom must hold before an up-switch is allowed.
pub const UPSWITCH_STABLE_MS: u64 = 2_000;

/// Per-`(subscriber, track)` hysteresis state for bandwidth-driven layer
/// switching.
///
/// [`LayerSwitchPolicy::decide`] returns the [`LayerKind`] to switch to (to be
/// passed to the existing keyframe-gated
/// [`LayerSelectorTable::select_layer`](crate::simulcast::LayerSelectorTable::select_layer)),
/// or `None` when no switch should happen yet.
#[derive(Debug, Clone, Default)]
pub struct LayerSwitchPolicy {
    /// When a higher layer first became affordable; cleared whenever the
    /// desired layer stops exceeding the current one.
    headroom_since_ms: Option<u64>,
}

impl LayerSwitchPolicy {
    /// Create a fresh policy (no headroom history).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide whether to switch layers.
    ///
    /// - `current` — the layer the subscriber is on (or heading to via a
    ///   pending keyframe-gated switch). `None` = not bootstrapped yet.
    /// - `estimate_bps` — the subscriber's bandwidth estimate.
    /// - `rates` — `(kind, measured rate)` for each available layer,
    ///   **ordered low→high**; `None` rate = not yet measured (such layers are
    ///   never chosen for an up-switch since we cannot verify they fit).
    ///
    /// Down-switches are returned immediately; up-switches only after the
    /// higher layer has been affordable for [`UPSWITCH_STABLE_MS`].
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn decide(
        &mut self,
        current: Option<LayerKind>,
        estimate_bps: u64,
        rates: &[(LayerKind, Option<u64>)],
        now_ms: u64,
    ) -> Option<LayerKind> {
        if rates.is_empty() {
            return None;
        }
        let budget = (estimate_bps as f64 * LAYER_FIT_SAFETY) as u64;

        // Highest layer with a *measured* rate that fits the budget; when
        // nothing measurable fits, fall back to the lowest available layer.
        let desired = rates
            .iter()
            .rev()
            .find_map(|(kind, rate)| match rate {
                Some(r) if *r <= budget => Some(*kind),
                _ => None,
            })
            .unwrap_or(rates[0].0);

        let Some(current) = current else {
            // Not bootstrapped: aim straight at the affordable layer.
            self.headroom_since_ms = None;
            return Some(desired);
        };

        match desired.cmp(&current) {
            std::cmp::Ordering::Equal => {
                self.headroom_since_ms = None;
                None
            }
            std::cmp::Ordering::Less => {
                // Congestion: switch down immediately.
                self.headroom_since_ms = None;
                Some(desired)
            }
            std::cmp::Ordering::Greater => {
                // Headroom: require it to be stable before switching up.
                let since = *self.headroom_since_ms.get_or_insert(now_ms);
                if now_ms.saturating_sub(since) >= UPSWITCH_STABLE_MS {
                    self.headroom_since_ms = None;
                    Some(desired)
                } else {
                    None
                }
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── BandwidthEstimator ─────────────────────────────────────────────────────

    #[test]
    fn clean_twcc_feedback_converges_upward() {
        let mut bwe = BandwidthEstimator::default();
        let start = bwe.estimate_bps();
        // 30 clean feedbacks at 100 ms intervals — no loss, flat delay.
        for i in 0..30u64 {
            bwe.on_twcc(20, 0, 0, i * 100);
        }
        let end = bwe.estimate_bps();
        assert!(
            end > start,
            "estimate must grow on clean feedback: {start} → {end}"
        );
        // 29 intervals × 100 ms × 200_000 bps/s = 580_000 of growth.
        assert_eq!(end, start + 580_000);
    }

    #[test]
    fn additive_increase_is_capped_per_step() {
        let mut bwe = BandwidthEstimator::default();
        bwe.on_twcc(20, 0, 0, 0);
        let before = bwe.estimate_bps();
        // 10 s gap: uncapped would add 2_000_000, the cap limits it to 100_000.
        bwe.on_twcc(20, 0, 0, 10_000);
        assert_eq!(bwe.estimate_bps(), before + 100_000);
    }

    #[test]
    fn loss_above_threshold_backs_off_multiplicatively() {
        let mut bwe = BandwidthEstimator::default();
        let before = bwe.estimate_bps(); // 600_000
                                         // 2 lost out of 20 = 10% > 5% threshold.
        bwe.on_twcc(18, 2, 0, 100);
        let after = bwe.estimate_bps();
        assert_eq!(after, 510_000, "600_000 × 0.85");
        assert!(after < before);
        // Repeated loss keeps decreasing.
        bwe.on_twcc(18, 2, 0, 200);
        assert_eq!(bwe.estimate_bps(), 433_500);
    }

    #[test]
    fn loss_below_threshold_does_not_back_off() {
        let mut bwe = BandwidthEstimator::default();
        bwe.on_twcc(100, 0, 0, 0);
        let before = bwe.estimate_bps();
        // 4 lost out of 100 = 4% < 5%: still clean → increase.
        bwe.on_twcc(96, 4, 0, 100);
        assert!(bwe.estimate_bps() > before);
    }

    #[test]
    fn rising_delay_trend_backs_off_without_loss() {
        let mut bwe = BandwidthEstimator::default();
        let before = bwe.estimate_bps();
        // No loss, but the delay trend exceeds 1 ms → queue building.
        bwe.on_twcc(20, 0, 5_000, 100);
        assert_eq!(bwe.estimate_bps(), (before * 85) / 100);
    }

    #[test]
    fn estimate_respects_min_floor() {
        let mut bwe = BandwidthEstimator::default();
        for i in 0..200u64 {
            bwe.on_twcc(10, 10, 0, i * 100); // 50% loss forever
        }
        assert_eq!(bwe.estimate_bps(), 50_000, "must bottom out at min_bps");
    }

    #[test]
    fn estimate_respects_max_ceiling() {
        let mut bwe = BandwidthEstimator::default();
        for i in 0..10_000u64 {
            bwe.on_twcc(20, 0, 0, i * 100);
        }
        assert_eq!(bwe.estimate_bps(), 8_000_000, "must cap at max_bps");
    }

    #[test]
    fn remb_clamps_the_estimate() {
        let mut bwe = BandwidthEstimator::default();
        // Grow the AIMD estimate well above 600k.
        for i in 0..30u64 {
            bwe.on_twcc(20, 0, 0, i * 100);
        }
        assert!(bwe.estimate_bps() > 1_000_000);
        // Receiver reports only 400 kbps available.
        bwe.on_remb(400_000, 3_000);
        assert_eq!(bwe.estimate_bps(), 400_000, "REMB must clamp the estimate");
    }

    #[test]
    fn remb_is_ewma_smoothed() {
        let mut bwe = BandwidthEstimator::default();
        bwe.on_remb(1_000_000, 0);
        assert_eq!(bwe.remb_bps(), Some(1_000_000), "first sample taken as-is");
        // A sudden dip is smoothed: 0.5×200k + 0.5×1M = 600k.
        bwe.on_remb(200_000, 100);
        assert_eq!(bwe.remb_bps(), Some(600_000));
        // And recovers gradually: 0.5×1M + 0.5×600k = 800k.
        bwe.on_remb(1_000_000, 200);
        assert_eq!(bwe.remb_bps(), Some(800_000));
    }

    #[test]
    fn remb_clamp_releases_as_remb_recovers() {
        let mut bwe = BandwidthEstimator::default();
        bwe.on_remb(100_000, 0);
        assert_eq!(bwe.estimate_bps(), 100_000);
        // REMB recovers far above the AIMD value → AIMD dominates again.
        bwe.on_remb(9_000_000, 100);
        bwe.on_remb(9_000_000, 200);
        bwe.on_remb(9_000_000, 300);
        assert!(
            bwe.estimate_bps() <= 600_000,
            "AIMD value rules when REMB is high"
        );
        assert!(bwe.estimate_bps() >= 500_000);
    }

    #[test]
    fn empty_feedback_changes_nothing() {
        let mut bwe = BandwidthEstimator::default();
        let before = bwe.estimate_bps();
        bwe.on_twcc(0, 0, 0, 100);
        bwe.on_twcc(0, 0, 0, 200);
        assert_eq!(bwe.estimate_bps(), before);
    }

    // ── ThroughputEwma ─────────────────────────────────────────────────────────

    #[test]
    fn throughput_unmeasured_until_first_window_folds() {
        let mut m = ThroughputEwma::new();
        m.on_bytes(1000, 0);
        m.on_bytes(1000, 100);
        assert_eq!(m.rate_bps(), None, "window not yet folded");
        m.on_bytes(1000, 500); // fold: 2000 bytes over 500 ms = 32 kbps
        assert_eq!(m.rate_bps(), Some(32_000));
    }

    #[test]
    fn throughput_steady_stream_measures_correct_rate() {
        let mut m = ThroughputEwma::new();
        // 1250 bytes every 10 ms = 1 Mbps, for 3 s.
        for i in 0..300u64 {
            m.on_bytes(1250, i * 10);
        }
        let rate = m.rate_bps().expect("measured");
        assert!(
            (900_000..=1_100_000).contains(&rate),
            "steady 1 Mbps stream must measure ≈1 Mbps, got {rate}"
        );
    }

    #[test]
    fn throughput_ewma_tracks_rate_drop_gradually() {
        let mut m = ThroughputEwma::new();
        for i in 0..100u64 {
            m.on_bytes(1250, i * 10); // 1 Mbps for 1 s
        }
        let high = m.rate_bps().unwrap();
        for i in 100..200u64 {
            m.on_bytes(125, i * 10); // drop to 100 kbps for 1 s
        }
        let low = m.rate_bps().unwrap();
        assert!(low < high);
        assert!(
            low > 100_000,
            "EWMA must lag, not jump straight to 100 kbps"
        );
    }

    // ── LayerSwitchPolicy ──────────────────────────────────────────────────────

    fn rates_low_high() -> Vec<(LayerKind, Option<u64>)> {
        vec![
            (LayerKind::Low, Some(200_000)),
            (LayerKind::Mid, Some(700_000)),
            (LayerKind::High, Some(2_000_000)),
        ]
    }

    #[test]
    fn picks_highest_layer_fitting_safety_budget() {
        let mut p = LayerSwitchPolicy::new();
        // 1 Mbps estimate → budget 850k → Mid (700k) fits, High (2M) doesn't.
        // No current layer → immediate.
        assert_eq!(
            p.decide(None, 1_000_000, &rates_low_high(), 0),
            Some(LayerKind::Mid)
        );
    }

    #[test]
    fn down_switch_is_immediate() {
        let mut p = LayerSwitchPolicy::new();
        // On High, estimate collapses to 500k → budget 425k → only Low fits.
        assert_eq!(
            p.decide(Some(LayerKind::High), 500_000, &rates_low_high(), 0),
            Some(LayerKind::Low)
        );
    }

    #[test]
    fn up_switch_requires_stable_headroom() {
        let mut p = LayerSwitchPolicy::new();
        let rates = rates_low_high();
        // On Low, estimate now affords Mid — but not yet for 2 s.
        assert_eq!(p.decide(Some(LayerKind::Low), 1_000_000, &rates, 0), None);
        assert_eq!(
            p.decide(Some(LayerKind::Low), 1_000_000, &rates, 1_000),
            None
        );
        assert_eq!(
            p.decide(Some(LayerKind::Low), 1_000_000, &rates, 1_999),
            None
        );
        // 2 s of stable headroom → up-switch fires.
        assert_eq!(
            p.decide(Some(LayerKind::Low), 1_000_000, &rates, 2_000),
            Some(LayerKind::Mid)
        );
    }

    #[test]
    fn headroom_timer_resets_when_headroom_lost() {
        let mut p = LayerSwitchPolicy::new();
        let rates = rates_low_high();
        assert_eq!(p.decide(Some(LayerKind::Low), 1_000_000, &rates, 0), None);
        // Estimate dips back to Low territory → timer must reset.
        assert_eq!(p.decide(Some(LayerKind::Low), 250_000, &rates, 1_000), None);
        // Headroom returns at t=1500; 2 s from *here*, not from t=0.
        assert_eq!(
            p.decide(Some(LayerKind::Low), 1_000_000, &rates, 1_500),
            None
        );
        assert_eq!(
            p.decide(Some(LayerKind::Low), 1_000_000, &rates, 3_000),
            None
        );
        assert_eq!(
            p.decide(Some(LayerKind::Low), 1_000_000, &rates, 3_500),
            Some(LayerKind::Mid)
        );
    }

    #[test]
    fn no_change_when_already_on_best_layer() {
        let mut p = LayerSwitchPolicy::new();
        assert_eq!(
            p.decide(Some(LayerKind::Mid), 1_000_000, &rates_low_high(), 0),
            None
        );
    }

    #[test]
    fn unmeasured_layers_are_not_chosen_for_up_switch() {
        let mut p = LayerSwitchPolicy::new();
        let rates = vec![
            (LayerKind::Low, Some(200_000)),
            (LayerKind::High, None), // never measured — cannot verify it fits
        ];
        assert_eq!(p.decide(Some(LayerKind::Low), 5_000_000, &rates, 0), None);
        assert_eq!(
            p.decide(Some(LayerKind::Low), 5_000_000, &rates, 60_000),
            None
        );
    }

    #[test]
    fn falls_back_to_lowest_when_nothing_fits() {
        let mut p = LayerSwitchPolicy::new();
        // Estimate below even the Low layer's rate → still pick Low (floor).
        assert_eq!(
            p.decide(Some(LayerKind::Mid), 100_000, &rates_low_high(), 0),
            Some(LayerKind::Low)
        );
    }

    #[test]
    fn empty_rate_list_yields_no_decision() {
        let mut p = LayerSwitchPolicy::new();
        assert_eq!(p.decide(Some(LayerKind::Low), 1_000_000, &[], 0), None);
    }
}
