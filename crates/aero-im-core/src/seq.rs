//! Per-subject event sequence sources (ROADMAP 第三版 方向一 — 实时投递完整性).
//!
//! [`ImService`](crate::ImService) (and `aero-server`'s `LiveService`) stamp a
//! `"seq"` key into every `RoomEvent` / `StreamEvent` **at publish time** (see
//! `aero_bus::stamped_event_bytes`), so NATS at-least-once redeliveries carry
//! the *same* seq and clients can dedup/order them. This module supplies the
//! numbers:
//!
//! - [`LocalSeqProvider`] — process-local `DashMap<String, AtomicU64>`. The
//!   zero-config default; correct for a single instance.
//! - `impl SeqProvider for aero_storage::SeqStore` — Redis `INCR` per subject
//!   (`aero:seq:{subject}`), so multiple instances publishing into the same
//!   room/stream draw from one cluster-wide sequence. Wired in
//!   `bin/aero-server.rs` via `ImService::with_seq` / `LiveService::with_seq`.
//!
//! ## Contract
//! - **Per-subject monotonic**: each `im.room.{id}` / `live.stream.{id}` subject
//!   has its own strictly-increasing counter.
//! - **Gaps are legal**: a publisher can mint a value and crash before
//!   publishing, or the provider can restart (local impl). Consumers use seq
//!   only for dedup and *relative* order — they must never wait for a missing
//!   value.
//! - **Failure degrades, never blocks**: when the source is unavailable the
//!   provider returns `None` and the event is published unstamped (clients pass
//!   unstamped events through unchanged) rather than delaying delivery.

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use dashmap::DashMap;

/// Source of per-subject monotonic sequence numbers, consulted once per
/// publish. Object-safe so [`ImService`](crate::ImService) can hold any impl
/// behind `Arc<dyn SeqProvider>`.
#[async_trait]
pub trait SeqProvider: Send + Sync + 'static {
    /// Next sequence value for `subject`, or `None` when the source is
    /// unavailable (the caller then publishes an **unstamped** event — delivery
    /// is never blocked on sequencing).
    async fn next_seq(&self, subject: &str) -> Option<u64>;
}

/// Process-local default: one `AtomicU64` per subject in a [`DashMap`].
///
/// Monotonic within this process only — after a restart (or on a second
/// instance) counting restarts at 1. That still satisfies the contract for a
/// single-instance deployment; clustered deployments wire the Redis-backed
/// [`aero_storage::SeqStore`] instead so all nodes share one sequence.
#[derive(Default)]
pub struct LocalSeqProvider {
    counters: DashMap<String, AtomicU64>,
}

impl LocalSeqProvider {
    /// Fresh provider with no counters (every subject starts at 1).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SeqProvider for LocalSeqProvider {
    async fn next_seq(&self, subject: &str) -> Option<u64> {
        let counter = self.counters.entry(subject.to_owned()).or_default();
        // `fetch_add` returns the previous value, so the sequence is 1-based —
        // matching Redis `INCR` on a fresh key.
        Some(counter.fetch_add(1, Ordering::Relaxed) + 1)
    }
}

/// Cluster-correct impl: Redis `INCR` on `aero:seq:{subject}` (see
/// [`aero_storage::SeqStore`]). A Redis error degrades to `None` (publish
/// unstamped) with a warning — sequencing must never block delivery.
#[async_trait]
impl SeqProvider for aero_storage::SeqStore {
    async fn next_seq(&self, subject: &str) -> Option<u64> {
        match self.next(subject).await {
            Ok(n) => Some(n),
            Err(err) => {
                tracing::warn!(?err, %subject, "redis seq INCR failed; publishing unstamped");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;

    #[tokio::test]
    async fn local_provider_is_monotonic_per_subject() {
        let p = LocalSeqProvider::new();
        let mut prev = 0;
        for expected in 1..=100u64 {
            let got = p.next_seq("im.room.a").await.expect("local provider never fails");
            assert_eq!(got, expected, "1-based, strictly increasing, no gaps locally");
            assert!(got > prev);
            prev = got;
        }
    }

    #[tokio::test]
    async fn local_provider_counts_each_subject_independently() {
        let p = LocalSeqProvider::new();
        assert_eq!(p.next_seq("im.room.a").await, Some(1));
        assert_eq!(p.next_seq("im.room.a").await, Some(2));
        // A different subject starts its own sequence at 1 ...
        assert_eq!(p.next_seq("live.stream.x").await, Some(1));
        // ... and advancing it does not disturb the first one.
        assert_eq!(p.next_seq("im.room.a").await, Some(3));
    }

    #[tokio::test]
    async fn local_provider_is_monotonic_under_concurrency() {
        // Many tasks pull from one subject; the set of minted values must be
        // exactly 1..=N (each value handed out once — the dedup-key property).
        let p = Arc::new(LocalSeqProvider::new());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let p = p.clone();
            handles.push(tokio::spawn(async move {
                let mut got = Vec::new();
                for _ in 0..50 {
                    got.push(p.next_seq("im.room.contended").await.unwrap());
                }
                got
            }));
        }
        let mut all = Vec::new();
        for h in handles {
            all.extend(h.await.unwrap());
        }
        all.sort_unstable();
        let expected: Vec<u64> = (1..=400).collect();
        assert_eq!(all, expected, "every seq minted exactly once, 1..=N");
    }

    #[tokio::test]
    async fn trait_object_usage_works() {
        // The exact shape `ImService` holds: a shared trait object.
        let p: Arc<dyn SeqProvider> = Arc::new(LocalSeqProvider::new());
        assert_eq!(p.next_seq("s").await, Some(1));
        assert_eq!(p.next_seq("s").await, Some(2));
    }
}
