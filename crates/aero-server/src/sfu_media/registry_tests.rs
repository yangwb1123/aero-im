use std::sync::Arc;

use aero_common::{
    CallId, ParticipantId, SfuMediaKind, SfuPublishedTrack, SfuPublisherDescription,
    SfuSubscription,
};
use aero_live_webrtc::{BridgeRtp, KeyframeRequestKind, MediaForwarder, SfuForwarder, SfuRouter};

use super::{SfuMediaError, SfuMediaRegistry};

const SUBSCRIBER_OFFER: &str = "v=0\r\n\
o=- 4611731400430051336 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0 1\r\n\
a=msid-semantic: WMS\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
c=IN IP4 0.0.0.0\r\n\
a=ice-ufrag:abcd\r\n\
a=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
a=fingerprint:sha-256 11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=sendrecv\r\n\
a=rtcp-mux\r\n\
a=rtpmap:111 opus/48000/2\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
c=IN IP4 0.0.0.0\r\n\
a=ice-ufrag:abcd\r\n\
a=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
a=fingerprint:sha-256 11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00\r\n\
a=setup:actpass\r\n\
a=mid:1\r\n\
a=sendrecv\r\n\
a=rtcp-mux\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n";

fn publisher(
    participant: ParticipantId,
    mid: &str,
    media_kind: SfuMediaKind,
) -> SfuPublisherDescription {
    SfuPublisherDescription {
        participant,
        tracks: vec![SfuPublishedTrack {
            mid: mid.into(),
            media_kind,
        }],
    }
}

#[tokio::test]
async fn three_party_topology_is_revisioned_generation_scoped_and_call_isolated() {
    let router = SfuRouter::new();
    let concrete = Arc::new(SfuForwarder::new(router.clone()));
    let forwarder: Arc<dyn MediaForwarder> = concrete.clone();
    let registry = SfuMediaRegistry::new(router, forwarder, "127.0.0.1:0", "127.0.0.1");
    let call = CallId::new();
    let other_call = CallId::new();
    let publisher_a = ParticipantId::new();
    let publisher_b = ParticipantId::new();
    let subscriber = ParticipantId::new();

    let started = registry
        .start_session(call, subscriber, 1, SUBSCRIBER_OFFER)
        .await
        .expect("subscriber media session starts");
    registry
        .request_publisher_keyframe(call, subscriber, "1", None, KeyframeRequestKind::Fir)
        .expect("cross-node FIR is queued on the sole local owner");
    registry
        .request_publisher_remb(call, subscriber, "1", 640_000)
        .expect("cross-node REMB is queued on the sole local owner");
    assert_eq!(started.answer.topology.revision, 1);
    assert!(registry
        .observe_publisher(
            call,
            publisher(publisher_a, "0", SfuMediaKind::Audio),
            1,
            true,
        )
        .is_some());
    assert!(registry
        .observe_publisher(
            call,
            publisher(publisher_b, "0", SfuMediaKind::Video),
            1,
            true,
        )
        .is_some());
    assert!(
        registry
            .observe_publisher(
                call,
                publisher(publisher_b, "0", SfuMediaKind::Video),
                1,
                true,
            )
            .is_none(),
        "at-least-once redelivery must not bump the revision"
    );

    let topology = registry.topology(call);
    assert_eq!(topology.revision, 3);
    assert_eq!(topology.publishers.len(), 3);
    assert_eq!(topology.required_recv_slots(subscriber), 2);
    assert_eq!(
        registry.local_publishers(call),
        vec![(registry.publisher(call, subscriber).unwrap(), 1)],
        "join replay must announce only owner tasks hosted on this node"
    );
    let subscriptions = vec![
        SfuSubscription {
            publisher: publisher_a,
            pub_mid: "0".into(),
            out_mid: "0".into(),
        },
        SfuSubscription {
            publisher: publisher_b,
            pub_mid: "0".into(),
            out_mid: "1".into(),
        },
    ];
    registry
        .replace_subscriptions(
            call,
            subscriber,
            started.answer.session_generation,
            topology.revision,
            subscriptions.clone(),
        )
        .expect("both publisher-scoped routes are valid");
    assert_eq!(
        registry.session_subscriptions(call, subscriber),
        subscriptions
    );

    assert!(matches!(
        registry.replace_subscriptions(
            call,
            subscriber,
            started.answer.session_generation,
            topology.revision - 1,
            Vec::new(),
        ),
        Err(SfuMediaError::StaleTopology {
            expected: 3,
            actual: 2
        })
    ));
    assert!(matches!(
        registry.replace_subscriptions(
            call,
            subscriber,
            started.answer.session_generation.wrapping_add(1),
            topology.revision,
            Vec::new(),
        ),
        Err(SfuMediaError::StaleSession)
    ));

    let packet = BridgeRtp::new(publisher_a, "0", vec![0xAA], true).to_inbound();
    assert_eq!(concrete.on_call_rtp(other_call, publisher_a, &packet), 0);
    assert_eq!(concrete.on_call_rtp(call, publisher_a, &packet), 1);

    let removal = registry.remove_session_with_topology(call, subscriber);
    assert!(removal.removed);
    assert!(removal.ended_egress_generation.is_some());
    assert_eq!(removal.topology.unwrap().revision, 4);
    let duplicate = registry.remove_session_with_topology(call, subscriber);
    assert!(!duplicate.removed, "cleanup is idempotent");
    assert!(duplicate.topology.is_none());
    assert!(duplicate.ended_egress_generation.is_none());
    registry.remove_call(call);
}

#[tokio::test]
async fn unchanged_publisher_reconnect_preserves_other_subscribers_routes() {
    let router = SfuRouter::new();
    let concrete = Arc::new(SfuForwarder::new(router.clone()));
    let forwarder: Arc<dyn MediaForwarder> = concrete.clone();
    let registry = SfuMediaRegistry::new(router, forwarder, "127.0.0.1:0", "127.0.0.1");
    let call = CallId::new();
    let publisher = ParticipantId::new();
    let subscriber = ParticipantId::new();

    registry
        .start_session(call, publisher, 1, SUBSCRIBER_OFFER)
        .await
        .expect("publisher starts");
    let subscriber_session = registry
        .start_session(call, subscriber, 1, SUBSCRIBER_OFFER)
        .await
        .expect("subscriber starts");
    registry
        .replace_subscriptions(
            call,
            subscriber,
            subscriber_session.answer.session_generation,
            2,
            vec![SfuSubscription {
                publisher,
                pub_mid: "0".into(),
                out_mid: "0".into(),
            }],
        )
        .expect("route is installed");
    let packet = BridgeRtp::new(publisher, "0", vec![0xAA], true).to_inbound();
    assert_eq!(concrete.on_call_rtp(call, publisher, &packet), 1);

    let replacement = registry
        .start_session(call, publisher, 2, SUBSCRIBER_OFFER)
        .await
        .expect("publisher reconnects");
    assert_eq!(
        replacement.answer.topology.revision, 2,
        "an unchanged publisher description is not a topology change"
    );
    assert_eq!(
        concrete.on_call_rtp(call, publisher, &packet),
        1,
        "replacing the sole owner task must not erase other subscribers' routes"
    );
    registry.remove_call(call);
}

#[tokio::test]
async fn current_owner_task_exit_emits_revisioned_lifecycle_event_once() {
    let router = SfuRouter::new();
    let concrete = Arc::new(SfuForwarder::new(router.clone()));
    let forwarder: Arc<dyn MediaForwarder> = concrete;
    let registry = SfuMediaRegistry::new(router.clone(), forwarder, "127.0.0.1:0", "127.0.0.1");
    let mut events = registry
        .take_lifecycle_events()
        .expect("the process lifecycle worker claims the stream once");
    assert!(
        registry.take_lifecycle_events().is_none(),
        "a second cleanup worker could duplicate cross-node side effects"
    );

    let call = CallId::new();
    let participant = ParticipantId::new();
    let started = registry
        .start_session(call, participant, 1, SUBSCRIBER_OFFER)
        .await
        .expect("media owner starts");
    assert_eq!(started.answer.topology.revision, 1);

    // Model an owner loop ending without the explicit remove path. The task
    // remains the current generation in the registry, so `finished` must own
    // removal and emit the lifecycle work item.
    let task_cancel = registry
        .inner
        .lock()
        .sessions
        .get(&(call, participant))
        .expect("current generation is registered")
        .cancel
        .clone();
    task_cancel.cancel();

    let ended = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
        .await
        .expect("session owner exits promptly")
        .expect("lifecycle sender remains open");
    assert_eq!(ended.call_id, call);
    assert_eq!(ended.participant, participant);
    assert_eq!(ended.session_generation, started.answer.session_generation);
    assert!(ended.call_empty);
    assert!(ended.ended_egress_generation.is_some());
    assert_eq!(ended.topology.revision, 2);
    assert!(ended.topology.publishers.is_empty());
    assert!(!registry.has_session(call, participant));
    assert!(registry.is_current_ended_session(call, participant, ended.session_generation));
    assert!(router.participants(call).is_empty());

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "one current generation emits exactly one cleanup event"
    );
}

#[test]
fn stale_inactive_publisher_cannot_remove_a_newer_leg_generation() {
    let router = SfuRouter::new();
    let forwarder: Arc<dyn MediaForwarder> = Arc::new(SfuForwarder::new(router.clone()));
    let registry = SfuMediaRegistry::new(router, forwarder, "127.0.0.1:0", "127.0.0.1");
    let call = CallId::new();
    let participant = ParticipantId::new();
    let description = publisher(participant, "video", SfuMediaKind::Video);

    assert!(registry
        .observe_publisher(call, description.clone(), 2, true)
        .is_some());
    assert!(registry
        .observe_publisher(call, description.clone(), 1, false)
        .is_none());
    assert_eq!(
        registry.publisher(call, participant),
        Some(description.clone())
    );
    assert!(registry
        .observe_publisher(call, description, 2, false)
        .is_some());
    assert!(registry.publisher(call, participant).is_none());
}
