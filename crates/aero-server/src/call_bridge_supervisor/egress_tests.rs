use std::sync::Arc;

use aero_common::{CallId, ParticipantId};
use aero_live_webrtc::{
    CallEgress, CallUpstream, MediaForwarder, NullForwarder, SfuRouter, BOUND_BRIDGE_VERSION,
};
use async_trait::async_trait;
use uuid::Uuid;

use super::{
    BridgeSubscriberRegistry, CallBridgeSupervisor, EgressReservation, EgressReservationGuard,
    UdpRtpEgress, UpstreamFactory,
};

const AUDIO_OFFER: &str = "v=0\r\n\
o=- 4611731400430051336 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0\r\n\
a=msid-semantic: WMS\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:abcd\r\n\
a=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
a=fingerprint:sha-256 11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=sendrecv\r\n\
a=rtcp-mux\r\n\
a=rtpmap:111 opus/48000/2\r\n";

struct RefusingFactory;

#[async_trait]
impl UpstreamFactory for RefusingFactory {
    async fn connect(&self, _call: CallId, _peer: &str) -> Option<Box<dyn CallUpstream>> {
        None
    }
}

fn supervisor() -> CallBridgeSupervisor {
    CallBridgeSupervisor::new(
        SfuRouter::new(),
        Arc::new(NullForwarder) as Arc<dyn MediaForwarder>,
        Arc::new(RefusingFactory),
        BridgeSubscriberRegistry::default(),
    )
}

#[tokio::test]
async fn immediately_ended_egress_unregisters_and_can_retry() {
    let sup = supervisor();
    let call = CallId::new();
    let ended = CallEgress::new(call);
    let ended_tap = ended.tap();
    drop(ended);

    assert!(sup.ensure_egress(call, ended_tap).await);
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while sup.has_egress(call) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("an immediately-ended relay unregisters itself");

    let replacement = CallEgress::new(call);
    assert!(sup.ensure_egress(call, replacement.tap()).await);
    sup.cancel_call(call);
}

#[tokio::test]
async fn old_bind_completion_cannot_consume_a_new_reservation() {
    let sup = supervisor();
    let call = CallId::new();
    let old = super::EgressReservation {
        source_generation: 1,
        id: 41,
    };
    let replacement = super::EgressReservation {
        source_generation: 2,
        id: 42,
    };
    assert_eq!(sup.egress_connecting.lock().insert(call, old), None);
    let old_sender = UdpRtpEgress::bind(call, sup.subscribers.clone())
        .await
        .expect("bind old sender");
    let old_source = CallEgress::new(call);

    assert!(sup.cancel_egress(call));
    assert_eq!(sup.egress_connecting.lock().insert(call, replacement), None);
    assert!(!sup.spawn_reserved_egress(call, old, old_source.tap(), old_sender, false));
    assert_eq!(
        sup.egress_connecting.lock().get(&call).copied(),
        Some(replacement),
        "late old generation preserves the replacement reservation"
    );

    let new_sender = UdpRtpEgress::bind(call, sup.subscribers.clone())
        .await
        .expect("bind replacement sender");
    let new_source = CallEgress::new(call);
    assert!(sup.spawn_reserved_egress(call, replacement, new_source.tap(), new_sender, false));
    sup.cancel_call(call);
}

#[tokio::test]
async fn cancelling_a_bind_future_releases_only_its_exact_reservation() {
    let sup = supervisor();
    let call = CallId::new();
    let reservation = EgressReservation {
        source_generation: 7,
        id: 71,
    };
    sup.egress_connecting.lock().insert(call, reservation);
    let connecting = sup.egress_connecting.clone();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let pending = tokio::spawn(async move {
        let _guard = EgressReservationGuard {
            connecting,
            call,
            reservation,
        };
        let _ = ready_tx.send(());
        std::future::pending::<()>().await;
    });
    ready_rx.await.expect("reservation guard is installed");
    pending.abort();
    let error = pending.await.expect_err("pending bind is cancelled");
    assert!(error.is_cancelled());
    assert!(
        !sup.egress_connecting.lock().contains_key(&call),
        "future cancellation makes the source epoch immediately retryable"
    );

    let old = EgressReservation {
        source_generation: 8,
        id: 81,
    };
    let replacement = EgressReservation {
        source_generation: 9,
        id: 91,
    };
    sup.egress_connecting.lock().insert(call, old);
    let guard = EgressReservationGuard {
        connecting: sup.egress_connecting.clone(),
        call,
        reservation: old,
    };
    sup.egress_connecting.lock().insert(call, replacement);
    drop(guard);
    assert_eq!(
        sup.egress_connecting.lock().get(&call).copied(),
        Some(replacement),
        "an old guard cannot erase a newer reservation"
    );
}

#[tokio::test]
async fn stale_final_epoch_cleanup_preserves_reconnected_media_and_subscribers() {
    let sup = supervisor();
    let call = CallId::new();
    let first = ParticipantId::new();
    let second = ParticipantId::new();

    sup.accept_sfu_offer(call, first, 1, AUDIO_OFFER)
        .await
        .expect("first source epoch starts");
    let first_generation = sup
        .media
        .current_egress_tap(call)
        .expect("first source exists")
        .0;
    let removed = sup.media.remove_session_with_topology(call, first);
    assert_eq!(removed.ended_egress_generation, Some(first_generation));

    sup.accept_sfu_offer(call, second, 2, AUDIO_OFFER)
        .await
        .expect("replacement source epoch starts");
    let second_generation = sup
        .media
        .current_egress_tap(call)
        .expect("replacement source exists")
        .0;
    assert_ne!(second_generation, first_generation);
    let subscriber = "127.0.0.1:5001".parse().unwrap();
    assert_eq!(
        sup.subscribe_egress_generation(
            call,
            subscriber,
            Uuid::new_v4(),
            std::time::Duration::from_secs(60),
            BOUND_BRIDGE_VERSION,
        ),
        Some(true)
    );

    assert_eq!(sup.cancel_ended_media_epoch(call, first_generation), 0);
    assert!(sup.has_media_session(call, second));
    assert_eq!(
        sup.egress
            .lock()
            .get(&call)
            .map(|task| task.source_generation),
        Some(second_generation)
    );
    assert_eq!(sup.subscribers.subscribers(call).len(), 1);
    sup.cancel_call(call);
}

#[tokio::test]
async fn latest_final_epoch_cleanup_clears_orphans_and_consumes_its_tombstone() {
    let sup = supervisor();
    let call = CallId::new();
    let participant = ParticipantId::new();
    sup.accept_sfu_offer(call, participant, 1, AUDIO_OFFER)
        .await
        .expect("source epoch starts");
    let generation = sup.media.current_egress_tap(call).expect("source exists").0;
    let removed = sup.media.remove_session_with_topology(call, participant);
    assert_eq!(removed.ended_egress_generation, Some(generation));

    sup.egress_connecting.lock().insert(
        call,
        EgressReservation {
            source_generation: generation.saturating_sub(1),
            id: 1234,
        },
    );
    assert_eq!(sup.cancel_ended_media_epoch(call, generation), 0);
    assert!(!sup.egress_connecting.lock().contains_key(&call));
    assert!(!sup.has_egress(call));
    assert!(
        sup.media
            .with_ended_egress_epoch(call, generation, || ())
            .is_none(),
        "authoritative cleanup consumes the final epoch tombstone"
    );
}

#[tokio::test]
async fn concurrent_cancel_and_legacy_subscribe_cannot_leave_a_ghost() {
    let sup = supervisor();
    let call = CallId::new();
    let source = CallEgress::new(call);
    assert!(sup.ensure_egress(call, source.tap()).await);
    let addr = "127.0.0.1:5000".parse().unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));

    std::thread::scope(|scope| {
        let subscriber = sup.clone();
        let subscribe_barrier = barrier.clone();
        scope.spawn(move || {
            subscribe_barrier.wait();
            subscriber.subscribe_egress_legacy(call, addr)
        });
        let canceller = sup.clone();
        let cancel_barrier = barrier.clone();
        scope.spawn(move || {
            cancel_barrier.wait();
            canceller.cancel_egress(call)
        });
        barrier.wait();
    });

    assert!(!sup.has_egress(call));
    assert!(
        sup.subscribers.subscribers(call).is_empty(),
        "subscribe-before-cancel is cleared; subscribe-after-cancel is rejected"
    );
}
