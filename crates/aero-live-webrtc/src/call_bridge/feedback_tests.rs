use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;

use super::*;
use crate::{KeyframeRequestKind, SfuPeerSink};

#[derive(Default)]
struct RecordingSink {
    requests: Mutex<Vec<(String, Option<String>, KeyframeRequestKind)>>,
}

impl SfuPeerSink for RecordingSink {
    fn try_write_rtp(&self, _packet: InboundRtp) -> bool {
        false
    }

    fn try_request_keyframe(&self, mid: Mid, kind: KeyframeRequestKind) -> bool {
        self.try_request_keyframe_for_rid(mid, None, kind)
    }

    fn try_request_keyframe_for_rid(
        &self,
        mid: Mid,
        rid: Option<Rid>,
        kind: KeyframeRequestKind,
    ) -> bool {
        self.requests
            .lock()
            .push((mid.to_string(), rid.map(|rid| rid.to_string()), kind));
        true
    }
}

struct FeedbackUpstream {
    call: CallId,
    packets: VecDeque<BridgeRtp>,
    sink: Arc<RecordingSink>,
}

#[async_trait]
impl CallUpstream for FeedbackUpstream {
    fn call_id(&self) -> CallId {
        self.call
    }

    fn node_url(&self) -> &'static str {
        "http://remote.example"
    }

    fn feedback_sink(&self, _participant: ParticipantId) -> Option<Arc<dyn SfuPeerSink>> {
        Some(self.sink.clone())
    }

    async fn next_rtp(&mut self) -> Option<BridgeRtp> {
        self.packets.pop_front()
    }
}

type SinkMap = HashMap<(CallId, ParticipantId), Arc<dyn SfuPeerSink>>;
type BridgeSinkMap = HashMap<(CallId, ParticipantId, String), Arc<dyn SfuPeerSink>>;

#[derive(Clone, Default)]
struct RecordingForwarder {
    local_sinks: Arc<Mutex<SinkMap>>,
    bridge_sinks: Arc<Mutex<BridgeSinkMap>>,
    resets: Arc<Mutex<Vec<(CallId, ParticipantId)>>>,
    bridge_detaches: Arc<Mutex<Vec<(CallId, ParticipantId)>>>,
}

impl RecordingForwarder {
    fn effective_sink(
        &self,
        call: CallId,
        participant: ParticipantId,
    ) -> Option<Arc<dyn SfuPeerSink>> {
        self.local_sinks
            .lock()
            .get(&(call, participant))
            .cloned()
            .or_else(|| {
                self.bridge_sinks.lock().iter().find_map(
                    |((active_call, active_participant, _), sink)| {
                        (*active_call == call && *active_participant == participant)
                            .then(|| sink.clone())
                    },
                )
            })
    }
}

#[async_trait]
impl MediaForwarder for RecordingForwarder {
    async fn forward_rtp(&self, _call: CallId, _mid: &str, _packet: Bytes) {}

    fn attach_peer_sink(
        &self,
        call: CallId,
        participant: ParticipantId,
        sink: Arc<dyn SfuPeerSink>,
    ) -> bool {
        self.local_sinks
            .lock()
            .insert((call, participant), sink)
            .is_none()
    }

    fn attach_bridge_peer_sink(
        &self,
        call: CallId,
        participant: ParticipantId,
        bridge: &str,
        sink: Arc<dyn SfuPeerSink>,
    ) -> bool {
        self.bridge_sinks
            .lock()
            .insert((call, participant, bridge.to_owned()), sink)
            .is_none()
    }

    fn detach_bridge_peer_sink(
        &self,
        call: CallId,
        participant: ParticipantId,
        bridge: &str,
    ) -> bool {
        self.bridge_detaches.lock().push((call, participant));
        self.bridge_sinks
            .lock()
            .remove(&(call, participant, bridge.to_owned()))
            .is_some()
    }

    fn reset_peer_sink(&self, call: CallId, participant: ParticipantId) -> bool {
        self.resets.lock().push((call, participant));
        self.local_sinks
            .lock()
            .remove(&(call, participant))
            .is_some()
    }
}

#[tokio::test]
async fn bridge_registers_remote_feedback_and_drop_cleans_it_idempotently() {
    let call = CallId::new();
    let remote = ParticipantId::new();
    let router = SfuRouter::new();
    let sink = Arc::new(RecordingSink::default());
    let upstream = FeedbackUpstream {
        call,
        packets: VecDeque::from([BridgeRtp::new(remote, "video0", vec![0x65], true)]),
        sink: sink.clone(),
    };
    let forwarder = RecordingForwarder::default();
    let mut bridge = CallBridge::new(upstream, router.clone(), forwarder.clone());

    assert_eq!(bridge.pump_once().await, Some(0));
    let registered = forwarder.effective_sink(call, remote).unwrap();
    assert!(registered.try_request_keyframe(Mid::from("video0"), KeyframeRequestKind::Fir));
    assert_eq!(
        sink.requests.lock().as_slice(),
        &[("video0".into(), None, KeyframeRequestKind::Fir)]
    );

    drop(bridge);
    assert!(!router.participants(call).contains(&remote));
    assert!(forwarder.effective_sink(call, remote).is_none());
    assert_eq!(
        forwarder.bridge_detaches.lock().as_slice(),
        &[(call, remote)]
    );
}

#[tokio::test]
async fn closed_video_gate_retries_pli_until_a_keyframe_arrives() {
    let call = CallId::new();
    let remote = ParticipantId::new();
    let router = SfuRouter::new();
    let sink = Arc::new(RecordingSink::default());
    let delta = || BridgeRtp::new(remote, "video0", vec![0x01], false);
    let upstream = FeedbackUpstream {
        call,
        packets: VecDeque::from([
            delta(),
            delta(),
            delta(),
            BridgeRtp::new(remote, "video0", vec![0x65], true),
        ]),
        sink: sink.clone(),
    };
    let mut bridge = CallBridge::new(upstream, router, RecordingForwarder::default());

    assert_eq!(bridge.pump_once().await, Some(0));
    assert_eq!(sink.requests.lock().len(), 1, "first delta queues PLI");
    assert_eq!(bridge.pump_once().await, Some(0));
    assert_eq!(
        sink.requests.lock().len(),
        1,
        "packets inside the retry interval are coalesced"
    );

    tokio::time::sleep(KEYFRAME_RETRY_INTERVAL + std::time::Duration::from_millis(25)).await;
    assert_eq!(bridge.pump_once().await, Some(0));
    assert_eq!(
        sink.requests.lock().len(),
        2,
        "a missing keyframe causes a bounded retry"
    );
    assert_eq!(bridge.pump_once().await, Some(0));
    assert!(bridge.has_started_for(remote, "video0"));
}

#[tokio::test]
async fn idle_closed_gate_retries_pli_without_another_rtp_packet() {
    struct FirstThenPendingUpstream {
        call: CallId,
        first: Option<BridgeRtp>,
        sink: Arc<RecordingSink>,
    }

    #[async_trait]
    impl CallUpstream for FirstThenPendingUpstream {
        fn call_id(&self) -> CallId {
            self.call
        }

        fn node_url(&self) -> &'static str {
            "http://remote.example"
        }

        fn feedback_sink(&self, _participant: ParticipantId) -> Option<Arc<dyn SfuPeerSink>> {
            Some(self.sink.clone())
        }

        async fn next_rtp(&mut self) -> Option<BridgeRtp> {
            if self.first.is_some() {
                return self.first.take();
            }
            std::future::pending().await
        }
    }

    let call = CallId::new();
    let remote = ParticipantId::new();
    let sink = Arc::new(RecordingSink::default());
    let bridge = CallBridge::new(
        FirstThenPendingUpstream {
            call,
            first: Some(BridgeRtp::new(remote, "video0", vec![0x01], false)),
            sink: sink.clone(),
        },
        SfuRouter::new(),
        RecordingForwarder::default(),
    );
    let task = tokio::spawn(bridge.run());
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while sink.requests.lock().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the first delta queues an immediate PLI");

    tokio::time::sleep(KEYFRAME_RETRY_INTERVAL + std::time::Duration::from_millis(25)).await;
    assert!(
        sink.requests.lock().len() >= 2,
        "the timer retries even while upstream RTP is silent"
    );
    task.abort();
}

#[tokio::test]
async fn simulcast_layers_have_independent_gates_and_precise_feedback() {
    let call = CallId::new();
    let remote = ParticipantId::new();
    let sink = Arc::new(RecordingSink::default());
    let mut low = BridgeRtp::new(remote, "video0", vec![0x65], true);
    low.rid = Some(Rid::from("low"));
    let mut high = BridgeRtp::new(remote, "video0", vec![0x01], false);
    high.rid = Some(Rid::from("high"));
    let upstream = FeedbackUpstream {
        call,
        packets: VecDeque::from([low, high]),
        sink: sink.clone(),
    };
    let mut bridge = CallBridge::new(upstream, SfuRouter::new(), RecordingForwarder::default());

    assert_eq!(bridge.pump_once().await, Some(0));
    assert!(bridge.has_started_for_rid(remote, Mid::from("video0"), Some(Rid::from("low"))));
    assert!(!bridge.has_started_for_rid(remote, Mid::from("video0"), Some(Rid::from("high"))));

    assert_eq!(bridge.pump_once().await, Some(0));
    assert!(!bridge.has_started_for_rid(remote, Mid::from("video0"), Some(Rid::from("high"))));
    assert_eq!(
        sink.requests.lock().as_slice(),
        &[(
            "video0".into(),
            Some("high".into()),
            KeyframeRequestKind::Pli
        )]
    );
}

#[tokio::test]
async fn bridge_never_overwrites_a_coincident_local_owner_sink() {
    let call = CallId::new();
    let participant = ParticipantId::new();
    let router = SfuRouter::new();
    router.add_peer(call, participant, PeerRole::Bidirectional);

    let local_sink: Arc<dyn SfuPeerSink> = Arc::new(RecordingSink::default());
    let remote_sink = Arc::new(RecordingSink::default());
    let upstream = FeedbackUpstream {
        call,
        packets: VecDeque::from([BridgeRtp::new(participant, "video0", vec![0x65], true)]),
        sink: remote_sink,
    };
    let forwarder = RecordingForwarder::default();
    forwarder
        .local_sinks
        .lock()
        .insert((call, participant), local_sink.clone());

    let mut bridge = CallBridge::new(upstream, router.clone(), forwarder.clone());
    assert_eq!(bridge.pump_once().await, Some(0));
    let installed = forwarder
        .effective_sink(call, participant)
        .expect("the local owner sink remains installed");
    assert!(Arc::ptr_eq(&installed, &local_sink));

    drop(bridge);
    assert!(
        router.participants(call).contains(&participant),
        "bridge detach cannot remove the coincident local peer"
    );
    assert!(
        forwarder.resets.lock().is_empty(),
        "bridge detach cannot reset the coincident local owner sink"
    );
}

#[tokio::test]
async fn late_old_drop_cannot_delete_same_url_successor_sink() {
    let call = CallId::new();
    let participant = ParticipantId::new();
    let router = SfuRouter::new();
    let old_sink = Arc::new(RecordingSink::default());
    let new_sink = Arc::new(RecordingSink::default());
    let forwarder = RecordingForwarder::default();

    let mut old_bridge = CallBridge::new(
        FeedbackUpstream {
            call,
            packets: VecDeque::from([BridgeRtp::new(participant, "video0", vec![0x65], true)]),
            sink: old_sink,
        },
        router.clone(),
        forwarder.clone(),
    );
    let mut new_bridge = CallBridge::new(
        FeedbackUpstream {
            call,
            packets: VecDeque::from([BridgeRtp::new(participant, "video0", vec![0x65], true)]),
            sink: new_sink.clone(),
        },
        router.clone(),
        forwarder.clone(),
    );
    assert_eq!(old_bridge.pump_once().await, Some(0));
    assert_eq!(new_bridge.pump_once().await, Some(0));
    assert_eq!(
        forwarder.bridge_sinks.lock().len(),
        2,
        "same URL bridge incarnations have distinct fenced sink keys"
    );

    drop(old_bridge);
    assert_eq!(forwarder.bridge_sinks.lock().len(), 1);
    let successor = forwarder
        .effective_sink(call, participant)
        .expect("the replacement bridge sink survives old Drop");
    assert!(successor.try_request_keyframe(Mid::from("video0"), KeyframeRequestKind::Pli));
    assert_eq!(
        new_sink.requests.lock().as_slice(),
        &[("video0".into(), None, KeyframeRequestKind::Pli)]
    );
    assert!(router.participants(call).contains(&participant));

    drop(new_bridge);
    assert!(forwarder.effective_sink(call, participant).is_none());
    assert!(!router.participants(call).contains(&participant));
}
