//! Congestion-aware send pacing for the SRT data plane.
//!
//! [`Pacer`] is a pure, time-injected token bucket with a multiplicative-
//! decrease / additive-recovery rate controller layered on top, in the spirit
//! of SRT's live congestion control (`SRTO_MAXBW` + LiveCC):
//!
//! - **Token bucket**: the configured maximum bandwidth (bytes/sec) refills a
//!   bucket whose capacity is a few MTUs, so short bursts are absorbed but
//!   sustained sending is spaced out at the configured rate.
//! - **Congestion inputs**: smoothed RTT / RTT variance sampled from received
//!   ACK CIFs ([`Pacer::on_ack`]) and the retransmit-request (NAK) rate per
//!   window ([`Pacer::on_nak`]).
//! - **Back-off**: RTT inflating past `baseline * 1.25 (+ rttvar headroom)` or
//!   the per-window NAK count crossing [`NAK_WINDOW_THRESHOLD`] multiplies the
//!   send rate by 0.85, at most once per [`RATE_WINDOW`], floored at 5% of the
//!   configured rate.
//! - **Recovery**: each clean window (no congestion signal, no NAKs) adds 5%
//!   of the configured rate back, capped at the configured maximum.
//!
//! All time is injected as [`Instant`] parameters — the pacer never reads a
//! clock — and all arithmetic is integer (token units of byte·µs), so given
//! the same call sequence the pacer is fully deterministic.

// SRT-specific acronyms (ACK, NAK, RTT, MTU, …) are domain-standard.
#![allow(clippy::doc_markdown)]

use std::time::{Duration, Instant};

/// Default maximum send bandwidth: 1.5 MB/s = 12 Mbit/s, a comfortable
/// ceiling for a single 1080p live contribution feed.
pub const DEFAULT_MAX_BANDWIDTH: u64 = 1_500_000;

/// Nominal SRT MTU in bytes (the wire default; payloads are sized under it).
const MTU_BYTES: u64 = 1500;

/// Bucket capacity: a few MTUs so the encoder's natural micro-bursts pass
/// without delay while sustained traffic is spaced at the current rate.
const BUCKET_CAPACITY_BYTES: u64 = 4 * MTU_BYTES;

/// Token units per byte. Working in byte·µs keeps every refill/charge exact
/// in integer arithmetic: refilling at `rate` bytes/sec adds exactly `rate`
/// units per elapsed microsecond.
const TOKEN_SCALE: u64 = 1_000_000;

/// Rate-controller window. NAK counts accumulate per window, multiplicative
/// decreases fire at most once per window, and each clean window grants one
/// additive recovery step.
const RATE_WINDOW: Duration = Duration::from_millis(100);

/// More than this many NAK'd packets within one [`RATE_WINDOW`] is treated as
/// a congestion signal.
const NAK_WINDOW_THRESHOLD: u64 = 3;

/// Multiplicative decrease applied on a congestion signal: `rate * 85 / 100`.
const BACKOFF_NUMERATOR: u64 = 85;
const BACKOFF_DENOMINATOR: u64 = 100;

/// The rate never drops below this percentage of the configured maximum, so
/// a misbehaving receiver cannot stall the stream entirely.
const FLOOR_PERCENT: u64 = 5;

/// Additive recovery per clean window, as a percentage of the configured
/// maximum (full recovery from the floor takes ~19 windows ≈ 2 s).
const RECOVERY_PERCENT: u64 = 5;

/// RTT must exceed `baseline + baseline/4 (+ rttvar)` to count as inflated —
/// i.e. ~1.25× the lowest RTT observed, with variance headroom so jittery
/// links don't trigger spurious back-off.
const RTT_INFLATION_NUMERATOR: u64 = 1;
const RTT_INFLATION_DENOMINATOR: u64 = 4;

/// Verdict from [`Pacer::allowance`] for one prospective packet send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Allowance {
    /// Send now; the packet's cost has been charged to the bucket.
    Allow,
    /// Budget exhausted — hold the packet (and everything queued behind it)
    /// until at least this instant, then ask again. Nothing was charged.
    DeferUntil(Instant),
}

/// Deterministic, time-injected send pacer (token bucket + AIMD-style rate
/// control). See the module docs for the algorithm.
///
/// Construct with [`Pacer::new`], feed congestion signals via
/// [`Pacer::on_ack`] / [`Pacer::on_nak`], and gate every data-packet send
/// through [`Pacer::allowance`]. All three take `now` from the caller.
#[derive(Debug, Clone)]
pub struct Pacer {
    /// Configured maximum rate in bytes/sec (always ≥ 1).
    configured: u64,
    /// Current rate in bytes/sec, `floor ..= configured`.
    rate: u64,
    /// Lower clamp for `rate` (5% of `configured`, at least 1).
    floor: u64,
    /// Bucket fill in token units (byte·µs). May go negative when a packet
    /// larger than the bucket is charged — the deficit then delays
    /// subsequent sends, which is exactly the spacing we want.
    tokens: i64,
    /// Last instant the bucket was refilled; `None` until the first call.
    last_refill: Option<Instant>,
    /// Lowest smoothed RTT seen so far (µs) — the congestion baseline.
    baseline_rtt_us: Option<u64>,
    /// Start of the current rate window; `None` until the first call.
    window_start: Option<Instant>,
    /// NAK'd packets accumulated in the current window.
    window_naks: u64,
    /// Whether a congestion signal fired in the current window (suppresses
    /// the additive recovery step at the next rollover).
    window_congested: bool,
    /// Last multiplicative decrease, for the once-per-window gate.
    last_decrease_at: Option<Instant>,
}

impl Pacer {
    /// Create a pacer capped at `max_bytes_per_sec` (clamped to ≥ 1). The
    /// bucket starts full so the first few MTUs go out immediately.
    #[must_use]
    pub fn new(max_bytes_per_sec: u64) -> Self {
        let configured = max_bytes_per_sec.max(1);
        let floor = (configured * FLOOR_PERCENT / 100).max(1);
        Self {
            configured,
            rate: configured,
            floor,
            tokens: capacity_units_i64(),
            last_refill: None,
            baseline_rtt_us: None,
            window_start: None,
            window_naks: 0,
            window_congested: false,
            last_decrease_at: None,
        }
    }

    /// The configured maximum rate in bytes/sec.
    #[must_use]
    pub fn configured_rate(&self) -> u64 {
        self.configured
    }

    /// The current (congestion-adjusted) rate in bytes/sec.
    #[must_use]
    pub fn current_rate(&self) -> u64 {
        self.rate
    }

    /// Feed an RTT sample from a received ACK CIF (`rtt_us` / `rttvar_us` in
    /// microseconds). A sample below the baseline lowers the baseline; a
    /// sample above `baseline * 1.25 + rttvar` is a congestion signal and
    /// backs the rate off. Zero samples (untracked peers) are ignored.
    pub fn on_ack(&mut self, rtt_us: u32, rttvar_us: u32, now: Instant) {
        self.refill(now);
        self.roll_windows(now);
        if rtt_us == 0 {
            return;
        }
        let rtt = u64::from(rtt_us);
        match self.baseline_rtt_us {
            Some(baseline) if rtt >= baseline => {
                let threshold = baseline
                    .saturating_add(baseline * RTT_INFLATION_NUMERATOR / RTT_INFLATION_DENOMINATOR)
                    .saturating_add(u64::from(rttvar_us));
                if rtt > threshold {
                    self.backoff(now);
                }
            }
            _ => self.baseline_rtt_us = Some(rtt),
        }
    }

    /// Record `count` packets requested for retransmission by a NAK. When the
    /// per-window total crosses [`NAK_WINDOW_THRESHOLD`] the rate backs off.
    pub fn on_nak(&mut self, count: usize, now: Instant) {
        self.refill(now);
        self.roll_windows(now);
        let count = u64::try_from(count).unwrap_or(u64::MAX);
        self.window_naks = self.window_naks.saturating_add(count);
        if self.window_naks > NAK_WINDOW_THRESHOLD {
            self.backoff(now);
        }
    }

    /// Ask permission to send a `packet_len`-byte packet at `now`.
    ///
    /// [`Allowance::Allow`] charges the bucket and means "send immediately".
    /// [`Allowance::DeferUntil`] charges nothing; re-ask at (or after) the
    /// returned instant. A packet larger than the bucket capacity is allowed
    /// once the bucket is full and its overshoot is carried as a deficit.
    pub fn allowance(&mut self, now: Instant, packet_len: usize) -> Allowance {
        self.refill(now);
        self.roll_windows(now);
        let len = u64::try_from(packet_len).unwrap_or(u64::MAX);
        let need = i64::try_from(len.min(BUCKET_CAPACITY_BYTES) * TOKEN_SCALE).unwrap_or(i64::MAX);
        if self.tokens >= need {
            let cost = i64::try_from(len.saturating_mul(TOKEN_SCALE)).unwrap_or(i64::MAX);
            self.tokens = self.tokens.saturating_sub(cost);
            Allowance::Allow
        } else {
            let deficit = need.saturating_sub(self.tokens).unsigned_abs();
            let wait_us = deficit.div_ceil(self.rate);
            Allowance::DeferUntil(now + Duration::from_micros(wait_us))
        }
    }

    /// Accrue tokens for the time elapsed since the last refill, capped at
    /// the bucket capacity.
    fn refill(&mut self, now: Instant) {
        let Some(last) = self.last_refill else {
            self.last_refill = Some(now);
            return;
        };
        let elapsed_us = now.duration_since(last).as_micros();
        if elapsed_us == 0 {
            return;
        }
        let added = u128::from(self.rate) * elapsed_us;
        let next = (i128::from(self.tokens) + i128::try_from(added).unwrap_or(i128::MAX))
            .min(i128::from(capacity_units_i64()));
        // `next` ≤ capacity (i64) and ≥ the old i64 value, so it fits.
        self.tokens = i64::try_from(next).unwrap_or(i64::MAX);
        self.last_refill = Some(now);
    }

    /// Close out every window that has fully elapsed: grant one additive
    /// recovery step per clean window and reset the per-window counters.
    /// O(1) regardless of how much time passed.
    fn roll_windows(&mut self, now: Instant) {
        let Some(start) = self.window_start else {
            self.window_start = Some(now);
            return;
        };
        let elapsed = now.duration_since(start).as_micros() / RATE_WINDOW.as_micros();
        if elapsed == 0 {
            return;
        }
        let elapsed = u64::try_from(elapsed).unwrap_or(u64::MAX);
        // The first elapsed window carries the recorded state; any further
        // ones saw no events at all and are trivially clean.
        let mut clean = elapsed - 1;
        if !self.window_congested && self.window_naks == 0 {
            clean += 1;
        }
        if clean > 0 && self.rate < self.configured {
            let step = (self.configured * RECOVERY_PERCENT / 100).max(1);
            self.rate = self
                .rate
                .saturating_add(step.saturating_mul(clean))
                .min(self.configured);
        }
        let advance = RATE_WINDOW * u32::try_from(elapsed).unwrap_or(u32::MAX);
        self.window_start = Some(start + advance);
        self.window_naks = 0;
        self.window_congested = false;
    }

    /// Apply one multiplicative decrease, gated to once per [`RATE_WINDOW`]
    /// so a flurry of signals from a single congestion episode collapses the
    /// rate only once.
    fn backoff(&mut self, now: Instant) {
        self.window_congested = true;
        let gated = self
            .last_decrease_at
            .is_some_and(|t| now.duration_since(t) < RATE_WINDOW);
        if gated {
            return;
        }
        self.rate = (self.rate * BACKOFF_NUMERATOR / BACKOFF_DENOMINATOR).max(self.floor);
        self.last_decrease_at = Some(now);
    }
}

/// Bucket capacity in token units. 6000 bytes × 10^6 ≈ 6×10^9 — far inside
/// `i64` range, so the conversion can never fail.
fn capacity_units_i64() -> i64 {
    i64::try_from(BUCKET_CAPACITY_BYTES * TOKEN_SCALE).expect("capacity fits i64")
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    // ─── Token bucket: burst absorption + steady-state spacing ──────────────

    #[test]
    fn burst_of_a_few_mtus_is_absorbed_then_deferred() {
        let mut p = Pacer::new(1000);
        let t0 = Instant::now();
        // Bucket capacity is 4 MTUs (6000 bytes): four 1500-byte packets pass.
        for i in 0..4 {
            assert_eq!(
                p.allowance(t0, 1500),
                Allowance::Allow,
                "packet {i} of the initial burst must be absorbed"
            );
        }
        assert!(
            matches!(p.allowance(t0, 1500), Allowance::DeferUntil(_)),
            "5th packet must be deferred once the bucket is empty"
        );
    }

    #[test]
    fn steady_state_spacing_matches_configured_rate() {
        // 1500 B/s with 1500-byte packets → exactly one packet per second
        // once the initial bucket is drained.
        let mut p = Pacer::new(1500);
        let t0 = Instant::now();
        for _ in 0..4 {
            assert_eq!(p.allowance(t0, 1500), Allowance::Allow);
        }
        // Bucket empty: deficit = 1500 bytes → exactly 1 s at 1500 B/s.
        assert_eq!(p.allowance(t0, 1500), Allowance::DeferUntil(t0 + ms(1000)));
        // At the deferred instant the send is allowed…
        assert_eq!(p.allowance(t0 + ms(1000), 1500), Allowance::Allow);
        // …and the next packet is spaced one more second out.
        assert_eq!(
            p.allowance(t0 + ms(1000), 1500),
            Allowance::DeferUntil(t0 + ms(2000))
        );
    }

    #[test]
    fn defer_does_not_charge_the_bucket() {
        let mut p = Pacer::new(1000);
        let t0 = Instant::now();
        for _ in 0..4 {
            assert_eq!(p.allowance(t0, 1500), Allowance::Allow);
        }
        // Asking repeatedly while deferred must return the same instant —
        // deferrals are side-effect-free.
        let first = p.allowance(t0, 1000);
        let second = p.allowance(t0, 1000);
        assert_eq!(
            first, second,
            "repeated deferred queries must not consume budget"
        );
    }

    #[test]
    fn oversize_packet_is_allowed_via_deficit_and_delays_followers() {
        // A packet bigger than the whole bucket must still be sendable (the
        // bucket-full state allows it) and its overshoot is carried as a
        // deficit that pushes followers out.
        let mut p = Pacer::new(1000);
        let t0 = Instant::now();
        assert_eq!(p.allowance(t0, 10_000), Allowance::Allow);
        // tokens = 6000 − 10000 = −4000 bytes; next 1500-byte packet needs
        // 5500 bytes of refill → 5.5 s at 1000 B/s.
        assert_eq!(p.allowance(t0, 1500), Allowance::DeferUntil(t0 + ms(5500)));
    }

    // ─── RTT-driven back-off ─────────────────────────────────────────────────

    #[test]
    fn rtt_inflation_above_125_percent_of_baseline_backs_off() {
        let mut p = Pacer::new(1000);
        let t0 = Instant::now();
        p.on_ack(20_000, 0, t0); // baseline = 20 ms
        p.on_ack(24_000, 0, t0 + ms(10)); // 24 ms ≤ 25 ms threshold → clean
        assert_eq!(
            p.current_rate(),
            1000,
            "below-threshold RTT must not back off"
        );
        p.on_ack(26_000, 0, t0 + ms(20)); // 26 ms > 25 ms → back off
        assert_eq!(
            p.current_rate(),
            850,
            "rate must drop to 85% on RTT inflation"
        );
    }

    #[test]
    fn rttvar_gives_headroom_before_backoff() {
        let mut p = Pacer::new(1000);
        let t0 = Instant::now();
        p.on_ack(20_000, 0, t0); // baseline 20 ms
                                 // Threshold = 25 ms + 10 ms rttvar = 35 ms: 26 ms is jitter, not congestion.
        p.on_ack(26_000, 10_000, t0 + ms(10));
        assert_eq!(
            p.current_rate(),
            1000,
            "jitter within rttvar must not back off"
        );
        p.on_ack(40_000, 10_000, t0 + ms(20)); // 40 ms > 35 ms → congestion
        assert_eq!(p.current_rate(), 850);
    }

    #[test]
    fn baseline_tracks_the_minimum_rtt() {
        let mut p = Pacer::new(1000);
        let t0 = Instant::now();
        p.on_ack(30_000, 0, t0); // initial baseline 30 ms
        p.on_ack(20_000, 0, t0 + ms(10)); // lower sample re-bases to 20 ms
                                          // 26 ms would be fine against a 30 ms baseline but is inflated
                                          // against the re-based 20 ms one (threshold 25 ms).
        p.on_ack(26_000, 0, t0 + ms(20));
        assert_eq!(
            p.current_rate(),
            850,
            "baseline must follow the minimum RTT"
        );
    }

    #[test]
    fn zero_rtt_samples_are_ignored() {
        let mut p = Pacer::new(1000);
        let t0 = Instant::now();
        p.on_ack(0, 0, t0); // untracked peer field — must not become baseline
        p.on_ack(20_000, 0, t0 + ms(10));
        assert_eq!(
            p.current_rate(),
            1000,
            "first real sample is the baseline, no back-off"
        );
        p.on_ack(26_000, 0, t0 + ms(20));
        assert_eq!(p.current_rate(), 850, "real baseline must be 20 ms, not 0");
    }

    // ─── NAK-driven back-off ─────────────────────────────────────────────────

    #[test]
    fn nak_burst_above_threshold_backs_off() {
        let mut p = Pacer::new(1000);
        let t0 = Instant::now();
        p.on_nak(4, t0); // 4 > 3 → congestion
        assert_eq!(p.current_rate(), 850);
    }

    #[test]
    fn nak_counts_accumulate_within_a_window() {
        let mut p = Pacer::new(1000);
        let t0 = Instant::now();
        p.on_nak(2, t0);
        assert_eq!(p.current_rate(), 1000, "2 NAKs are below the threshold");
        p.on_nak(2, t0 + ms(10)); // running total 4 > 3 in the same window
        assert_eq!(
            p.current_rate(),
            850,
            "accumulated NAKs must trigger back-off"
        );
    }

    #[test]
    fn nak_counts_reset_at_window_boundaries() {
        let mut p = Pacer::new(1000);
        let t0 = Instant::now();
        p.on_nak(3, t0);
        p.on_nak(3, t0 + ms(150)); // new window — counter restarted at 0
        assert_eq!(
            p.current_rate(),
            1000,
            "NAKs spread across windows below the per-window threshold must not back off"
        );
    }

    // ─── Decrease gating, floor clamp, recovery ──────────────────────────────

    #[test]
    fn at_most_one_decrease_per_window() {
        let mut p = Pacer::new(1000);
        let t0 = Instant::now();
        p.on_nak(10, t0); // → 850
        p.on_nak(10, t0 + ms(10)); // same window: gated
        p.on_ack(20_000, 0, t0 + ms(20));
        p.on_ack(90_000, 0, t0 + ms(30)); // RTT signal in same window: gated too
        assert_eq!(
            p.current_rate(),
            850,
            "only one multiplicative decrease per window"
        );
        p.on_nak(10, t0 + ms(150)); // next window: a fresh decrease may fire
        assert_eq!(p.current_rate(), 722, "850 * 85 / 100 = 722");
    }

    #[test]
    fn rate_is_floored_at_5_percent_of_configured() {
        // A continuous storm: one NAK burst per window, so no window is ever
        // clean and every window may fire one decrease.
        let mut p = Pacer::new(1000);
        let t0 = Instant::now();
        for i in 0..40 {
            p.on_nak(10, t0 + ms(100 * (i + 1)));
        }
        assert_eq!(
            p.current_rate(),
            50,
            "sustained NAK storm must clamp at 5% of the configured rate"
        );
    }

    #[test]
    fn clean_windows_recover_additively_toward_configured() {
        let mut p = Pacer::new(1000);
        let t0 = Instant::now();
        p.on_ack(20_000, 0, t0);
        p.on_nak(10, t0); // → 850, window marked congested
                          // The congested window itself grants no recovery…
        p.on_ack(20_000, 0, t0 + ms(150));
        assert_eq!(
            p.current_rate(),
            850,
            "congested window must not grant recovery"
        );
        // …but each following clean window adds 5% of configured (50 B/s).
        p.on_ack(20_000, 0, t0 + ms(250));
        assert_eq!(p.current_rate(), 900);
        p.on_ack(20_000, 0, t0 + ms(350));
        assert_eq!(p.current_rate(), 950);
        p.on_ack(20_000, 0, t0 + ms(450));
        assert_eq!(p.current_rate(), 1000);
        // Recovery never overshoots the configured maximum.
        p.on_ack(20_000, 0, t0 + ms(950));
        assert_eq!(
            p.current_rate(),
            1000,
            "recovery must cap at the configured rate"
        );
    }

    #[test]
    fn several_elapsed_clean_windows_grant_several_steps_at_once() {
        let mut p = Pacer::new(1000);
        let t0 = Instant::now();
        p.on_nak(10, t0); // → 850
                          // 5 full windows elapse before the next signal: the congested one
                          // grants nothing, the 4 idle ones are clean → +200, capped at the
                          // configured 1000 (850 + 200 would overshoot).
        p.on_ack(20_000, 0, t0 + ms(550));
        assert_eq!(p.current_rate(), 1000);
    }

    #[test]
    fn backed_off_rate_slows_the_token_refill() {
        let mut p = Pacer::new(1000);
        let t0 = Instant::now();
        // Drain the bucket.
        for _ in 0..4 {
            assert_eq!(p.allowance(t0, 1500), Allowance::Allow);
        }
        p.on_nak(10, t0); // rate → 850
                          // 1500-byte deficit at 850 B/s = ceil(1.5e9 / 850) µs = 1_764_706 µs,
                          // visibly later than the 1.5 s it would take at the full rate.
        let Allowance::DeferUntil(at) = p.allowance(t0, 1500) else {
            panic!("bucket is empty — must defer");
        };
        assert_eq!(at, t0 + Duration::from_micros(1_764_706));
    }

    // ─── Construction edge cases ─────────────────────────────────────────────

    #[test]
    fn zero_configured_rate_is_clamped_to_one() {
        let mut p = Pacer::new(0);
        assert_eq!(p.configured_rate(), 1);
        assert_eq!(p.current_rate(), 1);
        // Still functional: a full bucket allows an MTU immediately.
        let t0 = Instant::now();
        assert_eq!(p.allowance(t0, 1500), Allowance::Allow);
    }

    #[test]
    fn floor_is_at_least_one_byte_per_second() {
        let mut p = Pacer::new(10); // 5% of 10 would truncate to 0
        let t0 = Instant::now();
        for i in 0..200 {
            p.on_nak(10, t0 + ms(100 * (i + 1)));
        }
        assert_eq!(p.current_rate(), 1, "floor must clamp at ≥ 1 B/s");
    }
}
