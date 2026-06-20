//! Behavioral spam / flood detection (ROADMAP5 方向五).
//!
//! The content [`Moderator`](crate::moderator::Moderator) classifies a message's
//! TEXT — it cannot catch a spammer who blasts clean-worded links to many rooms
//! in seconds. This guard tracks each sender's recent send *behaviour* over a
//! sliding window — message rate, same-content room fan-out, and duplicate
//! repeats — and throttles a sender whose pattern crosses the configured
//! thresholds.
//!
//! ## Storage backend
//!
//! The send accounting lives behind an [`ActivityStore`] seam with two impls:
//!
//! * **In-process** (default): a `DashMap` of per-sender event rings, each node
//!   counting independently with no network round-trip on the hot send path — a
//!   spammer hitting a single node is caught. The window/threshold logic is
//!   unchanged from the original; the async wrapper holds no `.await` across the
//!   map guard.
//! * **Redis** (opt-in via `AERO_SPAM_GUARD_REDIS`): three shared sorted-set
//!   sliding windows (rate / fan-out / duplicate), so a *distributed* spammer
//!   spraying the same content across nodes is aggregated cluster-wide rather
//!   than getting a fresh budget on each node. Strictly **fail-open** — any
//!   Redis error treats the send as [`SpamDecision::Allow`] and `warn`s, so a
//!   Redis outage degrades to "no behavioural throttle", never to dropping
//!   legitimate messages.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aero_common::{ParticipantId, RoomId};
use async_trait::async_trait;
use dashmap::DashMap;
use fred::prelude::{KeysInterface, RedisClient, SortedSetsInterface};

/// Why a send was throttled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpamReason {
    /// Too many messages from the sender within the window (flooding).
    Rate,
    /// The same content sent to too many distinct rooms within the window
    /// (cross-room blast — the canonical spammer signature).
    Fanout,
    /// The same content repeated too many times within the window.
    Duplicate,
}

/// Outcome of a [`SpamGuard::record`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpamDecision {
    Allow,
    Throttle(SpamReason),
}

/// Tunable thresholds. Each is a strict ceiling — `max_messages = 20` admits 20
/// messages in the window and throttles the 21st.
#[derive(Debug, Clone, Copy)]
pub struct SpamThresholds {
    /// Sliding-window length.
    pub window: Duration,
    /// Max messages per sender per window.
    pub max_messages: usize,
    /// Max distinct rooms a single piece of content may be sent to per window.
    pub max_rooms: usize,
    /// Max identical-content repeats per window.
    pub max_duplicates: usize,
}

impl Default for SpamThresholds {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(10),
            max_messages: 20,
            // Keep max_rooms < max_duplicates so a cross-room blast of one message
            // trips `Fanout` (the worse signal) before `Duplicate`.
            max_rooms: 5,
            max_duplicates: 10,
        }
    }
}

struct Activity {
    /// `(when, room, content_hash)`, time-ordered, pruned to the window per call.
    events: VecDeque<(Instant, RoomId, u64)>,
}

/// Pluggable send-accounting backend. Records one send and returns the throttle
/// decision for it. Implementations MUST be fail-open: a backend error yields
/// [`SpamDecision::Allow`] (never drop a legitimate message because storage
/// hiccuped).
#[async_trait]
trait ActivityStore: Send + Sync {
    async fn record(
        &self,
        sender: ParticipantId,
        room: RoomId,
        content_hash: u64,
        now: Instant,
    ) -> SpamDecision;
}

/// In-process `DashMap` backend (the historical default). No `.await` is held
/// across the map guard, so the async wrapper is `Send`.
struct InProcessActivityStore {
    thresholds: SpamThresholds,
    senders: DashMap<ParticipantId, Activity>,
}

#[async_trait]
impl ActivityStore for InProcessActivityStore {
    async fn record(
        &self,
        sender: ParticipantId,
        room: RoomId,
        content_hash: u64,
        now: Instant,
    ) -> SpamDecision {
        let mut entry = self
            .senders
            .entry(sender)
            .or_insert_with(|| Activity { events: VecDeque::new() });
        let act = entry.value_mut();

        // Drop events that have aged out of the window (events are time-ordered).
        while let Some((t, _, _)) = act.events.front() {
            if now.duration_since(*t) > self.thresholds.window {
                act.events.pop_front();
            } else {
                break;
            }
        }
        act.events.push_back((now, room, content_hash));

        if act.events.len() > self.thresholds.max_messages {
            return SpamDecision::Throttle(SpamReason::Rate);
        }
        let same_content = act.events.iter().filter(|(_, _, h)| *h == content_hash);
        let (dup_count, rooms): (usize, HashSet<RoomId>) =
            same_content.fold((0, HashSet::new()), |(n, mut rs), (_, r, _)| {
                rs.insert(*r);
                (n + 1, rs)
            });
        if dup_count > self.thresholds.max_duplicates {
            return SpamDecision::Throttle(SpamReason::Duplicate);
        }
        if rooms.len() > self.thresholds.max_rooms {
            return SpamDecision::Throttle(SpamReason::Fanout);
        }
        SpamDecision::Allow
    }
}

/// Redis-backed cross-node backend (opt-in). Each behavioural dimension is one
/// sorted-set sliding window scored by epoch-millis; every read first evicts
/// members older than `now - window` so the window is purely relative.
///
/// * **rate** — `aero:spam:rate:{sender}`: one member per send (a unique
///   sequence suffix), `ZCARD` is the send count in the window.
/// * **fan-out** — `aero:spam:fan:{sender}:{content_hash}`: the *room id* is the
///   member, so distinct rooms collapse and `ZCARD` is the distinct-room count
///   for this content.
/// * **duplicate** — `aero:spam:dup:{sender}:{content_hash}`: one member per
///   send of this content (unique suffix), `ZCARD` is the repeat count.
///
/// Every key gets an `EXPIRE` ≈ window on write so an idle sender leaves no
/// residue. Fail-open: any Redis error on this send aborts to `Allow`.
struct RedisActivityStore {
    client: RedisClient,
    thresholds: SpamThresholds,
    /// Monotonic per-process suffix making rate/duplicate members unique even at
    /// the same millisecond (sorted-set members are a *set* — equal members would
    /// otherwise collapse and undercount a burst).
    seq: AtomicU64,
}

impl RedisActivityStore {
    fn rate_key(sender: ParticipantId) -> String {
        format!("aero:spam:rate:{sender}")
    }
    fn fanout_key(sender: ParticipantId, content_hash: u64) -> String {
        format!("aero:spam:fan:{sender}:{content_hash}")
    }
    fn dup_key(sender: ParticipantId, content_hash: u64) -> String {
        format!("aero:spam:dup:{sender}:{content_hash}")
    }

    /// Whole-millisecond epoch clock for sorted-set scores (cross-node, unlike
    /// `Instant`). Saturates at 0 for a pre-epoch clock.
    fn now_millis() -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |d| d.as_millis() as f64)
    }

    fn window_ttl_secs(&self) -> i64 {
        // At least one whole window; +1s so a key read at the window boundary is
        // still present. The window's identity is relative (scores), so a
        // refreshed TTL can never widen it — the expiry is pure cleanup.
        i64::try_from(self.thresholds.window.as_secs()).unwrap_or(i64::MAX).saturating_add(1).max(1)
    }

    /// Insert one window member and return the post-prune window count. `member`
    /// must be unique-per-event for additive dimensions (rate/duplicate) and the
    /// room id for the set-collapsing fan-out dimension.
    async fn bump(
        &self,
        key: &str,
        member: &str,
        now_ms: f64,
    ) -> Result<i64, fred::error::RedisError> {
        let floor = now_ms - (self.thresholds.window.as_millis() as f64);
        // Evict members older than the window before counting. Use an f64 score
        // bound — fred's String bound does NOT understand Redis's `(` exclusive
        // wire prefix (that's `redis-cli` syntax, not the client API), so the old
        // `format!("({floor}")` errored at runtime and silently failed the whole
        // record open (every call → Allow). An inclusive `floor` upper bound drops
        // the boundary member too, which is fine for a behavioural window.
        let _: i64 = self.client.zremrangebyscore(key, 0.0, floor).await?;
        let _: i64 = self
            .client
            .zadd(key, None, None, false, false, (now_ms, member))
            .await?;
        self.client.expire::<(), _>(key, self.window_ttl_secs()).await?;
        let n: i64 = self.client.zcard(key).await?;
        Ok(n)
    }

    /// Fail-open inner: any Redis error propagates as `Err` and the caller maps
    /// it to `Allow`.
    async fn try_record(
        &self,
        sender: ParticipantId,
        room: RoomId,
        content_hash: u64,
    ) -> Result<SpamDecision, fred::error::RedisError> {
        let now_ms = Self::now_millis();
        let uniq = self.seq.fetch_add(1, AtomicOrdering::Relaxed);

        // rate: every send counts once (unique member per event).
        let rate = self
            .bump(&Self::rate_key(sender), &format!("{now_ms}:{uniq}"), now_ms)
            .await?;
        if usize::try_from(rate).unwrap_or(usize::MAX) > self.thresholds.max_messages {
            return Ok(SpamDecision::Throttle(SpamReason::Rate));
        }

        // duplicate: repeats of this content (unique member per event).
        let dup = self
            .bump(&Self::dup_key(sender, content_hash), &format!("{now_ms}:{uniq}"), now_ms)
            .await?;
        if usize::try_from(dup).unwrap_or(usize::MAX) > self.thresholds.max_duplicates {
            return Ok(SpamDecision::Throttle(SpamReason::Duplicate));
        }

        // fan-out: distinct rooms for this content (member = room id, so a
        // re-send to a room already counted does not grow the set).
        let rooms = self
            .bump(&Self::fanout_key(sender, content_hash), &room.to_string(), now_ms)
            .await?;
        if usize::try_from(rooms).unwrap_or(usize::MAX) > self.thresholds.max_rooms {
            return Ok(SpamDecision::Throttle(SpamReason::Fanout));
        }

        Ok(SpamDecision::Allow)
    }
}

#[async_trait]
impl ActivityStore for RedisActivityStore {
    async fn record(
        &self,
        sender: ParticipantId,
        room: RoomId,
        content_hash: u64,
        _now: Instant,
    ) -> SpamDecision {
        match self.try_record(sender, room, content_hash).await {
            Ok(decision) => decision,
            Err(e) => {
                // Fail-open: never drop a legitimate message because the shared
                // store hiccuped — degrade to "no behavioural throttle".
                tracing::warn!(error = ?e, "spam guard: redis record failed, failing open (allow)");
                SpamDecision::Allow
            }
        }
    }
}

/// Behavioral spam detector. Holds a pluggable [`ActivityStore`] (in-process by
/// default, Redis when opted in). Cheap to share behind an `Arc`.
pub struct SpamGuard {
    store: std::sync::Arc<dyn ActivityStore>,
}

impl SpamGuard {
    /// In-process guard (per-node accounting, no shared state).
    #[must_use]
    pub fn new(thresholds: SpamThresholds) -> Self {
        Self {
            store: std::sync::Arc::new(InProcessActivityStore {
                thresholds,
                senders: DashMap::new(),
            }),
        }
    }

    /// Redis-backed guard (cross-node aggregation). Opt-in; strictly fail-open.
    #[must_use]
    pub fn with_redis(thresholds: SpamThresholds, client: RedisClient) -> Self {
        Self {
            store: std::sync::Arc::new(RedisActivityStore {
                client,
                thresholds,
                seq: AtomicU64::new(0),
            }),
        }
    }

    /// Record a send from `sender` to `room` with text fingerprint `content_hash`
    /// and decide whether it should be throttled. `now` is injectable for the
    /// in-process backend's window math (the Redis backend uses the wall clock,
    /// which must be cluster-shared).
    ///
    /// The send is always recorded (a throttled attempt still counts as spam
    /// behaviour); the caller drops the message when the decision is `Throttle`.
    pub async fn record(
        &self,
        sender: ParticipantId,
        room: RoomId,
        content_hash: u64,
        now: Instant,
    ) -> SpamDecision {
        self.store.record(sender, room, content_hash, now).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> SpamGuard {
        SpamGuard::new(SpamThresholds {
            window: Duration::from_secs(10),
            max_messages: 5,
            max_rooms: 3,
            max_duplicates: 5,
        })
    }

    #[tokio::test]
    async fn rate_flood_is_throttled() {
        let g = guard();
        let sender = ParticipantId::new();
        let room = RoomId::new();
        let t0 = Instant::now();
        // 5 distinct messages to one room are fine; the 6th floods.
        for i in 0..5 {
            assert_eq!(g.record(sender, room, i, t0).await, SpamDecision::Allow, "msg {i} ok");
        }
        assert_eq!(
            g.record(sender, room, 99, t0).await,
            SpamDecision::Throttle(SpamReason::Rate)
        );
    }

    #[tokio::test]
    async fn cross_room_blast_of_same_content_is_throttled() {
        let g = guard();
        let sender = ParticipantId::new();
        let t0 = Instant::now();
        let hash = 0xdead_beef;
        // Same content to 3 rooms is allowed (max_rooms=3); the 4th distinct room
        // is a same-content blast → Fanout (dup ceiling is higher, so it trips first).
        for _ in 0..3 {
            assert_eq!(g.record(sender, RoomId::new(), hash, t0).await, SpamDecision::Allow);
        }
        assert_eq!(
            g.record(sender, RoomId::new(), hash, t0).await,
            SpamDecision::Throttle(SpamReason::Fanout),
            "same content to a 4th room is a cross-room blast",
        );
    }

    #[tokio::test]
    async fn repeated_duplicate_in_one_room_is_throttled() {
        // max_duplicates < max_messages so duplicates trip before the rate ceiling.
        let g = SpamGuard::new(SpamThresholds {
            window: Duration::from_secs(10),
            max_messages: 50,
            max_rooms: 50,
            max_duplicates: 3,
        });
        let sender = ParticipantId::new();
        let room = RoomId::new();
        let t0 = Instant::now();
        for _ in 0..3 {
            assert_eq!(g.record(sender, room, 7, t0).await, SpamDecision::Allow);
        }
        assert_eq!(
            g.record(sender, room, 7, t0).await,
            SpamDecision::Throttle(SpamReason::Duplicate)
        );
    }

    #[tokio::test]
    async fn distinct_content_per_room_is_not_fanout() {
        let g = guard();
        let sender = ParticipantId::new();
        let t0 = Instant::now();
        // Different content to 5 rooms: not a same-content blast, only rate-bound.
        for i in 0..5 {
            assert_eq!(
                g.record(sender, RoomId::new(), i, t0).await,
                SpamDecision::Allow,
                "msg {i}"
            );
        }
    }

    #[tokio::test]
    async fn window_expiry_resets_the_count() {
        let g = guard();
        let sender = ParticipantId::new();
        let room = RoomId::new();
        let t0 = Instant::now();
        for i in 0..5 {
            assert_eq!(g.record(sender, room, i, t0).await, SpamDecision::Allow);
        }
        // After the window passes, the old events prune and the sender is fresh.
        let later = t0 + Duration::from_secs(11);
        assert_eq!(
            g.record(sender, room, 100, later).await,
            SpamDecision::Allow,
            "window reset"
        );
    }

    #[tokio::test]
    async fn senders_are_independent() {
        let g = guard();
        let a = ParticipantId::new();
        let b = ParticipantId::new();
        let room = RoomId::new();
        let t0 = Instant::now();
        for i in 0..6 {
            let _ = g.record(a, room, i, t0).await; // a floods
        }
        // b is unaffected by a's flooding.
        assert_eq!(g.record(b, room, 0, t0).await, SpamDecision::Allow);
    }
}

/// Redis-gated integration test (run with a live Redis):
///
/// ```text
/// REDIS_URL=redis://localhost:6379 \
///   cargo test -p aero-im-core --lib -- --ignored spam_redis
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

    fn thresholds() -> SpamThresholds {
        SpamThresholds {
            window: Duration::from_secs(60),
            max_messages: 5,
            max_rooms: 3,
            max_duplicates: 5,
        }
    }

    #[tokio::test]
    #[ignore = "requires live Redis"]
    async fn spam_redis_rate_flood_is_throttled() {
        let g = SpamGuard::with_redis(thresholds(), client().await);
        // Fresh sender id keys are isolated, so the test is repeatable.
        let sender = ParticipantId::new();
        let room = RoomId::new();
        let t0 = Instant::now();
        for i in 0..5 {
            assert_eq!(g.record(sender, room, i, t0).await, SpamDecision::Allow, "msg {i}");
        }
        assert_eq!(
            g.record(sender, room, 99, t0).await,
            SpamDecision::Throttle(SpamReason::Rate)
        );
    }

    #[tokio::test]
    #[ignore = "requires live Redis"]
    async fn spam_redis_cross_room_blast_is_fanout() {
        let g = SpamGuard::with_redis(thresholds(), client().await);
        let sender = ParticipantId::new();
        let t0 = Instant::now();
        let hash = 0xc0ff_eeu64;
        for _ in 0..3 {
            assert_eq!(g.record(sender, RoomId::new(), hash, t0).await, SpamDecision::Allow);
        }
        assert_eq!(
            g.record(sender, RoomId::new(), hash, t0).await,
            SpamDecision::Throttle(SpamReason::Fanout)
        );
    }
}
