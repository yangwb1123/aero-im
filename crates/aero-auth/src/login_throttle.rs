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
//! State is in-process (`Mutex<HashMap>`), mirroring the message spam guard: each
//! node locks independently (sufficient for brute-force defense) with no DB
//! round-trip on the hot login path. The decision logic is a pure function of the
//! prior state + an injected `now`, so every transition is unit-tested.

use std::collections::HashMap;
use std::sync::Mutex;

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
        Self { max_failures: 5, window_secs: 300, lockout_secs: 900 }
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
        let first = if within_window { self.first_failure_at } else { now };
        if count >= cfg.max_failures {
            // Trip the lock and reset the counter so re-locking needs a fresh run
            // of failures after the lockout expires (not a single one).
            FailureState { count: 0, first_failure_at: 0, locked_until: now + cfg.lockout_secs }
        } else {
            FailureState { count, first_failure_at: first, locked_until: 0 }
        }
    }
}

/// In-process per-account login throttle.
pub struct LoginThrottle {
    states: Mutex<HashMap<String, FailureState>>,
    cfg: LockoutConfig,
}

impl LoginThrottle {
    #[must_use]
    pub fn new(cfg: LockoutConfig) -> Self {
        Self { states: Mutex::new(HashMap::new()), cfg }
    }

    /// Build from `AERO_LOGIN_LOCKOUT*` env, or `None` when the feature is off
    /// (`AERO_LOGIN_LOCKOUT` unset / not truthy). Overrides:
    /// `AERO_LOGIN_LOCKOUT_MAX_FAILURES`, `_WINDOW_SECS`, `_SECS`.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let on = std::env::var("AERO_LOGIN_LOCKOUT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if !on {
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
        Some(Self::new(cfg))
    }

    /// Normalize an account key so case / whitespace variants share one bucket
    /// (an attacker can't reset the counter by varying `Email@x` vs `email@x`).
    fn key(account: &str) -> String {
        account.trim().to_lowercase()
    }

    /// Is this account currently locked?
    #[must_use]
    pub fn is_locked(&self, account: &str, now: i64) -> bool {
        let states = self.states.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        states.get(&Self::key(account)).is_some_and(|s| s.is_locked(now))
    }

    /// Record a failed login; may trip the lock.
    pub fn record_failure(&self, account: &str, now: i64) {
        let mut states = self.states.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = states.entry(Self::key(account)).or_default();
        *entry = entry.after_failure(now, &self.cfg);
    }

    /// Record a successful login; clears any accumulated failures for the account.
    pub fn record_success(&self, account: &str) {
        let mut states = self.states.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        states.remove(&Self::key(account));
    }
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

    const CFG: LockoutConfig = LockoutConfig { max_failures: 3, window_secs: 100, lockout_secs: 600 };

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
        assert!(s.is_locked(20 + CFG.lockout_secs - 1), "still locked mid-window");
        assert!(!s.is_locked(20 + CFG.lockout_secs), "unlocks once the window passes");
    }

    #[test]
    fn failures_outside_window_reset_the_counter() {
        let mut s = FailureState::default();
        s = s.after_failure(0, &CFG);
        s = s.after_failure(50, &CFG); // count=2, within window
        // Next failure is > window_secs after the FIRST → window resets to count 1.
        s = s.after_failure(0 + CFG.window_secs + 1, &CFG);
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

    #[test]
    fn throttle_locks_and_success_clears() {
        let t = LoginThrottle::new(CFG);
        for now in [0, 1, 2] {
            assert!(!t.is_locked("User@x.com", now));
            t.record_failure("user@x.com", now);
        }
        // Case/whitespace-insensitive: the mixed-case lookup sees the lock.
        assert!(t.is_locked("  User@X.com ", 2), "locked after threshold, key-normalized");
        // A success clears the account.
        t.record_success("user@x.com");
        assert!(!t.is_locked("user@x.com", 2));
    }
}
