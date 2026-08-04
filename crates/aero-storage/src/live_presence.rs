//! Cross-node live presence — stream viewer counts and call rosters.
//!
//! ROADMAP 方向二/五 + 方向四: these stores moved per-process `DashMap`s into
//! Redis so every node agrees on viewer counts and call rosters.
//!
//! Stream viewer counting is **sharded 256 ways** (ROADMAP 方向四: Redis 热键分片)
//! to avoid a single sorted-set hot key when thousands of viewers join one stream.
//! Call rosters carry far fewer participants, so they stay as single keys.

use aero_common::{CallId, ParticipantId};
use fred::prelude::{ClientLike, RedisClient, SortedSetsInterface};
use fred::types::{CustomCommand, Ordering, ZRange, ZRangeBound, ZRangeKind};
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

/// Generation side state colocated with the legacy roster key in Redis
/// Cluster. The hash tag is the *entire* untagged roster key, so both keys have
/// the same CRC16 input without changing the established roster namespace.
#[must_use]
fn roster_generation_key(call: CallId) -> String {
    format!("live:roster:generation:{{{}}}", roster_key(call))
}

const ROSTER_STAMP_UNFENCED: &str = r"
redis.call('ZADD', KEYS[1], 'GT', ARGV[1], ARGV[2])
redis.call('HDEL', KEYS[2], ARGV[2])
return 1
";

const ROSTER_STAMP_GENERATION: &str = r"
local function compare_i64(a, b)
  if a == b then return 0 end
  local a_negative = string.sub(a, 1, 1) == '-'
  local b_negative = string.sub(b, 1, 1) == '-'
  if a_negative ~= b_negative then return a_negative and -1 or 1 end
  local a_digits = a_negative and string.sub(a, 2) or a
  local b_digits = b_negative and string.sub(b, 2) or b
  local magnitude
  if string.len(a_digits) ~= string.len(b_digits) then
    magnitude = string.len(a_digits) > string.len(b_digits) and 1 or -1
  else
    magnitude = a_digits > b_digits and 1 or -1
  end
  return a_negative and -magnitude or magnitude
end
local current = redis.call('HGET', KEYS[2], ARGV[2])
if current and compare_i64(current, ARGV[3]) > 0 then
  return 0
end
if current == ARGV[3] then
  redis.call('ZADD', KEYS[1], 'GT', ARGV[1], ARGV[2])
else
  redis.call('ZADD', KEYS[1], ARGV[1], ARGV[2])
end
redis.call('HSET', KEYS[2], ARGV[2], ARGV[3])
return 1
";

const ROSTER_LEAVE_UNFENCED: &str = r"
redis.call('ZREM', KEYS[1], ARGV[1])
redis.call('HDEL', KEYS[2], ARGV[1])
return 1
";

const ROSTER_LEAVE_GENERATION: &str = r"
local current = redis.call('HGET', KEYS[2], ARGV[1])
if not current or current ~= ARGV[2] then
  return 0
end
redis.call('ZREM', KEYS[1], ARGV[1])
redis.call('HDEL', KEYS[2], ARGV[1])
return 1
";

const ROSTER_PRUNE: &str = r"
local stale = redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', '(' .. ARGV[1])
if #stale == 0 then
  return 0
end
redis.call('ZREM', KEYS[1], unpack(stale))
redis.call('HDEL', KEYS[2], unpack(stale))
return #stale
";

const ROSTER_CLEAR: &str = r"
redis.call('DEL', KEYS[1])
redis.call('DEL', KEYS[2])
return 1
";

/// Whole seconds since the UNIX epoch for `t`, saturating at 0 for any
/// pre-epoch clock.
#[must_use]
fn epoch_secs(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64().trunc())
}

/// The stale-score threshold: any member whose heartbeat is strictly older than
/// `now - ttl` is stale.
#[must_use]
fn stale_threshold(now_secs: f64, ttl: Duration) -> f64 {
    (now_secs - ttl.as_secs_f64().trunc()).max(0.0)
}

/// The exclusive `ZREMRANGEBYSCORE` upper-bound for `threshold`.
///
/// fred treats arbitrary strings as lexicographic bounds, so constructing the
/// typed score range is load-bearing. Passing `format!("({threshold}")` looks
/// like valid Redis syntax but fred rejects it before sending the command.
#[must_use]
fn stale_max_arg(threshold: f64) -> ZRange {
    ZRange {
        kind: ZRangeKind::Exclusive,
        range: ZRangeBound::Score(threshold),
    }
}

/// Parse sorted-set members (participant id strings) back into ids.
#[must_use]
fn parse_participants(members: &[String]) -> Vec<ParticipantId> {
    members
        .iter()
        .filter_map(|m| ParticipantId::from_str(m).ok())
        .collect()
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
        Self {
            client,
            ttl: DEFAULT_TTL,
        }
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
        Self {
            client,
            ttl: DEFAULT_TTL,
        }
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

    /// Join or refresh a participant only when `generation` is not older than
    /// the generation currently owning that roster member.
    pub async fn join_generation(
        &self,
        call: CallId,
        participant: ParticipantId,
        generation: i64,
    ) -> anyhow::Result<bool> {
        self.stamp_generation(call, participant, generation).await
    }

    /// Generation-aware heartbeat. A newer generation takes ownership; the
    /// same generation refreshes liveness; an older generation is rejected.
    pub async fn heartbeat_generation(
        &self,
        call: CallId,
        participant: ParticipantId,
        generation: i64,
    ) -> anyhow::Result<bool> {
        self.stamp_generation(call, participant, generation).await
    }

    async fn stamp(&self, call: CallId, participant: ParticipantId) -> anyhow::Result<()> {
        let score = epoch_secs(SystemTime::now());
        self.eval_roster(
            ROSTER_STAMP_UNFENCED,
            call,
            vec![score.to_string(), participant.to_string()],
        )
        .await?;
        Ok(())
    }

    async fn stamp_generation(
        &self,
        call: CallId,
        participant: ParticipantId,
        generation: i64,
    ) -> anyhow::Result<bool> {
        let result = self
            .eval_roster(
                ROSTER_STAMP_GENERATION,
                call,
                vec![
                    epoch_secs(SystemTime::now()).to_string(),
                    participant.to_string(),
                    generation.to_string(),
                ],
            )
            .await?;
        Ok(result == 1)
    }

    pub async fn leave(&self, call: CallId, participant: ParticipantId) -> anyhow::Result<()> {
        self.eval_roster(ROSTER_LEAVE_UNFENCED, call, vec![participant.to_string()])
            .await?;
        Ok(())
    }

    /// Leave only when the exact generation still owns this roster member.
    pub async fn leave_generation(
        &self,
        call: CallId,
        participant: ParticipantId,
        generation: i64,
    ) -> anyhow::Result<bool> {
        Ok(self
            .eval_roster(
                ROSTER_LEAVE_GENERATION,
                call,
                vec![participant.to_string(), generation.to_string()],
            )
            .await?
            == 1)
    }

    /// Remove the entire cluster roster for an ended call.
    ///
    /// `DEL` is idempotent, so every node may apply the same durable `CallEnd`.
    pub async fn clear(&self, call: CallId) -> anyhow::Result<()> {
        self.eval_roster(ROSTER_CLEAR, call, Vec::new()).await?;
        Ok(())
    }

    pub async fn count(&self, call: CallId) -> anyhow::Result<u64> {
        let key = roster_key(call);
        self.prune(call).await?;
        let n: i64 = self.client.zcard(&key).await?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    pub async fn roster(&self, call: CallId) -> anyhow::Result<Vec<ParticipantId>> {
        let key = roster_key(call);
        self.prune(call).await?;
        let members: Vec<String> = self
            .client
            .zrange(&key, 0, -1, None, false, None, false)
            .await?;
        Ok(parse_participants(&members))
    }

    async fn prune(&self, call: CallId) -> anyhow::Result<()> {
        let threshold = stale_threshold(epoch_secs(SystemTime::now()), self.ttl);
        self.eval_roster(ROSTER_PRUNE, call, vec![threshold.to_string()])
            .await?;
        Ok(())
    }

    async fn eval_roster(
        &self,
        script: &'static str,
        call: CallId,
        arguments: Vec<String>,
    ) -> anyhow::Result<i64> {
        let keys = [roster_key(call), roster_generation_key(call)];
        let command = CustomCommand::new_static("EVAL", keys[0].as_str(), false);
        let mut args = Vec::with_capacity(4 + arguments.len());
        args.push(script.to_owned());
        args.push(keys.len().to_string());
        args.extend(keys);
        args.extend(arguments);
        let result = self.client.custom(command, args).await?;
        Ok(result)
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
    fn viewer_shard_key_is_deterministic() {
        let stream = Ulid::new();
        let pid = ParticipantId::new();
        let k1 = viewer_shard_key(stream, pid);
        let k2 = viewer_shard_key(stream, pid);
        assert_eq!(k1, k2);
        assert!(k1.contains("live:viewers:stream:"));
        assert!(k1.contains(":shard:"));
    }

    #[test]
    fn roster_generation_key_is_cluster_colocated_without_changing_roster_key() {
        let call = CallId::new();
        let roster = roster_key(call);
        let generation = roster_generation_key(call);
        assert_eq!(roster, format!("live:roster:call:{call}"));
        assert!(generation.contains(&format!("{{{roster}}}")));
    }

    #[test]
    fn roster_generation_lua_rejects_older_and_requires_exact_leave() {
        assert!(ROSTER_STAMP_GENERATION.contains("compare_i64(current, ARGV[3]) > 0"));
        assert!(ROSTER_LEAVE_GENERATION.contains("current ~= ARGV[2]"));
        assert!(ROSTER_PRUNE.contains("redis.call('HDEL', KEYS[2], unpack(stale))"));
        assert!(ROSTER_CLEAR.contains("redis.call('DEL', KEYS[2])"));
    }

    #[test]
    fn roster_member_parser_keeps_wire_format_and_drops_side_state_garbage() {
        let participant = ParticipantId::new();
        assert_eq!(
            parse_participants(&[participant.to_string(), "__aero_generation__:42".to_owned(),]),
            vec![participant]
        );
    }
}

/// Redis-gated integration tests (run with a live Redis):
///
/// ```text
/// REDIS_URL=redis://localhost:6379 \
///   cargo test -p aero-storage --lib -- --ignored live_presence_
/// ```
#[cfg(test)]
mod redis_tests {
    use super::*;
    use fred::prelude::{
        ClientLike, HashesInterface, KeysInterface, RedisClient, SortedSetsInterface,
    };

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
    async fn live_presence_call_roster_prunes_stale_score_with_fred_typed_bound() {
        let client = client().await;
        let call = CallId::new();
        let participant = ParticipantId::new();
        let key = roster_key(call);
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

        let roster = CallRosterStore::new(client.clone())
            .roster(call)
            .await
            .unwrap();
        assert!(roster.is_empty(), "stale roster member is pruned");
        let _: i64 = client.del(key).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires live Redis"]
    async fn live_presence_call_roster_generation_fences_late_heartbeat_and_leave() {
        let client = client().await;
        let call = CallId::new();
        let participant = ParticipantId::new();
        let roster_key = roster_key(call);
        let generation_key = roster_generation_key(call);
        let roster = CallRosterStore::new(client.clone());
        let old_generation = 9_007_199_254_740_992_i64;
        let current_generation = old_generation + 1;

        assert!(roster
            .join_generation(call, participant, old_generation)
            .await
            .unwrap());
        assert!(roster
            .join_generation(call, participant, current_generation)
            .await
            .unwrap());

        // Pin a sentinel score so a stale heartbeat would be observable even
        // when all test operations happen within the same wall-clock second.
        let _: i64 = client
            .zadd(
                &roster_key,
                None,
                None,
                false,
                false,
                (123.0, participant.to_string()),
            )
            .await
            .unwrap();
        assert!(!roster
            .heartbeat_generation(call, participant, old_generation)
            .await
            .unwrap());
        assert!(!roster
            .leave_generation(call, participant, old_generation)
            .await
            .unwrap());

        let score: Option<f64> = client
            .zscore(&roster_key, participant.to_string())
            .await
            .unwrap();
        let generation: Option<i64> = client
            .hget(&generation_key, participant.to_string())
            .await
            .unwrap();
        assert_eq!(score, Some(123.0), "stale heartbeat did not refresh g2");
        assert_eq!(
            generation,
            Some(current_generation),
            "stale leave did not remove g2"
        );

        assert!(roster
            .heartbeat_generation(call, participant, current_generation)
            .await
            .unwrap());
        assert!(roster
            .leave_generation(call, participant, current_generation)
            .await
            .unwrap());
        roster.clear(call).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires live Redis"]
    async fn live_presence_viewer_count_prunes_stale_score_with_fred_typed_bound() {
        let client = client().await;
        let stream = Ulid::new();
        let participant = ParticipantId::new();
        let key = viewer_shard_key(stream, participant);
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

        let count = StreamViewerStore::new(client.clone())
            .count(stream)
            .await
            .unwrap();
        assert_eq!(count, 0, "stale viewer is pruned");
        let _: i64 = client.del(key).await.unwrap();
    }
}
