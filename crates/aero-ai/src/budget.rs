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

use std::collections::HashMap;
use std::hash::Hash;
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

impl State {
    /// A fresh, empty window anchored at `now`.
    fn fresh(now: Instant) -> Self {
        Self { used: 0, window_start: now }
    }

    /// Roll over to a new window if `window` has elapsed since this one began.
    fn roll_if_elapsed(&mut self, now: Instant, window: Duration) {
        if now.duration_since(self.window_start) >= window {
            self.used = 0;
            self.window_start = now;
        }
    }

    /// Roll if needed, then consume up to `n` units, returning how many were
    /// granted (`0..=n`). The one place the window math lives, shared by the
    /// global and keyed budgets.
    fn acquire_up_to(&mut self, now: Instant, window: Duration, max: u32, n: u32) -> u32 {
        self.roll_if_elapsed(now, window);
        let grant = max.saturating_sub(self.used).min(n);
        self.used += grant;
        grant
    }

    /// Consume exactly `n` units iff that many remain this window (all-or-
    /// nothing); returns whether granted. Unlike [`Self::acquire_up_to`], a
    /// partial amount is never consumed — used for weighted charging where an
    /// unaffordable job must be deferred, not half-charged.
    fn try_acquire_n(&mut self, now: Instant, window: Duration, max: u32, n: u32) -> bool {
        self.roll_if_elapsed(now, window);
        if max.saturating_sub(self.used) >= n {
            self.used = self.used.saturating_add(n);
            true
        } else {
            false
        }
    }

    /// Units consumed in the current window, accounting for an elapsed roll-over
    /// (without mutating: a stale window reads as empty).
    fn used_now(&self, now: Instant, window: Duration) -> u32 {
        if now.duration_since(self.window_start) >= window {
            0
        } else {
            self.used
        }
    }
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
            state: Mutex::new(State::fresh(now)),
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
        st.acquire_up_to(now, self.window, self.max_per_window, n)
    }

    /// Atomically consume `n` units iff the current window has at least `n`
    /// remaining (all-or-nothing); returns whether granted. Used by the worker to
    /// charge a job's cost WEIGHT and defer the job when the window can't afford
    /// it (ROADMAP 方向四). `n == 0` is always granted and consumes nothing.
    pub fn try_acquire_n(&self, n: u32) -> bool {
        if n == 0 {
            return true;
        }
        let now = self.clock.now();
        let mut st = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        st.try_acquire_n(now, self.window, self.max_per_window, n)
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
        st.used_now(now, self.window)
    }

    /// The configured per-window ceiling.
    #[must_use]
    pub fn limit(&self) -> u32 {
        self.max_per_window
    }
}

/// A per-key (per-tenant) fixed-window cost budget.
///
/// Maintains an **independent** [`CostBudget`]-style window for each key `K`,
/// every window capped at the same `max_per_window` / `window`. Exhausting one
/// key's budget leaves every other key untouched — so one abusive workspace
/// cannot starve the others of paid AI capacity (ROADMAP 方向三: per-tenant
/// token/cost budget).
///
/// The intended key is [`aero_common::WorkspaceId`], but the type stays generic
/// over any `K: Eq + Hash + Clone` so it is trivially unit-testable and reusable.
/// A key's window is created **lazily** on first touch, so unseen keys cost
/// nothing.
///
/// Thread-safe: shared by all concurrent in-flight jobs via `&self`; a single
/// `Mutex<HashMap<K, _>>` guards the per-key windows. The critical section is
/// O(1) per call (one hash lookup + the same O(1) window math as [`CostBudget`]).
/// A `Mutex<HashMap>` (rather than a sharded map) is deliberate: keys are
/// few (workspaces) and the section is tiny, matching the global budget's design.
pub struct KeyedCostBudget<K: Eq + Hash + Clone> {
    max_per_window: u32,
    window: Duration,
    max_keys: usize,
    windows: Mutex<HashMap<K, State>>,
    clock: Box<dyn Clock>,
}

/// Default ceiling on simultaneously-tracked keys. Bounds memory so an
/// attacker-influenceable key space (e.g. mass workspace creation) cannot grow
/// the map without limit. Far above any realistic count of concurrently-active
/// tenants; elapsed windows are reclaimed before this is hit.
pub const DEFAULT_MAX_KEYS: usize = 100_000;

impl<K: Eq + Hash + Clone> std::fmt::Debug for KeyedCostBudget<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let live = self
            .windows
            .lock()
            .map_or_else(|e| e.into_inner().len(), |w| w.len());
        f.debug_struct("KeyedCostBudget")
            .field("max_per_window", &self.max_per_window)
            .field("window", &self.window)
            .field("live_keys", &live)
            .finish_non_exhaustive()
    }
}

impl<K: Eq + Hash + Clone> KeyedCostBudget<K> {
    /// Create a keyed budget admitting at most `max_per_window` units per rolling
    /// `window` **per key**. As with [`CostBudget`], a `max_per_window` of 0 is
    /// clamped to 1 so a misconfiguration can never wedge a key permanently.
    #[must_use]
    pub fn new(max_per_window: u32, window: Duration) -> Self {
        Self::with_clock(max_per_window, window, Box::new(SystemClock))
    }

    fn with_clock(max_per_window: u32, window: Duration, clock: Box<dyn Clock>) -> Self {
        Self {
            max_per_window: max_per_window.max(1),
            window,
            max_keys: DEFAULT_MAX_KEYS,
            windows: Mutex::new(HashMap::new()),
            clock,
        }
    }

    /// Override the live-key ceiling (default [`DEFAULT_MAX_KEYS`]). Once this
    /// many keys have active (non-elapsed) windows, a *new* key is denied rather
    /// than tracked, bounding memory. `0` is clamped to 1.
    #[must_use]
    pub fn with_max_keys(mut self, max_keys: usize) -> Self {
        self.max_keys = max_keys.max(1);
        self
    }

    /// Try to consume one unit of `key`'s budget.
    ///
    /// Returns `true` (consuming one unit) when `key`'s current window has
    /// capacity, `false` once that key's window is exhausted. Other keys are
    /// unaffected. Creates the key's window lazily on first touch.
    pub fn try_acquire(&self, key: K) -> bool {
        self.acquire_up_to(key, 1) == 1
    }

    /// Consume up to `n` units of `key`'s budget atomically, returning how many
    /// were granted (`0..=n`). Mirrors [`CostBudget::acquire_up_to`] but scoped to
    /// one key; the key's window is created lazily on first touch.
    pub fn acquire_up_to(&self, key: K, n: u32) -> u32 {
        // A zero-unit request never consumes budget, so it must never allocate a
        // map entry — otherwise a flood of `acquire_up_to(unique_key, 0)` would
        // grow the map unbounded for free.
        if n == 0 {
            return 0;
        }
        let now = self.clock.now();
        let mut map = self.windows.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !map.contains_key(&key) {
            // A new key is about to be tracked. First reclaim any windows that
            // have fully elapsed (dropping a zero-use rolled-over window is
            // behaviourally identical to recreating it fresh on next touch), then
            // enforce the live-key ceiling so an unbounded key space cannot
            // translate into unbounded memory.
            if map.len() >= self.max_keys {
                map.retain(|_, st| st.used_now(now, self.window) > 0);
            }
            if map.len() >= self.max_keys {
                return 0;
            }
        }
        let st = map.entry(key).or_insert_with(|| State::fresh(now));
        st.acquire_up_to(now, self.window, self.max_per_window, n)
    }

    /// Units remaining in `key`'s current window without consuming any.
    ///
    /// An untouched key reports the full [`Self::limit`] (its window is created
    /// lazily, so a read alone never allocates an entry).
    #[must_use]
    pub fn available(&self, key: &K) -> u32 {
        self.max_per_window.saturating_sub(self.used(key))
    }

    /// Units already consumed in `key`'s current window (diagnostics / metrics).
    ///
    /// Reports 0 for an untouched key or one whose window has rolled over. This
    /// is a pure read: it neither creates an entry nor mutates a stale window.
    #[must_use]
    pub fn used(&self, key: &K) -> u32 {
        let now = self.clock.now();
        let map = self.windows.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get(key).map_or(0, |st| st.used_now(now, self.window))
    }

    /// The configured per-window ceiling (identical for every key).
    #[must_use]
    pub fn limit(&self) -> u32 {
        self.max_per_window
    }

    /// Number of keys currently holding a tracked window (test/diagnostic).
    #[cfg(test)]
    fn live_keys(&self) -> usize {
        self.windows.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len()
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

    fn keyed_budget_with(
        max: u32,
        window: Duration,
    ) -> (KeyedCostBudget<&'static str>, Arc<FakeClock>) {
        let clk = Arc::new(FakeClock::new());
        let b = KeyedCostBudget::with_clock(max, window, Box::new(SharedClock(Arc::clone(&clk))));
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
    fn try_acquire_n_is_all_or_nothing() {
        let (b, _clk) = budget_with(5, Duration::from_secs(60));
        // Affordable amount is consumed in full.
        assert!(b.try_acquire_n(3));
        assert_eq!(b.available(), 2);
        // An amount larger than what remains is REFUSED and consumes nothing —
        // so a partially-affordable weighted job is deferred, not half-charged.
        assert!(!b.try_acquire_n(3));
        assert_eq!(b.available(), 2, "refused request must not consume");
        // Exactly-remaining is granted; zero is always granted and free.
        assert!(b.try_acquire_n(2));
        assert_eq!(b.available(), 0);
        assert!(b.try_acquire_n(0));
        assert!(!b.try_acquire_n(1));
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

    // ---- KeyedCostBudget ----------------------------------------------------

    #[test]
    fn keyed_each_key_admits_up_to_limit_then_denies() {
        let (b, _clk) = keyed_budget_with(3, Duration::from_secs(60));
        assert_eq!(b.limit(), 3);
        for _ in 0..3 {
            assert!(b.try_acquire("a"));
        }
        assert!(!b.try_acquire("a"), "4th acquire over a limit of 3 must be denied");
        assert_eq!(b.used(&"a"), 3);
        assert_eq!(b.available(&"a"), 0);
    }

    #[test]
    fn keyed_exhausting_one_key_leaves_others_with_full_budget() {
        // The core isolation guarantee: an abusive tenant cannot starve others.
        let (b, _clk) = keyed_budget_with(3, Duration::from_secs(60));

        // Drain key "a" completely.
        assert_eq!(b.acquire_up_to("a", 100), 3);
        assert!(!b.try_acquire("a"), "a is exhausted");
        assert_eq!(b.used(&"a"), 3);

        // "b" is entirely unaffected and still has its full, independent limit.
        assert_eq!(b.available(&"b"), 3, "untouched key reports the full ceiling");
        assert_eq!(b.used(&"b"), 0);
        assert!(b.try_acquire("b"));
        assert!(b.try_acquire("b"));
        assert!(b.try_acquire("b"));
        assert!(!b.try_acquire("b"), "b now exhausted on its own window");

        // Draining "b" still did not give "a" anything back.
        assert!(!b.try_acquire("a"));
        // And a third, never-touched key remains fully available.
        assert_eq!(b.available(&"c"), 3);
    }

    #[test]
    fn keyed_untouched_key_reads_do_not_allocate_or_consume() {
        let (b, _clk) = keyed_budget_with(4, Duration::from_secs(60));
        // Pure reads on a key we never acquired against.
        assert_eq!(b.used(&"ghost"), 0);
        assert_eq!(b.available(&"ghost"), 4);
        // Those reads must not have created a window: the key still has full budget.
        assert_eq!(b.acquire_up_to("ghost", 4), 4);
        assert_eq!(b.available(&"ghost"), 0);
    }

    #[test]
    fn keyed_acquire_up_to_grants_partial_then_zero_per_key() {
        let (b, _clk) = keyed_budget_with(5, Duration::from_secs(60));
        assert_eq!(b.available(&"a"), 5);
        assert_eq!(b.acquire_up_to("a", 3), 3);
        assert_eq!(b.available(&"a"), 2);
        assert_eq!(b.acquire_up_to("a", 10), 2, "clamped to remaining");
        assert_eq!(b.available(&"a"), 0);
        assert_eq!(b.acquire_up_to("a", 4), 0, "nothing left this window for a");

        // A different key is on its own fresh budget.
        assert_eq!(b.acquire_up_to("b", 10), 5, "b clamped to its own full limit");
        assert_eq!(b.available(&"b"), 0);
    }

    #[test]
    fn keyed_acquire_up_to_zero_is_a_noop() {
        let (b, _clk) = keyed_budget_with(5, Duration::from_secs(60));
        assert_eq!(b.acquire_up_to("a", 0), 0);
        assert_eq!(b.available(&"a"), 5);
        // A zero-unit request must NOT allocate a window — otherwise a flood of
        // unique zero-requests would grow the map unbounded (memory DoS).
        assert_eq!(b.live_keys(), 0, "n=0 must not allocate a key entry");
    }

    #[test]
    fn keyed_live_key_ceiling_denies_new_keys_then_eviction_reclaims() {
        // Cap at 2 live keys, 10s windows.
        let clk = Arc::new(FakeClock::new());
        let b = KeyedCostBudget::<&'static str>::with_clock(
            3,
            Duration::from_secs(10),
            Box::new(SharedClock(Arc::clone(&clk))),
        )
        .with_max_keys(2);

        // Two distinct active keys fill the map to capacity.
        assert!(b.try_acquire("a"));
        assert!(b.try_acquire("b"));
        assert_eq!(b.live_keys(), 2);

        // A third, brand-new key is DENIED (not tracked) while the map is full of
        // still-active windows — memory is bounded.
        assert_eq!(b.acquire_up_to("c", 1), 0, "new key denied at capacity");
        assert_eq!(b.live_keys(), 2, "denied key was not inserted");

        // Existing keys still work at capacity.
        assert!(b.try_acquire("a"), "already-tracked key unaffected by the cap");

        // Once the active windows elapse, the next new key triggers eviction of
        // the rolled-over windows and is admitted again.
        clk.advance(Duration::from_secs(11));
        assert!(b.try_acquire("c"), "new key admitted after stale windows reclaimed");
        assert!(b.live_keys() <= 2, "map stays within the key ceiling");
    }

    #[test]
    fn keyed_windows_reset_independently_after_elapsing() {
        let (b, clk) = keyed_budget_with(2, Duration::from_secs(10));

        // Exhaust "a" at t=0.
        assert!(b.try_acquire("a"));
        assert!(b.try_acquire("a"));
        assert!(!b.try_acquire("a"), "a exhausted within window");

        // Advance 5s, then first-touch "b" — its window is anchored at t=5s, so it
        // rolls over on a *different* schedule than "a".
        clk.advance(Duration::from_secs(5));
        assert!(b.try_acquire("b"));
        assert!(b.try_acquire("b"));
        assert!(!b.try_acquire("b"), "b exhausted within its own window");

        // t=11s: a's window (started t=0) has elapsed and refreshes...
        clk.advance(Duration::from_secs(6));
        assert!(b.try_acquire("a"), "a's new window admits again");
        assert!(b.try_acquire("a"));
        assert!(!b.try_acquire("a"), "a's new window exhausted at limit again");
        // ...but b's window (started t=5s) has NOT yet elapsed at t=11s.
        assert!(!b.try_acquire("b"), "b still in its first window at t=11s");

        // t=16s: b's window has now elapsed too and refreshes.
        clk.advance(Duration::from_secs(5));
        assert!(b.try_acquire("b"), "b's new window admits again");
        assert!(b.try_acquire("b"));
        assert!(!b.try_acquire("b"));
    }

    #[test]
    fn keyed_used_reports_zero_in_a_fresh_window_per_key() {
        let (b, clk) = keyed_budget_with(5, Duration::from_secs(10));
        assert!(b.try_acquire("a"));
        assert_eq!(b.used(&"a"), 1);
        clk.advance(Duration::from_secs(11));
        // No acquire yet, but a's window has rolled — used() reflects a fresh window.
        assert_eq!(b.used(&"a"), 0);
        assert_eq!(b.available(&"a"), 5);
    }

    #[test]
    fn keyed_zero_limit_is_clamped_to_one_per_key() {
        let (b, _clk) = keyed_budget_with(0, Duration::from_secs(60));
        assert_eq!(b.limit(), 1);
        assert!(b.try_acquire("a"));
        assert!(!b.try_acquire("a"));
        // Independent key still gets its own single unit.
        assert!(b.try_acquire("b"));
        assert!(!b.try_acquire("b"));
    }

    #[test]
    fn keyed_concurrent_same_key_never_exceeds_that_keys_limit() {
        use std::sync::Arc as StdArc;
        use std::thread;

        let b = StdArc::new(KeyedCostBudget::<&'static str>::new(50, Duration::from_secs(600)));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let b = StdArc::clone(&b);
            handles.push(thread::spawn(move || {
                let mut granted = 0u32;
                for _ in 0..100 {
                    if b.try_acquire("hot") {
                        granted += 1;
                    }
                }
                granted
            }));
        }
        let total: u32 = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(total, 50, "exactly limit-many acquisitions on the contended key");
    }

    #[test]
    fn keyed_concurrent_distinct_keys_each_get_their_own_full_limit() {
        use std::sync::Arc as StdArc;
        use std::thread;

        // 8 threads, each hammering a distinct key concurrently. Every key must
        // independently grant exactly its full limit — no cross-key interference.
        let b = StdArc::new(KeyedCostBudget::<u32>::new(50, Duration::from_secs(600)));
        let mut handles = Vec::new();
        for k in 0..8u32 {
            let b = StdArc::clone(&b);
            handles.push(thread::spawn(move || {
                let mut granted = 0u32;
                for _ in 0..100 {
                    if b.try_acquire(k) {
                        granted += 1;
                    }
                }
                granted
            }));
        }
        let total: u32 = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(total, 8 * 50, "each of the 8 keys granted its full independent limit");
    }
}
