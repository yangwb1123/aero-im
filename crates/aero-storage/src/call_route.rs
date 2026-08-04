//! Cross-node group-call routing registry (ROADMAP3 方向二 — 分布式通话基底).
//!
//! Records, in Redis with a TTL, which node currently hosts each group-call
//! participant's media leg. V2 stores one expiring lease per participant plus
//! a per-call index, so a healthy participant cannot keep a crashed peer's
//! route alive by refreshing a shared hash TTL.
//!
//! During a rolling upgrade every write is also mirrored to the original
//! `callroute:{call}` hash. An empty-valued marker field tells V2 readers that
//! a legacy participant field is backed by a V2 lease; old readers already
//! ignore empty node values. When a lease expires, a V2 reader removes the
//! stale index member and its matching legacy field. Legacy-only fields remain
//! visible, so old and new server versions can coexist.
//!
//! After every server is upgraded, set `AERO_CALL_ROUTE_V2_ONLY=true` (R2).
//! R2 stops the legacy dual-write and atomically deletes the compatibility
//! hash whenever the call is touched. This explicit second phase is what
//! removes markerless fields left by a crashed pre-V2 node; enabling it while
//! old nodes are still serving calls would intentionally hide those nodes.
//!
//! This is the call analogue of [`crate::stream_route::StreamRouteRegistry`]:
//! where a live stream has exactly **one** owning node (a single value with a
//! TTL), a group call is hosted by **several** nodes at once — so the unit of
//! state is a hash, and the interesting read is the per-node participant
//! census ([`CallRouteRegistry::nodes_for_call`]) that the topology policy
//! (`decide_call_topology` in `aero-live-webrtc`) turns into "which other
//! nodes must this node bridge media from".
//!
//! Mirrors [`crate::stream_route::StreamRouteRegistry`]'s Redis access style;
//! the aggregation/filtering over the hash is pure and unit-tested without
//! Redis (the live calls are gated `#[ignore]`, consistent with
//! `stream_route.rs`).

use aero_common::{CallId, ParticipantId};
use fred::prelude::{HashesInterface, KeysInterface, RedisClient};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::str::FromStr;
use std::time::Duration;

mod v2;
use v2::{DELETE_V2, DELETE_V2_IF_NODE, REGISTER_LEGACY, REGISTER_V2};

/// How long an individual participant route survives without a heartbeat.
///
/// The server heartbeat defaults to 30 seconds. Four heartbeat periods leave
/// room for scheduler stalls and transient Redis errors without retaining a
/// crashed route indefinitely.
pub const DEFAULT_TTL: Duration = Duration::from_secs(120);

const V2_MARKER_PREFIX: &str = "__aero_callroute_v2__:";
const V2_ONLY_ENV: &str = "AERO_CALL_ROUTE_V2_ONLY";

/// Redis-backed registry of `call -> { participant lease -> hosting node }`.
#[derive(Clone)]
pub struct CallRouteRegistry {
    client: RedisClient,
    ttl: Duration,
    v2_only: bool,
}

impl CallRouteRegistry {
    /// Build a registry using [`DEFAULT_TTL`].
    #[must_use]
    pub fn new(client: RedisClient) -> Self {
        Self {
            client,
            ttl: DEFAULT_TTL,
            v2_only: v2_only_from_env(std::env::var(V2_ONLY_ENV).ok().as_deref()),
        }
    }

    /// Override the entry TTL.
    #[must_use]
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    #[cfg(test)]
    fn with_v2_only(mut self, v2_only: bool) -> Self {
        self.v2_only = v2_only;
        self
    }

    fn legacy_key(call: CallId) -> String {
        format!("callroute:{call}")
    }

    fn index_key(call: CallId) -> String {
        format!("callroute:v2:{{{call}}}:participants")
    }

    fn lease_key(call: CallId, participant: &str) -> String {
        format!("callroute:v2:{{{call}}}:participant:{participant}")
    }

    fn generation_key(call: CallId, participant: &str) -> String {
        format!("callroute:v2:{{{call}}}:participant:{participant}:generation")
    }

    fn marker_prefix(participant: &str) -> String {
        format!("{V2_MARKER_PREFIX}{participant}:")
    }

    fn ttl_secs(&self) -> i64 {
        i64::try_from(self.ttl.as_secs().max(1)).unwrap_or(i64::MAX / 2)
    }

    /// Record (or refresh) that `participant`'s media leg for `call` is hosted
    /// on `node_url`. Idempotent; only this participant's V2 lease is re-armed.
    ///
    /// The legacy hash is dual-written for rolling upgrades. Its empty marker
    /// value is invisible to old aggregation code.
    pub async fn register_participant(
        &self,
        call: CallId,
        participant: ParticipantId,
        node_url: &str,
    ) -> anyhow::Result<()> {
        let participant = participant.to_string();
        let node = normalize_node(node_url);
        let lease_key = Self::lease_key(call, &participant);
        let index_key = Self::index_key(call);
        let ttl = self.ttl_secs();
        let index_ttl = ttl.saturating_mul(2);

        self.eval_i64(
            REGISTER_V2,
            vec![
                lease_key,
                index_key,
                Self::generation_key(call, &participant),
            ],
            vec![
                node.to_owned(),
                participant.clone(),
                ttl.to_string(),
                index_ttl.to_string(),
            ],
        )
        .await?;

        if self.v2_only {
            // R2 cutover: all nodes must be upgraded before enabling this.
            // Removing the whole compatibility hash is atomic and prevents
            // markerless fields left by crashed old nodes from surviving.
            let _: () = self.client.del(Self::legacy_key(call)).await?;
            return Ok(());
        }

        let marker_prefix = Self::marker_prefix(&participant);
        let marker = format!("{marker_prefix}{}", ulid::Ulid::new());
        let legacy_key = Self::legacy_key(call);
        self.eval_i64(
            REGISTER_LEGACY,
            vec![legacy_key],
            vec![
                participant,
                node.to_owned(),
                marker_prefix,
                marker,
                ttl.to_string(),
            ],
        )
        .await?;
        Ok(())
    }

    /// Refresh `participant`'s mapping and individual lease (alias of
    /// [`Self::register_participant`]).
    pub async fn heartbeat(
        &self,
        call: CallId,
        participant: ParticipantId,
        node_url: &str,
    ) -> anyhow::Result<()> {
        self.register_participant(call, participant, node_url).await
    }

    /// Drop `participant`'s V2 lease, index member, and compatibility mapping.
    pub async fn unregister_participant(
        &self,
        call: CallId,
        participant: ParticipantId,
    ) -> anyhow::Result<()> {
        let participant = participant.to_string();
        let legacy_key = Self::legacy_key(call);
        let markers = if self.v2_only {
            Vec::new()
        } else {
            let legacy: HashMap<String, String> = self.client.hgetall(&legacy_key).await?;
            markers_for(&legacy, &participant)
        };

        self.eval_i64(
            DELETE_V2,
            vec![
                Self::lease_key(call, &participant),
                Self::index_key(call),
                Self::generation_key(call, &participant),
            ],
            vec![participant.clone()],
        )
        .await?;
        if self.v2_only {
            let _: () = self.client.del(legacy_key).await?;
            return Ok(());
        }
        self.delete_legacy_if_unchanged(&legacy_key, &participant, "", &markers)
            .await?;
        Ok(())
    }

    /// Drop **every** participant mapped to `node_url` for `call` — the
    /// "last local participant left, deregister this node" path. Also clears
    /// any stale local entries left by participants that vanished without an
    /// explicit leave.
    pub async fn unregister_node(&self, call: CallId, node_url: &str) -> anyhow::Result<()> {
        let target = normalize_node(node_url);
        let snapshot = self.snapshot(call).await?;
        let participants: Vec<String> = snapshot
            .entries
            .iter()
            .filter(|(_, node)| normalize_node(node) == target)
            .map(|(participant, _)| participant.clone())
            .collect();

        for participant in participants {
            self.eval_i64(
                DELETE_V2_IF_NODE,
                vec![
                    Self::lease_key(call, &participant),
                    Self::index_key(call),
                    Self::generation_key(call, &participant),
                ],
                vec![participant.clone(), target.to_owned()],
            )
            .await?;
            let markers = snapshot
                .legacy_markers
                .get(&participant)
                .map_or(&[] as &[String], Vec::as_slice);
            self.delete_legacy_if_unchanged(&Self::legacy_key(call), &participant, target, markers)
                .await?;
        }
        Ok(())
    }

    /// The nodes currently hosting participants of `call`, with each node's
    /// participant count, sorted by node URL (deterministic). Empty when the
    /// call is unregistered or all of its participant leases expired.
    pub async fn nodes_for_call(&self, call: CallId) -> anyhow::Result<Vec<(String, u32)>> {
        Ok(aggregate_nodes(&self.snapshot(call).await?.entries))
    }

    /// The participants of `call` whose media leg is hosted on `node_url`,
    /// sorted by id (deterministic).
    pub async fn participants_on(
        &self,
        call: CallId,
        node_url: &str,
    ) -> anyhow::Result<Vec<ParticipantId>> {
        Ok(participants_on_node(
            &self.snapshot(call).await?.entries,
            node_url,
        ))
    }
}

struct RouteSnapshot {
    entries: HashMap<String, String>,
    legacy_markers: HashMap<String, Vec<String>>,
}

fn v2_only_from_env(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Normalize a node base URL so `http://a` and `http://a/` are the same node
/// (same rule as `stream_route`'s comparison).
fn normalize_node(url: &str) -> &str {
    url.trim_end_matches('/')
}

fn marker_participant(field: &str) -> Option<&str> {
    let suffix = field.strip_prefix(V2_MARKER_PREFIX)?;
    let (participant, generation) = suffix.split_once(':')?;
    if participant.is_empty() || generation.is_empty() {
        return None;
    }
    Some(participant)
}

fn markers_by_participant(entries: &HashMap<String, String>) -> HashMap<String, Vec<String>> {
    let mut markers: HashMap<String, Vec<String>> = HashMap::new();
    for field in entries.keys() {
        if let Some(participant) = marker_participant(field) {
            markers
                .entry(participant.to_owned())
                .or_default()
                .push(field.clone());
        }
    }
    for fields in markers.values_mut() {
        fields.sort();
    }
    markers
}

fn markers_for(entries: &HashMap<String, String>, participant: &str) -> Vec<String> {
    markers_by_participant(entries)
        .remove(participant)
        .unwrap_or_default()
}

/// Merge an old-version hash with live V2 leases.
///
/// A participant named by either the V2 index or a compatibility marker must
/// never fall back to its legacy field in the same snapshot: if its lease is
/// gone, that legacy field is stale and is being pruned. Fields with no V2
/// evidence are old-version writers and remain visible during rolling deploys.
fn merge_routes(
    legacy: &HashMap<String, String>,
    v2: &HashMap<String, String>,
    v2_candidates: &BTreeSet<String>,
) -> HashMap<String, String> {
    let mut merged: HashMap<String, String> = legacy
        .iter()
        .filter(|(participant, _)| marker_participant(participant).is_none())
        .filter(|(participant, _)| !v2_candidates.contains(*participant))
        .map(|(participant, node)| (participant.clone(), node.clone()))
        .collect();
    merged.extend(
        v2.iter()
            .map(|(participant, node)| (participant.clone(), normalize_node(node).to_owned())),
    );
    merged
}

/// Group a call's `participant -> node` hash into `(node, participant_count)`
/// pairs, sorted by node URL. Pure so it unit-tests without Redis. Empty node
/// values (malformed writes) are dropped rather than surfacing a bogus target.
fn aggregate_nodes(entries: &HashMap<String, String>) -> Vec<(String, u32)> {
    let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
    for node in entries.values() {
        let node = normalize_node(node);
        if node.is_empty() {
            continue;
        }
        *counts.entry(node).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .map(|(node, n)| (node.to_owned(), n))
        .collect()
}

/// Filter a call's `participant -> node` hash down to the participants hosted
/// on `node_url`, sorted by id. Members that fail to decode are dropped
/// (defensive: one malformed field must not poison the whole roster).
fn participants_on_node(entries: &HashMap<String, String>, node_url: &str) -> Vec<ParticipantId> {
    let target = normalize_node(node_url);
    let mut out: Vec<ParticipantId> = entries
        .iter()
        .filter(|(_, node)| normalize_node(node) == target)
        .filter_map(|(participant, _)| ParticipantId::from_str(participant).ok())
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_map(pairs: &[(ParticipantId, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(p, node)| (p.to_string(), (*node).to_owned()))
            .collect()
    }

    #[test]
    fn keys_are_namespaced_deterministic_and_cluster_colocated() {
        let c = CallId(ulid::Ulid::nil());
        let p = ParticipantId(ulid::Ulid::nil()).to_string();
        assert_eq!(CallRouteRegistry::legacy_key(c), format!("callroute:{c}"));
        assert_eq!(
            CallRouteRegistry::index_key(c),
            format!("callroute:v2:{{{c}}}:participants")
        );
        assert_eq!(
            CallRouteRegistry::lease_key(c, &p),
            format!("callroute:v2:{{{c}}}:participant:{p}")
        );
        assert_eq!(
            CallRouteRegistry::generation_key(c, &p),
            format!("callroute:v2:{{{c}}}:participant:{p}:generation")
        );
        assert!(CallRouteRegistry::index_key(c).contains(&format!("{{{c}}}")));
        assert!(CallRouteRegistry::lease_key(c, &p).contains(&format!("{{{c}}}")));
        assert!(CallRouteRegistry::generation_key(c, &p).contains(&format!("{{{c}}}")));
    }

    #[test]
    fn default_lease_spans_four_default_heartbeat_periods() {
        assert_eq!(DEFAULT_TTL, Duration::from_secs(120));
    }

    #[test]
    fn v2_only_is_explicit_and_fail_safe() {
        for enabled in ["1", "true", " TRUE ", "yes", "on"] {
            assert!(v2_only_from_env(Some(enabled)), "{enabled}");
        }
        for disabled in ["", "0", "false", "no", "typo"] {
            assert!(!v2_only_from_env(Some(disabled)), "{disabled}");
        }
        assert!(!v2_only_from_env(None));
    }

    #[test]
    fn compatibility_markers_are_invisible_to_old_aggregation() {
        let participant = ParticipantId::new();
        let mut entries = entry_map(&[(participant, "http://node-a.example")]);
        entries.insert(
            format!(
                "{}generation",
                CallRouteRegistry::marker_prefix(&participant.to_string())
            ),
            String::new(),
        );

        assert_eq!(
            aggregate_nodes(&entries),
            vec![("http://node-a.example".to_owned(), 1)]
        );
        assert_eq!(markers_for(&entries, &participant.to_string()).len(), 1);
    }

    #[test]
    fn merge_prefers_v2_and_never_revives_expired_v2_legacy_field() {
        let (old, moved, expired) = (
            ParticipantId::new().to_string(),
            ParticipantId::new().to_string(),
            ParticipantId::new().to_string(),
        );
        let legacy = HashMap::from([
            (old.clone(), "http://old-node".to_owned()),
            (moved.clone(), "http://stale-node".to_owned()),
            (expired.clone(), "http://crashed-node".to_owned()),
        ]);
        let v2 = HashMap::from([(moved.clone(), "http://current-node/".to_owned())]);
        let candidates = BTreeSet::from([moved.clone(), expired.clone()]);

        assert_eq!(
            merge_routes(&legacy, &v2, &candidates),
            HashMap::from([
                (old, "http://old-node".to_owned()),
                (moved, "http://current-node".to_owned()),
            ])
        );
    }

    #[test]
    fn aggregate_nodes_counts_and_sorts_deterministically() {
        let (a, b, c) = (
            ParticipantId::new(),
            ParticipantId::new(),
            ParticipantId::new(),
        );
        let entries = entry_map(&[
            (a, "http://node-b.example"),
            (b, "http://node-a.example"),
            (c, "http://node-b.example"),
        ]);
        assert_eq!(
            aggregate_nodes(&entries),
            vec![
                ("http://node-a.example".to_owned(), 1),
                ("http://node-b.example".to_owned(), 2),
            ],
            "grouped by node, counted, sorted by node URL"
        );
    }

    #[test]
    fn aggregate_nodes_normalizes_trailing_slash() {
        let (a, b) = (ParticipantId::new(), ParticipantId::new());
        let entries = entry_map(&[(a, "http://node-a.example/"), (b, "http://node-a.example")]);
        assert_eq!(
            aggregate_nodes(&entries),
            vec![("http://node-a.example".to_owned(), 2)],
            "slash variants are the same node"
        );
    }

    #[test]
    fn aggregate_nodes_drops_empty_values_and_handles_empty_input() {
        let a = ParticipantId::new();
        let entries = entry_map(&[(a, "")]);
        assert!(
            aggregate_nodes(&entries).is_empty(),
            "empty node value dropped"
        );
        assert!(
            aggregate_nodes(&HashMap::new()).is_empty(),
            "empty hash → no nodes"
        );
    }

    #[test]
    fn participants_on_node_filters_sorts_and_drops_garbage() {
        let (a, b, c) = (
            ParticipantId::new(),
            ParticipantId::new(),
            ParticipantId::new(),
        );
        let mut entries = entry_map(&[
            (a, "http://node-a.example"),
            (b, "http://node-b.example"),
            (c, "http://node-a.example/"),
        ]);
        entries.insert("not-a-ulid".to_owned(), "http://node-a.example".to_owned());

        let mut expected = vec![a, c];
        expected.sort();
        // Slash-insensitive lookup; malformed member silently dropped.
        assert_eq!(
            participants_on_node(&entries, "http://node-a.example/"),
            expected
        );
        assert_eq!(
            participants_on_node(&entries, "http://node-b.example"),
            vec![b]
        );
        assert!(participants_on_node(&entries, "http://node-c.example").is_empty());
    }
}

/// Redis-gated integration test (run with a live Redis):
///
/// ```text
/// REDIS_URL=redis://localhost:6379 \
///   cargo test -p aero-storage --lib -- --ignored callroute_
/// ```
#[cfg(test)]
mod redis_tests {
    use super::*;
    use fred::prelude::{ClientLike, HashesInterface, KeysInterface, RedisClient, SetsInterface};

    async fn client() -> RedisClient {
        let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
        let c = RedisClient::new(
            fred::types::RedisConfig::from_url(&url).unwrap(),
            None,
            None,
            None,
        );
        c.connect();
        c.wait_for_connect().await.unwrap();
        c
    }

    async fn legacy_register(
        registry: &CallRouteRegistry,
        call: CallId,
        participant: ParticipantId,
        node: &str,
    ) {
        let key = CallRouteRegistry::legacy_key(call);
        registry
            .client
            .hset::<(), _, _>(&key, (participant.to_string(), normalize_node(node)))
            .await
            .unwrap();
        registry
            .client
            .expire::<(), _>(&key, registry.ttl_secs())
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires live Redis"]
    async fn callroute_register_census_unregister_roundtrip() {
        let reg = CallRouteRegistry::new(client().await);
        let call = CallId::new();
        let (a, b, c) = (
            ParticipantId::new(),
            ParticipantId::new(),
            ParticipantId::new(),
        );

        assert!(
            reg.nodes_for_call(call).await.unwrap().is_empty(),
            "unregistered call is nowhere"
        );

        reg.register_participant(call, a, "http://node-a:3030")
            .await
            .unwrap();
        reg.register_participant(call, b, "http://node-a:3030/")
            .await
            .unwrap();
        reg.register_participant(call, c, "http://node-b:3030")
            .await
            .unwrap();

        assert_eq!(
            reg.nodes_for_call(call).await.unwrap(),
            vec![
                ("http://node-a:3030".to_owned(), 2),
                ("http://node-b:3030".to_owned(), 1)
            ],
        );
        let mut on_a = vec![a, b];
        on_a.sort();
        assert_eq!(
            reg.participants_on(call, "http://node-a:3030")
                .await
                .unwrap(),
            on_a
        );

        // Heartbeat is idempotent — census unchanged.
        reg.heartbeat(call, a, "http://node-a:3030").await.unwrap();
        assert_eq!(reg.nodes_for_call(call).await.unwrap().len(), 2);
        let raw: HashMap<String, String> = reg
            .client
            .hgetall(CallRouteRegistry::legacy_key(call))
            .await
            .unwrap();
        assert_eq!(
            markers_for(&raw, &a.to_string()).len(),
            1,
            "heartbeat rotates rather than accumulating compatibility markers"
        );

        reg.unregister_participant(call, a).await.unwrap();
        assert_eq!(
            reg.nodes_for_call(call).await.unwrap(),
            vec![
                ("http://node-a:3030".to_owned(), 1),
                ("http://node-b:3030".to_owned(), 1)
            ],
        );

        // Node-level deregistration sweeps the remaining node-a entry.
        reg.unregister_node(call, "http://node-a:3030")
            .await
            .unwrap();
        assert_eq!(
            reg.nodes_for_call(call).await.unwrap(),
            vec![("http://node-b:3030".to_owned(), 1)],
        );

        reg.unregister_participant(call, c).await.unwrap();
        assert!(
            reg.nodes_for_call(call).await.unwrap().is_empty(),
            "empty call cleaned up"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Redis"]
    async fn callroute_r1_unions_legacy_and_v2_without_confusing_old_readers() {
        let reg = CallRouteRegistry::new(client().await).with_v2_only(false);
        let call = CallId::new();
        let old = ParticipantId::new();
        let new = ParticipantId::new();

        legacy_register(&reg, call, old, "http://old-node:3030").await;
        reg.register_participant(call, new, "http://new-node:3030")
            .await
            .unwrap();

        assert_eq!(
            reg.nodes_for_call(call).await.unwrap(),
            vec![
                ("http://new-node:3030".to_owned(), 1),
                ("http://old-node:3030".to_owned(), 1),
            ]
        );

        let raw: HashMap<String, String> = reg
            .client
            .hgetall(CallRouteRegistry::legacy_key(call))
            .await
            .unwrap();
        assert_eq!(
            aggregate_nodes(&raw),
            vec![
                ("http://new-node:3030".to_owned(), 1),
                ("http://old-node:3030".to_owned(), 1),
            ],
            "pre-V2 readers ignore empty migration markers"
        );
        assert_eq!(markers_for(&raw, &new.to_string()).len(), 1);

        reg.unregister_participant(call, old).await.unwrap();
        reg.unregister_participant(call, new).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires live Redis"]
    async fn callroute_expired_v2_peer_is_pruned_while_old_peer_refreshes_shared_hash() {
        let reg = CallRouteRegistry::new(client().await)
            .with_ttl(Duration::from_secs(2))
            .with_v2_only(false);
        let call = CallId::new();
        let crashed = ParticipantId::new();
        let old_healthy = ParticipantId::new();

        reg.register_participant(call, crashed, "http://crashed-node:3030")
            .await
            .unwrap();
        legacy_register(&reg, call, old_healthy, "http://old-healthy:3030").await;

        tokio::time::sleep(Duration::from_secs(1)).await;
        // A pre-V2 healthy node re-arms the *whole* compatibility hash. This
        // preserves the crashed field past its individual V2 lease expiry and
        // recreates the original production failure deterministically.
        legacy_register(&reg, call, old_healthy, "http://old-healthy:3030").await;
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        legacy_register(&reg, call, old_healthy, "http://old-healthy:3030").await;

        let legacy_key = CallRouteRegistry::legacy_key(call);
        let before: Option<String> = reg
            .client
            .hget(&legacy_key, crashed.to_string())
            .await
            .unwrap();
        assert_eq!(before.as_deref(), Some("http://crashed-node:3030"));

        assert_eq!(
            reg.nodes_for_call(call).await.unwrap(),
            vec![("http://old-healthy:3030".to_owned(), 1)],
            "expired per-participant lease suppresses the re-armed legacy field"
        );
        let after: Option<String> = reg
            .client
            .hget(&legacy_key, crashed.to_string())
            .await
            .unwrap();
        assert!(after.is_none(), "read-side prune removes stale fallback");
        let still_indexed: bool = reg
            .client
            .sismember(CallRouteRegistry::index_key(call), crashed.to_string())
            .await
            .unwrap();
        assert!(!still_indexed, "read-side prune removes stale index member");

        reg.unregister_participant(call, old_healthy).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires live Redis"]
    async fn callroute_r2_ignores_and_atomically_cleans_markerless_legacy_routes() {
        let reg = CallRouteRegistry::new(client().await).with_v2_only(true);
        let call = CallId::new();
        let stale_old = ParticipantId::new();
        let current = ParticipantId::new();

        legacy_register(&reg, call, stale_old, "http://stale-old-node:3030").await;
        reg.register_participant(call, current, "http://current-node:3030")
            .await
            .unwrap();

        // Simulate a final delayed pre-V2 write after the R2 node registered.
        // The next R2 census must neither admit it nor leave it in Redis.
        legacy_register(&reg, call, stale_old, "http://stale-old-node:3030").await;
        assert_eq!(
            reg.nodes_for_call(call).await.unwrap(),
            vec![("http://current-node:3030".to_owned(), 1)]
        );
        let legacy_exists: i64 = reg
            .client
            .exists(CallRouteRegistry::legacy_key(call))
            .await
            .unwrap();
        assert_eq!(legacy_exists, 0, "R2 deletes the whole compatibility hash");

        reg.unregister_participant(call, current).await.unwrap();
    }
}
