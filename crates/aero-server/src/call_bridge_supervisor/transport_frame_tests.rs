//! Generation-bound UDP call-bridge wire tests.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use aero_common::{CallId, ParticipantId};
use aero_live_webrtc::{
    encode_bound_bridge_frame, encode_bridge_frame, BridgeRtp, CallEgress, CallUpstream,
    BOUND_BRIDGE_COMPAT_VERSION,
};
use bytes::Bytes;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::transport::UdpRtpUpstream;
use super::{BridgeSubscriberRegistry, UdpRtpEgress};

fn loopback_destination(upstream: &UdpRtpUpstream) -> SocketAddr {
    SocketAddr::new(
        Ipv4Addr::LOCALHOST.into(),
        upstream.local_addr().unwrap().port(),
    )
}

fn packet(payload: &'static [u8]) -> BridgeRtp {
    BridgeRtp::new(
        ParticipantId::new(),
        "video0",
        Bytes::from_static(payload),
        true,
    )
}

#[tokio::test]
async fn udp_puller_receives_only_a_generation_bound_frame() {
    let call = CallId::new();
    let mut upstream = UdpRtpUpstream::bind(call, "http://peer.example")
        .await
        .unwrap();
    let destination = loopback_destination(&upstream);
    let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();

    peer.send_to(b"not-a-bridge-frame", destination)
        .await
        .unwrap();
    let legacy = packet(b"legacy");
    peer.send_to(&encode_bridge_frame(&legacy), destination)
        .await
        .unwrap();
    let expected = packet(&[0x80, 0x60, 0x11, 0x22, 0xAB]);
    peer.send_to(
        &encode_bound_bridge_frame(call, upstream.subscription_id(), &expected),
        destination,
    )
    .await
    .unwrap();

    let received = tokio::time::timeout(Duration::from_secs(2), upstream.next_rtp())
        .await
        .expect("puller must yield within 2s");
    assert_eq!(received, Some(expected));
    assert_eq!(upstream.call_id(), call);
    assert_eq!(upstream.node_url(), "http://peer.example");
}

#[tokio::test]
async fn egress_to_puller_roundtrips_over_localhost() {
    let call = CallId::new();
    let mut puller = UdpRtpUpstream::bind(call, "http://owner").await.unwrap();
    let puller_addr = loopback_destination(&puller);

    let egress = CallEgress::new(call);
    let registry = BridgeSubscriberRegistry::default();
    assert!(registry.subscribe_generation(
        call,
        puller_addr,
        puller.subscription_id(),
        Duration::from_secs(60)
    ));
    let sender = UdpRtpEgress::bind(call, registry).await.unwrap();
    let cancel = CancellationToken::new();
    let pump = tokio::spawn(sender.run(egress.tap(), cancel.clone()));

    let expected = packet(&[0x80, 0x60, 0xDE, 0xAD, 0xBE, 0xEF]);
    egress.publish(expected.clone());
    let received = tokio::time::timeout(Duration::from_secs(2), puller.next_rtp())
        .await
        .expect("puller must yield within 2s");
    assert_eq!(received, Some(expected));

    cancel.cancel();
    let _ = pump.await;
}

#[tokio::test]
async fn egress_sends_v3_to_a_generation_aware_pre_negotiation_puller() {
    let call = CallId::new();
    let mut puller = UdpRtpUpstream::bind(call, "http://owner").await.unwrap();
    let puller_addr = loopback_destination(&puller);
    let egress = CallEgress::new(call);
    let registry = BridgeSubscriberRegistry::default();
    assert!(registry.subscribe_generation_version(
        call,
        puller_addr,
        puller.subscription_id(),
        Duration::from_secs(60),
        BOUND_BRIDGE_COMPAT_VERSION
    ));
    let sender = UdpRtpEgress::bind(call, registry).await.unwrap();
    let cancel = CancellationToken::new();
    let pump = tokio::spawn(sender.run(egress.tap(), cancel.clone()));

    let original = packet(b"v3-compatibility");
    let expected = original.clone().with_keyframe_requirement(false);
    egress.publish(original);
    let received = tokio::time::timeout(Duration::from_secs(2), puller.next_rtp())
        .await
        .expect("new puller must decode old generation-bound v3 media");
    assert_eq!(received, Some(expected));

    cancel.cancel();
    let _ = pump.await;
}

#[tokio::test]
async fn late_subscriber_receives_subsequent_generation_bound_packet() {
    let call = CallId::new();
    let mut puller = UdpRtpUpstream::bind(call, "http://owner").await.unwrap();
    let puller_addr = loopback_destination(&puller);

    let egress = CallEgress::new(call);
    let registry = BridgeSubscriberRegistry::default();
    let sender = UdpRtpEgress::bind(call, registry.clone()).await.unwrap();
    let cancel = CancellationToken::new();
    let pump = tokio::spawn(sender.run(egress.tap(), cancel.clone()));

    // The relay is already active and has observed media before the remote node
    // finishes its subscribe control call.
    egress.publish(packet(b"before-subscribe"));
    tokio::time::sleep(Duration::from_millis(20)).await;

    assert!(registry.subscribe_generation(
        call,
        puller_addr,
        puller.subscription_id(),
        Duration::from_secs(60)
    ));
    let expected = packet(b"after-subscribe");
    egress.publish(expected.clone());

    let received = tokio::time::timeout(Duration::from_secs(2), puller.next_rtp())
        .await
        .expect("a subscriber registered after relay startup must receive later media");
    assert_eq!(received, Some(expected));

    cancel.cancel();
    let _ = pump.await;
}

#[tokio::test]
async fn reused_udp_port_drops_a_delayed_frame_from_a_different_call() {
    let current_call = CallId::new();
    let stale_call = CallId::new();
    let mut puller = UdpRtpUpstream::bind(current_call, "http://owner")
        .await
        .unwrap();
    let destination = loopback_destination(&puller);
    let owner = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();

    let stale = packet(b"stale-call");
    owner
        .send_to(
            &encode_bound_bridge_frame(stale_call, puller.subscription_id(), &stale),
            destination,
        )
        .await
        .unwrap();
    let expected = packet(b"current-call");
    owner
        .send_to(
            &encode_bound_bridge_frame(current_call, puller.subscription_id(), &expected),
            destination,
        )
        .await
        .unwrap();

    let received = tokio::time::timeout(Duration::from_secs(2), puller.next_rtp())
        .await
        .expect("valid current-call media follows the stale datagram");
    assert_eq!(received, Some(expected));
}

#[tokio::test]
async fn reused_udp_port_drops_a_frame_from_the_wrong_generation() {
    let call = CallId::new();
    let mut puller = UdpRtpUpstream::bind(call, "http://owner").await.unwrap();
    let destination = loopback_destination(&puller);
    let owner = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();

    let stale = packet(b"stale-generation");
    owner
        .send_to(
            &encode_bound_bridge_frame(call, Uuid::new_v4(), &stale),
            destination,
        )
        .await
        .unwrap();
    let expected = packet(b"current-generation");
    owner
        .send_to(
            &encode_bound_bridge_frame(call, puller.subscription_id(), &expected),
            destination,
        )
        .await
        .unwrap();

    let received = tokio::time::timeout(Duration::from_secs(2), puller.next_rtp())
        .await
        .expect("valid current-generation media follows the stale datagram");
    assert_eq!(received, Some(expected));
}
