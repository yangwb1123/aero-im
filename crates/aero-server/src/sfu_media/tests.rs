use std::sync::Arc;

use aero_common::{CallId, ParticipantId};
use aero_live_webrtc::{BridgeRtp, CallEgress, MediaForwarder, SfuForwarder, SfuRouter};
use bytes::Bytes;

use super::{
    extract_sdp_mids, resolve_advertised_addr, SfuMediaError, SfuMediaRegistry, SfuMediaSession,
};

const SENDRECV_OFFER: &str = "v=0\r\n\
o=- 4611731400430051336 2 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0 1\r\n\
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
a=rtpmap:111 opus/48000/2\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:abcd\r\n\
a=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
a=fingerprint:sha-256 11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00\r\n\
a=setup:actpass\r\n\
a=mid:1\r\n\
a=sendrecv\r\n\
a=rtcp-mux\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n";

#[tokio::test]
async fn binds_a_media_socket_and_answers_only_valid_offers() {
    let fwd = Arc::new(SfuForwarder::new(SfuRouter::new()));
    let call = CallId::new();
    let participant = ParticipantId::new();
    let mut session = SfuMediaSession::bind(call, participant, fwd, None, "127.0.0.1:0")
        .await
        .unwrap();

    assert_eq!(session.local_addr().ip().to_string(), "127.0.0.1");
    assert_ne!(session.local_addr().port(), 0, "ephemeral port assigned");
    assert_eq!(session.participant(), participant);
    assert!(session.accept_offer("definitely not an sdp").is_err());
}

#[tokio::test]
async fn answer_contains_the_bound_host_candidate() {
    let fwd: Arc<dyn MediaForwarder> = Arc::new(SfuForwarder::new(SfuRouter::new()));
    let mut session = SfuMediaSession::bind(
        CallId::new(),
        ParticipantId::new(),
        fwd,
        None,
        "127.0.0.1:0",
    )
    .await
    .unwrap();
    let answer = session
        .accept_offer(SENDRECV_OFFER)
        .expect("real str0m answer");
    assert!(answer.contains("a=candidate:"));
    assert!(answer.contains(&session.local_addr().port().to_string()));
    assert!(answer.contains("a=sendrecv"));
}

#[tokio::test]
async fn deliver_publishes_metadata_complete_packet_to_shared_call_egress() {
    let call = CallId::new();
    let participant = ParticipantId::new();
    let forwarder = Arc::new(SfuForwarder::new(SfuRouter::new()));
    let egress = CallEgress::new(call);
    let mut tap = egress.tap();
    let session = SfuMediaSession::bind(
        call,
        participant,
        forwarder,
        Some(egress.clone()),
        "127.0.0.1:0",
    )
    .await
    .unwrap();

    let mut expected = BridgeRtp::new(
        participant,
        "video0",
        Bytes::from_static(&[0x65, 0xAA, 0xBB]),
        true,
    );
    expected.rid = Some("high".into());
    expected.pt = 127u8.into();
    expected.seq_no = 0x1_0002u64.into();
    expected.rtp_time = 0xAABB_CCDD;
    expected.ssrc = 0x1122_3344u32.into();
    expected.marker = true;

    session.deliver(&expected.to_inbound());

    assert_eq!(
        tap.next_rtp().await,
        Some(expected),
        "the media-session egress leg must retain every forwarding-relevant RTP field"
    );
}

#[test]
fn sdp_mid_extraction_is_bounded_unique_and_ordered() {
    let sdp = "v=0\r\na=mid:0\r\na=mid:0\r\na=mid:video-main\r\na=mid:\r\n";
    assert_eq!(extract_sdp_mids(sdp), vec!["0", "video_main"]);
}

#[tokio::test]
async fn advertised_address_must_match_the_bound_socket_family() {
    let bound = "127.0.0.1:54321".parse().unwrap();
    assert_eq!(
        resolve_advertised_addr("127.0.0.2", bound).await.unwrap(),
        "127.0.0.2:54321".parse().unwrap()
    );
    let error = resolve_advertised_addr("::1", bound).await.unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
}

#[tokio::test]
async fn registry_owns_sessions_shares_egress_and_applies_trickle_ice() {
    let router = SfuRouter::new();
    let concrete = Arc::new(SfuForwarder::new(router.clone()));
    let forwarder: Arc<dyn MediaForwarder> = concrete.clone();
    let registry = SfuMediaRegistry::new(router, forwarder, "127.0.0.1:0", "127.0.0.1");
    let call = CallId::new();
    let first = ParticipantId::new();
    let second = ParticipantId::new();

    let first_started = registry
        .start_session(call, first, 1, SENDRECV_OFFER)
        .await
        .expect("first session starts");
    let egress_generation = first_started.egress_generation;
    assert_eq!(first_started.answer.mids, vec!["0", "1"]);
    assert!(first_started.answer.sdp.contains("a=candidate:"));
    assert!(registry.has_session(call, first));
    assert_eq!(concrete.peer_count(), 1);

    let second_started = registry
        .start_session(call, second, 1, SENDRECV_OFFER)
        .await
        .expect("second session starts");
    assert_eq!(
        second_started.egress_generation, egress_generation,
        "all live participants share one source epoch"
    );
    assert_eq!(registry.session_count(call), 2);
    assert_eq!(concrete.peer_count(), 2);

    registry
        .add_remote_candidate(
            call,
            first,
            "candidate:1 1 udp 2113937151 127.0.0.1 59999 typ host".into(),
        )
        .await
        .expect("ICE is parsed by the owner task before ack");

    assert_eq!(registry.remove_session(call, first), (true, false));
    assert_eq!(registry.remove_session(call, second), (true, true));
    assert_eq!(registry.session_count(call), 0);
    assert_eq!(concrete.peer_count(), 0);
}

#[tokio::test]
async fn malformed_reconnect_does_not_replace_a_working_session() {
    let router = SfuRouter::new();
    let forwarder: Arc<dyn MediaForwarder> = Arc::new(SfuForwarder::new(router.clone()));
    let registry = SfuMediaRegistry::new(router, forwarder, "127.0.0.1:0", "127.0.0.1");
    let call = CallId::new();
    let participant = ParticipantId::new();
    registry
        .start_session(call, participant, 1, SENDRECV_OFFER)
        .await
        .unwrap();

    let Err(error) = registry
        .start_session(call, participant, 1, "v=0\r\n")
        .await
    else {
        panic!("offer without media must be rejected");
    };
    assert!(matches!(error, SfuMediaError::InvalidTopology(_)));
    assert!(registry.has_session(call, participant));
    registry.remove_call(call);
}

#[tokio::test]
async fn start_commit_after_last_old_exit_uses_a_fresh_egress_epoch() {
    let router = SfuRouter::new();
    let forwarder: Arc<dyn MediaForwarder> = Arc::new(SfuForwarder::new(router.clone()));
    let registry = SfuMediaRegistry::new(router, forwarder, "127.0.0.1:0", "127.0.0.1");
    let call = CallId::new();
    let first = ParticipantId::new();
    let second = ParticipantId::new();

    let first_started = registry
        .start_session(call, first, 1, SENDRECV_OFFER)
        .await
        .expect("first epoch starts");
    let first_generation = first_started.egress_generation;
    let (reached, proceed) = registry.pause_next_start_before_commit();
    let starting = {
        let registry = registry.clone();
        tokio::spawn(async move {
            registry
                .start_session(call, second, 1, SENDRECV_OFFER)
                .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), reached)
        .await
        .expect("second bind reaches the deterministic pre-commit barrier")
        .expect("barrier sender remains live");

    let removed = registry.remove_session_with_topology(call, first);
    assert_eq!(
        removed.ended_egress_generation,
        Some(first_generation),
        "the old final session removes only its own source epoch"
    );
    proceed.send(()).expect("release second commit");
    let second_started = starting
        .await
        .expect("start task joins")
        .expect("second session commits");
    assert_ne!(second_started.egress_generation, first_generation);
    assert_eq!(
        registry
            .current_egress_tap(call)
            .map(|(generation, _)| generation),
        Some(second_started.egress_generation)
    );
    assert!(registry.has_session(call, second));
    registry.remove_call(call);
}

#[tokio::test]
async fn removal_between_commit_and_finalize_returns_stale_without_router_ghost() {
    let router = SfuRouter::new();
    let concrete = Arc::new(SfuForwarder::new(router.clone()));
    let forwarder: Arc<dyn MediaForwarder> = concrete.clone();
    let registry = SfuMediaRegistry::new(router.clone(), forwarder, "127.0.0.1:0", "127.0.0.1");
    let call = CallId::new();
    let participant = ParticipantId::new();
    let (reached, proceed) = registry.pause_next_start_before_finalize();
    let starting = {
        let registry = registry.clone();
        tokio::spawn(async move {
            registry
                .start_session(call, participant, 1, SENDRECV_OFFER)
                .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), reached)
        .await
        .expect("start reaches the deterministic post-commit barrier")
        .expect("barrier sender remains live");

    let removal = registry.remove_session_with_topology(call, participant);
    assert!(removal.removed);
    assert!(removal.ended_egress_generation.is_some());
    proceed.send(()).expect("release start finalization");

    let result = starting.await.expect("start task joins");
    assert!(matches!(result, Err(SfuMediaError::StaleSession)));
    assert!(!registry.has_session(call, participant));
    assert!(registry.current_egress_tap(call).is_none());
    assert!(router.local_participants(call).is_empty());
    assert_eq!(
        concrete.peer_count(),
        0,
        "a removed generation cannot leave a command sink behind"
    );
}

#[tokio::test]
async fn logical_rejoin_invalidates_only_the_matching_queued_exit() {
    let router = SfuRouter::new();
    let forwarder: Arc<dyn MediaForwarder> = Arc::new(SfuForwarder::new(router.clone()));
    let registry = SfuMediaRegistry::new(router, forwarder, "127.0.0.1:0", "127.0.0.1");
    let call = CallId::new();
    let participant = ParticipantId::new();
    registry
        .inner
        .lock()
        .session_epochs
        .insert((call, participant), 41);

    let _guard = registry.lock_lifecycle(call).await;
    assert!(registry.is_current_ended_session(call, participant, 41));
    registry.invalidate_ended_session(call, participant);
    assert!(!registry.is_current_ended_session(call, participant, 41));
    assert!(!registry.complete_ended_session(call, participant, 41));
}
