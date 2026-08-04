//! Deterministic cancellation/race tests for PULL bridge reconciliation.

use std::sync::Arc;

use aero_common::{CallId, ParticipantId};
use aero_live_webrtc::{CallEgress, CallUpstream, LoopbackUpstream, PeerRole};
use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::oneshot;

use super::{
    normalize_node, tests::supervisor as make_supervisor, CallBridgeSupervisor, UpstreamFactory,
};

struct GatedFactory {
    entered: Mutex<Option<oneshot::Sender<()>>>,
    release: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
    egresses: Mutex<Vec<Arc<CallEgress>>>,
}

impl GatedFactory {
    fn new() -> (Arc<Self>, oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        (
            Arc::new(Self {
                entered: Mutex::new(Some(entered_tx)),
                release: tokio::sync::Mutex::new(Some(release_rx)),
                egresses: Mutex::new(Vec::new()),
            }),
            entered_rx,
            release_tx,
        )
    }
}

#[async_trait]
impl UpstreamFactory for GatedFactory {
    async fn connect(&self, call: CallId, peer_url: &str) -> Option<Box<dyn CallUpstream>> {
        if let Some(entered) = self.entered.lock().take() {
            let _ = entered.send(());
        }
        let release = self.release.lock().await.take()?;
        release.await.ok()?;

        let egress = Arc::new(CallEgress::new(call));
        let upstream = LoopbackUpstream::new(peer_url, egress.tap());
        self.egresses.lock().push(egress);
        Some(Box::new(upstream))
    }
}

async fn begin_gated_connect(
    supervisor: &CallBridgeSupervisor,
    call: CallId,
    peer: &str,
    entered: oneshot::Receiver<()>,
) -> tokio::task::JoinHandle<usize> {
    let supervisor = supervisor.clone();
    let peer = peer.to_owned();
    let task = tokio::spawn(async move { supervisor.ensure_bridges(call, &[peer]).await });
    entered
        .await
        .expect("factory connect reaches its deterministic barrier");
    task
}

#[tokio::test]
async fn aborted_connect_releases_only_its_exact_reservation() {
    let call = CallId::new();
    let peer = "http://peer.example";
    let key = (call, normalize_node(peer));

    let (factory, entered, release) = GatedFactory::new();
    let (supervisor, router) = make_supervisor(factory);
    router.add_peer(call, ParticipantId::new(), PeerRole::Bidirectional);
    let task = begin_gated_connect(&supervisor, call, peer, entered).await;
    assert!(supervisor.connecting.lock().contains_key(&key));

    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(
        !supervisor.connecting.lock().contains_key(&key),
        "dropping factory.connect releases its exact in-flight reservation"
    );
    drop(release);

    let (factory, entered, release) = GatedFactory::new();
    let (supervisor, router) = make_supervisor(factory);
    router.add_peer(call, ParticipantId::new(), PeerRole::Bidirectional);
    let task = begin_gated_connect(&supervisor, call, peer, entered).await;
    let old = *supervisor
        .connecting
        .lock()
        .get(&key)
        .expect("old reservation is installed");
    let replacement = supervisor
        .next_id
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    assert_ne!(old, replacement);
    supervisor
        .connecting
        .lock()
        .insert(key.clone(), replacement);

    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(
        supervisor.connecting.lock().get(&key).copied(),
        Some(replacement),
        "the old future's Drop guard cannot erase an ABA replacement"
    );
    supervisor.connecting.lock().remove(&key);
    drop(release);
}

#[tokio::test]
async fn connect_resuming_after_final_local_removal_cannot_create_a_ghost() {
    let call = CallId::new();
    let local = ParticipantId::new();
    let peer = "http://peer.example";
    let key = (call, normalize_node(peer));
    let (factory, entered, release) = GatedFactory::new();
    let (supervisor, router) = make_supervisor(factory);
    router.add_peer(call, local, PeerRole::Bidirectional);

    let task = begin_gated_connect(&supervisor, call, peer, entered).await;
    assert!(supervisor.connecting.lock().contains_key(&key));
    assert!(
        router.remove_peer(call, local),
        "the gated connect loses the call's final local media peer"
    );
    assert_eq!(
        supervisor.cancel_call(call),
        0,
        "final cleanup removes the in-flight reservation before connect resumes"
    );

    release.send(()).expect("resume the transport handshake");
    assert_eq!(
        task.await.expect("reconcile task completes"),
        0,
        "commit rechecks local ownership instead of reviving the call"
    );
    assert_eq!(supervisor.bridge_count(call), 0);
    assert!(
        !supervisor.connecting.lock().contains_key(&key),
        "the rejected commit leaves no connecting ghost"
    );

    assert_eq!(supervisor.bridge_count(call), 0);
    assert!(supervisor.connecting.lock().is_empty());
}

#[tokio::test]
async fn stale_reconcile_after_cleanup_is_rejected_at_commit() {
    let call = CallId::new();
    let local = ParticipantId::new();
    let peer = "http://peer.example";
    let key = (call, normalize_node(peer));
    let (factory, entered, release) = GatedFactory::new();
    let (supervisor, router) = make_supervisor(factory);
    router.add_peer(call, local, PeerRole::Bidirectional);
    assert!(router.remove_peer(call, local));
    assert_eq!(supervisor.cancel_call(call), 0);

    // Model a delayed topology event that begins reconciliation only after
    // final cleanup. It may complete transport setup, but the commit fence must
    // still reject it because this node owns no local participant anymore.
    let task = begin_gated_connect(&supervisor, call, peer, entered).await;
    assert!(supervisor.connecting.lock().contains_key(&key));
    release.send(()).expect("resume stale reconciliation");
    assert_eq!(task.await.expect("stale reconcile completes"), 0);
    assert_eq!(supervisor.bridge_count(call), 0);
    assert!(!supervisor.connecting.lock().contains_key(&key));
}
