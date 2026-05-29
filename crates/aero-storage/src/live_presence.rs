//! Cross-node live presence — stream viewer counts and call rosters.
//!
//! ROADMAP 方向二/五: today `Hub`'s `stream_watchers` / `call_rosters` are
//! process-local `DashMap`s, so with more than one server node a viewer count
//! or a call roster is "each node counts its own" — wrong in a cluster. These
//! stores move that state into Redis so every node agrees, mirroring how
//! [`crate::presence::PresenceStore`] already makes room presence cluster-correct.
//!
//! Design: a **sorted set with heartbeat**, one per stream / call. The member is
//! the participant id; the score is the participant's last-seen epoch second.
//! `join`/`heartbeat` is a `ZADD` that (re)stamps the score; `leave` is a `ZREM`.
//! A read (`count` / `viewers` / `roster`) first prunes anyone whose heartbeat is
//! older than the TTL (`ZREMRANGEBYSCORE 0 (now-ttl)`) so a crashed client that
//! never sent `leave` ages out automatically — the same self-healing property
//! the TTL gives `PresenceStore`. The TTL is configurable per store.
//!
//! This module is purely additive: it introduces NEW stores and does not touch
//! any existing repo or signature. The score/threshold math and key builders are
//! free functions with no Redis dependency, so they unit-test directly (the live
//! Redis calls can't run in-sandbox, consistent with `presence.rs`).

use aero_common::{CallId, ParticipantId};
use fred::prelude::{RedisClient, SortedSetsInterface};
use fred::types::Ordering;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use ulid::Ulid;

/// Default heartbeat TTL: a member not re-stamped within this window is pruned
/// on the next read. Chosen to comfortably exceed a typical client heartbeat
/// interval while still evicting crashed clients promptly.
pub const DEFAULT_TTL: Duration = Duration::from_secs(30);

// ---------- Pure, DB-free helpers (unit-tested) ----------

/// Redis key for a stream's viewer sorted set.
#[must_use]
fn viewer_key(stream: Ulid) -> String {
    format!("live:viewers:stream:{stream}")
}

/// Redis key for a call's roster sorted set.
#[must_use]
fn roster_key(call: CallId) -> String {
    format!("live:roster:call:{call}")
}

/// Whole seconds since the UNIX epoch for `t`, saturating at 0 for any
/// pre-epoch clock. Never panics. Returned as `f64` because that is the native
/// sorted-set score type; the value is whole seconds, which is exactly
/// representable in `f64` for any realistic timestamp (far beyond year 9999).
#[must_use]
fn epoch_secs(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64().trunc())
}

/// The stale-score threshold: any member whose heartbeat is strictly older than
/// `now - ttl` is stale. Saturates at 0 so an early/skewed clock never produces
/// a negative bound.
#[must_use]
fn stale_threshold(now_secs: f64, ttl: Duration) -> f64 {
    (now_secs - ttl.as_secs_f64().trunc()).max(0.0)
}

/// The exclusive `ZREMRANGEBYSCORE` upper-bound argument for `threshold`,
/// i.e. `(threshold`. Removing `[0, (threshold)` evicts every member whose
/// heartbeat second is `< threshold` while keeping anyone seen at-or-after it.
/// The threshold is a whole second, so it Display-formats without a fraction.
#[must_use]
fn stale_max_arg(threshold: f64) -> String {
    format!("({threshold}")
}

// ---------- Stream viewer count ----------

/// Cluster-correct viewer set for a live stream, keyed by stream id.
///
/// Replaces `Hub::stream_watchers` (a per-process `DashMap`) for counting:
/// every node `join`s/`heartbeat`s into the same Redis sorted set, so
/// `count` is the true global audience regardless of which node a viewer's
/// WHEP/WS connection landed on.
#[derive(Clone)]
pub struct StreamViewerStore {
    client: RedisClient,
    ttl: Duration,
}

impl StreamViewerStore {
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

    /// Register `participant` as a viewer of `stream` (or refresh an existing
    /// entry). Stamps the member's score with the current epoch second.
    pub async fn join(&self, stream: Ulid, participant: ParticipantId) -> anyhow::Result<()> {
        self.stamp(stream, participant).await
    }

    /// Refresh `participant`'s liveness on `stream`. Identical to [`Self::join`];
    /// named separately so call sites read as a periodic keep-alive.
    pub async fn heartbeat(&self, stream: Ulid, participant: ParticipantId) -> anyhow::Result<()> {
        self.stamp(stream, participant).await
    }

    async fn stamp(&self, stream: Ulid, participant: ParticipantId) -> anyhow::Result<()> {
        let key = viewer_key(stream);
        let score = epoch_secs(SystemTime::now());
        // ZADD member=participant score=now. Ordering::GreaterThan keeps the
        // score monotonic if two updates race, never moving a member backward.
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
        let key = viewer_key(stream);
        let _: i64 = self.client.zrem(&key, participant.to_string()).await?;
        Ok(())
    }

    /// Current global viewer count for `stream`, after pruning stale heartbeats.
    pub async fn count(&self, stream: Ulid) -> anyhow::Result<u64> {
        let key = viewer_key(stream);
        self.prune(&key).await?;
        let n: i64 = self.client.zcard(&key).await?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Current global viewer set for `stream`, after pruning stale heartbeats.
    pub async fn viewers(&self, stream: Ulid) -> anyhow::Result<Vec<ParticipantId>> {
        let key = viewer_key(stream);
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
}

// ---------- Call roster ----------

/// Cluster-correct participant roster for a call, keyed by call id.
///
/// Replaces `Hub::call_rosters` (a per-process `DashMap`) so a group call's
/// roster is consistent across nodes: a participant connected to node A is
/// visible to a participant on node B.
#[derive(Clone)]
pub struct CallRosterStore {
    client: RedisClient,
    ttl: Duration,
}

impl CallRosterStore {
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

    /// Add `participant` to `call`'s roster (or refresh an existing entry).
    pub async fn join(&self, call: CallId, participant: ParticipantId) -> anyhow::Result<()> {
        self.stamp(call, participant).await
    }

    /// Refresh `participant`'s liveness on `call`. Identical to [`Self::join`].
    pub async fn heartbeat(&self, call: CallId, participant: ParticipantId) -> anyhow::Result<()> {
        self.stamp(call, participant).await
    }

    async fn stamp(&self, call: CallId, participant: ParticipantId) -> anyhow::Result<()> {
        let key = roster_key(call);
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

    /// Remove `participant` from `call`'s roster.
    pub async fn leave(&self, call: CallId, participant: ParticipantId) -> anyhow::Result<()> {
        let key = roster_key(call);
        let _: i64 = self.client.zrem(&key, participant.to_string()).await?;
        Ok(())
    }

    /// Current global participant count for `call`, after pruning stale entries.
    pub async fn count(&self, call: CallId) -> anyhow::Result<u64> {
        let key = roster_key(call);
        self.prune(&key).await?;
        let n: i64 = self.client.zcard(&key).await?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Current global roster for `call`, after pruning stale entries.
    pub async fn roster(&self, call: CallId) -> anyhow::Result<Vec<ParticipantId>> {
        let key = roster_key(call);
        self.prune(&key).await?;
        let members: Vec<String> = self
            .client
            .zrange(&key, 0, -1, None, false, None, false)
            .await?;
        Ok(parse_participants(&members))
    }

    async fn prune(&self, key: &str) -> anyhow::Result<()> {
        let threshold = stale_threshold(epoch_secs(SystemTime::now()), self.ttl);
        let _: i64 = self
            .client
            .zremrangebyscore(key, 0.0, stale_max_arg(threshold))
            .await?;
        Ok(())
    }
}

/// Parse sorted-set members back into [`ParticipantId`]s, dropping any that fail
/// to decode (defensive: a malformed member should never poison the whole list).
#[must_use]
fn parse_participants(members: &[String]) -> Vec<ParticipantId> {
    members
        .iter()
        .filter_map(|m| ParticipantId::from_str(m).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        epoch_secs, parse_participants, roster_key, stale_max_arg, stale_threshold, viewer_key,
        DEFAULT_TTL,
    };
    use aero_common::{CallId, ParticipantId};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use ulid::Ulid;

    #[test]
    fn keys_are_namespaced_and_stable() {
        let stream = Ulid::from_parts(42, 7);
        let call = CallId::new();
        let vk = viewer_key(stream);
        let rk = roster_key(call);

        assert_eq!(vk, format!("live:viewers:stream:{stream}"));
        assert_eq!(rk, format!("live:roster:call:{call}"));
        // Distinct namespaces so viewer and roster sets never collide.
        assert!(vk.starts_with("live:viewers:stream:"));
        assert!(rk.starts_with("live:roster:call:"));
        assert_ne!(vk, rk);
        // Deterministic for the same id.
        assert_eq!(vk, viewer_key(stream));
    }

    /// Whole-second f64s used in the threshold math are exact, so equality is a
    /// valid assertion here; this helper keeps clippy's `float_cmp` lint happy.
    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < f64::EPSILON
    }

    #[test]
    fn epoch_secs_is_monotonic_and_non_negative() {
        assert!(approx(epoch_secs(UNIX_EPOCH), 0.0));
        let t1 = UNIX_EPOCH + Duration::from_secs(1_000);
        let t2 = UNIX_EPOCH + Duration::from_secs(2_000);
        assert!(approx(epoch_secs(t1), 1_000.0));
        assert!(epoch_secs(t2) > epoch_secs(t1));
        // A pre-epoch instant saturates at 0 rather than going negative.
        let pre = UNIX_EPOCH - Duration::from_secs(5);
        assert!(approx(epoch_secs(pre), 0.0));
        // Sanity: "now" is well past the epoch.
        assert!(epoch_secs(SystemTime::now()) > 1_700_000_000.0);
    }

    #[test]
    fn stale_threshold_is_now_minus_ttl() {
        // The prune upper bound is exactly now - ttl.
        assert!(approx(stale_threshold(1_000.0, Duration::from_secs(30)), 970.0));
        assert!(approx(stale_threshold(1_000.0, DEFAULT_TTL), 1_000.0 - 30.0));
        // Zero TTL means everything strictly before "now" is stale.
        assert!(approx(stale_threshold(1_000.0, Duration::from_secs(0)), 1_000.0));
    }

    #[test]
    fn stale_threshold_saturates_at_zero() {
        // When ttl exceeds elapsed time, the bound floors at 0 (never negative),
        // so the prune is a harmless no-op on a freshly-populated set.
        assert!(approx(stale_threshold(10.0, Duration::from_secs(30)), 0.0));
        assert!(approx(stale_threshold(0.0, DEFAULT_TTL), 0.0));
    }

    #[test]
    fn stale_max_arg_is_exclusive() {
        // Redis exclusive-bound syntax: members with score < threshold are pruned,
        // a member stamped exactly at `threshold` survives. Whole seconds format
        // without a fractional part.
        assert_eq!(stale_max_arg(970.0), "(970");
        assert_eq!(stale_max_arg(0.0), "(0");
    }

    #[test]
    fn fresh_member_survives_its_own_threshold() {
        // End-to-end of the score math: a member seen at `now` has score == now,
        // the prune removes score < (now - ttl); since now >= now - ttl, the
        // member is kept. (Guards against an off-by-one making live clients vanish.)
        let now = epoch_secs(SystemTime::now());
        let threshold = stale_threshold(now, DEFAULT_TTL);
        assert!(now >= threshold, "a just-seen member must not be pruned");
    }

    #[test]
    fn parse_participants_roundtrips_and_drops_garbage() {
        let a = ParticipantId::new();
        let b = ParticipantId::new();
        let members = vec![a.to_string(), "not-a-ulid".to_string(), b.to_string()];
        let parsed = parse_participants(&members);
        // Valid ids decode in order; the malformed entry is silently dropped.
        assert_eq!(parsed, vec![a, b]);
        // Empty input yields empty output.
        assert!(parse_participants(&[]).is_empty());
    }
}
