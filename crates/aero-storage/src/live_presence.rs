//! Cross-node live presence — stream viewer counts and call rosters.
//!
//! ROADMAP 方向二/五 + 方向四: these stores moved per-process `DashMap`s into
//! Redis so every node agrees on viewer counts and call rosters.
//!
//! Stream viewer counting is **sharded 256 ways** (ROADMAP 方向四: Redis 热键分片)
//! to avoid a single sorted-set hot key when thousands of viewers join one stream.
//! Call rosters carry far fewer participants, so they stay as single keys.

use aero_common::{CallId, ParticipantId};
use fred::prelude::{KeysInterface, RedisClient, SortedSetsInterface};
use fred::types::Ordering;
use futures::future;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use ulid::Ulid;

/// Number of shards per stream. 256 way — same as [`crate::presence::PresenceStore`].
const SHARD_COUNT: u64 = 256;

/// Default heartbeat TTL: a member not re-stamped within this window is pruned
/// on the next read.
pub const DEFAULT_TTL: Duration = Duration::from_secs(30);

// ---------- Pure, DB-free helpers (unit-tested) ----------

/// Redis key for a shard of a stream's viewer sorted set.
#[must_use]
fn viewer_shard_key(stream: Ulid, pid: ParticipantId) -> String {
    let shard = pid.to_uuid().as_bytes()[15] as u64;
    format!("live:viewers:stream:{stream}:shard:{shard}")
}

/// Redis key for the i-th viewer shard of a stream.
#[must_use]
fn viewer_shard_key_by_idx(stream: Ulid, idx: u64) -> String {
    format!("live:viewers:stream:{stream}:shard:{idx}")
}

/// Redis key for a call's roster sorted set.
#[must_use]
fn roster_key(call: CallId) -> String {
    format!("live:roster:call:{call}")
}

/// Whole seconds since the UNIX epoch for `t`, saturating at 0 for any
/// pre-epoch clock.
#[must_use]
fn epoch_secs(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64().trunc())
}

/// The stale-score threshold: any member whose heartbeat is strictly older than
/// `now - ttl` is stale.
#[must_use]
fn stale_threshold(now_secs: f64, ttl: Duration) -> f64 {
    (now_secs - ttl.as_secs_f64().trunc()).max(0.0)
}

/// The exclusive `ZREMRANGEBYSCORE` upper-bound argument for `threshold`,
/// i.e. `(threshold`.
#[must_use]
fn stale_max_arg(threshold: f64) -> String {
    format!("({threshold}")
}

/// Parse sorted-set members (participant id strings) back into ids.
#[must_use]
fn parse_participants(members: &[String]) -> Vec<ParticipantId> {
    members.iter().filter_map(|m| ParticipantId::from_str(m).ok()).collect()
}

// ---------- Stream viewer count (256-way sharded) ----------

/// Cluster-correct viewer set for a live stream, keyed by stream id.
///
/// **Sharded 256 ways** (ROADMAP 方向四): each viewer writes to
/// `live:viewers:stream:{stream}:shard:{uid%256}`, so concurrent ZADDs from
/// thousands of viewers spread across 256 keys instead of contending on one.
/// Reads iterate all shards in parallel and aggregate.
#[derive(Clone)]
pub struct StreamViewerStore {
    client: RedisClient,
    ttl: Duration,
}

impl StreamViewerStore {
    #[must_use]
    pub fn new(client: RedisClient) -> Self {
        Self { client, ttl: DEFAULT_TTL }
    }

    #[must_use]
    pub fn with_ttl(client: RedisClient, ttl: Duration) -> Self {
        Self { client, ttl }
    }

    #[must_use]
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Register `participant` as a viewer of `stream`.
    pub async fn join(&self, stream: Ulid, participant: ParticipantId) -> anyhow::Result<()> {
        self.stamp(stream, participant).await
    }

    /// Refresh `participant`'s liveness on `stream`.
    pub async fn heartbeat(&self, stream: Ulid, participant: ParticipantId) -> anyhow::Result<()> {
        self.stamp(stream, participant).await
    }

    async fn stamp(&self, stream: Ulid, participant: ParticipantId) -> anyhow::Result<()> {
        let key = viewer_shard_key(stream, participant);
        let score = epoch_secs(SystemTime::now());
        let _: i64 = self
            .client
            .zadd(&key, None, Some(Ordering::GreaterThan), false, false, (score, participant.to_string()))
            .await?;
        Ok(())
    }

    /// Remove `participant` from `stream`'s viewer set.
    pub async fn leave(&self, stream: Ulid, participant: ParticipantId) -> anyhow::Result<()> {
        let key = viewer_shard_key(stream, participant);
        let _: i64 = self.client.zrem(&key, participant.to_string()).await?;
        Ok(())
    }

    /// Current global viewer count for `stream`, aggregated across all shards.
    pub async fn count(&self, stream: Ulid) -> anyhow::Result<u64> {
        let now_secs = epoch_secs(SystemTime::now());
        let threshold = stale_threshold(now_secs, self.ttl);
        let max_arg = stale_max_arg(threshold);
        let client = &self.client;

        let futs: Vec<_> = (0..SHARD_COUNT)
            .map(|idx| {
                let key = viewer_shard_key_by_idx(stream, idx);
                let ma = max_arg.clone();
                async move {
                    let _: i64 = client.zremrangebyscore(&key, 0.0, &ma).await.unwrap_or(0);
                    let n: i64 = client.zcard(&key).await.unwrap_or(0);
                    u64::try_from(n).unwrap_or(0)
                }
            })
            .collect();

        let results: Vec<u64> = future::join_all(futs).await;
        Ok(results.iter().sum())
    }

    /// Current global viewer set for `stream`, aggregated across all shards.
    pub async fn viewers(&self, stream: Ulid) -> anyhow::Result<Vec<ParticipantId>> {
        let now_secs = epoch_secs(SystemTime::now());
        let threshold = stale_threshold(now_secs, self.ttl);
        let max_arg = stale_max_arg(threshold);
        let client = &self.client;

        let futs: Vec<_> = (0..SHARD_COUNT)
            .map(|idx| {
                let key = viewer_shard_key_by_idx(stream, idx);
                let ma = max_arg.clone();
                async move {
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
}

// ---------- Call roster (single key, small cardinality) ----------

/// Cluster-correct participant roster for a call, keyed by call id.
///
/// Unsharded because a call typically has 2–50 participants, so the hot-key
/// contention risk is negligible.
#[derive(Clone)]
pub struct CallRosterStore {
    client: RedisClient,
    ttl: Duration,
}

impl CallRosterStore {
    #[must_use]
    pub fn new(client: RedisClient) -> Self {
        Self { client, ttl: DEFAULT_TTL }
    }

    #[must_use]
    pub fn with_ttl(client: RedisClient, ttl: Duration) -> Self {
        Self { client, ttl }
    }

    #[must_use]
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    pub async fn join(&self, call: CallId, participant: ParticipantId) -> anyhow::Result<()> {
        self.stamp(call, participant).await
    }

    pub async fn heartbeat(&self, call: CallId, participant: ParticipantId) -> anyhow::Result<()> {
        self.stamp(call, participant).await
    }

    async fn stamp(&self, call: CallId, participant: ParticipantId) -> anyhow::Result<()> {
        let key = roster_key(call);
        let score = epoch_secs(SystemTime::now());
        let _: i64 = self
            .client
            .zadd(&key, None, Some(Ordering::GreaterThan), false, false, (score, participant.to_string()))
            .await?;
        Ok(())
    }

    pub async fn leave(&self, call: CallId, participant: ParticipantId) -> anyhow::Result<()> {
        let key = roster_key(call);
        let _: i64 = self.client.zrem(&key, participant.to_string()).await?;
        Ok(())
    }

    pub async fn count(&self, call: CallId) -> anyhow::Result<u64> {
        let key = roster_key(call);
        self.prune(&key).await?;
        let n: i64 = self.client.zcard(&key).await?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    pub async fn roster(&self, call: CallId) -> anyhow::Result<Vec<ParticipantId>> {
        let key = roster_key(call);
        self.prune(&key).await?;
        let members: Vec<String> = self.client.zrange(&key, 0, -1, None, false, None, false).await?;
        Ok(parse_participants(&members))
    }

    async fn prune(&self, key: &str) -> anyhow::Result<()> {
        let threshold = stale_threshold(epoch_secs(SystemTime::now()), self.ttl);
        let _: i64 = self.client.zremrangebyscore(key, 0.0, stale_max_arg(threshold)).await?;
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
        assert_eq!(stale_max_arg(55.0), "(55");
    }

    #[test]
    fn viewer_shard_key_is_deterministic() {
        let stream = Ulid::new();
        let pid = ParticipantId::new();
        let k1 = viewer_shard_key(stream, pid);
        let k2 = viewer_shard_key(stream, pid);
        assert_eq!(k1, k2);
        assert!(k1.contains("live:viewers:stream:"));
        assert!(k1.contains(":shard:"));
    }
}
