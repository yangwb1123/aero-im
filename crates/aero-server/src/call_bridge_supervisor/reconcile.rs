//! Cross-node pull-set reconciliation.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use aero_common::CallId;
use aero_live_webrtc::{CallBridge, CallUpstream};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::{normalize_node, BoxedUpstream, BridgeTask, CallBridgeSupervisor, SharedForwarder};

/// Exact-match cleanup for a PULL reservation held across `factory.connect`.
///
/// Async cancellation drops locals instead of running an error branch. The
/// reservation id also makes that drop ABA-safe: an old future cannot remove a
/// newer reservation for the same `(call, peer)` key.
struct PullReservationGuard {
    connecting: Arc<Mutex<HashMap<(CallId, String), u64>>>,
    key: (CallId, String),
    reservation_id: u64,
}

impl Drop for PullReservationGuard {
    fn drop(&mut self) {
        let mut connecting = self.connecting.lock();
        if connecting.get(&self.key) == Some(&self.reservation_id) {
            connecting.remove(&self.key);
        }
    }
}

impl CallBridgeSupervisor {
    /// Ensure one running bridge per listed peer node for `call`.
    ///
    /// The in-flight reservation spans the async transport handshake, making
    /// concurrent bus/timer reconciliation idempotent. A refused or naturally
    /// ended transport is not registered and can be retried on the next tick.
    pub async fn ensure_bridges(&self, call: CallId, peer_urls: &[String]) -> usize {
        let mut spawned = 0;
        for raw in peer_urls {
            let url = normalize_node(raw);
            if url.is_empty() {
                continue;
            }
            let key = (call, url.clone());
            let reservation_id = {
                // Global PULL lock order is connecting -> active. Lifecycle
                // cleanup uses the same order whenever both maps are touched.
                let mut connecting = self.connecting.lock();
                let active = self.active.lock();
                if active.contains_key(&key) || connecting.contains_key(&key) {
                    continue;
                }
                let reservation_id = self.next_id.fetch_add(1, Ordering::Relaxed);
                connecting.insert(key.clone(), reservation_id);
                reservation_id
            };
            let _reservation_guard = PullReservationGuard {
                connecting: self.connecting.clone(),
                key: key.clone(),
                reservation_id,
            };
            let Some(upstream) = self.factory.connect(call, &url).await else {
                continue;
            };
            if self.spawn_reserved_bridge(call, &url, reservation_id, upstream) {
                spawned += 1;
            }
        }
        spawned
    }

    /// Reconcile the running pull set to the latest desired peer census.
    ///
    /// Returns `(spawned, cancelled)`.
    pub async fn reconcile_bridges(&self, call: CallId, peer_urls: &[String]) -> (usize, usize) {
        let desired: HashSet<String> = peer_urls
            .iter()
            .map(|url| normalize_node(url))
            .filter(|url| !url.is_empty())
            .collect();
        let stale: Vec<String> = self
            .active
            .lock()
            .keys()
            .filter(|(candidate, peer)| *candidate == call && !desired.contains(peer))
            .map(|(_, peer)| peer.clone())
            .collect();
        let cancelled = stale
            .iter()
            .filter(|peer| self.cancel_bridge(call, peer))
            .count();
        self.connecting
            .lock()
            .retain(|(candidate, peer), _| *candidate != call || desired.contains(peer));

        let mut desired = desired.into_iter().collect::<Vec<_>>();
        desired.sort();
        let spawned = self.ensure_bridges(call, &desired).await;
        (spawned, cancelled)
    }

    /// Consume the in-flight reservation and register the task atomically.
    ///
    /// A start latch prevents an immediately-ended upstream from running its
    /// self-removal before the registry entry exists. Holding `connecting`
    /// through the `active` insert also closes the cancel-after-connect race:
    /// lifecycle cleanup either removes the reservation first (and this returns
    /// false) or observes and cancels the newly registered task.
    fn spawn_reserved_bridge(
        &self,
        call: CallId,
        peer_url: &str,
        reservation_id: u64,
        upstream: Box<dyn CallUpstream>,
    ) -> bool {
        let key = (call, peer_url.to_owned());
        let mut connecting = self.connecting.lock();
        if connecting.get(&key) != Some(&reservation_id) {
            return false;
        }
        // A connect can outlive the browser leg that caused reconciliation.
        // Keep the reservation lock through this ownership fence and the
        // active-map commit so final cleanup either wins first or observes and
        // removes the fully registered task.
        if !self.router.has_local_participants(call) {
            connecting.remove(&key);
            return false;
        }
        let mut active_guard = self.active.lock();
        if active_guard.contains_key(&key) {
            connecting.remove(&key);
            return false;
        }
        connecting.remove(&key);

        let id = reservation_id;
        let cancel = CancellationToken::new();
        let bridge = CallBridge::new(
            BoxedUpstream(upstream),
            self.router.clone(),
            SharedForwarder(self.forwarder.clone()),
        );
        let task_cancel = cancel.clone();
        let key_url = peer_url.to_owned();
        let active = self.active.clone();
        let (registered_tx, registered_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            if registered_rx.await.is_err() {
                return;
            }
            tokio::select! {
                () = bridge.run() => {
                    debug!(%call, peer = %key_url, "call-bridge: upstream ended");
                }
                () = task_cancel.cancelled() => {
                    debug!(%call, peer = %key_url, "call-bridge: cancelled");
                }
            }
            // A late old task must not remove a successor for the same key.
            let mut guard = active.lock();
            if guard
                .get(&(call, key_url.clone()))
                .is_some_and(|task| task.id == id)
            {
                guard.remove(&(call, key_url));
            }
        });
        active_guard.insert(key, BridgeTask { id, cancel, handle });
        drop(active_guard);
        drop(connecting);
        let _ = registered_tx.send(());
        true
    }
}
