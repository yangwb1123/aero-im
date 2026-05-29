//! Cross-node stream routing registry (ROADMAP 方向二 — sticky routing).
//!
//! Records, in Redis with a TTL, which node currently ingests each live stream:
//! `stream_id -> that node's public base URL`. The ingesting node refreshes the
//! entry while it holds the publisher; a crashed node's entry ages out.
//!
//! This enables **sticky routing**: a WHEP (or HLS) pull that lands on a node
//! which is *not* ingesting the stream looks the stream up here and redirects the
//! client to the owning node — so a stream ingested on node A is reachable from
//! node B without any inter-node media relay. (SFU cascade/relay, which load-
//! splits one stream across nodes, is a separate heavier mechanism.)
//!
//! Mirrors [`crate::presence::PresenceStore`]'s Redis access style.

use fred::prelude::{Expiration, KeysInterface, RedisClient};
use std::time::Duration;
use ulid::Ulid;

/// How long a stream→node mapping survives without a refresh. The owning node
/// re-publishes (heartbeats) well within this so the entry stays warm; if the
/// node dies, the mapping expires and stale redirects stop.
pub const DEFAULT_TTL: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct StreamRouteRegistry {
    client: RedisClient,
    ttl: Duration,
}

impl StreamRouteRegistry {
    #[must_use]
    pub fn new(client: RedisClient) -> Self {
        Self { client, ttl: DEFAULT_TTL }
    }

    #[must_use]
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    fn key(stream: Ulid) -> String {
        format!("streamroute:{stream}")
    }

    /// Record (or refresh) that `node_url` ingests `stream`. Idempotent; also
    /// serves as the heartbeat that keeps the mapping from expiring.
    pub async fn publish(&self, stream: Ulid, node_url: &str) -> anyhow::Result<()> {
        let key = Self::key(stream);
        self.client
            .set::<(), _, _>(
                &key,
                node_url,
                Some(Expiration::EX(
                    i64::try_from(self.ttl.as_secs()).unwrap_or(i64::MAX),
                )),
                None,
                false,
            )
            .await?;
        Ok(())
    }

    /// Refresh an existing mapping's TTL (alias of [`Self::publish`]).
    pub async fn heartbeat(&self, stream: Ulid, node_url: &str) -> anyhow::Result<()> {
        self.publish(stream, node_url).await
    }

    /// Drop the mapping when the publisher leaves this node.
    pub async fn unpublish(&self, stream: Ulid) -> anyhow::Result<()> {
        let _: () = self.client.del(Self::key(stream)).await?;
        Ok(())
    }

    /// The public base URL of the node currently ingesting `stream`, or `None`
    /// if no node holds it (never published, or the mapping expired).
    pub async fn locate(&self, stream: Ulid) -> anyhow::Result<Option<String>> {
        let v: Option<String> = self.client.get(Self::key(stream)).await?;
        Ok(v)
    }
}

/// Decide where a pull for `stream` should be served, given the node we are
/// (`local_base`) and where the registry says the stream lives (`located`).
///
/// Pure so it unit-tests without Redis:
/// * `Some(url)` — redirect the client to that (different) node's base URL;
/// * `None` — serve locally / 404 (the stream is here, unregistered, or its
///   registered home is this very node — a stale entry we shouldn't loop to).
#[must_use]
pub fn redirect_base(local_base: &str, located: Option<&str>) -> Option<String> {
    match located {
        Some(home) if !home.is_empty() && !same_node(home, local_base) => Some(home.to_owned()),
        _ => None,
    }
}

/// Compare two node base URLs ignoring a trailing slash so `http://a` and
/// `http://a/` are the same node.
fn same_node(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_namespaced_and_deterministic() {
        let s = Ulid::nil();
        assert_eq!(StreamRouteRegistry::key(s), format!("streamroute:{s}"));
        assert_eq!(StreamRouteRegistry::key(s), StreamRouteRegistry::key(s));
    }

    #[test]
    fn redirect_only_to_a_different_node() {
        // Stream lives on another node → redirect there.
        assert_eq!(
            redirect_base("http://a.example", Some("http://b.example")),
            Some("http://b.example".to_owned())
        );
        // Stream's registered home IS this node (stale/local) → no redirect.
        assert_eq!(redirect_base("http://a.example", Some("http://a.example")), None);
        // Trailing-slash difference is still the same node → no redirect loop.
        assert_eq!(redirect_base("http://a.example", Some("http://a.example/")), None);
        // Not registered anywhere → serve locally / 404.
        assert_eq!(redirect_base("http://a.example", None), None);
        // Empty registry value is treated as absent.
        assert_eq!(redirect_base("http://a.example", Some("")), None);
    }
}

/// PG/Redis-gated integration test (run with a live Redis):
///
/// ```text
/// REDIS_URL=redis://localhost:6379 \
///   cargo test -p aero-storage --lib -- --ignored streamroute_
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
    async fn streamroute_publish_locate_unpublish_roundtrip() {
        let reg = StreamRouteRegistry::new(client().await);
        let s = Ulid::new();
        assert_eq!(reg.locate(s).await.unwrap(), None, "unpublished stream is nowhere");
        reg.publish(s, "http://node-a:3030").await.unwrap();
        assert_eq!(reg.locate(s).await.unwrap().as_deref(), Some("http://node-a:3030"));
        reg.unpublish(s).await.unwrap();
        assert_eq!(reg.locate(s).await.unwrap(), None, "unpublished again");
    }
}
