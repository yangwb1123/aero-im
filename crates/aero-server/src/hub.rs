//! In-memory WebSocket connection registry.
//!
//! Maps `ParticipantId` to one or more open WebSocket sinks. A single user may have
//! multiple devices, so each participant holds a `Vec<WsSender>`.
//!
//! NATS is the source of truth for cross-instance delivery; the Hub fans-out within
//! the local process only. Each process subscribes to `im.room.*` (durable consumer
//! per instance) and pushes incoming envelopes through the Hub.
//!
//! ## Back-pressure (OOM guard)
//!
//! Each connection's outbound queue is a **bounded** `mpsc` channel. A slow or
//! stalled client therefore cannot make the broadcaster grow memory without
//! limit. When a queue is full, fan-out never blocks: it drops the message and
//! (per [`WsConfig::disconnect_on_full`]) disconnects the laggy client by
//! cancelling its [`CancellationToken`] and pruning it from the registry.
//!
//! ## O(1) unregister
//!
//! A `participant → {rooms, streams, calls}` reverse index means disconnecting a
//! client only touches that client's own memberships instead of scanning every
//! room / stream / call in the process.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use aero_common::metrics::{self, names};
use aero_common::{CallId, ParticipantId, RoomId};
use dashmap::DashMap;
use serde::Serialize;
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio_util::sync::CancellationToken;
use tracing::debug;
use ulid::Ulid;

use crate::config::WsConfig;

/// The frame enqueued once per loss episode in drop-only mode (ROADMAP 第三版
/// 方向一: slow-consumer resync). It tells the client "frames were dropped
/// while your queue was full — re-pull what you missed via the REST `?since=`
/// path". One per episode, not per dropped frame, so a stalled client costs
/// one extra frame rather than a flood.
const RESYNC_FRAME: &str = r#"{"type":"resync"}"#;

/// A handle to one connection's bounded outbound queue plus a kill-switch.
///
/// Cloneable: the Hub keeps a clone to fan-out, while the connection task keeps
/// one for its own replies. `close` lets the broadcaster (or any holder) tear
/// down a misbehaving connection without coordinating through the socket task.
#[derive(Clone)]
pub struct WsSender {
    tx: mpsc::Sender<axum::extract::ws::Message>,
    close: CancellationToken,
    /// True while this connection is inside a loss episode: at least one frame
    /// was dropped on a full queue (drop-only mode) and the client has not yet
    /// been told. Shared across clones (the Hub's fan-out copy sets it; any
    /// copy observing recovered capacity clears it by enqueueing one
    /// [`RESYNC_FRAME`]).
    lossy: Arc<AtomicBool>,
}

impl WsSender {
    /// Wrap a bounded sender + its cancellation token.
    #[must_use]
    pub fn new(
        tx: mpsc::Sender<axum::extract::ws::Message>,
        close: CancellationToken,
    ) -> Self {
        Self { tx, close, lossy: Arc::new(AtomicBool::new(false)) }
    }

    /// Non-blocking enqueue. Never awaits, so the broadcaster can't be stalled
    /// by a slow consumer.
    fn try_send(
        &self,
        msg: axum::extract::ws::Message,
    ) -> Result<(), TrySendError<axum::extract::ws::Message>> {
        self.tx.try_send(msg)
    }

    /// Signal this connection to shut down (laggy / evicted client).
    pub fn close(&self) {
        self.close.cancel();
    }

    /// True if both handles feed the same underlying channel (identity for
    /// unregister, since `Sender` has no pointer identity).
    #[must_use]
    pub fn same_channel(&self, other: &WsSender) -> bool {
        self.tx.same_channel(&other.tx)
    }
}

/// Reverse index of everything a single participant is subscribed to, so cleanup
/// on disconnect is proportional to *that participant's* footprint, not the
/// global topology.
#[derive(Default)]
struct ParticipantSubs {
    rooms: HashSet<RoomId>,
    streams: HashSet<Ulid>,
    calls: HashSet<CallId>,
}

impl ParticipantSubs {
    fn is_empty(&self) -> bool {
        self.rooms.is_empty() && self.streams.is_empty() && self.calls.is_empty()
    }
}

#[derive(Default)]
pub struct Hub {
    /// participant → connections
    conns: DashMap<ParticipantId, Vec<WsSender>>,
    /// room → joined participants (for presence broadcasts within this process)
    rooms: DashMap<RoomId, Vec<ParticipantId>>,
    /// live stream → watching participants (for danmaku/gift/viewer fan-out)
    stream_watchers: DashMap<Ulid, Vec<ParticipantId>>,
    /// group call → joined participants (P6 mesh roster)
    call_rosters: DashMap<CallId, Vec<ParticipantId>>,
    /// participant → {rooms, streams, calls} reverse index (O(1)-ish unregister)
    subs: DashMap<ParticipantId, ParticipantSubs>,
    /// When a connection's bounded queue is full, also disconnect it (vs. only
    /// dropping the message).
    disconnect_on_full: bool,
    /// Metrics sink for the connection gauge. `None` (the default / production
    /// path) emits to the process-global registry via the free functions; tests
    /// inject a fresh [`Registry`] so the gauge can be asserted in isolation,
    /// immune to other parallel tests mutating the global gauge.
    metrics: Option<Arc<aero_common::metrics::Registry>>,
}

impl Hub {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Self::with_ws_config(WsConfig::default())
    }

    /// Construct with explicit WS back-pressure policy.
    #[must_use]
    pub fn with_ws_config(cfg: WsConfig) -> Arc<Self> {
        Arc::new(Self {
            disconnect_on_full: cfg.disconnect_on_full,
            ..Self::default()
        })
    }

    /// Test-only: build a Hub whose connection gauge writes to `registry`
    /// (instead of the process-global one), so gauge assertions are isolated from
    /// other parallel tests sharing the global registry.
    #[cfg(test)]
    fn with_metrics_registry(
        cfg: WsConfig,
        registry: Arc<aero_common::metrics::Registry>,
    ) -> Arc<Self> {
        Arc::new(Self {
            disconnect_on_full: cfg.disconnect_on_full,
            metrics: Some(registry),
            ..Self::default()
        })
    }

    /// Increment the connection gauge on whichever registry is wired.
    fn gauge_inc(&self) {
        match &self.metrics {
            Some(r) => r.inc_gauge(names::WS_CONNECTIONS),
            None => metrics::inc_gauge(names::WS_CONNECTIONS),
        }
    }

    /// Decrement the connection gauge on whichever registry is wired.
    fn gauge_dec(&self) {
        match &self.metrics {
            Some(r) => r.dec_gauge(names::WS_CONNECTIONS),
            None => metrics::dec_gauge(names::WS_CONNECTIONS),
        }
    }

    pub fn register(&self, pid: ParticipantId, tx: WsSender) {
        self.conns.entry(pid).or_default().push(tx);
        // Connection-count gauge (ROADMAP 方向四): one inc per *connection*, so a
        // multi-device user contributes once per open socket. Paired with the
        // dec in `unregister` / laggy-prune so the gauge tracks live sockets.
        self.gauge_inc();
        debug!(%pid, "ws registered");
    }

    pub fn unregister(&self, pid: ParticipantId, tx: &WsSender) {
        // Did this close the participant's LAST open socket? Only then may we
        // tear down their subscriptions. Fan-out routes by participant id (every
        // socket of `pid` receives), so purging the room/stream/call forward
        // maps while another socket is still open would silently stop delivery
        // to that socket — e.g. closing the phone muting the still-open desktop
        // (ROADMAP 方向一). A missing `conns` entry means no sockets remain, so
        // teardown is also correct (and idempotent) in that case.
        let last_socket_closed = if let Some(mut entry) = self.conns.get_mut(&pid) {
            let before = entry.len();
            entry.retain(|s| !s.same_channel(tx));
            // Decrement once per connection actually removed (idempotent if the
            // socket was already pruned by the laggy-client path in `fan_out_raw`).
            for _ in 0..(before - entry.len()) {
                self.gauge_dec();
            }
            let empty = entry.is_empty();
            if empty {
                drop(entry);
                self.conns.remove(&pid);
            }
            empty
        } else {
            true
        };

        // Purge this participant from every room/stream/call it joined — but only
        // once the last socket is gone. We visit only `pid`'s *own* subscriptions
        // via the reverse index, so cost is O(this participant's footprint)
        // instead of O(total rooms + streams + calls).
        if last_socket_closed {
            if let Some((_, subs)) = self.subs.remove(&pid) {
                for room in &subs.rooms {
                    remove_from_forward(&self.rooms, room, pid);
                }
                for stream in &subs.streams {
                    remove_from_forward(&self.stream_watchers, stream, pid);
                }
                for call in &subs.calls {
                    remove_from_forward(&self.call_rosters, call, pid);
                }
            }
        }
        debug!(%pid, "ws unregistered");
    }

    pub fn join_room(&self, room: RoomId, pid: ParticipantId) {
        let mut entry = self.rooms.entry(room).or_default();
        if !entry.contains(&pid) {
            entry.push(pid);
        }
        self.subs.entry(pid).or_default().rooms.insert(room);
    }

    #[must_use]
    pub fn room_members_online(&self, room: RoomId) -> Vec<ParticipantId> {
        self.rooms.get(&room).map(|e| e.clone()).unwrap_or_default()
    }

    /// The rooms a participant has joined on THIS node (from the reverse index).
    /// Used to fan out cluster-wide presence heartbeats/leaves to Redis on ping
    /// and disconnect. Empty when the participant has joined no room channels.
    #[must_use]
    pub fn rooms_of(&self, pid: ParticipantId) -> Vec<RoomId> {
        self.subs.get(&pid).map(|s| s.rooms.iter().copied().collect()).unwrap_or_default()
    }

    // ---- live-stream watcher tracking (P4 互动直播) ----

    /// Mark a participant as watching a stream. Idempotent per participant
    /// (one entry even across multiple devices), so the viewer count reflects
    /// distinct people.
    pub fn watch_stream(&self, stream_id: Ulid, pid: ParticipantId) {
        let mut entry = self.stream_watchers.entry(stream_id).or_default();
        if !entry.contains(&pid) {
            entry.push(pid);
        }
        self.subs.entry(pid).or_default().streams.insert(stream_id);
    }

    /// Stop watching a stream; drops the watcher set entirely once empty.
    pub fn unwatch_stream(&self, stream_id: Ulid, pid: ParticipantId) {
        remove_from_forward(&self.stream_watchers, &stream_id, pid);
        if let Some(mut subs) = self.subs.get_mut(&pid) {
            subs.streams.remove(&stream_id);
            let empty = subs.is_empty();
            drop(subs);
            if empty {
                self.subs.remove_if(&pid, |_, v| v.is_empty());
            }
        }
    }

    /// Participants currently watching a stream (this process only).
    #[must_use]
    pub fn stream_watchers(&self, stream_id: Ulid) -> Vec<ParticipantId> {
        self.stream_watchers.get(&stream_id).map(|e| e.clone()).unwrap_or_default()
    }

    /// Distinct viewer count for a stream (this process only).
    #[must_use]
    pub fn stream_viewer_count(&self, stream_id: Ulid) -> u32 {
        self.stream_watchers.get(&stream_id).map_or(0, |e| e.len() as u32)
    }

    /// Send a JSON-serializable payload to every connection of every recipient.
    pub fn fan_out<T: Serialize>(&self, recipients: &[ParticipantId], payload: &T) {
        let json = match serde_json::to_string(payload) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = ?e, "hub fan_out serialize");
                return;
            }
        };
        self.fan_out_raw(recipients, &json);
    }

    /// Send a pre-serialized text frame to every connection of every recipient.
    /// Useful when the JSON has already been built upstream.
    ///
    /// Enqueues without ever blocking. If a connection's bounded queue is full
    /// the message is dropped (slow-consumer policy) and, when configured, the
    /// connection is disconnected and pruned. Closed connections are pruned too.
    pub fn fan_out_raw(&self, recipients: &[ParticipantId], text: &str) {
        for pid in recipients {
            // `get_mut` so we can prune dead/laggy senders in place. The write
            // guard is scoped to this one participant's entry.
            let Some(mut senders) = self.conns.get_mut(pid) else { continue };
            let mut drop_idx: Vec<usize> = Vec::new();
            for (i, tx) in senders.iter().enumerate() {
                match tx.try_send(axum::extract::ws::Message::Text(text.to_owned())) {
                    Ok(()) => {
                        // Slow-consumer resync (ROADMAP 第三版 方向一): the queue
                        // has capacity again — if frames were dropped while it was
                        // full (drop-only mode), close the loss episode by
                        // enqueueing exactly ONE resync marker so the client
                        // re-pulls what it missed via the REST `?since=` path.
                        if tx.lossy.swap(false, Ordering::Relaxed) {
                            match tx
                                .try_send(axum::extract::ws::Message::Text(RESYNC_FRAME.to_owned()))
                            {
                                Ok(()) => {
                                    debug!(%pid, "ws loss episode ended — resync frame enqueued");
                                }
                                Err(TrySendError::Full(_)) => {
                                    // The delivered frame consumed the last slot;
                                    // the episode stays open and the marker is
                                    // retried on the next successful delivery.
                                    tx.lossy.store(true, Ordering::Relaxed);
                                }
                                Err(TrySendError::Closed(_)) => {
                                    // Receiver vanished between the two sends;
                                    // reap below like any closed handle.
                                    drop_idx.push(i);
                                }
                            }
                        }
                    }
                    Err(TrySendError::Full(_)) => {
                        // Slow consumer: drop this frame. Optionally evict.
                        if self.disconnect_on_full {
                            // No lossy bookkeeping here: disconnect mode already
                            // self-heals — the evicted client reconnects with its
                            // `?since=` cursor and the WS backfill replays what
                            // it missed.
                            tx.close();
                            drop_idx.push(i);
                            debug!(%pid, "ws send queue full — disconnecting laggy client");
                        } else {
                            // Drop-only mode: open (or extend) a loss episode so
                            // the client is told to resync once capacity returns.
                            tx.lossy.store(true, Ordering::Relaxed);
                            debug!(%pid, "ws send queue full — dropping frame (episode open)");
                        }
                    }
                    Err(TrySendError::Closed(_)) => {
                        // Receiver gone; reap the stale handle.
                        drop_idx.push(i);
                    }
                }
            }
            if !drop_idx.is_empty() {
                // Each pruned sender is a live socket going away (closed receiver
                // or evicted laggy client) without a matching `unregister`, so keep
                // the connection gauge honest by decrementing here too.
                for _ in 0..drop_idx.len() {
                    self.gauge_dec();
                }
                // Remove highest indices first so earlier ones stay valid.
                for i in drop_idx.into_iter().rev() {
                    senders.swap_remove(i);
                }
                let empty = senders.is_empty();
                drop(senders);
                if empty {
                    self.conns.remove(pid);
                }
            }
        }
    }

    // ---- group-call roster (P6 mesh) ----

    /// Add a participant to a call roster, returning the members who were
    /// *already* present (whom the joiner must establish a connection to).
    pub fn call_join(&self, call_id: CallId, pid: ParticipantId) -> Vec<ParticipantId> {
        let mut entry = self.call_rosters.entry(call_id).or_default();
        let existing: Vec<ParticipantId> = entry.iter().copied().filter(|p| *p != pid).collect();
        if !entry.contains(&pid) {
            entry.push(pid);
        }
        drop(entry);
        self.subs.entry(pid).or_default().calls.insert(call_id);
        existing
    }

    /// Remove a participant from a call roster; drops the roster once empty.
    pub fn call_leave(&self, call_id: CallId, pid: ParticipantId) {
        remove_from_forward(&self.call_rosters, &call_id, pid);
        if let Some(mut subs) = self.subs.get_mut(&pid) {
            subs.calls.remove(&call_id);
            let empty = subs.is_empty();
            drop(subs);
            if empty {
                self.subs.remove_if(&pid, |_, v| v.is_empty());
            }
        }
    }

    /// Current members of a call (this process only).
    #[must_use]
    pub fn call_members(&self, call_id: CallId) -> Vec<ParticipantId> {
        self.call_rosters.get(&call_id).map(|e| e.clone()).unwrap_or_default()
    }
}

/// Remove `pid` from one forward-map entry, dropping the entry once empty. Shared
/// by `unregister` (via the reverse index) and the explicit leave/unwatch paths.
fn remove_from_forward<K: std::hash::Hash + Eq>(
    map: &DashMap<K, Vec<ParticipantId>>,
    key: &K,
    pid: ParticipantId,
) {
    if let Some(mut entry) = map.get_mut(key) {
        entry.retain(|p| *p != pid);
        if entry.is_empty() {
            drop(entry);
            map.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::CallId;

    /// Build a registered connection with a bounded queue of `cap` slots.
    /// Returns the sender handle (as the Hub stores it) and the receiver so the
    /// test can drain / leave it stalled.
    fn make_conn(
        cap: usize,
    ) -> (WsSender, mpsc::Receiver<axum::extract::ws::Message>, CancellationToken) {
        let (tx, rx) = mpsc::channel(cap);
        let close = CancellationToken::new();
        (WsSender::new(tx, close.clone()), rx, close)
    }

    /// Read the unlabeled `aero_ws_connections` gauge value from a *specific*
    /// registry's exposition. The WS-gauge tests inject a fresh registry so they
    /// can assert absolute values without racing the process-global gauge that
    /// every other connection test mutates in parallel.
    fn ws_gauge_in(registry: &aero_common::metrics::Registry) -> f64 {
        let out = registry.render_prometheus();
        for line in out.lines() {
            if let Some(rest) = line.strip_prefix(&format!("{} ", names::WS_CONNECTIONS)) {
                return rest.trim().parse().unwrap_or(0.0);
            }
        }
        0.0
    }

    #[test]
    fn call_roster_join_returns_existing_and_dedups() {
        let hub = Hub::default();
        let call = CallId::new();
        let a = ParticipantId::new();
        let b = ParticipantId::new();

        // First joiner sees nobody.
        assert!(hub.call_join(call, a).is_empty());
        // Second joiner sees the first.
        assert_eq!(hub.call_join(call, b), vec![a]);
        // Re-joining is idempotent and still reports the others.
        assert_eq!(hub.call_join(call, a), vec![b]);
        assert_eq!(hub.call_members(call).len(), 2);

        hub.call_leave(call, a);
        assert_eq!(hub.call_members(call), vec![b]);
        // Last leaver drops the roster entirely.
        hub.call_leave(call, b);
        assert!(hub.call_members(call).is_empty());
    }

    #[test]
    fn unregister_one_of_two_sockets_keeps_subscriptions() {
        // Multi-device: closing one socket must NOT tear down the participant's
        // room subscription while another socket is still open (ROADMAP 方向一).
        let hub = Hub::default();
        let pid = ParticipantId::new();
        let room = RoomId::new();
        let (tx1, _rx1, _c1) = make_conn(8);
        let (tx2, mut rx2, _c2) = make_conn(8);
        hub.register(pid, tx1.clone());
        hub.register(pid, tx2);
        hub.join_room(room, pid);
        assert!(hub.rooms.get(&room).is_some_and(|e| e.contains(&pid)));

        // Close the FIRST socket (the "phone").
        hub.unregister(pid, &tx1);
        assert_eq!(hub.conns.get(&pid).map(|e| e.len()), Some(1), "one socket remains");
        assert!(
            hub.rooms.get(&room).is_some_and(|e| e.contains(&pid)),
            "the still-open desktop socket must keep its room subscription"
        );
        // And the remaining socket still receives room fan-out.
        hub.fan_out_raw(&[pid], "still-here");
        assert_eq!(drain_text(&mut rx2), vec!["still-here".to_owned()]);

        // Close the LAST socket → subscription is torn down.
        let tx2_again = hub.conns.get(&pid).unwrap()[0].clone();
        hub.unregister(pid, &tx2_again);
        assert!(hub.conns.get(&pid).is_none(), "no sockets left");
        assert!(
            !hub.rooms.get(&room).is_some_and(|e| e.contains(&pid)),
            "last socket closing tears down the room subscription"
        );
    }

    #[test]
    fn fan_out_delivers_to_registered_connection() {
        let hub = Hub::default();
        let pid = ParticipantId::new();
        let (tx, mut rx, _close) = make_conn(8);
        hub.register(pid, tx);

        hub.fan_out_raw(&[pid], "hello");
        match rx.try_recv() {
            Ok(axum::extract::ws::Message::Text(t)) => assert_eq!(t, "hello"),
            other => panic!("expected text frame, got {other:?}"),
        }
    }

    #[test]
    fn full_queue_drops_frame_when_disconnect_disabled() {
        let cfg = WsConfig { send_queue_capacity: 2, disconnect_on_full: false };
        let hub = Hub::with_ws_config(cfg);
        let pid = ParticipantId::new();
        let (tx, _rx, close) = make_conn(2); // never drained → fills up

        hub.register(pid, tx);
        // Capacity 2: first two land, the rest are dropped — never blocks.
        hub.fan_out_raw(&[pid], "a");
        hub.fan_out_raw(&[pid], "b");
        hub.fan_out_raw(&[pid], "c"); // dropped
        hub.fan_out_raw(&[pid], "d"); // dropped

        // Drop-only policy: the connection is NOT torn down or pruned.
        assert!(!close.is_cancelled());
        assert_eq!(hub.conns.get(&pid).map(|e| e.len()), Some(1));
    }

    /// Drain every immediately-available frame as text.
    fn drain_text(rx: &mut mpsc::Receiver<axum::extract::ws::Message>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(axum::extract::ws::Message::Text(t)) = rx.try_recv() {
            out.push(t);
        }
        out
    }

    #[test]
    fn loss_episode_emits_exactly_one_resync_once_capacity_returns() {
        let cfg = WsConfig { send_queue_capacity: 2, disconnect_on_full: false };
        let hub = Hub::with_ws_config(cfg);
        let pid = ParticipantId::new();
        let (tx, mut rx, close) = make_conn(2);
        hub.register(pid, tx);

        // Fill the queue, then drop two frames → one loss episode opens.
        hub.fan_out_raw(&[pid], "a");
        hub.fan_out_raw(&[pid], "b");
        hub.fan_out_raw(&[pid], "dropped-1");
        hub.fan_out_raw(&[pid], "dropped-2");
        assert_eq!(drain_text(&mut rx), vec!["a", "b"], "no resync while still lossy");

        // Capacity is back: the next delivered frame closes the episode with
        // exactly ONE resync marker (not one per dropped frame).
        hub.fan_out_raw(&[pid], "c");
        assert_eq!(
            drain_text(&mut rx),
            vec!["c".to_owned(), super::RESYNC_FRAME.to_owned()],
            "one resync after the episode, ordered after the frame that closed it"
        );

        // Healthy again: subsequent deliveries carry no further resync.
        hub.fan_out_raw(&[pid], "d");
        assert_eq!(drain_text(&mut rx), vec!["d"]);

        // A NEW episode re-arms the marker.
        hub.fan_out_raw(&[pid], "e");
        hub.fan_out_raw(&[pid], "f");
        hub.fan_out_raw(&[pid], "dropped-3");
        assert_eq!(drain_text(&mut rx), vec!["e", "f"]);
        hub.fan_out_raw(&[pid], "g");
        assert_eq!(
            drain_text(&mut rx),
            vec!["g".to_owned(), super::RESYNC_FRAME.to_owned()],
            "each loss episode ends with its own single resync"
        );
        // Drop-only mode never tears the connection down.
        assert!(!close.is_cancelled());
    }

    #[test]
    fn resync_retries_when_marker_finds_no_capacity() {
        // When the frame that closes the episode consumes the LAST slot, the
        // marker can't be enqueued yet — the episode must stay open and the
        // marker land after a later delivery instead of being silently lost.
        let cfg = WsConfig { send_queue_capacity: 2, disconnect_on_full: false };
        let hub = Hub::with_ws_config(cfg);
        let pid = ParticipantId::new();
        let (tx, mut rx, _close) = make_conn(2);
        hub.register(pid, tx);

        hub.fan_out_raw(&[pid], "a");
        hub.fan_out_raw(&[pid], "b"); // queue full: [a, b]
        hub.fan_out_raw(&[pid], "dropped"); // opens the episode
        // Drain only ONE frame, leaving exactly one free slot.
        assert!(matches!(rx.try_recv(), Ok(axum::extract::ws::Message::Text(t)) if t == "a"));

        // `c` takes the last slot; the marker finds the queue full → deferred.
        hub.fan_out_raw(&[pid], "c");
        assert_eq!(drain_text(&mut rx), vec!["b", "c"], "marker deferred, not lost");

        // Next delivery has room behind it → the deferred marker lands (once).
        hub.fan_out_raw(&[pid], "d");
        assert_eq!(drain_text(&mut rx), vec!["d".to_owned(), super::RESYNC_FRAME.to_owned()]);
    }

    #[test]
    fn disconnect_mode_does_not_emit_resync() {
        // disconnect_on_full self-heals via the reconnect `?since=` backfill, so
        // no resync marker must ever be produced there (the connection is gone).
        let cfg = WsConfig { send_queue_capacity: 1, disconnect_on_full: true };
        let hub = Hub::with_ws_config(cfg);
        let pid = ParticipantId::new();
        let (tx, mut rx, close) = make_conn(1);
        hub.register(pid, tx);

        hub.fan_out_raw(&[pid], "a");
        hub.fan_out_raw(&[pid], "overflow"); // full → evict
        assert!(close.is_cancelled());
        assert_eq!(drain_text(&mut rx), vec!["a"], "no resync marker in disconnect mode");
    }

    #[test]
    fn full_queue_disconnects_and_prunes_laggy_client() {
        let cfg = WsConfig { send_queue_capacity: 1, disconnect_on_full: true };
        let hub = Hub::with_ws_config(cfg);
        let pid = ParticipantId::new();
        let (tx, _rx, close) = make_conn(1); // stalled consumer

        hub.register(pid, tx);
        hub.fan_out_raw(&[pid], "fills the single slot");
        // Next frame finds the queue full → disconnect + prune.
        hub.fan_out_raw(&[pid], "overflow");

        assert!(close.is_cancelled(), "laggy client should be signalled to close");
        assert!(hub.conns.get(&pid).is_none(), "laggy connection should be pruned");
    }

    #[test]
    fn closed_receiver_is_reaped_on_fan_out() {
        let hub = Hub::default();
        let pid = ParticipantId::new();
        let (tx, rx, _close) = make_conn(4);
        hub.register(pid, tx);
        drop(rx); // receiver gone → channel closed

        hub.fan_out_raw(&[pid], "anything");
        assert!(hub.conns.get(&pid).is_none(), "closed connection should be reaped");
    }

    #[test]
    fn unregister_removes_only_the_matching_connection_then_purges_on_membership() {
        // Multiple devices share one participant entry. `unregister` removes the
        // specific connection by channel identity; the *other* connection's
        // handle is left intact.
        let hub = Hub::default();
        let pid = ParticipantId::new();
        let (tx1, _rx1, _c1) = make_conn(4);
        let (tx2, _rx2, _c2) = make_conn(4);
        hub.register(pid, tx1.clone());
        hub.register(pid, tx2.clone());
        assert_eq!(hub.conns.get(&pid).map(|e| e.len()), Some(2));

        hub.unregister(pid, &tx1);
        // Only tx1 was removed; tx2 remains registered.
        assert_eq!(hub.conns.get(&pid).map(|e| e.len()), Some(1));

        hub.unregister(pid, &tx2);
        // Last connection gone → participant key dropped entirely.
        assert!(hub.conns.get(&pid).is_none());
    }

    #[test]
    fn unregister_purges_participant_from_memberships_and_reverse_index() {
        // Behaviour parity with the original global scan: a disconnect removes
        // the participant from every room/stream/call it had joined.
        let hub = Hub::default();
        let pid = ParticipantId::new();
        let room = RoomId::new();
        let (tx, _rx, _c) = make_conn(4);
        hub.register(pid, tx.clone());
        hub.join_room(room, pid);
        assert_eq!(hub.room_members_online(room), vec![pid]);
        assert!(hub.subs.contains_key(&pid));

        hub.unregister(pid, &tx);
        assert!(hub.room_members_online(room).is_empty());
        assert!(!hub.subs.contains_key(&pid));
    }

    #[test]
    fn unregister_cleans_rooms_streams_and_calls_via_reverse_index() {
        let hub = Hub::default();
        let pid = ParticipantId::new();
        let other = ParticipantId::new();
        let room = RoomId::new();
        let stream = Ulid::new();
        let call = CallId::new();

        let (tx, _rx, _c) = make_conn(4);
        hub.register(pid, tx.clone());
        // A second participant shares each forward set, so entries must survive.
        let (tx_o, _rx_o, _c_o) = make_conn(4);
        hub.register(other, tx_o);

        hub.join_room(room, pid);
        hub.join_room(room, other);
        hub.watch_stream(stream, pid);
        hub.watch_stream(stream, other);
        hub.call_join(call, pid);
        hub.call_join(call, other);

        hub.unregister(pid, &tx);

        // pid is gone from every forward set...
        assert_eq!(hub.room_members_online(room), vec![other]);
        assert_eq!(hub.stream_watchers(stream), vec![other]);
        assert_eq!(hub.call_members(call), vec![other]);
        // ...and its reverse index entry is removed entirely.
        assert!(!hub.subs.contains_key(&pid));
        // The sharing participant's subscriptions are untouched.
        assert!(hub.subs.contains_key(&other));
    }

    #[test]
    fn last_leaver_drops_forward_entries_entirely() {
        let hub = Hub::default();
        let pid = ParticipantId::new();
        let room = RoomId::new();
        let stream = Ulid::new();
        let call = CallId::new();
        let (tx, _rx, _c) = make_conn(4);
        hub.register(pid, tx.clone());

        hub.join_room(room, pid);
        hub.watch_stream(stream, pid);
        hub.call_join(call, pid);

        hub.unregister(pid, &tx);

        // With nobody left, the forward maps drop the keys entirely (no empty Vecs).
        assert!(hub.rooms.get(&room).is_none());
        assert!(hub.stream_watchers.get(&stream).is_none());
        assert!(hub.call_rosters.get(&call).is_none());
    }

    #[test]
    fn explicit_unwatch_and_leave_update_reverse_index() {
        let hub = Hub::default();
        let pid = ParticipantId::new();
        let stream = Ulid::new();
        let call = CallId::new();
        let (tx, _rx, _c) = make_conn(4);
        hub.register(pid, tx);

        hub.watch_stream(stream, pid);
        hub.call_join(call, pid);
        assert!(hub.subs.contains_key(&pid));

        hub.unwatch_stream(stream, pid);
        // Still has the call subscription.
        assert!(hub.subs.contains_key(&pid));
        hub.call_leave(call, pid);
        // Now empty → reverse-index entry pruned.
        assert!(!hub.subs.contains_key(&pid));
        assert!(hub.stream_watchers(stream).is_empty());
        assert!(hub.call_members(call).is_empty());
    }

    #[test]
    fn ws_gauge_tracks_register_and_unregister() {
        // Fresh registry ⇒ isolated, absolute assertions (starts at 0).
        let reg = Arc::new(aero_common::metrics::Registry::new());
        let hub = Hub::with_metrics_registry(WsConfig::default(), reg.clone());
        let pid = ParticipantId::new();
        let (tx, _rx, _c) = make_conn(4);

        assert!((ws_gauge_in(&reg) - 0.0).abs() < 1e-9, "starts at 0");
        hub.register(pid, tx.clone());
        assert!((ws_gauge_in(&reg) - 1.0).abs() < 1e-9, "register ⇒ +1");

        hub.unregister(pid, &tx);
        assert!((ws_gauge_in(&reg) - 0.0).abs() < 1e-9, "unregister ⇒ back to 0");
    }

    #[test]
    fn ws_gauge_counts_each_connection_for_multi_device() {
        // Two devices for one participant ⇒ gauge counts *connections* (2),
        // matching one inc per register and one dec per unregister.
        let reg = Arc::new(aero_common::metrics::Registry::new());
        let hub = Hub::with_metrics_registry(WsConfig::default(), reg.clone());
        let pid = ParticipantId::new();
        let (tx1, _rx1, _c1) = make_conn(4);
        let (tx2, _rx2, _c2) = make_conn(4);

        hub.register(pid, tx1.clone());
        hub.register(pid, tx2.clone());
        assert!((ws_gauge_in(&reg) - 2.0).abs() < 1e-9, "two conns ⇒ 2");

        hub.unregister(pid, &tx1);
        assert!((ws_gauge_in(&reg) - 1.0).abs() < 1e-9, "one left ⇒ 1");
        hub.unregister(pid, &tx2);
        assert!((ws_gauge_in(&reg) - 0.0).abs() < 1e-9, "both gone ⇒ 0");
    }

    #[test]
    fn ws_gauge_idempotent_when_unregistering_already_pruned_conn() {
        // A laggy client pruned by fan_out_raw decrements the gauge once; the
        // socket task's later `unregister` must NOT double-decrement.
        let reg = Arc::new(aero_common::metrics::Registry::new());
        let cfg = WsConfig { send_queue_capacity: 1, disconnect_on_full: true };
        let hub = Hub::with_metrics_registry(cfg, reg.clone());
        let pid = ParticipantId::new();
        let (tx, _rx, _close) = make_conn(1); // stalled consumer

        hub.register(pid, tx.clone());
        assert!((ws_gauge_in(&reg) - 1.0).abs() < 1e-9);

        hub.fan_out_raw(&[pid], "fills the single slot");
        hub.fan_out_raw(&[pid], "overflow"); // full → evict + prune (gauge -1)
        assert!((ws_gauge_in(&reg) - 0.0).abs() < 1e-9, "eviction ⇒ back to 0");

        // The connection is already gone; unregister finds nothing to remove and
        // therefore does not decrement again (no negative gauge).
        hub.unregister(pid, &tx);
        assert!(
            (ws_gauge_in(&reg) - 0.0).abs() < 1e-9,
            "unregister of an already-pruned conn must not double-decrement"
        );
    }
}
