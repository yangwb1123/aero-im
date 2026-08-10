//! Optional per-account login throttle / lockout (ROADMAP5 方向五 — auth-abuse depth).
//!
//! After `max_failures` failed logins within `window_secs`, an account is locked
//! for `lockout_secs`, so credential-stuffing against a *single* account is blunted
//! — depth beyond the existing per-IP rate limit (which a distributed attacker
//! sidesteps by rotating IPs).
//!
//! **OFF by default** (opt-in via `AERO_LOGIN_LOCKOUT`). A hard lockout can be
//! abused to deny a victim service, and NIST 800-63B cautions against it as the
//! *sole* control — so operators enable it deliberately, alongside the per-IP limit
//! and 2FA, rather than it being forced on.
//!
//! ## Storage backend
//!
//! The failure accounting lives behind a [`FailureStore`] seam with two impls:
//!
//! * **In-process** (default): `Mutex<HashMap>`, each node locks independently
//!   with no DB round-trip on the hot login path — sufficient for brute-force
//!   defense against a single node. The decision logic ([`FailureState`]) is a
//!   pure function of prior state + an injected `now`, so every transition is
//!   unit-tested.
//! * **Redis** (opt-in via `AERO_LOGIN_LOCKOUT_REDIS`): a shared `INCR`/`EXPIRE`
//!   window counter + a `SETEX` lock key, so a *distributed* attacker spraying the
//!   same account across nodes is aggregated and locked out cluster-wide rather
//!   than starting fresh on each node. Strictly **fail-open** — any Redis error
//!   treats the account as *not* locked and silently drops the failure record, so
//!   a Redis outage degrades to "no lockout", never to a self-inflicted `DoS` that
//!   locks legitimate users out. Uses a TTL-based sliding window (Redis server
//!   time) rather than the in-process explicit `first_failure_at` window; the two
//!   are behaviourally equivalent for the threshold semantics that matter here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use fred::prelude::{KeysInterface, RedisClient};
use fred::types::Expiration;

/// Lockout policy tunables.
#[derive(Debug, Clone, Copy)]
pub struct LockoutConfig {
    /// Failures within `window_secs` that trip a lockout.
    pub max_failures: u32,
    /// Sliding window (seconds) over which failures accumulate.
    pub window_secs: i64,
    /// How long (seconds) an account stays locked once tripped.
    pub lockout_secs: i64,
}

impl Default for LockoutConfig {
    /// 5 failures in 5 minutes → locked for 15 minutes.
    fn default() -> Self {
        Self {
            max_failures: 5,
            window_secs: 300,
            lockout_secs: 900,
        }
    }
}

/// Per-account failure accounting. Pure value type — all transitions are total
/// functions of the prior state, an injected `now` (unix seconds), and the config.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FailureState {
    /// Failures in the current window (reset to 0 the moment a lock trips).
    count: u32,
    /// When the current failure window started.
    first_failure_at: i64,
    /// Locked while `now < locked_until`; `0` = not locked.
    locked_until: i64,
}

impl FailureState {
    fn is_locked(&self, now: i64) -> bool {
        now < self.locked_until
    }

    /// Fold a failed attempt into the next state.
    fn after_failure(self, now: i64, cfg: &LockoutConfig) -> FailureState {
        // While locked the window is authoritative — further failures don't extend it.
        if self.is_locked(now) {
            return self;
        }
        let within_window = self.count > 0 && now - self.first_failure_at <= cfg.window_secs;
        let count = if within_window { self.count + 1 } else { 1 };
        let first = if within_window {
            self.first_failure_at
        } else {
            now
        };
        if count >= cfg.max_failures {
            // Trip the lock and reset the counter so re-locking needs a fresh run
            // of failures after the lockout expires (not a single one).
            FailureState {
                count: 0,
                first_failure_at: 0,
                locked_until: now + cfg.lockout_secs,
            }
        } else {
            FailureState {
                count,
                first_failure_at: first,
                locked_until: 0,
            }
        }
    }
}

/// Pluggable failure-accounting backend. Keys arrive already normalized by
/// [`LoginThrottle::key`]. Implementations MUST be fail-open: a backend error on
/// `is_locked` returns `false` (never lock a user out because storage hiccuped).
#[async_trait]
trait FailureStore: Send + Sync {
    async fn is_locked(&self, key: &str, now: i64) -> bool;
    async fn record_failure(&self, key: &str, now: i64);
    async fn record_success(&self, key: &str);
    /// Evict entries that can no longer affect any decision. Default no-op — the
    /// Redis backend self-expires via TTL, so only the in-process map needs it.
    /// Returns the number of entries removed.
    async fn sweep(&self, _now: i64) -> usize {
        0
    }
}

/// In-process `Mutex<HashMap>` backend (the historical default). No `.await` is
/// held across the lock, so the async wrapper is `Send`.
struct InProcessFailureStore {
    states: Mutex<HashMap<String, FailureState>>,
    cfg: LockoutConfig,
}

#[async_trait]
impl FailureStore for InProcessFailureStore {
    async fn is_locked(&self, key: &str, now: i64) -> bool {
        let states = self.states.lock().unwrap_or_else(PoisonError::into_inner);
        states.get(key).is_some_and(|s| s.is_locked(now))
    }

    async fn record_failure(&self, key: &str, now: i64) {
        let mut states = self.states.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = states.entry(key.to_string()).or_default();
        *entry = entry.after_failure(now, &self.cfg);
    }

    async fn record_success(&self, key: &str) {
        let mut states = self.states.lock().unwrap_or_else(PoisonError::into_inner);
        states.remove(key);
    }

    async fn sweep(&self, now: i64) -> usize {
        let mut states = self.states.lock().unwrap_or_else(PoisonError::into_inner);
        let before = states.len();
        // Keep an entry only while it can still gate a login: it is currently
        // locked, OR it holds failures within the live accumulation window. Past
        // both (lock expired AND window lapsed) it is indistinguishable from an
        // absent key — `record_failure` would restart from a clean state — so
        // dropping it changes no future decision. Without this, every failed
        // attempt against a DISTINCT account (incl. non-existent emails) leaves a
        // permanent entry only a process restart would clear.
        states.retain(|_, s| {
            s.is_locked(now) || (s.count > 0 && now - s.first_failure_at <= self.cfg.window_secs)
        });
        before - states.len()
    }
}

/// Redis-backed cross-node backend (opt-in). A `SETEX` lock key gates an account;
/// an `INCR`+`EXPIRE` window counter aggregates failures cluster-wide. Fail-open.
struct RedisFailureStore {
    client: RedisClient,
    cfg: LockoutConfig,
}

impl RedisFailureStore {
    fn lock_key(key: &str) -> String {
        format!("aero:loginlock:{key}")
    }
    fn count_key(key: &str) -> String {
        format!("aero:loginfail:{key}")
    }

    /// Atomic-ish failure fold. `INCR` is atomic; the "Nth failure trips the lock"
    /// check has a tiny benign race (two concurrent Nth failures both set the same
    /// lock — idempotent). Mirrors the in-process semantics: a failure while locked
    /// is a no-op, and tripping the lock resets the counter (re-lock needs a fresh
    /// run after expiry).
    async fn try_record_failure(&self, key: &str) -> Result<(), fred::error::RedisError> {
        // Already locked ⇒ no-op (don't extend the window).
        let locked: i64 = self.client.exists(Self::lock_key(key)).await?;
        if locked > 0 {
            return Ok(());
        }
        let ckey = Self::count_key(key);
        let n: i64 = self.client.incr(&ckey).await?;
        if n == 1 {
            // First failure of the window: arm the sliding-window TTL.
            self.client
                .expire::<(), _>(&ckey, self.cfg.window_secs)
                .await?;
        }
        if n >= i64::from(self.cfg.max_failures) {
            // Trip the lock, reset the counter.
            self.client
                .set::<(), _, _>(
                    Self::lock_key(key),
                    "1",
                    Some(Expiration::EX(self.cfg.lockout_secs)),
                    None,
                    false,
                )
                .await?;
            let _: i64 = self.client.del(&ckey).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl FailureStore for RedisFailureStore {
    async fn is_locked(&self, key: &str, _now: i64) -> bool {
        // Fail-open: a Redis error means "not locked" — never deny a legitimate
        // user because the shared store is unreachable.
        match self.client.exists::<i64, _>(Self::lock_key(key)).await {
            Ok(n) => n > 0,
            Err(e) => {
                tracing::warn!(error = ?e, "login throttle: redis is_locked failed, failing open");
                false
            }
        }
    }

    async fn record_failure(&self, key: &str, _now: i64) {
        if let Err(e) = self.try_record_failure(key).await {
            tracing::warn!(error = ?e, "login throttle: redis record_failure failed, dropping");
        }
    }

    async fn record_success(&self, key: &str) {
        // Best-effort clear; an error here only delays GC of a stale counter.
        let _: Result<i64, _> = self.client.del(Self::count_key(key)).await;
        let _: Result<i64, _> = self.client.del(Self::lock_key(key)).await;
    }
}

/// Optional per-account login throttle. Holds a pluggable [`FailureStore`]
/// (in-process by default, Redis when opted in).
pub struct LoginThrottle {
    store: Arc<dyn FailureStore>,
}

impl LoginThrottle {
    /// In-process throttle (no shared state).
    #[must_use]
    pub fn new(cfg: LockoutConfig) -> Self {
        Self {
            store: Arc::new(InProcessFailureStore {
                states: Mutex::new(HashMap::new()),
                cfg,
            }),
        }
    }

    /// Redis-backed throttle (cross-node aggregation).
    #[must_use]
    pub fn with_redis(cfg: LockoutConfig, client: RedisClient) -> Self {
        Self {
            store: Arc::new(RedisFailureStore { client, cfg }),
        }
    }

    /// Build from `AERO_LOGIN_LOCKOUT*` env, in-process backend. `None` when the
    /// feature is off. Kept for callers without a Redis handle (e.g. tests).
    #[must_use]
    pub fn from_env() -> Option<Self> {
        Self::from_env_with_redis(None)
    }

    /// Build from env. When `AERO_LOGIN_LOCKOUT_REDIS` is truthy AND a `client` is
    /// supplied, uses the cross-node Redis backend; otherwise in-process. `None`
    /// when `AERO_LOGIN_LOCKOUT` is off.
    #[must_use]
    pub fn from_env_with_redis(client: Option<RedisClient>) -> Option<Self> {
        if !env_truthy("AERO_LOGIN_LOCKOUT") {
            return None;
        }
        let mut cfg = LockoutConfig::default();
        if let Some(v) = env_u32("AERO_LOGIN_LOCKOUT_MAX_FAILURES") {
            cfg.max_failures = v.max(1);
        }
        if let Some(v) = env_i64("AERO_LOGIN_LOCKOUT_WINDOW_SECS") {
            cfg.window_secs = v.max(1);
        }
        if let Some(v) = env_i64("AERO_LOGIN_LOCKOUT_SECS") {
            cfg.lockout_secs = v.max(1);
        }
        match (env_truthy("AERO_LOGIN_LOCKOUT_REDIS"), client) {
            (true, Some(c)) => {
                tracing::info!(
                    "login lockout: cross-node Redis backend (AERO_LOGIN_LOCKOUT_REDIS)"
                );
                Some(Self::with_redis(cfg, c))
            }
            _ => Some(Self::new(cfg)),
        }
    }

    /// Normalize an account key so case / whitespace variants share one bucket
    /// (an attacker can't reset the counter by varying `Email@x` vs `email@x`).
    fn key(account: &str) -> String {
        account.trim().to_lowercase()
    }

    /// Is this account currently locked?
    pub async fn is_locked(&self, account: &str, now: i64) -> bool {
        self.store.is_locked(&Self::key(account), now).await
    }

    /// Record a failed login; may trip the lock.
    pub async fn record_failure(&self, account: &str, now: i64) {
        self.store.record_failure(&Self::key(account), now).await;
    }

    /// Record a successful login; clears any accumulated failures for the account.
    pub async fn record_success(&self, account: &str) {
        self.store.record_success(&Self::key(account)).await;
    }

    /// Evict failure-accounting entries that can no longer lock or accumulate
    /// (in-process backend only; the Redis backend self-expires via TTL). Returns
    /// the number removed. Driven periodically so the default in-process map
    /// cannot grow unbounded under credential-stuffing across many distinct
    /// accounts. `now` is unix seconds, matching [`Self::record_failure`].
    pub async fn sweep(&self, now: i64) -> usize {
        self.store.sweep(now).await
    }
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn env_u32(name: &str) -> Option<u32> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn env_i64(name: &str) -> Option<i64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: LockoutConfig = LockoutConfig {
        max_failures: 3,
        window_secs: 100,
        lockout_secs: 600,
    };

    #[test]
    fn stays_unlocked_below_threshold() {
        let mut s = FailureState::default();
        s = s.after_failure(0, &CFG);
        s = s.after_failure(10, &CFG);
        assert_eq!(s.count, 2);
        assert!(!s.is_locked(20), "2 < 3 failures ⇒ not locked");
    }

    #[test]
    fn locks_at_threshold_for_the_lockout_window() {
        let mut s = FailureState::default();
        for t in [0, 10, 20] {
            s = s.after_failure(t, &CFG);
        }
        assert!(s.is_locked(20), "3rd failure trips the lock");
        assert!(
            s.is_locked(20 + CFG.lockout_secs - 1),
            "still locked mid-window"
        );
        assert!(
            !s.is_locked(20 + CFG.lockout_secs),
            "unlocks once the window passes"
        );
    }

    #[test]
    fn failures_outside_window_reset_the_counter() {
        let mut s = FailureState::default();
        s = s.after_failure(0, &CFG);
        s = s.after_failure(50, &CFG); // count=2, within window
                                       // Next failure is > window_secs after the FIRST → window resets to count 1.
        s = s.after_failure(CFG.window_secs + 1, &CFG);
        assert_eq!(s.count, 1, "stale window restarts the count");
        assert!(!s.is_locked(CFG.window_secs + 1));
    }

    #[test]
    fn locked_account_does_not_extend_on_more_failures() {
        let mut s = FailureState::default();
        for t in [0, 1, 2] {
            s = s.after_failure(t, &CFG);
        }
        let locked_until = s.locked_until;
        // A failure while locked is a no-op (the window stays put).
        let s2 = s.after_failure(100, &CFG);
        assert_eq!(s2.locked_until, locked_until);
    }

    #[test]
    fn re_locking_needs_a_fresh_run_after_expiry() {
        let mut s = FailureState::default();
        for t in [0, 1, 2] {
            s = s.after_failure(t, &CFG);
        }
        // After the lock expires, a single failure must NOT immediately re-lock.
        let after = s.locked_until;
        let s2 = s.after_failure(after, &CFG);
        assert_eq!(s2.count, 1);
        assert!(!s2.is_locked(after));
    }

    #[tokio::test]
    async fn throttle_locks_and_success_clears() {
        let t = LoginThrottle::new(CFG);
        for now in [0, 1, 2] {
            assert!(!t.is_locked("User@x.com", now).await);
            t.record_failure("user@x.com", now).await;
        }
        // Case/whitespace-insensitive: the mixed-case lookup sees the lock.
        assert!(
            t.is_locked("  User@X.com ", 2).await,
            "locked after threshold, key-normalized"
        );
        // A success clears the account.
        t.record_success("user@x.com").await;
        assert!(!t.is_locked("user@x.com", 2).await);
    }

    #[tokio::test]
    async fn sweep_evicts_only_decision_dead_entries() {
        let t = LoginThrottle::new(CFG); // max 3 / window 100 / lockout 600
                                         // Account A: one failure (count=1, within window). Account B: locked.
        t.record_failure("a@x.com", 0).await;
        for now in [0, 1, 2] {
            t.record_failure("b@x.com", now).await;
        }
        assert!(t.is_locked("b@x.com", 2).await);

        // Within A's window AND B's lockout ⇒ nothing is droppable.
        assert_eq!(t.sweep(5).await, 0, "live entries are kept");
        assert!(t.is_locked("b@x.com", 5).await, "lock survives a sweep");

        // Past A's window AND B's lockout ⇒ both entries are now decision-dead.
        let later = CFG.window_secs + CFG.lockout_secs + 10;
        assert_eq!(t.sweep(later).await, 2, "stale entries are evicted");
        assert!(!t.is_locked("b@x.com", later).await);
    }
}
