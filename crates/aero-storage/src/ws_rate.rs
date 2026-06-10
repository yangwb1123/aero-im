//! Per-workspace (tenant) request counter — Redis fixed window (ROADMAP3
//! 方向五 — 租户公平).
//!
//! One counter per `(workspace, epoch-minute)` at key
//! `aero:wsrate:{workspace}:{minute}`: each charged request is an `INCR`, and
//! the first increment of a window arms a 120-second `EXPIRE` so dead windows
//! garbage-collect themselves (the window's identity is in the key, so a
//! refreshed TTL could never extend a window — the expiry is purely cleanup).
//! Because every node increments the same Redis key, the resulting ceiling is
//! cluster-wide — a tenant cannot multiply its budget by spraying requests
//! across gateway nodes.
//!
//! This store only *counts*; the tier→limit policy, the fail-open decision on
//! Redis errors, and the enforcement call sites live in the gateway
//! (`aero-server`'s `ws_rate` module). Mirrors
//! [`crate::presence::PresenceStore`]'s Redis access style; the key/window
//! helpers are pure so they unit-test offline.

use aero_common::WorkspaceId;
use fred::prelude::{KeysInterface, RedisClient};
use std::time::{SystemTime, UNIX_EPOCH};

/// TTL armed on a window key's first increment. Two full window lengths: long
/// enough that a window being read at its boundary is still present, short
/// enough that idle tenants leave no residue.
pub const WINDOW_TTL_SECS: i64 = 120;

// ---------- Pure, Redis-free helpers (unit-tested) ----------

/// The fixed-window index for a wall-clock instant: whole minutes since the
/// UNIX epoch.
#[must_use]
pub fn epoch_minute(unix_secs: u64) -> u64 {
    unix_secs / 60
}

/// Redis key for one workspace's counter in one minute window.
#[must_use]
pub fn window_key(workspace: WorkspaceId, minute: u64) -> String {
    format!("aero:wsrate:{workspace}:{minute}")
}

/// Whole seconds since the UNIX epoch, saturating at 0 for a pre-epoch clock.
fn now_unix_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Cluster-wide per-workspace fixed-window request counter.
#[derive(Clone)]
pub struct WsRateStore {
    client: RedisClient,
}

impl WsRateStore {
    /// Build a store over an established Redis client.
    #[must_use]
    pub fn new(client: RedisClient) -> Self {
        Self { client }
    }

    /// Charge one request to `workspace` in the *current* minute window,
    /// returning the post-increment count for that window.
    ///
    /// # Errors
    /// Any Redis transport/command failure. Callers enforce availability over
    /// enforcement (fail-open) on error — that policy is theirs, not this store's.
    pub async fn incr_current(&self, workspace: WorkspaceId) -> anyhow::Result<u64> {
        self.incr_at(workspace, epoch_minute(now_unix_secs())).await
    }

    /// Charge one request to `workspace` in the explicit `minute` window.
    /// Split out from [`Self::incr_current`] so tests pin the window.
    ///
    /// # Errors
    /// Any Redis transport/command failure.
    pub async fn incr_at(&self, workspace: WorkspaceId, minute: u64) -> anyhow::Result<u64> {
        let key = window_key(workspace, minute);
        let n: i64 = self.client.incr(&key).await?;
        if n == 1 {
            // First hit of this window: arm the cleanup TTL. The count above is
            // already durable, so a failure here only delays garbage collection.
            self.client.expire::<(), _>(&key, WINDOW_TTL_SECS).await?;
        }
        Ok(u64::try_from(n).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_minute_is_whole_minutes() {
        assert_eq!(epoch_minute(0), 0);
        assert_eq!(epoch_minute(59), 0);
        assert_eq!(epoch_minute(60), 1);
        assert_eq!(epoch_minute(61), 1);
        assert_eq!(epoch_minute(3_600), 60);
        // A fixed reference instant: 2023-11-14T22:13:20Z.
        assert_eq!(epoch_minute(1_700_000_000), 28_333_333);
    }

    #[test]
    fn window_key_embeds_workspace_and_minute() {
        let ws = WorkspaceId::new();
        let key = window_key(ws, 28_333_333);
        assert_eq!(key, format!("aero:wsrate:{ws}:28333333"));
        assert!(key.starts_with("aero:wsrate:"));
    }

    #[test]
    fn window_keys_differ_across_minutes_and_workspaces() {
        let a = WorkspaceId::new();
        let b = WorkspaceId::new();
        // Same tenant, adjacent minutes → distinct counters (fixed window).
        assert_ne!(window_key(a, 100), window_key(a, 101));
        // Same minute, different tenants → distinct counters (per-tenant budget).
        assert_ne!(window_key(a, 100), window_key(b, 100));
    }

    #[test]
    fn window_ttl_covers_two_windows() {
        // One window is a whole minute (see `epoch_minute`); the cleanup TTL
        // must cover at least two so a counter read at the boundary is never
        // expired mid-window.
        let ttl = u64::try_from(WINDOW_TTL_SECS).expect("TTL is positive");
        assert!(ttl >= 2 * 60, "ttl {ttl}s shorter than two 60s windows");
    }
}

/// Redis-gated integration test (run with a live Redis):
///
/// ```text
/// REDIS_URL=redis://localhost:6379 \
///   cargo test -p aero-storage --lib -- --ignored wsrate_
/// ```
#[cfg(test)]
mod redis_tests {
    use super::*;
    use fred::prelude::{ClientLike, RedisClient};

    async fn client() -> RedisClient {
        let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
        let c =
            RedisClient::new(fred::types::RedisConfig::from_url(&url).unwrap(), None, None, None);
        c.connect();
        c.wait_for_connect().await.unwrap();
        c
    }

    #[tokio::test]
    #[ignore = "requires live Redis"]
    async fn wsrate_incr_counts_per_window_and_per_workspace() {
        let store = WsRateStore::new(client().await);
        let ws_a = WorkspaceId::new();
        let ws_b = WorkspaceId::new();
        let minute = epoch_minute(now_unix_secs());

        // Monotonic within one tenant's window.
        assert_eq!(store.incr_at(ws_a, minute).await.unwrap(), 1);
        assert_eq!(store.incr_at(ws_a, minute).await.unwrap(), 2);
        assert_eq!(store.incr_at(ws_a, minute).await.unwrap(), 3);

        // A different tenant in the same minute has an independent budget.
        assert_eq!(store.incr_at(ws_b, minute).await.unwrap(), 1);

        // The next minute window starts fresh for the first tenant.
        assert_eq!(store.incr_at(ws_a, minute + 1).await.unwrap(), 1);

        // The window key carries a cleanup TTL (armed on first increment).
        let ttl: i64 = {
            use fred::prelude::KeysInterface as _;
            store.client.ttl(window_key(ws_a, minute)).await.unwrap()
        };
        assert!(ttl > 0 && ttl <= WINDOW_TTL_SECS, "ttl armed, got {ttl}");
    }
}
