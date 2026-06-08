//! Presence tracking — who is online in which room, **cluster-wide**.
//!
//! ROADMAP 方向一: room presence is a Redis **sorted set with heartbeat**, one
//! per room (`presence:room:{room}`). The member is the participant id; the
//! score is their last-seen epoch second. `join`/`heartbeat` is a `ZADD` that
//! (re)stamps the score; `leave` is a `ZREM`. A read (`count` / `members`)
//! first prunes anyone whose heartbeat is older than the TTL
//! (`ZREMRANGEBYSCORE 0 (now-ttl)`), so a crashed client that never sent
//! `leave` ages out automatically. Because every node stamps into the same
//! Redis key, `count`/`members` are the true cluster-wide roster regardless of
//! which node a participant's WebSocket landed on — the local `Hub` roster is
//! only a degradation fallback when Redis is unreachable.
//!
//! This mirrors [`crate::live_presence::StreamViewerStore`] exactly (same
//! score/threshold math), keyed by `RoomId` instead of a stream id. The pure
//! helpers carry no Redis dependency so they unit-test directly.

use aero_common::{ParticipantId, RoomId};
use fred::prelude::{KeysInterface, RedisClient, SortedSetsInterface};
use fred::types::Ordering;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Default heartbeat TTL: a member not re-stamped within this window is pruned
/// on the next read. Comfortably exceeds a typical WS client heartbeat interval
/// while still evicting crashed clients promptly.
pub const DEFAULT_TTL: Duration = Duration::from_secs(45);

// ---------- Pure, DB-free helpers (unit-tested) ----------

/// Redis key for a room's presence sorted set.
#[must_use]
fn room_key(room: RoomId) -> String {
    format!("presence:room:{room}")
}

/// Whole seconds since the UNIX epoch, saturating at 0 for any pre-epoch clock.
#[must_use]
fn epoch_secs(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64().trunc())
}

/// The stale-score threshold: a member whose heartbeat is strictly older than
/// `now - ttl` is stale. Saturates at 0 so a skewed clock never goes negative.
#[must_use]
fn stale_threshold(now_secs: f64, ttl: Duration) -> f64 {
    (now_secs - ttl.as_secs_f64().trunc()).max(0.0)
}

/// The exclusive `ZREMRANGEBYSCORE` upper-bound argument for `threshold`, i.e.
/// `(threshold` — evicts every member seen strictly before `threshold`.
#[must_use]
fn stale_max_arg(threshold: f64) -> String {
    format!("({threshold}")
}

/// Parse sorted-set members (participant id strings) back into ids, skipping
/// any that fail to parse rather than failing the whole read.
#[must_use]
fn parse_participants(members: &[String]) -> Vec<ParticipantId> {
    members.iter().filter_map(|m| ParticipantId::from_str(m).ok()).collect()
}

/// Cluster-correct room presence, keyed by room id.
#[derive(Clone)]
pub struct PresenceStore {
    client: RedisClient,
    ttl: Duration,
}

impl PresenceStore {
    /// Build a store using [`DEFAULT_TTL`].
    #[must_use]
    pub fn new(client: RedisClient) -> Self {
        Self { client, ttl: DEFAULT_TTL }
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

    /// Refresh `participant`'s liveness in `room`. Identical to [`Self::join`];
    /// named separately so call sites read as a periodic keep-alive.
    pub async fn heartbeat(&self, room: RoomId, participant: ParticipantId) -> anyhow::Result<()> {
        self.stamp(room, participant).await
    }

    async fn stamp(&self, room: RoomId, participant: ParticipantId) -> anyhow::Result<()> {
        let key = room_key(room);
        let score = epoch_secs(SystemTime::now());
        // ZADD member=participant score=now. `GreaterThan` keeps the score
        // monotonic if two updates race, never moving a member backward.
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
        let key = room_key(room);
        let _: i64 = self.client.zrem(&key, participant.to_string()).await?;
        Ok(())
    }

    /// Cluster-wide online count for `room`, after pruning stale heartbeats.
    pub async fn count(&self, room: RoomId) -> anyhow::Result<u64> {
        let key = room_key(room);
        self.prune(&key).await?;
        let n: i64 = self.client.zcard(&key).await?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Cluster-wide online member set for `room`, after pruning stale heartbeats.
    pub async fn members(&self, room: RoomId) -> anyhow::Result<Vec<ParticipantId>> {
        let key = room_key(room);
        self.prune(&key).await?;
        let members: Vec<String> = self
            .client
            .zrange(&key, 0, -1, None, false, None, false)
            .await?;
        Ok(parse_participants(&members))
    }

    /// Evict members whose heartbeat is older than the TTL.
    async fn prune(&self, key: &str) -> anyhow::Result<()> {
        let threshold = stale_threshold(epoch_secs(SystemTime::now()), self.ttl);
        let _: i64 = self
            .client
            .zremrangebyscore(key, 0.0, stale_max_arg(threshold))
            .await?;
        Ok(())
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
        assert_eq!(epoch_secs(t), 1.0); // truncated to whole seconds
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
    fn parse_participants_skips_garbage() {
        let p = ParticipantId::new();
        let members = vec![p.to_string(), "not-an-id".to_string()];
        assert_eq!(parse_participants(&members), vec![p]);
    }
}
