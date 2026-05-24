//! In-memory WebSocket connection registry.
//!
//! Maps `ParticipantId` to one or more open WebSocket sinks. A single user may have
//! multiple devices, so each participant holds a `Vec<UnboundedSender>`.
//!
//! NATS is the source of truth for cross-instance delivery; the Hub fans-out within
//! the local process only. Each process subscribes to `im.room.*` (durable consumer
//! per instance) and pushes incoming envelopes through the Hub.

use std::sync::Arc;

use aero_common::{CallId, ParticipantId, RoomId};
use dashmap::DashMap;
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;
use tracing::debug;
use ulid::Ulid;

pub type WsSender = UnboundedSender<axum::extract::ws::Message>;

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
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn register(&self, pid: ParticipantId, tx: WsSender) {
        self.conns.entry(pid).or_default().push(tx);
        debug!(%pid, "ws registered");
    }

    pub fn unregister(&self, pid: ParticipantId, tx: &WsSender) {
        if let Some(mut entry) = self.conns.get_mut(&pid) {
            entry.retain(|s| !same_sender(s, tx));
            if entry.is_empty() {
                drop(entry);
                self.conns.remove(&pid);
            }
        }
        for mut entry in self.rooms.iter_mut() {
            entry.value_mut().retain(|p| *p != pid);
        }
        for mut entry in self.stream_watchers.iter_mut() {
            entry.value_mut().retain(|p| *p != pid);
        }
        self.call_rosters.retain(|_, members| {
            members.retain(|p| *p != pid);
            !members.is_empty()
        });
        debug!(%pid, "ws unregistered");
    }

    pub fn join_room(&self, room: RoomId, pid: ParticipantId) {
        let mut entry = self.rooms.entry(room).or_default();
        if !entry.contains(&pid) {
            entry.push(pid);
        }
    }

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
    }

    /// Stop watching a stream; drops the watcher set entirely once empty.
    pub fn unwatch_stream(&self, stream_id: Ulid, pid: ParticipantId) {
        if let Some(mut entry) = self.stream_watchers.get_mut(&stream_id) {
            entry.retain(|p| *p != pid);
            if entry.is_empty() {
                drop(entry);
                self.stream_watchers.remove(&stream_id);
            }
        }
    }

    /// Participants currently watching a stream (this process only).
    pub fn stream_watchers(&self, stream_id: Ulid) -> Vec<ParticipantId> {
        self.stream_watchers.get(&stream_id).map(|e| e.clone()).unwrap_or_default()
    }

    /// Distinct viewer count for a stream (this process only).
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
    pub fn fan_out_raw(&self, recipients: &[ParticipantId], text: &str) {
        for pid in recipients {
            if let Some(senders) = self.conns.get(pid) {
                for tx in senders.iter() {
                    let _ = tx.send(axum::extract::ws::Message::Text(text.to_owned()));
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
        existing
    }

    /// Remove a participant from a call roster; drops the roster once empty.
    pub fn call_leave(&self, call_id: CallId, pid: ParticipantId) {
        if let Some(mut entry) = self.call_rosters.get_mut(&call_id) {
            entry.retain(|p| *p != pid);
            if entry.is_empty() {
                drop(entry);
                self.call_rosters.remove(&call_id);
            }
        }
    }

    /// Current members of a call (this process only).
    #[must_use]
    pub fn call_members(&self, call_id: CallId) -> Vec<ParticipantId> {
        self.call_rosters.get(&call_id).map(|e| e.clone()).unwrap_or_default()
    }
}

fn same_sender(a: &WsSender, b: &WsSender) -> bool {
    // UnboundedSender doesn't expose pointer identity; compare via same_channel.
    a.same_channel(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::CallId;

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
}
