//! SFU per-peer media session — the server-side str0m event loop that finally
//! **drives** the SFU forwarder (ROADMAP 方向五).
//!
//! [`SfuForwarder::on_rtp`](aero_live_webrtc::SfuForwarder::on_rtp) and the whole
//! BWE / simulcast / keyframe machinery were built and unit-tested, but nothing
//! ever called them — no socket loop fed the SFU. This module is that loop, one
//! per participant, mirroring `aero-live-whip`'s WHEP receive loop:
//!
//! 1. bind a media UDP socket and create an
//!    [`SfuPeer`](aero_live_webrtc::SfuPeer) (a str0m `Rtc` in RTP mode);
//! 2. answer the participant's recvonly/sendrecv SDP offer
//!    ([`SfuMediaSession::accept_offer`]);
//! 3. run the loop: drain [`SfuPeer::poll`](aero_live_webrtc::SfuPeer::poll) —
//!    sending ICE/DTLS/RTCP `Transmit` datagrams and, on each decrypted
//!    **`Media`** packet, the SFU `on_rtp` handler
//!    (`SfuMediaSession::deliver`) forwards it to local subscribers AND publishes
//!    it to the call's cross-node [`CallEgress`]; feed inbound UDP /
//!    timeouts back via
//!    [`SfuPeer::handle_datagram`](aero_live_webrtc::SfuPeer::handle_datagram) /
//!    [`SfuPeer::handle_timeout`](aero_live_webrtc::SfuPeer::handle_timeout).
//!
//! Like WHIP/WHEP, the ICE/DTLS/SRTP **handshake that delivers real media** is
//! exercised by a real WebRTC peer (browser, or a paired `str0m`), not in CI; the
//! loop's structure + SDP answer are tested here, and the two halves of the
//! `on_rtp` handler are tested in `forward.rs` (forwarding) and the call-bridge
//! tests (egress relay).

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::Arc;

use aero_common::{
    CallId, ParticipantId, SfuPublishedTrack, SfuPublisherDescription, SfuSubscription,
};
use aero_live_webrtc::{
    canonical_mid, CallEgress, CallEgressTap, KeyframeRequestKind, MediaForwarder, Mid, PeerRole,
    Rid, SfuError, SfuRouter,
};
use parking_lot::Mutex;
use tokio::sync::mpsc;
#[cfg(test)]
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

mod session;
mod topology;
#[cfg(test)]
use session::resolve_advertised_addr;
pub use session::{SfuMediaSession, SfuMediaSessionHandle};
pub use topology::SfuTopologySnapshot;
use topology::{parse_offer_layout, sorted_publishers, SfuReceiveSlot};
#[cfg(test)]
mod registry_tests;
#[cfg(test)]
mod tests;

const MAX_SUBSCRIPTIONS: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum SfuMediaError {
    #[error("media socket: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Sfu(#[from] SfuError),
    #[error("SFU session not found")]
    NotFound,
    #[error("SFU session command queue is full")]
    QueueFull,
    #[error("SFU session command queue is closed")]
    Closed,
    #[error("SDP offer has no media mids")]
    NoMedia,
    #[error("invalid SFU topology: {0}")]
    InvalidTopology(String),
    #[error("stale SFU topology revision: expected {expected}, got {actual}")]
    StaleTopology { expected: u64, actual: u64 },
    #[error("stale SFU media session generation")]
    StaleSession,
    #[error("invalid SFU subscription: {0}")]
    InvalidSubscription(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SfuSessionAnswer {
    pub sdp: String,
    pub local_addr: SocketAddr,
    pub mids: Vec<String>,
    pub session_generation: u64,
    pub leg_generation: i64,
    pub topology: SfuTopologySnapshot,
}

pub(crate) struct SfuSessionStart {
    pub answer: SfuSessionAnswer,
    pub egress_generation: u64,
    pub egress_tap: CallEgressTap,
}

/// Emitted exactly once when the owner task for a current SFU media generation
/// exits without first being removed by an explicit leave/reconnect/call-end.
///
/// The registry has already removed the publisher and incremented the topology
/// revision when this event is emitted. The server lifecycle worker uses it to
/// fan that snapshot to local subscribers, publish a cluster-wide inactive
/// publisher event, and clear Redis/call-route state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SfuSessionEnded {
    pub call_id: CallId,
    pub participant: ParticipantId,
    /// Monotonic owner-task incarnation for `(call, participant)`.
    pub session_generation: u64,
    /// Durable cluster-wide incarnation from `call_participants`.
    pub leg_generation: i64,
    pub topology: SfuTopologySnapshot,
    pub call_empty: bool,
    /// Source epoch removed when this was the final local media session.
    /// Lifecycle cleanup must fence against this value before touching a call
    /// that may already have reconnected.
    pub ended_egress_generation: Option<u64>,
}

pub(crate) struct SfuMediaRemoval {
    pub removed: bool,
    pub topology: Option<SfuTopologySnapshot>,
    pub ended_egress_generation: Option<u64>,
}

struct SessionTask {
    id: u64,
    cancel: CancellationToken,
    handle: Option<JoinHandle<()>>,
    command: SfuMediaSessionHandle,
    mids: Vec<String>,
    receive_slots: Vec<SfuReceiveSlot>,
    subscriptions: Vec<SfuSubscription>,
    egress_generation: u64,
    leg_generation: i64,
}

#[derive(Clone)]
struct EgressSource {
    generation: u64,
    bus: CallEgress,
}

#[derive(Default)]
struct RegistryState {
    sessions: HashMap<(CallId, ParticipantId), SessionTask>,
    /// Latest media incarnation per participant. It is retained after a
    /// spontaneous exit until lifecycle cleanup consumes it, which fences an
    /// old async leave from a reconnect.
    session_epochs: HashMap<(CallId, ParticipantId), u64>,
    egresses: HashMap<CallId, EgressSource>,
    /// Last allocated source epoch per call, retained after removal so a late
    /// lifecycle event cannot match a newer epoch that also ended meanwhile.
    egress_epochs: HashMap<CallId, u64>,
    publishers: HashMap<(CallId, ParticipantId), Vec<SfuPublishedTrack>>,
    publisher_generations: HashMap<(CallId, ParticipantId), i64>,
    revisions: HashMap<CallId, u64>,
    next_id: u64,
    next_egress_generation: u64,
}

impl RegistryState {
    fn update_publisher(
        &mut self,
        call: CallId,
        participant: ParticipantId,
        mut tracks: Vec<SfuPublishedTrack>,
        leg_generation: i64,
        active: bool,
    ) -> bool {
        tracks.sort_by(|left, right| left.mid.cmp(&right.mid));
        let key = (call, participant);
        let current_generation = self.publisher_generations.get(&key).copied();
        let changed = if active {
            if current_generation.is_some_and(|current| {
                leg_generation < current || (leg_generation == 0 && current > 0)
            }) {
                false
            } else {
                self.publisher_generations.insert(key, leg_generation);
                let tracks_changed = self.publishers.get(&key) != Some(&tracks);
                if tracks_changed {
                    self.publishers.insert(key, tracks);
                }
                tracks_changed
            }
        } else if current_generation == Some(leg_generation)
            || (leg_generation == 0 && current_generation == Some(0))
        {
            self.publisher_generations.remove(&key);
            self.publishers.remove(&key).is_some()
        } else {
            false
        };
        if changed {
            let revision = self.revisions.entry(call).or_default();
            *revision = revision.wrapping_add(1).max(1);
        }
        changed
    }

    fn topology(&self, call: CallId) -> SfuTopologySnapshot {
        let publishers = sorted_publishers(self.publishers.iter().filter_map(
            |((active_call, participant), tracks)| {
                (*active_call == call).then_some(SfuPublisherDescription {
                    participant: *participant,
                    tracks: tracks.clone(),
                })
            },
        ));
        SfuTopologySnapshot {
            call_id: call,
            revision: self.revisions.get(&call).copied().unwrap_or(0),
            publishers,
        }
    }
}

/// Production owner of every `(call, participant)` SFU media session.
///
/// The registry is embedded in [`crate::call_bridge_supervisor::CallBridgeSupervisor`],
/// which already lives in [`crate::state::AppState`]. This avoids another boot
/// field while still giving WS offer/ICE/leave/disconnect handlers a real,
/// non-test lifecycle.
#[derive(Clone)]
pub struct SfuMediaRegistry {
    router: SfuRouter,
    forwarder: Arc<dyn MediaForwarder>,
    inner: Arc<Mutex<RegistryState>>,
    lifecycle_tx: mpsc::UnboundedSender<SfuSessionEnded>,
    lifecycle_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<SfuSessionEnded>>>>,
    /// Fixed stripes avoid an unbounded per-call lock map while serializing
    /// starts and lifecycle cleanup for the same call.
    lifecycle_locks: Arc<[tokio::sync::Mutex<()>]>,
    bind_addr: Arc<str>,
    advertise_host: Arc<str>,
    #[cfg(test)]
    start_commit_hook: Arc<Mutex<Option<StartCommitHook>>>,
    #[cfg(test)]
    start_finalize_hook: Arc<Mutex<Option<StartCommitHook>>>,
}

const LIFECYCLE_LOCK_STRIPES: usize = 256;

#[cfg(test)]
struct StartCommitHook {
    reached: oneshot::Sender<()>,
    proceed: oneshot::Receiver<()>,
}

impl SfuMediaRegistry {
    #[must_use]
    pub fn from_env(router: SfuRouter, forwarder: Arc<dyn MediaForwarder>) -> Self {
        let bind_addr =
            std::env::var("AERO_SFU_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:0".to_owned());
        let advertise_host = std::env::var("AERO_SFU_ADVERTISE_HOST")
            .or_else(|_| std::env::var("AERO_INGEST_HOST"))
            .unwrap_or_else(|_| "127.0.0.1".to_owned());
        Self::new(router, forwarder, bind_addr, advertise_host)
    }

    #[must_use]
    pub fn new(
        router: SfuRouter,
        forwarder: Arc<dyn MediaForwarder>,
        bind_addr: impl Into<Arc<str>>,
        advertise_host: impl Into<Arc<str>>,
    ) -> Self {
        let (lifecycle_tx, lifecycle_rx) = mpsc::unbounded_channel();
        Self {
            router,
            forwarder,
            inner: Arc::new(Mutex::new(RegistryState::default())),
            lifecycle_tx,
            lifecycle_rx: Arc::new(Mutex::new(Some(lifecycle_rx))),
            lifecycle_locks: (0..LIFECYCLE_LOCK_STRIPES)
                .map(|_| tokio::sync::Mutex::new(()))
                .collect::<Vec<_>>()
                .into(),
            bind_addr: bind_addr.into(),
            advertise_host: advertise_host.into(),
            #[cfg(test)]
            start_commit_hook: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            start_finalize_hook: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(test)]
    fn pause_next_start_before_commit(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached_tx, reached_rx) = oneshot::channel();
        let (proceed_tx, proceed_rx) = oneshot::channel();
        *self.start_commit_hook.lock() = Some(StartCommitHook {
            reached: reached_tx,
            proceed: proceed_rx,
        });
        (reached_rx, proceed_tx)
    }

    #[cfg(test)]
    fn pause_next_start_before_finalize(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached_tx, reached_rx) = oneshot::channel();
        let (proceed_tx, proceed_rx) = oneshot::channel();
        *self.start_finalize_hook.lock() = Some(StartCommitHook {
            reached: reached_tx,
            proceed: proceed_rx,
        });
        (reached_rx, proceed_tx)
    }

    fn lifecycle_lock_index(call: CallId) -> usize {
        let mut hasher = DefaultHasher::new();
        call.hash(&mut hasher);
        // u64→usize narrows only on 32-bit targets; a zero stripe is a safe
        // fallback for a hash index.
        usize::try_from(hasher.finish()).unwrap_or(0) % LIFECYCLE_LOCK_STRIPES
    }

    pub(crate) async fn lock_lifecycle(&self, call: CallId) -> tokio::sync::MutexGuard<'_, ()> {
        self.lifecycle_locks[Self::lifecycle_lock_index(call)]
            .lock()
            .await
    }

    /// Take the sole reliable lifecycle stream. It is intentionally single-
    /// consumer: one process-level worker owns all Hub/NATS/Redis cleanup, while
    /// the unbounded channel retains rare session exits that happen before that
    /// worker begins polling.
    pub fn take_lifecycle_events(&self) -> Option<mpsc::UnboundedReceiver<SfuSessionEnded>> {
        self.lifecycle_rx.lock().take()
    }

    pub(crate) async fn start_session(
        &self,
        call: CallId,
        participant: ParticipantId,
        leg_generation: i64,
        offer: &str,
    ) -> Result<SfuSessionStart, SfuMediaError> {
        // Serializing starts keeps reconnect replacement atomic without ever
        // holding a synchronous registry lock across socket/DNS awaits.
        let _lifecycle_guard = self.lock_lifecycle(call).await;
        let layout = parse_offer_layout(offer).map_err(SfuMediaError::InvalidTopology)?;
        let mut published_tracks = layout.published.clone();
        published_tracks.sort_by(|left, right| left.mid.cmp(&right.mid));
        let mids = extract_sdp_mids(offer);
        if mids.is_empty() {
            return Err(SfuMediaError::NoMedia);
        }

        let mut session = SfuMediaSession::bind_advertised(
            call,
            participant,
            self.forwarder.clone(),
            None,
            &self.bind_addr,
            &self.advertise_host,
        )
        .await?;
        let answer_sdp = session.accept_offer(offer)?;
        let local_addr = session.local_addr();
        let command_handle = session.command_handle();
        #[cfg(test)]
        let start_commit_hook = { self.start_commit_hook.lock().take() };
        #[cfg(test)]
        if let Some(hook) = start_commit_hook {
            let _ = hook.reached.send(());
            let _ = hook.proceed.await;
        }

        // Only replace the previous leg after socket/DNS/SDP work is known-good.
        // Egress source selection, session attachment, and registry insertion
        // commit in one lock region so a concurrent final `finished()` cannot
        // leave this session publishing into an unregistered old bus.
        let (
            existing,
            preserve_publisher_routes,
            id,
            cancel,
            topology,
            egress_generation,
            egress_tap,
        ) = {
            let mut inner = self.inner.lock();
            let existing = inner.sessions.remove(&(call, participant));
            let preserve_publisher_routes = existing.is_some()
                && inner.publishers.get(&(call, participant)) == Some(&published_tracks);
            let source = inner.egresses.get(&call).cloned().unwrap_or_else(|| {
                inner.next_egress_generation = inner.next_egress_generation.wrapping_add(1).max(1);
                let source = EgressSource {
                    generation: inner.next_egress_generation,
                    bus: CallEgress::new(call),
                };
                inner.egress_epochs.insert(call, source.generation);
                inner.egresses.insert(call, source.clone());
                source
            });
            session.set_egress(source.bus.clone());
            let egress_tap = source.bus.tap();
            let id = inner.next_id;
            inner.next_id = inner.next_id.wrapping_add(1);
            let cancel = CancellationToken::new();
            inner.update_publisher(
                call,
                participant,
                published_tracks.clone(),
                leg_generation,
                true,
            );
            inner.session_epochs.insert((call, participant), id);
            inner.sessions.insert(
                (call, participant),
                SessionTask {
                    id,
                    cancel: cancel.clone(),
                    handle: None,
                    command: command_handle.clone(),
                    mids: mids.clone(),
                    receive_slots: layout.receive_slots,
                    subscriptions: Vec::new(),
                    egress_generation: source.generation,
                    leg_generation,
                },
            );
            let topology = inner.topology(call);
            (
                existing,
                preserve_publisher_routes,
                id,
                cancel,
                topology,
                source.generation,
                egress_tap,
            )
        };
        if let Some(existing) = existing {
            existing.cancel.cancel();
            if let Some(handle) = existing.handle {
                handle.abort();
            }
            if preserve_publisher_routes {
                self.forwarder.reset_peer_sink(call, participant);
            } else {
                self.forwarder.detach_peer_sink(call, participant);
            }
            self.router.remove_peer(call, participant);
        }

        #[cfg(test)]
        let start_finalize_hook = { self.start_finalize_hook.lock().take() };
        #[cfg(test)]
        if let Some(hook) = start_finalize_hook {
            let _ = hook.reached.send(());
            let _ = hook.proceed.await;
        }

        self.router
            .add_peer(call, participant, PeerRole::Bidirectional);
        for track in &published_tracks {
            self.router.add_track(call, &track.mid, participant);
        }
        self.forwarder
            .attach_peer_sink(call, participant, Arc::new(command_handle.clone()));

        let registry = self.clone();
        let run_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            session.run(run_cancel).await;
            registry.finished(call, participant, id);
        });
        let mut pending_task = Some(task);
        let finalized = {
            let mut inner = self.inner.lock();
            inner
                .sessions
                .get_mut(&(call, participant))
                .is_some_and(|entry| {
                    if entry.id == id {
                        entry.handle = pending_task.take();
                        true
                    } else {
                        false
                    }
                })
        };
        if !finalized {
            // A disconnect/leave removed this generation after its registry
            // commit but before router/sink finalization. Never return an
            // answer for that stale owner, and remove every external trace that
            // was installed after the removal.
            if let Some(task) = pending_task {
                task.abort();
            }
            self.forwarder.detach_peer_sink(call, participant);
            self.router.remove_peer(call, participant);
            return Err(SfuMediaError::StaleSession);
        }

        Ok(SfuSessionStart {
            answer: SfuSessionAnswer {
                sdp: answer_sdp,
                local_addr,
                mids,
                session_generation: id,
                leg_generation,
                topology,
            },
            egress_generation,
            egress_tap,
        })
    }

    pub async fn add_remote_candidate(
        &self,
        call: CallId,
        participant: ParticipantId,
        candidate: String,
    ) -> Result<(), SfuMediaError> {
        let handle = {
            let inner = self.inner.lock();
            let task = inner
                .sessions
                .get(&(call, participant))
                .ok_or(SfuMediaError::NotFound)?;
            task.command.clone()
        };
        handle.add_remote_candidate(candidate).await
    }

    pub async fn add_remote_candidate_for_generation(
        &self,
        call: CallId,
        participant: ParticipantId,
        generation: u64,
        candidate: String,
    ) -> Result<(), SfuMediaError> {
        let handle = {
            let inner = self.inner.lock();
            let task = inner
                .sessions
                .get(&(call, participant))
                .ok_or(SfuMediaError::NotFound)?;
            if task.id != generation {
                return Err(SfuMediaError::StaleSession);
            }
            task.command.clone()
        };
        handle.add_remote_candidate(candidate).await
    }

    fn publisher_command(
        &self,
        call: CallId,
        publisher: ParticipantId,
        mid: &str,
    ) -> Result<(SfuMediaSessionHandle, String), SfuMediaError> {
        let mid = canonical_mid(mid.trim());
        if mid.is_empty() || mid.len() > 64 {
            return Err(SfuMediaError::InvalidSubscription(
                "publisher MID is empty or exceeds 64 bytes".into(),
            ));
        }
        let inner = self.inner.lock();
        let session = inner
            .sessions
            .get(&(call, publisher))
            .ok_or(SfuMediaError::NotFound)?;
        let has_published_mid = inner
            .publishers
            .get(&(call, publisher))
            .is_some_and(|tracks| tracks.iter().any(|track| track.mid == mid));
        if !has_published_mid {
            return Err(SfuMediaError::InvalidSubscription(
                "publisher MID is not in the active topology".into(),
            ));
        }
        Ok((session.command.clone(), mid))
    }

    /// Queue a bridged subscriber's PLI/FIR on the publisher's sole owner task.
    pub fn request_publisher_keyframe(
        &self,
        call: CallId,
        publisher: ParticipantId,
        mid: &str,
        rid: Option<Rid>,
        kind: KeyframeRequestKind,
    ) -> Result<(), SfuMediaError> {
        let (command, mid) = self.publisher_command(call, publisher, mid)?;
        command.request_keyframe(Mid::from(mid.as_str()), rid, kind)
    }

    /// Queue a bridged subscriber's aggregate REMB on the publisher owner task.
    pub fn request_publisher_remb(
        &self,
        call: CallId,
        publisher: ParticipantId,
        mid: &str,
        bitrate_bps: u64,
    ) -> Result<(), SfuMediaError> {
        let (command, mid) = self.publisher_command(call, publisher, mid)?;
        command.request_remb(Mid::from(mid.as_str()), bitrate_bps)
    }

    /// Validate and atomically install one subscriber's publisher-scoped route
    /// plan for the current topology revision.
    pub fn replace_subscriptions(
        &self,
        call: CallId,
        subscriber: ParticipantId,
        session_generation: u64,
        revision: u64,
        routes: Vec<SfuSubscription>,
    ) -> Result<usize, SfuMediaError> {
        if routes.len() > MAX_SUBSCRIPTIONS {
            return Err(SfuMediaError::InvalidSubscription(
                "more than 64 routes".into(),
            ));
        }
        let mut inner = self.inner.lock();
        let current_revision = inner.revisions.get(&call).copied().unwrap_or(0);
        if revision != current_revision {
            return Err(SfuMediaError::StaleTopology {
                expected: current_revision,
                actual: revision,
            });
        }
        let receive_slots = inner
            .sessions
            .get(&(call, subscriber))
            .ok_or(SfuMediaError::NotFound)?;
        if receive_slots.id != session_generation {
            return Err(SfuMediaError::StaleSession);
        }
        let receive_slots = receive_slots.receive_slots.clone();
        let mut normalized = Vec::with_capacity(routes.len());
        let mut seen_sources = HashSet::new();
        let mut seen_outputs = HashSet::new();
        for route in routes {
            if route.publisher == subscriber {
                return Err(SfuMediaError::InvalidSubscription(
                    "cannot subscribe to your own publisher leg".into(),
                ));
            }
            let pub_mid = canonical_mid(route.pub_mid.trim());
            let out_mid = canonical_mid(route.out_mid.trim());
            if pub_mid.is_empty() || out_mid.is_empty() || pub_mid.len() > 64 || out_mid.len() > 64
            {
                return Err(SfuMediaError::InvalidSubscription(
                    "MID is empty or exceeds 64 bytes".into(),
                ));
            }
            if !seen_sources.insert((route.publisher, pub_mid.clone())) {
                return Err(SfuMediaError::InvalidSubscription(
                    "duplicate publisher track".into(),
                ));
            }
            if !seen_outputs.insert(out_mid.clone()) {
                return Err(SfuMediaError::InvalidSubscription(
                    "one outbound MID cannot carry multiple publisher tracks".into(),
                ));
            }
            let source = inner
                .publishers
                .get(&(call, route.publisher))
                .and_then(|tracks| tracks.iter().find(|track| track.mid == pub_mid))
                .ok_or_else(|| {
                    SfuMediaError::InvalidSubscription(
                        "publisher or publisher MID is not in the current topology".into(),
                    )
                })?;
            let output = receive_slots
                .iter()
                .find(|slot| slot.mid == out_mid)
                .ok_or_else(|| {
                    SfuMediaError::InvalidSubscription(
                        "out_mid is not a negotiated receive-capable transceiver".into(),
                    )
                })?;
            if source.media_kind != output.media_kind {
                return Err(SfuMediaError::InvalidSubscription(
                    "publisher and outbound media kinds differ".into(),
                ));
            }
            normalized.push(SfuSubscription {
                publisher: route.publisher,
                pub_mid,
                out_mid,
            });
        }

        let Some(session) = inner.sessions.get_mut(&(call, subscriber)) else {
            return Err(SfuMediaError::NotFound);
        };
        session.subscriptions.clone_from(&normalized);
        let queued_keyframes = self
            .forwarder
            .replace_subscriptions(call, subscriber, &normalized);
        Ok(queued_keyframes)
    }

    /// Fold a server-authored publisher event into this node's topology.
    /// Returns a fresh snapshot only when it changed local state.
    pub fn observe_publisher(
        &self,
        call: CallId,
        publisher: SfuPublisherDescription,
        leg_generation: i64,
        active: bool,
    ) -> Option<SfuTopologySnapshot> {
        let mut inner = self.inner.lock();
        inner
            .update_publisher(
                call,
                publisher.participant,
                publisher.tracks,
                leg_generation,
                active,
            )
            .then(|| inner.topology(call))
    }

    #[must_use]
    pub fn topology(&self, call: CallId) -> SfuTopologySnapshot {
        self.inner.lock().topology(call)
    }

    #[must_use]
    pub fn publisher(
        &self,
        call: CallId,
        participant: ParticipantId,
    ) -> Option<SfuPublisherDescription> {
        self.inner
            .lock()
            .publishers
            .get(&(call, participant))
            .cloned()
            .map(|tracks| SfuPublisherDescription {
                participant,
                tracks,
            })
    }

    /// Publishers whose owner tasks live on this node, for idempotent topology
    /// replay when a new/reconnected call member joins.
    #[must_use]
    pub fn local_publishers(&self, call: CallId) -> Vec<(SfuPublisherDescription, i64)> {
        let inner = self.inner.lock();
        let publishers = sorted_publishers(inner.sessions.keys().filter_map(
            |(active_call, participant)| {
                (*active_call == call).then(|| {
                    inner
                        .publishers
                        .get(&(*active_call, *participant))
                        .cloned()
                        .map(|tracks| SfuPublisherDescription {
                            participant: *participant,
                            tracks,
                        })
                })?
            },
        ));
        publishers
            .into_iter()
            .filter_map(|publisher| {
                inner
                    .publisher_generations
                    .get(&(call, publisher.participant))
                    .copied()
                    .map(|generation| (publisher, generation))
            })
            .collect()
    }

    fn finished(&self, call: CallId, participant: ParticipantId, id: u64) {
        let ended = {
            let mut inner = self.inner.lock();
            if inner
                .sessions
                .get(&(call, participant))
                .is_some_and(|entry| entry.id == id)
            {
                let task = inner
                    .sessions
                    .remove(&(call, participant))
                    .expect("the current generation was just verified");
                inner.update_publisher(call, participant, Vec::new(), task.leg_generation, false);
                let call_empty = !inner
                    .sessions
                    .keys()
                    .any(|(active_call, _)| *active_call == call);
                let ended_egress_generation = call_empty
                    .then(|| {
                        inner
                            .egresses
                            .get(&call)
                            .is_some_and(|source| source.generation == task.egress_generation)
                            .then(|| inner.egresses.remove(&call))
                            .flatten()
                            .map(|source| source.generation)
                    })
                    .flatten();
                Some(SfuSessionEnded {
                    call_id: call,
                    participant,
                    session_generation: id,
                    leg_generation: task.leg_generation,
                    topology: inner.topology(call),
                    call_empty,
                    ended_egress_generation,
                })
            } else {
                None
            }
        };
        if let Some(ended) = ended {
            self.forwarder.detach_peer_sink(call, participant);
            self.router.remove_peer(call, participant);
            if self.lifecycle_tx.send(ended).is_err() {
                warn!(
                    %call,
                    %participant,
                    "SFU lifecycle worker is unavailable; session-end fan-out was not delivered"
                );
            }
        }
    }

    /// Cancel one participant media leg. Returns whether it existed and whether
    /// this removal left the call with no local media sessions.
    pub fn remove_session(&self, call: CallId, participant: ParticipantId) -> (bool, bool) {
        let removal = self.remove_session_with_topology(call, participant);
        (removal.removed, removal.ended_egress_generation.is_some())
    }

    /// Variant of [`Self::remove_session`] that returns the revisioned topology
    /// clients must renegotiate against.
    pub(crate) fn remove_session_with_topology(
        &self,
        call: CallId,
        participant: ParticipantId,
    ) -> SfuMediaRemoval {
        self.remove_session_matching_generation(call, participant, None)
    }

    pub(crate) fn remove_session_generation_with_topology(
        &self,
        call: CallId,
        participant: ParticipantId,
        leg_generation: i64,
    ) -> SfuMediaRemoval {
        self.remove_session_matching_generation(call, participant, Some(leg_generation))
    }

    fn remove_session_matching_generation(
        &self,
        call: CallId,
        participant: ParticipantId,
        expected_leg_generation: Option<i64>,
    ) -> SfuMediaRemoval {
        let (task, topology, ended_egress_generation) = {
            let mut inner = self.inner.lock();
            let task = inner
                .sessions
                .get(&(call, participant))
                .is_some_and(|task| {
                    expected_leg_generation
                        .map_or(true, |generation| task.leg_generation == generation)
                })
                .then(|| inner.sessions.remove(&(call, participant)))
                .flatten();
            if let Some(task) = &task {
                if inner.session_epochs.get(&(call, participant)) == Some(&task.id) {
                    inner.session_epochs.remove(&(call, participant));
                }
            }
            let topology = task.as_ref().map(|task| {
                inner.update_publisher(call, participant, Vec::new(), task.leg_generation, false);
                inner.topology(call)
            });
            let ended_egress_generation = task.as_ref().and_then(|task| {
                let call_empty = !inner
                    .sessions
                    .keys()
                    .any(|(active_call, _)| *active_call == call);
                (call_empty
                    && inner
                        .egresses
                        .get(&call)
                        .is_some_and(|source| source.generation == task.egress_generation))
                .then(|| inner.egresses.remove(&call))
                .flatten()
                .map(|source| source.generation)
            });
            (task, topology, ended_egress_generation)
        };
        let Some(task) = task else {
            return SfuMediaRemoval {
                removed: false,
                topology: None,
                ended_egress_generation: None,
            };
        };
        task.cancel.cancel();
        if let Some(handle) = task.handle {
            handle.abort();
        }
        self.forwarder.detach_peer_sink(call, participant);
        self.router.remove_peer(call, participant);
        SfuMediaRemoval {
            removed: true,
            topology,
            ended_egress_generation,
        }
    }

    /// Cancel every media leg for a call.
    pub fn remove_call(&self, call: CallId) -> usize {
        let participants: Vec<_> = self
            .inner
            .lock()
            .sessions
            .keys()
            .filter_map(|(active_call, participant)| (*active_call == call).then_some(*participant))
            .collect();
        let count = participants.len();
        for participant in participants {
            self.remove_session(call, participant);
        }
        let mut inner = self.inner.lock();
        inner
            .publishers
            .retain(|(active_call, _), _| *active_call != call);
        inner
            .publisher_generations
            .retain(|(active_call, _), _| *active_call != call);
        inner
            .session_epochs
            .retain(|(active_call, _), _| *active_call != call);
        inner.egresses.remove(&call);
        inner.egress_epochs.remove(&call);
        inner.revisions.remove(&call);
        count
    }

    /// Cancel all SFU media legs owned by a disconnected participant and return
    /// the affected calls for roster/orchestrator cleanup.
    pub fn remove_participant(&self, participant: ParticipantId) -> Vec<CallId> {
        self.remove_participant_with_topology(participant)
            .into_iter()
            .map(|(call, _, _)| call)
            .collect()
    }

    /// Remove all local legs for a participant and return each changed call's
    /// new topology snapshot.
    pub fn remove_participant_with_topology(
        &self,
        participant: ParticipantId,
    ) -> Vec<(CallId, SfuTopologySnapshot, Option<u64>)> {
        let calls: Vec<_> = self
            .inner
            .lock()
            .sessions
            .keys()
            .filter_map(|(call, active)| (*active == participant).then_some(*call))
            .collect();
        calls
            .into_iter()
            .filter_map(|call| {
                let removal = self.remove_session_with_topology(call, participant);
                removal
                    .topology
                    .map(|topology| (call, topology, removal.ended_egress_generation))
            })
            .collect()
    }

    #[must_use]
    pub fn session_count(&self, call: CallId) -> usize {
        self.inner
            .lock()
            .sessions
            .keys()
            .filter(|(active_call, _)| *active_call == call)
            .count()
    }

    pub(crate) fn current_egress_tap(&self, call: CallId) -> Option<(u64, CallEgressTap)> {
        self.inner
            .lock()
            .egresses
            .get(&call)
            .map(|source| (source.generation, source.bus.tap()))
    }

    /// Whether a spontaneous-exit event still owns the latest absent media
    /// incarnation for this participant.
    ///
    /// Callers that perform async external cleanup must hold
    /// [`Self::lock_lifecycle`] while checking and acting.
    pub(crate) fn is_current_ended_session(
        &self,
        call: CallId,
        participant: ParticipantId,
        generation: u64,
    ) -> bool {
        let inner = self.inner.lock();
        !inner.sessions.contains_key(&(call, participant))
            && inner.session_epochs.get(&(call, participant)) == Some(&generation)
    }

    /// A successful logical rejoin supersedes any queued exit for an older
    /// media incarnation, even before the replacement offer arrives.
    pub(crate) fn invalidate_ended_session(
        &self,
        call: CallId,
        participant: ParticipantId,
    ) -> Option<u64> {
        let mut inner = self.inner.lock();
        if !inner.sessions.contains_key(&(call, participant)) {
            inner.session_epochs.remove(&(call, participant));
        }
        (!inner.egresses.contains_key(&call)
            && !inner
                .sessions
                .keys()
                .any(|(active_call, _)| *active_call == call))
        .then(|| inner.egress_epochs.get(&call).copied())
        .flatten()
    }

    /// Consume a spontaneous-exit marker after every external surface has
    /// converged. Exact matching prevents delayed completion from consuming a
    /// replacement incarnation.
    pub(crate) fn complete_ended_session(
        &self,
        call: CallId,
        participant: ParticipantId,
        generation: u64,
    ) -> bool {
        let mut inner = self.inner.lock();
        let current = !inner.sessions.contains_key(&(call, participant))
            && inner.session_epochs.get(&(call, participant)) == Some(&generation);
        if current {
            inner.session_epochs.remove(&(call, participant));
        }
        current
    }

    pub(crate) fn with_current_egress_epoch<R>(
        &self,
        call: CallId,
        generation: u64,
        action: impl FnOnce() -> R,
    ) -> Option<R> {
        let inner = self.inner.lock();
        (inner
            .egresses
            .get(&call)
            .is_some_and(|source| source.generation == generation))
        .then(action)
    }

    /// Run `cleanup` while the media registry lock proves that `generation`
    /// remains the latest ended source epoch for `call`.
    ///
    /// The closure must not call back into `SfuMediaRegistry`. Keeping the guard
    /// across supervisor cleanup closes the check→cancel race with a reconnect
    /// commit, which takes this same lock first.
    pub(crate) fn with_ended_egress_epoch<R>(
        &self,
        call: CallId,
        generation: u64,
        cleanup: impl FnOnce() -> R,
    ) -> Option<R> {
        let inner = self.inner.lock();
        let ended = inner.egress_epochs.get(&call) == Some(&generation)
            && !inner.egresses.contains_key(&call)
            && !inner
                .sessions
                .keys()
                .any(|(active_call, _)| *active_call == call);
        ended.then(cleanup)
    }

    /// Run and consume final-source cleanup under the same registry lock.
    /// Unlike [`Self::with_ended_egress_epoch`], this is the authoritative
    /// teardown path and removes the tombstone after cleanup so completed calls
    /// do not accumulate epoch state.
    pub(crate) fn consume_ended_egress_epoch<R>(
        &self,
        call: CallId,
        generation: u64,
        cleanup: impl FnOnce() -> R,
    ) -> Option<R> {
        let mut inner = self.inner.lock();
        let ended = inner.egress_epochs.get(&call) == Some(&generation)
            && !inner.egresses.contains_key(&call)
            && !inner
                .sessions
                .keys()
                .any(|(active_call, _)| *active_call == call);
        if !ended {
            return None;
        }
        let result = cleanup();
        inner.egress_epochs.remove(&call);
        Some(result)
    }

    #[must_use]
    pub fn has_session(&self, call: CallId, participant: ParticipantId) -> bool {
        self.inner
            .lock()
            .sessions
            .contains_key(&(call, participant))
    }

    #[must_use]
    pub fn session_mids(&self, call: CallId, participant: ParticipantId) -> Vec<String> {
        self.inner
            .lock()
            .sessions
            .get(&(call, participant))
            .map_or_else(Vec::new, |session| session.mids.clone())
    }

    #[must_use]
    pub fn session_subscriptions(
        &self,
        call: CallId,
        participant: ParticipantId,
    ) -> Vec<SfuSubscription> {
        self.inner
            .lock()
            .sessions
            .get(&(call, participant))
            .map_or_else(Vec::new, |session| session.subscriptions.clone())
    }
}

/// Extract the bounded, unique media IDs declared in an SDP offer.
#[must_use]
pub fn extract_sdp_mids(sdp: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    sdp.lines()
        .filter_map(|line| line.trim().strip_prefix("a=mid:"))
        .map(str::trim)
        .filter(|mid| !mid.is_empty() && mid.len() <= 64)
        .map(canonical_mid)
        .filter(|mid| seen.insert(mid.clone()))
        .collect()
}
