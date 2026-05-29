//! In-memory cost / call-rate budget for paid AI work.
//!
//! The AI worker calls **paid** upstreams (Anthropic / Voyage). A retry storm or
//! an abusive tenant could otherwise burn an unbounded number of paid calls
//! (ROADMAP 方向三: 成本治理). [`CostBudget`] is a simple fixed-window throttle:
//! at most `max_per_window` admissions per rolling `window`. When the window is
//! exhausted the worker *defers* (backs off) rather than continuing to spend.
//!
//! Deliberately simple and in-memory (per worker process). Cross-process /
//! per-tenant quotas would live in Redis — out of scope here and called out in
//! the ROADMAP. The window length and ceiling are both configurable.
//!
//! Thread-safe: a single [`CostBudget`] is shared by all concurrent in-flight
//! jobs via `&` and guarded by a `Mutex`. The critical section is O(1).

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Pluggable clock so the budget's window logic is unit-testable without sleeps.
trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// Real monotonic clock used in production.
struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

struct State {
    /// Admissions counted in the current window.
    used: u32,
    /// When the current window began.
    window_start: Instant,
}

/// A fixed-window call/cost budget.
///
/// `try_acquire` returns `true` if there is remaining budget in the current
/// window (consuming one unit) and `false` once the ceiling is hit. The window
/// resets automatically once `window` has elapsed since it began.
pub struct CostBudget {
    max_per_window: u32,
    window: Duration,
    state: Mutex<State>,
    clock: Box<dyn Clock>,
}

impl std::fmt::Debug for CostBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CostBudget")
            .field("max_per_window", &self.max_per_window)
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

impl CostBudget {
    /// Create a budget admitting at most `max_per_window` units per rolling
    /// `window`. A `max_per_window` of 0 is clamped to 1 so the worker can never
    /// wedge permanently on a misconfiguration.
    #[must_use]
    pub fn new(max_per_window: u32, window: Duration) -> Self {
        Self::with_clock(max_per_window, window, Box::new(SystemClock))
    }

    fn with_clock(max_per_window: u32, window: Duration, clock: Box<dyn Clock>) -> Self {
        let now = clock.now();
        Self {
            max_per_window: max_per_window.max(1),
            window,
            state: Mutex::new(State { used: 0, window_start: now }),
            clock,
        }
    }

    /// Try to consume one unit of budget.
    ///
    /// Returns `true` and decrements remaining budget when capacity exists;
    /// returns `false` when the current window is exhausted.
    pub fn try_acquire(&self) -> bool {
        self.acquire_up_to(1) == 1
    }

    /// Consume up to `n` units atomically, returning how many were granted
    /// (`0..=n`). The single primitive both `try_acquire` and the worker's
    /// batch-sizing use, so the window roll-over is handled in exactly one place.
    ///
    /// The worker calls this to decide how large a batch to claim: it never
    /// claims more jobs than it has budget to pay for, so a claimed job is never
    /// orphaned waiting on budget.
    pub fn acquire_up_to(&self, n: u32) -> u32 {
        let now = self.clock.now();
        let mut st = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if now.duration_since(st.window_start) >= self.window {
            st.used = 0;
            st.window_start = now;
        }
        let remaining = self.max_per_window.saturating_sub(st.used);
        let grant = remaining.min(n);
        st.used += grant;
        grant
    }

    /// Units remaining in the current window without consuming any.
    #[must_use]
    pub fn available(&self) -> u32 {
        self.max_per_window.saturating_sub(self.used())
    }

    /// Units already consumed in the current window (for diagnostics / metrics).
    #[must_use]
    pub fn used(&self) -> u32 {
        let now = self.clock.now();
        let st = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if now.duration_since(st.window_start) >= self.window {
            0
        } else {
            st.used
        }
    }

    /// The configured per-window ceiling.
    #[must_use]
    pub fn limit(&self) -> u32 {
        self.max_per_window
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Test clock whose "now" is driven manually.
    struct FakeClock {
        base: Instant,
        offset_ms: AtomicU64,
    }
    impl FakeClock {
        fn new() -> Self {
            Self { base: Instant::now(), offset_ms: AtomicU64::new(0) }
        }
        fn advance(&self, d: Duration) {
            self.offset_ms.fetch_add(
                u64::try_from(d.as_millis()).unwrap_or(u64::MAX),
                Ordering::SeqCst,
            );
        }
    }
    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            self.base + Duration::from_millis(self.offset_ms.load(Ordering::SeqCst))
        }
    }

    // FakeClock is shared between the test body and the budget; wrap in Arc and
    // adapt via a thin Clock newtype so both can hold it.
    use std::sync::Arc;
    struct SharedClock(Arc<FakeClock>);
    impl Clock for SharedClock {
        fn now(&self) -> Instant {
            self.0.now()
        }
    }

    fn budget_with(max: u32, window: Duration) -> (CostBudget, Arc<FakeClock>) {
        let clk = Arc::new(FakeClock::new());
        let b = CostBudget::with_clock(max, window, Box::new(SharedClock(Arc::clone(&clk))));
        (b, clk)
    }

    #[test]
    fn admits_up_to_limit_then_denies() {
        let (b, _clk) = budget_with(3, Duration::from_secs(60));
        assert!(b.try_acquire());
        assert!(b.try_acquire());
        assert!(b.try_acquire());
        assert!(!b.try_acquire(), "4th acquire over a limit of 3 must be denied");
        assert!(!b.try_acquire());
        assert_eq!(b.used(), 3);
        assert_eq!(b.limit(), 3);
    }

    #[test]
    fn acquire_up_to_grants_partial_then_zero() {
        let (b, _clk) = budget_with(5, Duration::from_secs(60));
        assert_eq!(b.available(), 5);
        // Ask for more than remains → granted only what's left.
        assert_eq!(b.acquire_up_to(3), 3);
        assert_eq!(b.available(), 2);
        assert_eq!(b.acquire_up_to(10), 2, "clamped to remaining");
        assert_eq!(b.available(), 0);
        assert_eq!(b.acquire_up_to(4), 0, "nothing left this window");
    }

    #[test]
    fn acquire_up_to_zero_is_a_noop() {
        let (b, _clk) = budget_with(5, Duration::from_secs(60));
        assert_eq!(b.acquire_up_to(0), 0);
        assert_eq!(b.available(), 5);
    }

    #[test]
    fn window_resets_after_elapsing() {
        let (b, clk) = budget_with(2, Duration::from_secs(10));
        assert!(b.try_acquire());
        assert!(b.try_acquire());
        assert!(!b.try_acquire(), "exhausted within window");

        // Not yet elapsed.
        clk.advance(Duration::from_secs(9));
        assert!(!b.try_acquire(), "still same window at t=9s");

        // Window elapsed → budget refreshes.
        clk.advance(Duration::from_secs(2)); // t = 11s
        assert!(b.try_acquire(), "new window should admit again");
        assert!(b.try_acquire());
        assert!(!b.try_acquire(), "new window exhausted at limit again");
    }

    #[test]
    fn used_reports_zero_in_a_fresh_window() {
        let (b, clk) = budget_with(5, Duration::from_secs(10));
        assert!(b.try_acquire());
        assert_eq!(b.used(), 1);
        clk.advance(Duration::from_secs(11));
        // No acquire yet, but the window has rolled — used() reflects a fresh window.
        assert_eq!(b.used(), 0);
    }

    #[test]
    fn zero_limit_is_clamped_to_one() {
        // A misconfigured 0 must not wedge the worker forever.
        let (b, _clk) = budget_with(0, Duration::from_secs(60));
        assert_eq!(b.limit(), 1);
        assert!(b.try_acquire());
        assert!(!b.try_acquire());
    }

    #[test]
    fn concurrent_acquire_never_exceeds_limit() {
        use std::sync::Arc as StdArc;
        use std::thread;

        let b = StdArc::new(CostBudget::new(50, Duration::from_secs(600)));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let b = StdArc::clone(&b);
            handles.push(thread::spawn(move || {
                let mut granted = 0u32;
                for _ in 0..100 {
                    if b.try_acquire() {
                        granted += 1;
                    }
                }
                granted
            }));
        }
        let total: u32 = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(total, 50, "exactly limit-many acquisitions across all threads");
    }
}
