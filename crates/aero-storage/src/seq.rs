//! Cluster-wide per-subject event sequencer (ROADMAP 第三版 方向一).
//!
//! One Redis counter per NATS subject (`aero:seq:{subject}`), advanced with
//! `INCR` — atomic in Redis, so every node hands out values from the same
//! per-room/per-stream sequence. This is what makes the publish-time `seq`
//! stamp (see `aero_bus::seq`) **cluster-correct**: two instances publishing
//! into the same room never mint the same seq for different events.
//!
//! Contract (mirrors the stamp's): per-subject monotonic; gaps are legal (a
//! crashed publisher may consume a value it never publishes); only dedup and
//! relative order matter, so consumers must never wait for a missing seq.
//! Keys are deliberately left without TTL — a few bytes per active room/stream,
//! and expiring one would restart its sequence below already-delivered values.
//!
//! Mirrors [`crate::presence::PresenceStore`]'s Redis access style.

use fred::prelude::{KeysInterface, RedisClient};

/// Redis key for a subject's sequence counter.
#[must_use]
fn seq_key(subject: &str) -> String {
    format!("aero:seq:{subject}")
}

/// Redis `INCR`-backed sequence source, one counter per subject.
#[derive(Clone)]
pub struct SeqStore {
    client: RedisClient,
}

impl SeqStore {
    /// Wrap an existing Redis client (shared with the other stores).
    #[must_use]
    pub fn new(client: RedisClient) -> Self {
        Self { client }
    }

    /// Next sequence number for `subject` (atomic, cluster-wide). Starts at 1
    /// for a fresh subject.
    ///
    /// # Errors
    /// Returns the underlying Redis error; callers degrade to publishing an
    /// unstamped event rather than blocking delivery.
    pub async fn next(&self, subject: &str) -> anyhow::Result<u64> {
        let n: i64 = self.client.incr(seq_key(subject)).await?;
        // INCR can only go negative if the key was externally set below -1;
        // saturate rather than wrap so a corrupted counter can't masquerade as
        // a huge (order-breaking) seq.
        Ok(u64::try_from(n).unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_namespaced_and_deterministic() {
        assert_eq!(seq_key("im.room.abc"), "aero:seq:im.room.abc");
        assert_eq!(seq_key("im.room.abc"), seq_key("im.room.abc"));
        // Distinct subjects never share a counter key.
        assert_ne!(seq_key("im.room.a"), seq_key("live.stream.a"));
    }
}

/// Redis-gated integration test (run with a live Redis):
///
/// ```text
/// REDIS_URL=redis://localhost:6379 \
///   cargo test -p aero-storage --lib -- --ignored seqstore_
/// ```
#[cfg(test)]
mod redis_tests {
    use super::*;
    use fred::prelude::{ClientLike, RedisClient};

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

    #[tokio::test]
    #[ignore = "requires live Redis"]
    async fn seqstore_is_monotonic_per_subject_and_independent_across_subjects() {
        let store = SeqStore::new(client().await);
        // Unique subjects per run so reruns don't see stale counters.
        let a = format!("im.room.{}", ulid::Ulid::new());
        let b = format!("live.stream.{}", ulid::Ulid::new());

        let a1 = store.next(&a).await.unwrap();
        let a2 = store.next(&a).await.unwrap();
        let a3 = store.next(&a).await.unwrap();
        assert_eq!(
            (a1, a2, a3),
            (1, 2, 3),
            "fresh subject counts from 1, strictly increasing"
        );

        // A different subject has its own counter, unaffected by `a`'s.
        assert_eq!(store.next(&b).await.unwrap(), 1);
        assert_eq!(store.next(&a).await.unwrap(), 4);
    }
}
