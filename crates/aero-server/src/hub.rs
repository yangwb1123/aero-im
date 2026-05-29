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
use std::sync::Arc;

use aero_common::{CallId, ParticipantId, RoomId};
use dashmap::DashMap;
use serde::Serialize;
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio_util::sync::CancellationToken;
use tracing::debug;
use ulid::Ulid;

use crate::config::WsConfig;

/// A handle to one connection's bounded outbound queue plus a kill-switch.
///
/// Cloneable: the Hub keeps a clone to fan-out, while the connection task keeps
/// one for its own replies. `close` lets the broadcaster (or any holder) tear
/// down a misbehaving connection without coordinating through the socket task.
#[derive(Clone)]
pub struct WsSender {
    tx: mpsc::Sender<axum::extract::ws::Message>,
    close: CancellationToken,
}

impl WsSender {
    /// Wrap a bounded sender + its cancellation token.
    #[must_use]
    pub fn new(
        tx: mpsc::Sender<axum::extract::ws::Message>,
        close: CancellationToken,
    ) -> Self {
        Self { tx, close }
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

    pub fn register(&self, pid: ParticipantId, tx: WsSender) {
        self.conns.entry(pid).or_default().push(tx);
        debug!(%pid, "ws registered");
    }

    pub fn unregister(&self, pid: ParticipantId, tx: &WsSender) {
        if let Some(mut entry) = self.conns.get_mut(&pid) {
            entry.retain(|s| !s.same_channel(tx));
            if entry.is_empty() {
                drop(entry);
                self.conns.remove(&pid);
            }
        }
        // Purge this participant from every room/stream/call it joined. Behaviour
        // is identical to the previous global scan (which removed `pid` from all
        // maps on each unregister) — but now we visit only `pid`'s *own*
        // subscriptions via the reverse index, so cost is O(this participant's
        // footprint) instead of O(total rooms + streams + calls).
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
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        // Slow consumer: drop this frame. Optionally evict.
                        if self.disconnect_on_full {
                            tx.close();
                            drop_idx.push(i);
                            debug!(%pid, "ws send queue full — disconnecting laggy client");
                        } else {
                            debug!(%pid, "ws send queue full — dropping frame");
                        }
                    }
                    Err(TrySendError::Closed(_)) => {
                        // Receiver gone; reap the stale handle.
                        drop_idx.push(i);
                    }
                }
            }
            if !drop_idx.is_empty() {
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
}
