//! Cross-node group-call routing registry (ROADMAP3 方向二 — 分布式通话基底).
//!
//! Records, in Redis with a TTL, which node currently hosts each group-call
//! participant's media leg: one hash per call, `participant -> that node's
//! public base URL`. Every node that holds at least one participant of the
//! call refreshes the entry while the participant is connected; if a node
//! crashes without unregistering, the whole-call entry ages out.
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
use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::time::Duration;

/// How long a call's routing hash survives without a refresh. Every
/// register/heartbeat re-arms it; if all nodes hosting the call go silent
/// (crash without unregistering), the entry expires and stale bridge targets
/// stop being handed out.
pub const DEFAULT_TTL: Duration = Duration::from_secs(30);

/// Redis-backed registry of `call -> { participant -> hosting node }`.
#[derive(Clone)]
pub struct CallRouteRegistry {
    client: RedisClient,
    ttl: Duration,
}

impl CallRouteRegistry {
    /// Build a registry using [`DEFAULT_TTL`].
    #[must_use]
    pub fn new(client: RedisClient) -> Self {
        Self { client, ttl: DEFAULT_TTL }
    }

    /// Override the entry TTL.
    #[must_use]
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    fn key(call: CallId) -> String {
        format!("callroute:{call}")
    }

    /// Record (or refresh) that `participant`'s media leg for `call` is hosted
    /// on `node_url`. Idempotent; re-arms the whole-call TTL, so it doubles as
    /// the per-participant heartbeat.
    pub async fn register_participant(
        &self,
        call: CallId,
        participant: ParticipantId,
        node_url: &str,
    ) -> anyhow::Result<()> {
        let key = Self::key(call);
        self.client
            .hset::<(), _, _>(&key, (participant.to_string(), normalize_node(node_url)))
            .await?;
        self.client
            .expire::<(), _>(&key, i64::try_from(self.ttl.as_secs()).unwrap_or(i64::MAX))
            .await?;
        Ok(())
    }

    /// Refresh `participant`'s mapping and the call entry's TTL (alias of
    /// [`Self::register_participant`]).
    pub async fn heartbeat(
        &self,
        call: CallId,
        participant: ParticipantId,
        node_url: &str,
    ) -> anyhow::Result<()> {
        self.register_participant(call, participant, node_url).await
    }

    /// Drop `participant`'s mapping for `call`; deletes the call entry when it
    /// becomes empty. (Redis removes an emptied hash on its own — the explicit
    /// `DEL` is defensive and makes the cleanup observable.)
    pub async fn unregister_participant(
        &self,
        call: CallId,
        participant: ParticipantId,
    ) -> anyhow::Result<()> {
        let key = Self::key(call);
        let _: i64 = self.client.hdel(&key, participant.to_string()).await?;
        self.cleanup_if_empty(&key).await
    }

    /// Drop **every** participant mapped to `node_url` for `call` — the
    /// "last local participant left, deregister this node" path. Also clears
    /// any stale local entries left by participants that vanished without an
    /// explicit leave. Deletes the call entry when it becomes empty.
    pub async fn unregister_node(&self, call: CallId, node_url: &str) -> anyhow::Result<()> {
        let key = Self::key(call);
        let entries: HashMap<String, String> = self.client.hgetall(&key).await?;
        let target = normalize_node(node_url);
        let fields: Vec<String> = entries
            .iter()
            .filter(|(_, node)| normalize_node(node) == target)
            .map(|(field, _)| field.clone())
            .collect();
        if !fields.is_empty() {
            let _: i64 = self.client.hdel(&key, fields).await?;
        }
        self.cleanup_if_empty(&key).await
    }

    /// The nodes currently hosting participants of `call`, with each node's
    /// participant count, sorted by node URL (deterministic). Empty when the
    /// call is unregistered or its entry expired.
    pub async fn nodes_for_call(&self, call: CallId) -> anyhow::Result<Vec<(String, u32)>> {
        let entries: HashMap<String, String> = self.client.hgetall(Self::key(call)).await?;
        Ok(aggregate_nodes(&entries))
    }

    /// The participants of `call` whose media leg is hosted on `node_url`,
    /// sorted by id (deterministic).
    pub async fn participants_on(
        &self,
        call: CallId,
        node_url: &str,
    ) -> anyhow::Result<Vec<ParticipantId>> {
        let entries: HashMap<String, String> = self.client.hgetall(Self::key(call)).await?;
        Ok(participants_on_node(&entries, node_url))
    }

    async fn cleanup_if_empty(&self, key: &str) -> anyhow::Result<()> {
        let remaining: i64 = self.client.hlen(key).await?;
        if remaining == 0 {
            let _: () = self.client.del(key).await?;
        }
        Ok(())
    }
}

/// Normalize a node base URL so `http://a` and `http://a/` are the same node
/// (same rule as `stream_route`'s comparison).
fn normalize_node(url: &str) -> &str {
    url.trim_end_matches('/')
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
    counts.into_iter().map(|(node, n)| (node.to_owned(), n)).collect()
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
    fn key_is_namespaced_and_deterministic() {
        let c = CallId(ulid::Ulid::nil());
        assert_eq!(CallRouteRegistry::key(c), format!("callroute:{c}"));
        assert_eq!(CallRouteRegistry::key(c), CallRouteRegistry::key(c));
        // Distinct namespace from the stream registry's `streamroute:` keys.
        assert!(CallRouteRegistry::key(c).starts_with("callroute:"));
    }

    #[test]
    fn aggregate_nodes_counts_and_sorts_deterministically() {
        let (a, b, c) = (ParticipantId::new(), ParticipantId::new(), ParticipantId::new());
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
        assert!(aggregate_nodes(&entries).is_empty(), "empty node value dropped");
        assert!(aggregate_nodes(&HashMap::new()).is_empty(), "empty hash → no nodes");
    }

    #[test]
    fn participants_on_node_filters_sorts_and_drops_garbage() {
        let (a, b, c) = (ParticipantId::new(), ParticipantId::new(), ParticipantId::new());
        let mut entries = entry_map(&[
            (a, "http://node-a.example"),
            (b, "http://node-b.example"),
            (c, "http://node-a.example/"),
        ]);
        entries.insert("not-a-ulid".to_owned(), "http://node-a.example".to_owned());

        let mut expected = vec![a, c];
        expected.sort();
        // Slash-insensitive lookup; malformed member silently dropped.
        assert_eq!(participants_on_node(&entries, "http://node-a.example/"), expected);
        assert_eq!(participants_on_node(&entries, "http://node-b.example"), vec![b]);
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
    use fred::prelude::{ClientLike, RedisClient};

    async fn client() -> RedisClient {
        let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
        let c = RedisClient::new(fred::types::RedisConfig::from_url(&url).unwrap(), None, None, None);
        c.connect();
        c.wait_for_connect().await.unwrap();
        c
    }

    #[tokio::test]
    #[ignore = "requires live Redis"]
    async fn callroute_register_census_unregister_roundtrip() {
        let reg = CallRouteRegistry::new(client().await);
        let call = CallId::new();
        let (a, b, c) = (ParticipantId::new(), ParticipantId::new(), ParticipantId::new());

        assert!(reg.nodes_for_call(call).await.unwrap().is_empty(), "unregistered call is nowhere");

        reg.register_participant(call, a, "http://node-a:3030").await.unwrap();
        reg.register_participant(call, b, "http://node-a:3030/").await.unwrap();
        reg.register_participant(call, c, "http://node-b:3030").await.unwrap();

        assert_eq!(
            reg.nodes_for_call(call).await.unwrap(),
            vec![("http://node-a:3030".to_owned(), 2), ("http://node-b:3030".to_owned(), 1)],
        );
        let mut on_a = vec![a, b];
        on_a.sort();
        assert_eq!(reg.participants_on(call, "http://node-a:3030").await.unwrap(), on_a);

        // Heartbeat is idempotent — census unchanged.
        reg.heartbeat(call, a, "http://node-a:3030").await.unwrap();
        assert_eq!(reg.nodes_for_call(call).await.unwrap().len(), 2);

        reg.unregister_participant(call, a).await.unwrap();
        assert_eq!(
            reg.nodes_for_call(call).await.unwrap(),
            vec![("http://node-a:3030".to_owned(), 1), ("http://node-b:3030".to_owned(), 1)],
        );

        // Node-level deregistration sweeps the remaining node-a entry.
        reg.unregister_node(call, "http://node-a:3030").await.unwrap();
        assert_eq!(
            reg.nodes_for_call(call).await.unwrap(),
            vec![("http://node-b:3030".to_owned(), 1)],
        );

        reg.unregister_participant(call, c).await.unwrap();
        assert!(reg.nodes_for_call(call).await.unwrap().is_empty(), "empty call cleaned up");
    }
}
