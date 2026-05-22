//! In-memory WebSocket connection registry.
//!
//! Maps `ParticipantId` to one or more open WebSocket sinks. A single user may have
//! multiple devices, so each participant holds a `Vec<UnboundedSender>`.
//!
//! NATS is the source of truth for cross-instance delivery; the Hub fans-out within
//! the local process only. Each process subscribes to `im.room.*` (durable consumer
//! per instance) and pushes incoming envelopes through the Hub.

use std::sync::Arc;

use aero_common::{ParticipantId, RoomId};
use dashmap::DashMap;
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;
use tracing::debug;

pub type WsSender = UnboundedSender<axum::extract::ws::Message>;

#[derive(Default)]
pub struct Hub {
    /// participant → connections
    conns: DashMap<ParticipantId, Vec<WsSender>>,
    /// room → joined participants (for presence broadcasts within this process)
    rooms: DashMap<RoomId, Vec<ParticipantId>>,
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
}

fn same_sender(a: &WsSender, b: &WsSender) -> bool {
    // UnboundedSender doesn't expose pointer identity; compare via same_channel.
    a.same_channel(b)
}
