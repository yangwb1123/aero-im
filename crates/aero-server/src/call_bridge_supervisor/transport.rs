//! UDP pull-side transport for cross-node call bridges.
//!
//! Kept separate from the lifecycle registry so the production supervisor stays
//! below the repository's hard source-size limit.

use std::sync::Arc;
use std::time::Duration;

use aero_common::{CallId, ParticipantId};
use aero_live_webrtc::{
    decode_bound_bridge_frame, BridgeRtp, CallUpstream, InboundRtp, KeyframeRequestKind, Mid,
    SfuPeerSink, BOUND_BRIDGE_VERSION,
};
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::subscribers::DEFAULT_SUBSCRIBER_LEASE;
use super::UpstreamFactory;

const FEEDBACK_CAPACITY: usize = 128;
const SUBSCRIBER_REFRESH_INTERVAL: Duration = Duration::from_secs(15);

struct FeedbackCommand {
    publisher: ParticipantId,
    pub_mid: String,
    pub_rid: Option<String>,
    feedback: &'static str,
    bitrate_bps: Option<u64>,
}

#[derive(Clone)]
struct RemoteFeedbackRelay {
    tx: mpsc::Sender<FeedbackCommand>,
}

impl RemoteFeedbackRelay {
    fn spawn(http: reqwest::Client, peer_url: String, secret: String, call: CallId) -> Self {
        let (tx, mut rx) = mpsc::channel::<FeedbackCommand>(FEEDBACK_CAPACITY);
        tokio::spawn(async move {
            let url = format!(
                "{}/api/internal/call-bridge/feedback",
                peer_url.trim_end_matches('/')
            );
            while let Some(command) = rx.recv().await {
                let request = http
                    .post(&url)
                    .bearer_auth(&secret)
                    .timeout(Duration::from_secs(2))
                    .json(&serde_json::json!({
                        "call_id": call,
                        "publisher": command.publisher,
                        "pub_mid": command.pub_mid,
                        "pub_rid": command.pub_rid,
                        "feedback": command.feedback,
                        "bitrate_bps": command.bitrate_bps,
                    }));
                match request.send().await {
                    Ok(response) if response.status().is_success() => {}
                    Ok(response) => {
                        debug!(
                            status = %response.status(),
                            %call,
                            %command.publisher,
                            "call-bridge: feedback rejected"
                        );
                    }
                    Err(error) => {
                        debug!(
                            ?error,
                            %call,
                            %command.publisher,
                            "call-bridge: feedback POST failed"
                        );
                    }
                }
            }
        });
        Self { tx }
    }

    fn sink(&self, publisher: ParticipantId) -> Arc<dyn SfuPeerSink> {
        Arc::new(RemoteFeedbackSink {
            publisher,
            tx: self.tx.clone(),
        })
    }
}

struct RemoteFeedbackSink {
    publisher: ParticipantId,
    tx: mpsc::Sender<FeedbackCommand>,
}

impl SfuPeerSink for RemoteFeedbackSink {
    fn try_write_rtp(&self, _packet: InboundRtp) -> bool {
        false
    }

    fn try_request_keyframe(&self, mid: Mid, kind: KeyframeRequestKind) -> bool {
        self.try_request_keyframe_for_rid(mid, None, kind)
    }

    fn try_request_keyframe_for_rid(
        &self,
        mid: Mid,
        rid: Option<aero_live_webrtc::Rid>,
        kind: KeyframeRequestKind,
    ) -> bool {
        let feedback = match kind {
            KeyframeRequestKind::Pli => "pli",
            KeyframeRequestKind::Fir => "fir",
        };
        self.tx
            .try_send(FeedbackCommand {
                publisher: self.publisher,
                pub_mid: mid.to_string(),
                pub_rid: rid.map(|rid| rid.to_string()),
                feedback,
                bitrate_bps: None,
            })
            .is_ok()
    }

    fn try_request_remb(&self, mid: Mid, bitrate_bps: u64) -> bool {
        self.tx
            .try_send(FeedbackCommand {
                publisher: self.publisher,
                pub_mid: mid.to_string(),
                pub_rid: None,
                feedback: "remb",
                bitrate_bps: Some(bitrate_bps),
            })
            .is_ok()
    }
}

/// The production [`UpstreamFactory`]: the real node-to-node RTP puller.
///
/// [`connect`](UpstreamFactory::connect) binds a UDP socket, **announces** its
/// receive address to the owning node's subscribe endpoint (so that node's
/// egress starts pushing this call's RTP here), and returns a real
/// [`UdpRtpUpstream`]. Backend links are trusted, so the media is plain RTP
/// framed by [`bridge_frame`](aero_live_webrtc::bridge_frame) — DTLS-SRTP is
/// only on the client leg. The bind → announce → receive → decode → fan-in path
/// is exercised over localhost; the remaining staging step is a real two-node
/// run with reachable advertised addresses.
#[derive(Clone)]
pub struct NodeRtpPullerFactory {
    /// Host other nodes should send this node's bridged RTP to (its reachable
    /// address; the bound UDP port is appended). From
    /// `AERO_BRIDGE_ADVERTISE_HOST`.
    advertise_host: String,
    /// Shared cluster secret for the subscribe control call (`Bearer`). Without
    /// it no authenticated upstream can be established.
    secret: Option<String>,
    /// Reused outbound HTTP client for the subscribe POST.
    http: reqwest::Client,
    /// Renewal cadence for the owner-side expiring subscriber lease.
    refresh_interval: Duration,
}

impl NodeRtpPullerFactory {
    /// Build the factory with the address this node advertises to peers and the
    /// shared cluster secret for the subscribe control call.
    #[must_use]
    pub fn new(advertise_host: impl Into<String>, secret: Option<String>) -> Self {
        Self {
            advertise_host: advertise_host.into(),
            secret,
            http: reqwest::Client::new(),
            refresh_interval: SUBSCRIBER_REFRESH_INTERVAL,
        }
    }

    #[cfg(test)]
    fn with_refresh_interval(mut self, interval: Duration) -> Self {
        self.refresh_interval = interval;
        self
    }
}

#[async_trait]
impl UpstreamFactory for NodeRtpPullerFactory {
    async fn connect(&self, call: CallId, peer_url: &str) -> Option<Box<dyn CallUpstream>> {
        let Some(secret) = self
            .secret
            .as_deref()
            .filter(|secret| !secret.trim().is_empty())
        else {
            debug!(
                peer_url,
                "call-bridge: cluster secret missing; skipping bridge"
            );
            return None;
        };
        let mut up = match UdpRtpUpstream::bind(call, peer_url).await {
            Ok(up) => up,
            Err(error) => {
                debug!(
                    ?error,
                    peer_url, "call-bridge: udp puller bind failed; skipping bridge"
                );
                return None;
            }
        };
        let subscription_id = up.subscription_id;
        let local = match up.local_addr() {
            Ok(local) => local,
            Err(error) => {
                debug!(
                    ?error,
                    peer_url, "call-bridge: udp puller address unavailable; skipping bridge"
                );
                return None;
            }
        };
        let advertised = format!("{}:{}", self.advertise_host, local.port());
        let subscription = subscribe_to_peer(
            &self.http,
            peer_url,
            secret,
            call,
            &advertised,
            subscription_id,
        )
        .await;
        if subscription == SubscribeOutcome::Rejected {
            // An explicit client-error response proves the owner did not admit
            // this destination, so leave the supervisor key free for retry.
            return None;
        }
        // A timeout, connection loss, or 5xx is commit-uncertain: keep the same
        // socket alive and renew. This covers a response lost after either a v2
        // lease or a rolling-upgrade legacy entry was committed; dropping the
        // socket would leave the owner sending forever to a dead UDP port.
        up.subscription = Some(RemoteSubscription::spawn(
            self.http.clone(),
            peer_url.to_owned(),
            secret.to_owned(),
            call,
            advertised,
            subscription_id,
            self.refresh_interval,
        ));
        up.feedback = Some(RemoteFeedbackRelay::spawn(
            self.http.clone(),
            peer_url.to_owned(),
            secret.to_owned(),
            call,
        ));
        Some(Box::new(up))
    }
}

/// Real node-to-node RTP puller over plain UDP.
///
/// Receives framed datagrams on a bound UDP socket—each carrying one remote
/// participant's RTP—and yields them for the [`aero_live_webrtc::CallBridge`]
/// to fan into the local SFU.
pub(super) struct UdpRtpUpstream {
    pub(super) call: CallId,
    /// Pull incarnation encoded into every accepted v4 media datagram.
    subscription_id: uuid::Uuid,
    node_url: String,
    socket: tokio::net::UdpSocket,
    feedback: Option<RemoteFeedbackRelay>,
    /// Keeps the owner-side destination lease alive and removes it on drop.
    subscription: Option<RemoteSubscription>,
    /// Receive scratch buffer, sized for a jumbo-ish RTP datagram + frame header.
    buf: Vec<u8>,
    /// Avoid a per-packet debug log while still proving the UDP/wire leg became
    /// live during staging diagnosis.
    logged_first_frame: bool,
}

impl UdpRtpUpstream {
    /// Bind an ephemeral UDP socket to receive `call`'s bridged RTP from
    /// `peer_url`.
    pub(super) async fn bind(call: CallId, peer_url: &str) -> std::io::Result<Self> {
        let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
        Ok(Self {
            call,
            subscription_id: uuid::Uuid::new_v4(),
            node_url: peer_url.to_owned(),
            socket,
            feedback: None,
            subscription: None,
            buf: vec![0u8; 2048],
            logged_first_frame: false,
        })
    }

    /// The local address whose port the peer's egress should push frames to.
    pub(super) fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.socket.local_addr()
    }

    #[cfg(test)]
    pub(super) fn subscription_id(&self) -> uuid::Uuid {
        self.subscription_id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubscribeOutcome {
    Accepted,
    Rejected,
    Uncertain,
}

/// Announce `addr` as where to push `call`'s bridged RTP. Transport/5xx
/// failures are commit-uncertain and therefore keep the receive socket alive
/// for lease renewal; explicit client errors reject the bridge.
async fn subscribe_to_peer(
    http: &reqwest::Client,
    peer_url: &str,
    secret: &str,
    call: CallId,
    addr: &str,
    subscription_id: uuid::Uuid,
) -> SubscribeOutcome {
    let url = format!(
        "{}/api/internal/call-bridge/subscribe",
        peer_url.trim_end_matches('/')
    );
    let request = http
        .post(&url)
        .bearer_auth(secret)
        .timeout(Duration::from_secs(2))
        .json(&serde_json::json!({
            "call_id": call.to_string(),
            "addr": addr,
            "lease_secs": DEFAULT_SUBSCRIBER_LEASE.as_secs(),
            "subscription_id": subscription_id,
            "wire_version": BOUND_BRIDGE_VERSION,
        }));
    match request.send().await {
        Ok(response) if response.status().is_success() => SubscribeOutcome::Accepted,
        Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => {
            // A Join may reach this node before the remote browser's first SFU
            // offer has committed an egress source. Keep the supervisor key
            // free so the later SfuPublisher event/repair loop can retry.
            debug!(
                status = %response.status(),
                %call,
                peer_url,
                reason = "owner_egress_not_ready_or_endpoint_disabled",
                "call-bridge: subscribe not ready"
            );
            SubscribeOutcome::Rejected
        }
        Ok(response)
            if response.status().is_client_error()
                && response.status() != reqwest::StatusCode::REQUEST_TIMEOUT =>
        {
            debug!(
                status = %response.status(),
                %call,
                peer_url,
                "call-bridge: subscribe rejected"
            );
            SubscribeOutcome::Rejected
        }
        Ok(response) => {
            debug!(
                status = %response.status(),
                %call,
                peer_url,
                "call-bridge: subscribe outcome uncertain; retaining socket for renewal"
            );
            SubscribeOutcome::Uncertain
        }
        Err(error) => {
            debug!(
                ?error,
                %call,
                peer_url, "call-bridge: subscribe outcome uncertain; retaining socket for renewal"
            );
            SubscribeOutcome::Uncertain
        }
    }
}

async fn unsubscribe_from_peer(
    http: &reqwest::Client,
    peer_url: &str,
    secret: &str,
    call: CallId,
    addr: &str,
    subscription_id: uuid::Uuid,
) -> bool {
    let url = format!(
        "{}/api/internal/call-bridge/unsubscribe",
        peer_url.trim_end_matches('/')
    );
    match http
        .post(url)
        .bearer_auth(secret)
        .timeout(Duration::from_secs(2))
        .json(&serde_json::json!({
            "call_id": call.to_string(),
            "addr": addr,
            "subscription_id": subscription_id,
        }))
        .send()
        .await
    {
        Ok(response)
            if response.status().is_success()
                || response.status() == reqwest::StatusCode::NOT_FOUND =>
        {
            // The owner may have cleared every destination while this puller
            // was unwinding. Absence is the desired idempotent final state.
            true
        }
        Ok(response) => {
            debug!(
                status = %response.status(),
                %call,
                peer_url,
                "call-bridge: unsubscribe rejected"
            );
            false
        }
        Err(error) => {
            debug!(?error, %call, peer_url, "call-bridge: unsubscribe POST failed");
            false
        }
    }
}

/// Background owner-side lease renewal tied to one UDP puller's lifetime.
struct RemoteSubscription {
    cancel: CancellationToken,
    handle: JoinHandle<()>,
    http: reqwest::Client,
    peer_url: String,
    secret: String,
    call: CallId,
    addr: String,
    subscription_id: uuid::Uuid,
}

impl RemoteSubscription {
    fn spawn(
        http: reqwest::Client,
        peer_url: String,
        secret: String,
        call: CallId,
        addr: String,
        subscription_id: uuid::Uuid,
        refresh_interval: Duration,
    ) -> Self {
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task_http = http.clone();
        let task_peer = peer_url.clone();
        let task_secret = secret.clone();
        let task_addr = addr.clone();
        let task_subscription_id = subscription_id;
        let handle = tokio::spawn(async move {
            let mut tick = tokio::time::interval_at(
                tokio::time::Instant::now() + refresh_interval,
                refresh_interval,
            );
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    () = task_cancel.cancelled() => break,
                    _ = tick.tick() => {
                        // Failure is intentionally non-terminal: the owner prunes
                        // the old lease, while a later successful refresh safely
                        // restores this still-live puller's address.
                        let _outcome = subscribe_to_peer(
                            &task_http,
                            &task_peer,
                            &task_secret,
                            call,
                            &task_addr,
                            task_subscription_id,
                        ).await;
                    }
                }
            }
        });
        Self {
            cancel,
            handle,
            http,
            peer_url,
            secret,
            call,
            addr,
            subscription_id,
        }
    }
}

impl Drop for RemoteSubscription {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.handle.abort();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let http = self.http.clone();
        let peer_url = self.peer_url.clone();
        let secret = self.secret.clone();
        let call = self.call;
        let addr = self.addr.clone();
        let subscription_id = self.subscription_id;
        runtime.spawn(async move {
            let _ = unsubscribe_from_peer(&http, &peer_url, &secret, call, &addr, subscription_id)
                .await;
        });
    }
}

#[async_trait]
impl CallUpstream for UdpRtpUpstream {
    fn call_id(&self) -> CallId {
        self.call
    }

    fn node_url(&self) -> &str {
        &self.node_url
    }

    fn feedback_sink(&self, participant: ParticipantId) -> Option<Arc<dyn SfuPeerSink>> {
        self.feedback.as_ref().map(|relay| relay.sink(participant))
    }

    async fn next_rtp(&mut self) -> Option<BridgeRtp> {
        loop {
            let n = match self.socket.recv(&mut self.buf).await {
                Ok(n) => n,
                // Socket closed / fatal error → end of stream; the bridge winds
                // down.
                Err(error) => {
                    debug!(?error, node_url = %self.node_url, "call-bridge: udp recv ended");
                    return None;
                }
            };
            // Strictly bind media to this call and socket incarnation. This
            // drops delayed traffic after ephemeral-port reuse. A v2 frame from
            // an old owner also fails closed during a mixed-version rollout;
            // upgrade owners before pullers to avoid that temporary media gap.
            if let Some(frame) = decode_bound_bridge_frame(&self.buf[..n]) {
                if frame.call == self.call && frame.subscription_id == self.subscription_id {
                    if !self.logged_first_frame {
                        self.logged_first_frame = true;
                        debug!(
                            call = %self.call,
                            node_url = %self.node_url,
                            "call-bridge: first generation-bound RTP frame received"
                        );
                    }
                    return Some(frame.packet);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};

    use axum::{
        extract::State,
        http::{header, HeaderMap, StatusCode},
        routing::post,
        Json, Router,
    };
    use parking_lot::Mutex;
    use serde_json::Value;

    use super::super::{BridgeSubscriberRegistry, CallBridgeSupervisor};
    use super::*;
    use aero_live_webrtc::{NullForwarder, PeerRole, SfuRouter};

    struct SubscribeControl {
        status: AtomicU16,
        requests: AtomicUsize,
    }

    async fn controlled_subscribe(State(control): State<Arc<SubscribeControl>>) -> StatusCode {
        control.requests.fetch_add(1, Ordering::SeqCst);
        StatusCode::from_u16(control.status.load(Ordering::SeqCst)).expect("test status is valid")
    }

    async fn subscribe_server(
        status: StatusCode,
    ) -> (
        Arc<SubscribeControl>,
        std::net::SocketAddr,
        tokio::task::JoinHandle<()>,
    ) {
        let control = Arc::new(SubscribeControl {
            status: AtomicU16::new(status.as_u16()),
            requests: AtomicUsize::new(0),
        });
        let app = Router::new()
            .route(
                "/api/internal/call-bridge/subscribe",
                post(controlled_subscribe),
            )
            .with_state(control.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (control, address, server)
    }

    fn supervisor(factory: NodeRtpPullerFactory) -> CallBridgeSupervisor {
        CallBridgeSupervisor::new(
            SfuRouter::new(),
            Arc::new(NullForwarder),
            Arc::new(factory),
            BridgeSubscriberRegistry::default(),
        )
    }

    async fn accept_subscribe() -> StatusCode {
        StatusCode::NO_CONTENT
    }

    async fn capture_feedback(
        State(tx): State<mpsc::Sender<Value>>,
        headers: HeaderMap,
        Json(mut value): Json<Value>,
    ) -> StatusCode {
        value["authorization"] = Value::String(
            headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned(),
        );
        let _ = tx.send(value).await;
        StatusCode::NO_CONTENT
    }

    struct LeaseControl {
        subscribes: AtomicUsize,
        unsubscribes: AtomicUsize,
        requests: Mutex<Vec<(&'static str, Value)>>,
    }

    async fn capture_subscribe(
        State(control): State<Arc<LeaseControl>>,
        Json(body): Json<Value>,
    ) -> StatusCode {
        control.subscribes.fetch_add(1, Ordering::SeqCst);
        control.requests.lock().push(("subscribe", body));
        StatusCode::NO_CONTENT
    }

    async fn capture_unsubscribe(
        State(control): State<Arc<LeaseControl>>,
        Json(body): Json<Value>,
    ) -> StatusCode {
        control.unsubscribes.fetch_add(1, Ordering::SeqCst);
        control.requests.lock().push(("unsubscribe", body));
        StatusCode::NO_CONTENT
    }

    #[tokio::test]
    async fn missing_secret_does_not_contact_or_register_peer() {
        let (control, address, server) = subscribe_server(StatusCode::NO_CONTENT).await;
        let call = CallId::new();
        let peer = format!("http://{address}");
        let supervisor = supervisor(NodeRtpPullerFactory::new("127.0.0.1", None));

        assert_eq!(
            supervisor
                .ensure_bridges(call, std::slice::from_ref(&peer))
                .await,
            0
        );
        assert_eq!(supervisor.bridge_count(call), 0);
        assert!(!supervisor.is_bridged(call, &peer));
        assert_eq!(
            control.requests.load(Ordering::SeqCst),
            0,
            "missing credentials must fail before the subscribe request"
        );

        server.abort();
    }

    #[tokio::test]
    async fn explicit_reject_retries_but_uncertain_commit_keeps_the_socket() {
        let (control, address, server) = subscribe_server(StatusCode::UNAUTHORIZED).await;
        let call = CallId::new();
        let peer = format!("http://{address}");
        let supervisor = supervisor(NodeRtpPullerFactory::new(
            "127.0.0.1",
            Some("cluster-secret".into()),
        ));
        supervisor
            .router
            .add_peer(call, ParticipantId::new(), PeerRole::Bidirectional);

        assert_eq!(
            supervisor
                .ensure_bridges(call, std::slice::from_ref(&peer))
                .await,
            0
        );
        assert_eq!(supervisor.bridge_count(call), 0);
        assert!(!supervisor.is_bridged(call, &peer));

        control
            .status
            .store(StatusCode::SERVICE_UNAVAILABLE.as_u16(), Ordering::SeqCst);
        assert_eq!(
            supervisor
                .ensure_bridges(call, std::slice::from_ref(&peer))
                .await,
            1,
            "5xx may follow a committed write, so the receive socket stays live"
        );
        assert!(supervisor.is_bridged(call, &peer));
        assert_eq!(supervisor.bridge_count(call), 1);
        assert!(supervisor.cancel_bridge(call, &peer));

        control
            .status
            .store(StatusCode::NO_CONTENT.as_u16(), Ordering::SeqCst);
        assert_eq!(
            supervisor
                .ensure_bridges(call, std::slice::from_ref(&peer))
                .await,
            1,
            "an explicit cancellation can reconnect after recovery"
        );
        assert_eq!(control.requests.load(Ordering::SeqCst), 3);

        supervisor.cancel_call(call);
        server.abort();
    }

    #[tokio::test]
    async fn live_puller_renews_lease_and_drop_unsubscribes() {
        let control = Arc::new(LeaseControl {
            subscribes: AtomicUsize::new(0),
            unsubscribes: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .route(
                "/api/internal/call-bridge/subscribe",
                post(capture_subscribe),
            )
            .route(
                "/api/internal/call-bridge/unsubscribe",
                post(capture_unsubscribe),
            )
            .with_state(control.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let factory = NodeRtpPullerFactory::new("127.0.0.1", Some("cluster-secret".into()))
            .with_refresh_interval(Duration::from_millis(20));
        let upstream = factory
            .connect(CallId::new(), &format!("http://{address}"))
            .await
            .expect("initial authenticated subscription succeeds");
        tokio::time::timeout(Duration::from_secs(1), async {
            while control.subscribes.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("lease renewal arrives");

        drop(upstream);
        tokio::time::timeout(Duration::from_secs(1), async {
            while control.unsubscribes.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("best-effort unsubscribe arrives");
        let requests = control.requests.lock();
        let subscription_ids: Vec<&str> = requests
            .iter()
            .map(|(_, body)| {
                body["subscription_id"]
                    .as_str()
                    .expect("generation-aware request carries an incarnation")
            })
            .collect();
        assert!(
            subscription_ids.windows(2).all(|ids| ids[0] == ids[1]),
            "initial subscribe, refresh, and unsubscribe use one generation"
        );
        assert_eq!(requests.last().map(|(kind, _)| *kind), Some("unsubscribe"));
        server.abort();
    }

    #[tokio::test]
    async fn already_absent_unsubscribe_is_idempotent_success() {
        let app = Router::new().route(
            "/api/internal/call-bridge/unsubscribe",
            post(|| async { StatusCode::NOT_FOUND }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        assert!(
            unsubscribe_from_peer(
                &reqwest::Client::new(),
                &format!("http://{address}"),
                "cluster-secret",
                CallId::new(),
                "127.0.0.1:45678",
                uuid::Uuid::new_v4(),
            )
            .await,
            "an already-cleared owner destination is the desired final state"
        );
        server.abort();
    }

    #[tokio::test]
    async fn remote_feedback_sink_posts_keyframe_and_remb_over_bounded_worker() {
        let (tx, mut rx) = mpsc::channel(4);
        let app = Router::new()
            .route(
                "/api/internal/call-bridge/subscribe",
                post(accept_subscribe),
            )
            .route("/api/internal/call-bridge/feedback", post(capture_feedback))
            .with_state(tx);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let call = CallId::new();
        let publisher = ParticipantId::new();
        let factory = NodeRtpPullerFactory::new("127.0.0.1", Some("cluster-secret".into()));
        let upstream = factory
            .connect(call, &format!("http://{address}"))
            .await
            .expect("UDP puller and feedback relay start");
        let sink = upstream.feedback_sink(publisher).unwrap();
        assert!(sink.try_request_keyframe_for_rid(
            Mid::from("video0"),
            Some(aero_live_webrtc::Rid::from("high")),
            KeyframeRequestKind::Pli
        ));
        assert!(sink.try_request_remb(Mid::from("video0"), 640_000));

        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let second = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first["call_id"], call.to_string());
        assert_eq!(first["publisher"], publisher.to_string());
        assert_eq!(first["pub_mid"], "video0");
        assert_eq!(first["pub_rid"], "high");
        assert_eq!(first["feedback"], "pli");
        assert_eq!(first["authorization"], "Bearer cluster-secret");
        assert_eq!(second["feedback"], "remb");
        assert_eq!(second["bitrate_bps"], 640_000);

        drop(sink);
        drop(upstream);
        server.abort();
    }
}
