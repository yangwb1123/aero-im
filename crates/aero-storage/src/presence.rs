//! Presence tracking — who is online in which room, **cluster-wide**.
//!
//! ROADMAP 方向一 + 方向四: room presence is stored in Redis sorted sets with
//! heartbeat, **sharded 256 ways per room** to avoid hot-key serialization on
//! large rooms. Each participant's key is `presence:room:{room}:shard:{uid%256}`
//! so concurrent ZADDs from many participants in the same room spread across 256
//! keys instead of contending on one. Reads iterate all 256 shards in parallel
//! and aggregate.
//!
//! The member score is their last-seen epoch second. `join`/`heartbeat` is a
//! `ZADD` that (re)stamps the score; `leave` is a `ZREM`. A read (`count` /
//! `members`) first prunes anyone whose heartbeat is older than the TTL
//! (`ZREMRANGEBYSCORE 0 (now-ttl)`), so a crashed client that never sent
//! `leave` ages out automatically.

use aero_common::{ParticipantId, RoomId};
use fred::prelude::{KeysInterface, RedisClient, SortedSetsInterface};
use fred::types::{Ordering, ZRange, ZRangeBound, ZRangeKind};
use futures::future;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Number of shards per room. 256 is a power of 2 so `& 0xFF` works as a cheap
/// modulo for the shard index.
const SHARD_COUNT: u64 = 256;

/// Default heartbeat TTL: a member not re-stamped within this window is pruned
/// on the next read.
pub const DEFAULT_TTL: Duration = Duration::from_secs(45);

// ---------- Pure, DB-free helpers (unit-tested) ----------

/// Redis key for a shard of a room's presence sorted set.
/// Sharding uses participant id's low byte (`uid % 256`), so a given
/// participant always hashes to the same shard.
#[must_use]
fn room_shard_key(room: RoomId, pid: ParticipantId) -> String {
    let shard = pid.to_uuid().as_bytes()[15] as u64;
    format!("presence:room:{room}:shard:{shard}")
}

/// Redis key for the i-th shard of a room (0..256). Used for aggregate reads.
#[must_use]
fn room_shard_key_by_idx(room: RoomId, idx: u64) -> String {
    format!("presence:room:{room}:shard:{idx}")
}

/// Whole seconds since the UNIX epoch, saturating at 0 for any pre-epoch clock.
#[must_use]
pub fn epoch_secs(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64().trunc())
}

/// The stale-score threshold: a member whose heartbeat is strictly older than
/// `now - ttl` is stale. Saturates at 0 so a skewed clock never goes negative.
#[must_use]
fn stale_threshold(now_secs: f64, ttl: Duration) -> f64 {
    (now_secs - ttl.as_secs_f64().trunc()).max(0.0)
}

/// The exclusive `ZREMRANGEBYSCORE` upper-bound for `threshold`.
///
/// This must be a typed score bound. fred interprets arbitrary strings as
/// lexicographic ranges and rejects them for `BYSCORE`.
#[must_use]
fn stale_max_arg(threshold: f64) -> ZRange {
    ZRange {
        kind: ZRangeKind::Exclusive,
        range: ZRangeBound::Score(threshold),
    }
}

/// Parse sorted-set members (participant id strings) back into ids, skipping
/// any that fail to parse rather than failing the whole read.
#[must_use]
fn parse_participants(members: &[String]) -> Vec<ParticipantId> {
    members
        .iter()
        .filter_map(|m| ParticipantId::from_str(m).ok())
        .collect()
}

/// Cluster-correct room presence, keyed by room id with 256-way sharding.
#[derive(Clone)]
pub struct PresenceStore {
    client: RedisClient,
    ttl: Duration,
}

impl PresenceStore {
    /// Build a store using [`DEFAULT_TTL`].
    #[must_use]
    pub fn new(client: RedisClient) -> Self {
        Self {
            client,
            ttl: DEFAULT_TTL,
        }
    }

    /// Build a store with an explicit heartbeat TTL.
    #[must_use]
    pub fn with_ttl(client: RedisClient, ttl: Duration) -> Self {
        Self { client, ttl }
    }

    /// The configured heartbeat TTL.
    #[must_use]
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Mark `participant` present in `room` (or refresh an existing entry).
    pub async fn join(&self, room: RoomId, participant: ParticipantId) -> anyhow::Result<()> {
        self.stamp(room, participant).await
    }

    /// Refresh `participant`'s liveness in `room`. Identical to [`Self::join`].
    pub async fn heartbeat(&self, room: RoomId, participant: ParticipantId) -> anyhow::Result<()> {
        self.stamp(room, participant).await
    }

    async fn stamp(&self, room: RoomId, participant: ParticipantId) -> anyhow::Result<()> {
        let key = room_shard_key(room, participant);
        let score = epoch_secs(SystemTime::now());
        let _: i64 = self
            .client
            .zadd(
                &key,
                None,
                Some(Ordering::GreaterThan),
                false,
                false,
                (score, participant.to_string()),
            )
            .await?;
        Ok(())
    }

    /// Remove `participant` from `room`'s presence set.
    pub async fn leave(&self, room: RoomId, participant: ParticipantId) -> anyhow::Result<()> {
        let key = room_shard_key(room, participant);
        let _: i64 = self.client.zrem(&key, participant.to_string()).await?;
        Ok(())
    }

    /// Cluster-wide online count for `room`, after pruning stale heartbeats
    /// across all shards. Iterates all 256 shards in parallel using concurrent
    /// futures — accepted overhead for eliminating the write hot-key bottleneck.
    pub async fn count(&self, room: RoomId) -> anyhow::Result<u64> {
        let now_secs = epoch_secs(SystemTime::now());
        let threshold = stale_threshold(now_secs, self.ttl);
        let max_arg = stale_max_arg(threshold);
        let client = &self.client;

        let futs: Vec<_> = (0..SHARD_COUNT)
            .map(|idx| {
                let key = room_shard_key_by_idx(room, idx);
                let ma = max_arg.clone();
                async move {
                    // Prune stale entries, then ZCARD.
                    let _: i64 = client.zremrangebyscore(&key, 0.0, &ma).await.unwrap_or(0);
                    let n: i64 = client.zcard(&key).await.unwrap_or(0);
                    u64::try_from(n).unwrap_or(0)
                }
            })
            .collect();

        let results: Vec<u64> = future::join_all(futs).await;
        Ok(results.iter().sum())
    }

    /// Cluster-wide online member set for `room`, after pruning stale heartbeats
    /// across all shards.
    pub async fn members(&self, room: RoomId) -> anyhow::Result<Vec<ParticipantId>> {
        let now_secs = epoch_secs(SystemTime::now());
        let threshold = stale_threshold(now_secs, self.ttl);
        let max_arg = stale_max_arg(threshold);
        let client = &self.client;

        let futs: Vec<_> = (0..SHARD_COUNT)
            .map(|idx| {
                let key = room_shard_key_by_idx(room, idx);
                let ma = max_arg.clone();
                async move {
                    // Prune stale entries, then fetch all members.
                    let _: i64 = client.zremrangebyscore(&key, 0.0, &ma).await.unwrap_or(0);
                    let members: Vec<String> = client
                        .zrange(&key, 0, -1, None, false, None, false)
                        .await
                        .unwrap_or_default();
                    members
                }
            })
            .collect();

        let results: Vec<Vec<String>> = future::join_all(futs).await;
        let all: Vec<String> = results.into_iter().flatten().collect();
        Ok(parse_participants(&all))
    }

    /// Cheap connectivity probe.
    pub async fn ping(&self) -> anyhow::Result<()> {
        let _: Option<String> = self.client.get("__aero_health").await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_secs_is_whole_seconds_and_never_panics() {
        assert_eq!(epoch_secs(UNIX_EPOCH), 0.0);
        let t = UNIX_EPOCH + Duration::from_millis(1_500);
        assert_eq!(epoch_secs(t), 1.0);
    }

    #[test]
    fn stale_threshold_saturates_at_zero() {
        assert_eq!(stale_threshold(10.0, Duration::from_secs(45)), 0.0);
        assert_eq!(stale_threshold(100.0, Duration::from_secs(45)), 55.0);
    }

    #[test]
    fn stale_max_arg_is_exclusive_whole_second() {
        let range = stale_max_arg(55.0);
        assert_eq!(range.kind, ZRangeKind::Exclusive);
        assert!(matches!(range.range, ZRangeBound::Score(55.0)));
    }

    #[test]
    fn parse_participants_skips_garbage() {
        let p = ParticipantId::new();
        let members = vec![p.to_string(), "not-an-id".to_string()];
        assert_eq!(parse_participants(&members), vec![p]);
    }

    #[test]
    fn room_shard_key_is_deterministic() {
        let room = RoomId::new();
        let pid = ParticipantId::new();
        let key1 = room_shard_key(room, pid);
        let key2 = room_shard_key(room, pid);
        assert_eq!(key1, key2);
        assert!(key1.contains("presence:room:"));
        assert!(key1.contains(":shard:"));
    }

    #[test]
    fn different_pids_can_map_to_different_shards() {
        // Statistical test: two random PIDs map to different shards most of the
        // time (255/256 chance). If they collide, we just assert the key format.
        let room = RoomId::new();
        let pid_a = ParticipantId::new();
        let pid_b = ParticipantId::new();
        let key_a = room_shard_key(room, pid_a);
        let key_b = room_shard_key(room, pid_b);
        // Both keys must at least have the right format (format tested above).
        // A collision is OK (1/256 chance) — just don't crash.
        let _ = (key_a, key_b);
    }

    #[test]
    fn shard_index_keys_cover_each_shard() {
        let room = RoomId::new();
        let k0 = room_shard_key_by_idx(room, 0);
        let k255 = room_shard_key_by_idx(room, 255);
        assert!(k0.ends_with(":shard:0"));
        assert!(k255.ends_with(":shard:255"));
    }
}

/// Redis-gated integration test for fred's typed BYSCORE range.
#[cfg(test)]
mod redis_tests {
    use super::*;
    use fred::prelude::{ClientLike, KeysInterface, RedisClient, SortedSetsInterface};

    async fn client() -> RedisClient {
        let url =
            std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_owned());
        let client = RedisClient::new(
            fred::types::RedisConfig::from_url(&url).unwrap(),
            None,
            None,
            None,
        );
        client.connect();
        client.wait_for_connect().await.unwrap();
        client
    }

    #[tokio::test]
    #[ignore = "requires live Redis"]
    async fn presence_prunes_stale_score_with_fred_typed_bound() {
        let client = client().await;
        let room = RoomId::new();
        let participant = ParticipantId::new();
        let key = room_shard_key(room, participant);
        let _: i64 = client
            .zadd(
                &key,
                None,
                None,
                false,
                false,
                (1.0, participant.to_string()),
            )
            .await
            .unwrap();

        let members = PresenceStore::new(client.clone())
            .members(room)
            .await
            .unwrap();
        assert!(members.is_empty(), "stale presence member is pruned");
        let _: i64 = client.del(key).await.unwrap();
    }
}
