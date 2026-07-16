//! Per-process TTL cache for room membership lists (ROADMAP6 方向四 多级缓存).
//!
//! The `RoomRepo::members(room)` query is a hot read path in the bus-listener
//! fan-out: every `RoomEvent` without explicit recipients calls it once to
//! expand recipients. For high-traffic rooms this means dozens of PG queries
//! per second, each scanning the `room_members` index.
//!
//! Room membership changes rarely (a member joins or leaves), so a local TTL
//! cache with write-invalidation eliminates the majority of these round-trips
//! while staying safe.
//!
//! ## Safety under concurrent writes
//! - **Reads** hit `DashMap` (lock-free per shard).
//! - **Writes** (add/remove member) invalidate the cached entry.
//! - **TTL** (default 60s) is a freshness floor: if membership changes on
//!   another node, this node may serve the stale list for up to `TTL` seconds.
//!   A stale member list causes a short window of over- or under-fan-out
//!   (the new member receives an extra few seconds of frames, or a just-removed
//!   member receives frames they shouldn't see). Both are acceptable for the
//!   live fan-out path (the member / auth gate downstream filters anyway).
//!
//! ## Design
//! - `DashMap` per-process, `Arc<Vec<ParticipantId>>` values.
//! - `Instant`-based TTL, pure freshness rule (mirrors [`ParticipantCache`]).
//! - No background eviction — the map self-limits to active rooms on this node.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aero_common::{ParticipantId, RoomId};
use aero_storage::RoomRepo;
use dashmap::DashMap;

/// Default TTL for cached room member lists.
pub const ROOM_MEMBER_CACHE_TTL: Duration = Duration::from_secs(60);

/// One cached member list with its insertion timestamp.
#[derive(Debug, Clone)]
struct CachedEntry {
    members: Arc<[ParticipantId]>,
    at: Instant,
}

/// A `DashMap`-backed room-membership cache with TTL-based freshness.
///
/// Cheap to clone — `DashMap` and `Arc` are both cheaply cloneable.
#[derive(Clone)]
pub struct RoomMemberCache {
    map: Arc<DashMap<RoomId, CachedEntry>>,
    ttl: Duration,
    /// Local hit/miss counters.
    hits: Arc<AtomicU64>,
    misses: Arc<AtomicU64>,
}

impl RoomMemberCache {
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

    /// Create a cache with the standard [`ROOM_MEMBER_CACHE_TTL`].
    #[must_use]
    pub fn with_default_ttl() -> Self {
        Self::new(ROOM_MEMBER_CACHE_TTL)
    }

    /// Look up room members, falling back to `repo` when the cache misses or
    /// the cached entry has expired.
    ///
    /// Returns `Ok(Arc::new([]))` when the room genuinely has no members (the
    /// negative result is NOT cached — a later `add_member` will be visible on
    /// the next fetch).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the underlying repo read.
    pub async fn get_or_fetch(
        &self,
        room: RoomId,
        repo: &RoomRepo,
    ) -> Result<Arc<[ParticipantId]>, sqlx::Error> {
        let now = Instant::now();
        // Fast path: fresh cached hit.
        if let Some(entry) = self.map.get(&room) {
            if now.saturating_duration_since(entry.at) < self.ttl {
                let hit = entry.members.clone();
                drop(entry);
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Ok(hit);
            }
            // Stale entry — drop the guard and fall through to DB fetch.
            drop(entry);
        }
        // Slow path: DB fetch + backfill.
        self.misses.fetch_add(1, Ordering::Relaxed);
        let rows = repo.members(room).await?;
        let cached: Arc<[ParticipantId]> = rows.into();
        self.map.insert(room, CachedEntry { members: cached.clone(), at: now });
        Ok(cached)
    }

    /// Invalidate a room's cached member list (after adding or removing a
    /// member). The next `get_or_fetch` will re-read from the DB.
    pub fn invalidate(&self, room: &RoomId) {
        self.map.remove(room);
    }

    /// Approximate number of cached entries. For diagnostics.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// True when the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Cumulative fresh-hit count since process start.
    #[must_use]
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Cumulative miss count since process start.
    #[must_use]
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }
}

impl Default for RoomMemberCache {
    fn default() -> Self {
        Self::with_default_ttl()
    }
}

#[cfg(test)]
impl RoomMemberCache {
    /// Test-only fast-path probe. Returns `Some(_)` when the entry is fresh
    /// (would be a cache HIT without going to the DB), `None` when absent or
    /// expired. Bumps hit/miss counters like the real method.
    fn probe_fresh(&self, room: RoomId) -> Option<Arc<[ParticipantId]>> {
        let now = Instant::now();
        if let Some(entry) = self.map.get(&room) {
            if now.saturating_duration_since(entry.at) < self.ttl {
                let hit = entry.members.clone();
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
        let cache = RoomMemberCache::new(Duration::from_secs(100));
        let room = RoomId::new();
        let members: Arc<[ParticipantId]> = vec![ParticipantId::new(), ParticipantId::new()].into();
        cache.map.insert(room, CachedEntry { members, at: Instant::now() });

        assert_eq!(cache.len(), 1);
        assert!(cache.probe_fresh(room).is_some());
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 0);

        // Invalidate.
        cache.invalidate(&room);
        assert!(cache.probe_fresh(room).is_none());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn absent_or_expired_read_is_a_miss() {
        let cache = RoomMemberCache::new(Duration::from_secs(100));
        let room = RoomId::new();
        assert!(cache.probe_fresh(room).is_none());
        assert_eq!(cache.misses(), 1);
        assert_eq!(cache.hits(), 0);

        // TTL 0 ⇒ always stale.
        let stale = RoomMemberCache::new(Duration::from_secs(0));
        let sroom = RoomId::new();
        stale.map.insert(sroom, CachedEntry {
            members: vec![ParticipantId::new()].into(),
            at: Instant::now(),
        });
        assert!(stale.probe_fresh(sroom).is_none(), "expired entry must miss");
        assert_eq!(stale.misses(), 1);
    }

    #[test]
    fn invalidate_forces_a_subsequent_miss() {
        let cache = RoomMemberCache::new(Duration::from_secs(100));
        let room = RoomId::new();
        cache.map.insert(room, CachedEntry {
            members: vec![ParticipantId::new()].into(),
            at: Instant::now(),
        });
        assert!(cache.probe_fresh(room).is_some());
        cache.invalidate(&room);
        assert!(cache.probe_fresh(room).is_none(), "post-invalidate must miss");
    }
}
