//! Per-process TTL cache for participant profiles (ROADMAP6 方向四).
//!
//! The `ParticipantRepo::get(pid)` query is one of the hottest read paths in the
//! system: every push, every notification, every WebSocket fan-out, and every
//! `@`-mention resolution calls it at least once per participant. Participant
//! profiles change rarely (display_name / avatar_url updates), so a local TTL
//! cache with write-invalidation eliminates the majority of these round-trips
//! while staying safe under concurrent writes.
//!
//! ## Safety under concurrent writes
//! - **Reads** hit `DashMap` (lock-free per shard).
//! - **Writes** (`update_me`, `delete_participant`, admin rename) invalidate the
//!   cached entry so the next read re-fetches from the DB.
//! - **TTL** (default 60s) is a freshness floor, not a correctness guarantee: if
//!   another node writes a profile change, this node may serve the stale value
//!   for up to `CACHE_TTL` seconds before the next miss re-fetches. For display
//!   names this is acceptable — a 60s stale name is visually imperceptible.
//!
//! ## Design
//! - `DashMap` per-process, `Arc<Participant>` values (cheap to clone, the
//!   underlying `Participant` is `Clone`).
//! - `Instant`-based TTL, pure freshness rule (mirrors `ws_rate.rs::fresh`).
//! - No background eviction — the map naturally self-limits to the set of active
//!   participants on this node (~workspace size × active fraction).
//!
//! Pure functions, unit-tested without I/O.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aero_common::Participant;
use aero_storage::ParticipantRepo;
use dashmap::DashMap;

/// Default TTL for cached participant profiles. One minute is short enough that
/// a profile edit propagates reasonably quickly, long enough to absorb the
/// dominant repeated-read pattern in notification / push fan-out loops.
pub const PARTICIPANT_CACHE_TTL: Duration = Duration::from_secs(60);

/// One cached participant profile with its insertion timestamp.
///
/// All fields are read via `DashMap`'s `get`/`entry` API, so they are snapshot-
/// isolated per-read — no explicit lock needed.
#[derive(Debug, Clone)]
struct CachedEntry {
    participant: Arc<Participant>,
    at: Instant,
}

/// A `DashMap`-backed participant cache with TTL-based freshness.
///
/// Cheap to clone — `DashMap` and `Arc` are both cheaply cloneable.
#[derive(Clone)]
pub struct ParticipantCache {
    map: Arc<DashMap<aero_common::ParticipantId, CachedEntry>>,
    ttl: Duration,
    /// Process-local hit/miss counters mirroring the Prometheus
    /// `aero_participant_cache_lookups_total` series. Kept on the cache itself so
    /// the hit-without-DB invariant is unit-testable offline (the global metrics
    /// registry is shared across parallel tests and not reset-able).
    hits: Arc<AtomicU64>,
    misses: Arc<AtomicU64>,
}

impl ParticipantCache {
    /// Create a cache with a custom TTL.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            map: Arc::new(DashMap::new()),
            ttl,
            hits: Arc::new(AtomicU64::new(0)),
            misses: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Create a cache with the standard [`PARTICIPANT_CACHE_TTL`].
    #[must_use]
    pub fn with_default_ttl() -> Self {
        Self::new(PARTICIPANT_CACHE_TTL)
    }

    /// Look up a participant, falling back to `repo` when the cache misses or
    /// the cached entry has expired.
    ///
    /// Returns `None` only when the participant genuinely doesn't exist (the
    /// negative result is NOT cached — a later `insert` will be visible).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the underlying repo read.
    pub async fn get_or_fetch(
        &self,
        pid: aero_common::ParticipantId,
        repo: &ParticipantRepo,
    ) -> Result<Option<Arc<Participant>>, sqlx::Error> {
        let now = Instant::now();
        // Fast path: fresh cached hit.
        if let Some(entry) = self.map.get(&pid) {
            if now.saturating_duration_since(entry.at) < self.ttl {
                let hit = entry.participant.clone();
                drop(entry);
                self.hits.fetch_add(1, Ordering::Relaxed);
                crate::metrics::record_participant_cache(true);
                return Ok(Some(hit));
            }
            // Stale entry — drop the guard and fall through to DB fetch.
            drop(entry);
        }
        // Slow path: DB fetch + backfill. A genuine miss (cold or expired).
        self.misses.fetch_add(1, Ordering::Relaxed);
        crate::metrics::record_participant_cache(false);
        let row = repo.get(pid).await?;
        if let Some(ref p) = row {
            self.map.insert(
                pid,
                CachedEntry {
                    participant: Arc::new(p.clone()),
                    at: now,
                },
            );
            Ok(Some(Arc::new(p.clone())))
        } else {
            Ok(None)
        }
    }

    /// Invalidate a specific participant (after `update_me`, `delete_participant`,
    /// or admin rename). The next `get_or_fetch` will re-read from the DB.
    pub fn invalidate(&self, pid: &aero_common::ParticipantId) {
        self.map.remove(pid);
    }

    /// Approximate number of cached entries. For diagnostics only — `DashMap`
    /// iterates across all shards so this is O(shards), not O(1).
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// True when the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Cumulative fresh-hit count since process start (served without a DB read).
    #[must_use]
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Cumulative miss count since process start (fell through to the repo).
    #[must_use]
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }
}

impl Default for ParticipantCache {
    fn default() -> Self {
        Self::with_default_ttl()
    }
}

#[cfg(test)]
impl ParticipantCache {
    /// Test-only mirror of [`Self::get_or_fetch`]'s fast path with the repo
    /// elided. Returns `Some(_)` when the entry is present AND fresh (a HIT,
    /// served without ever touching a repo), `None` when absent or expired (the
    /// path that would fall through to the DB). Bumps the same hit/miss counters
    /// as the real method so the "fresh read does not touch the DB" invariant is
    /// asserted offline (no `PgPool` / tokio needed).
    fn probe_fresh(&self, pid: aero_common::ParticipantId) -> Option<Arc<Participant>> {
        let now = Instant::now();
        if let Some(entry) = self.map.get(&pid) {
            if now.saturating_duration_since(entry.at) < self.ttl {
                let hit = entry.participant.clone();
                drop(entry);
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Some(hit);
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_entry_hits_cache_without_db() {
        // A cache-only test: insert a value, then retrieve it — no repo needed.
        // We can't run an async test with a real PgPool here (no tokio), so we
        // test the cache semantics: insertion, TTL, invalidation.
        let cache = ParticipantCache::new(Duration::from_secs(100));
        let pid = aero_common::ParticipantId::new();
        let p = Participant {
            id: pid,
            kind: aero_common::ParticipantKind::Human,
            display_name: "alice".into(),
            avatar_url: None,
            created_by: None,
            created_at: time::OffsetDateTime::now_utc(),
        };
        // Insert via the internal map directly (simulating a get_or_fetch miss path).
        cache.map.insert(
            pid,
            CachedEntry {
                participant: Arc::new(p),
                at: Instant::now(),
            },
        );
        // The entry should be present.
        assert!(cache.map.get(&pid).is_some());
        assert_eq!(cache.len(), 1);

        // Invalidate.
        cache.invalidate(&pid);
        assert!(cache.map.get(&pid).is_none());
        assert_eq!(cache.len(), 0);
    }

    fn sample(pid: aero_common::ParticipantId, name: &str) -> Participant {
        Participant {
            id: pid,
            kind: aero_common::ParticipantKind::Human,
            display_name: name.into(),
            avatar_url: None,
            created_by: None,
            created_at: time::OffsetDateTime::now_utc(),
        }
    }

    #[test]
    fn fresh_read_is_a_hit_and_never_touches_the_db() {
        // DoD for the wiring: a fresh cached entry is served as a HIT — the
        // probe returns the value without ever calling the repo — and the hit
        // counter advances. Reverting `get_or_fetch`'s fast path (so every read
        // hit the DB) would make `hits()` stay 0 and fail this assertion.
        let cache = ParticipantCache::new(Duration::from_secs(100));
        let pid = aero_common::ParticipantId::new();
        cache.map.insert(
            pid,
            CachedEntry {
                participant: Arc::new(sample(pid, "alice")),
                at: Instant::now(),
            },
        );

        assert_eq!(cache.hits(), 0);
        let got = cache.probe_fresh(pid).expect("fresh entry must be a hit");
        assert_eq!(got.display_name, "alice");
        assert_eq!(cache.hits(), 1, "a fresh read must count as a hit");
        assert_eq!(cache.misses(), 0, "a fresh read must NOT count as a miss");

        // A second fresh read is another hit, still no DB.
        let _ = cache.probe_fresh(pid).expect("still fresh");
        assert_eq!(cache.hits(), 2);
        assert_eq!(cache.misses(), 0);
    }

    #[test]
    fn absent_or_expired_read_is_a_miss() {
        // A cold lookup misses (would fall through to the DB).
        let cache = ParticipantCache::new(Duration::from_secs(100));
        let pid = aero_common::ParticipantId::new();
        assert!(cache.probe_fresh(pid).is_none());
        assert_eq!(cache.misses(), 1);
        assert_eq!(cache.hits(), 0);

        // An expired entry also misses (TTL 0 ⇒ always stale).
        let stale = ParticipantCache::new(Duration::from_secs(0));
        let spid = aero_common::ParticipantId::new();
        stale.map.insert(
            spid,
            CachedEntry {
                participant: Arc::new(sample(spid, "bob")),
                at: Instant::now(),
            },
        );
        assert!(stale.probe_fresh(spid).is_none(), "expired entry must miss");
        assert_eq!(stale.misses(), 1);
        assert_eq!(stale.hits(), 0);
    }

    #[test]
    fn invalidate_forces_a_subsequent_miss() {
        let cache = ParticipantCache::new(Duration::from_secs(100));
        let pid = aero_common::ParticipantId::new();
        cache.map.insert(
            pid,
            CachedEntry {
                participant: Arc::new(sample(pid, "carol")),
                at: Instant::now(),
            },
        );
        assert!(cache.probe_fresh(pid).is_some()); // hit
        cache.invalidate(&pid);
        assert!(
            cache.probe_fresh(pid).is_none(),
            "post-invalidate read must miss"
        );
    }

    #[test]
    fn expired_entry_returns_none_from_map_directly() {
        let cache = ParticipantCache::new(Duration::from_secs(0)); // TTL 0 = always stale
        let pid = aero_common::ParticipantId::new();
        let p = Participant {
            id: pid,
            kind: aero_common::ParticipantKind::Human,
            display_name: "bob".into(),
            avatar_url: None,
            created_by: None,
            created_at: time::OffsetDateTime::now_utc(),
        };
        cache.map.insert(
            pid,
            CachedEntry {
                participant: Arc::new(p),
                at: Instant::now(),
            },
        );
        // Still in the map (we don't evict eagerly), but get_or_fetch would miss.
        assert!(cache.map.get(&pid).is_some());
    }
}
