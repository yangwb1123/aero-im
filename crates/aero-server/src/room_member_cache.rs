//! Per-process TTL cache for non-authoritative room-membership hints.
//!
//! This cache must never decide whether a participant receives room content or
//! may perform a room-scoped operation. Cross-node invalidation is deliberately
//! best-effort, so a cached list can be stale for its whole TTL.
//!
//! The NATS-to-WebSocket path therefore reads
//! `RoomRepo::delivery_members(room)` from PostgreSQL for every event. That
//! authoritative query intersects room membership, workspace membership,
//! account/deactivation state, and mandatory-2FA policy before fan-out.
//!
//! ## Permitted use
//! - **Reads** hit `DashMap` (lock-free per shard).
//! - **Writes** (add/remove member) invalidate the cached entry.
//! - Consumers may use the result only for non-sensitive hints, diagnostics, or
//!   prefetching where stale over/under-counts are harmless.
//! - Any authorization or content-delivery decision must re-read an
//!   authoritative repository query.
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

    /// Look up a non-authoritative room-member hint, falling back to `repo` when
    /// the cache misses or the cached entry has expired.
    ///
    /// Do not use this method for authorization or content delivery.
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
        self.map.insert(
            room,
            CachedEntry {
                members: cached.clone(),
                at: now,
            },
        );
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
        cache.map.insert(
            room,
            CachedEntry {
                members,
                at: Instant::now(),
            },
        );

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
        stale.map.insert(
            sroom,
            CachedEntry {
                members: vec![ParticipantId::new()].into(),
                at: Instant::now(),
            },
        );
        assert!(
            stale.probe_fresh(sroom).is_none(),
            "expired entry must miss"
        );
        assert_eq!(stale.misses(), 1);
    }

    #[test]
    fn invalidate_forces_a_subsequent_miss() {
        let cache = RoomMemberCache::new(Duration::from_secs(100));
        let room = RoomId::new();
        cache.map.insert(
            room,
            CachedEntry {
                members: vec![ParticipantId::new()].into(),
                at: Instant::now(),
            },
        );
        assert!(cache.probe_fresh(room).is_some());
        cache.invalidate(&room);
        assert!(
            cache.probe_fresh(room).is_none(),
            "post-invalidate must miss"
        );
    }
}
