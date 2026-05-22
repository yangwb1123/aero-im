//! WebRTC Selective Forwarding Unit (SFU) — group calls + interactive live.
//!
//! ## Scope
//!
//! Provides the **routing data model** for an str0m-driven SFU:
//!
//! - [`SfuRouter`] — keyed by call id, tracks publishers and subscribers with
//!   simple add/remove semantics. Holds the per-call media routing topology.
//! - [`PeerRole`] — `Publisher` / `Subscriber` / `Bidirectional`.
//! - [`MediaForwarder`] — abstraction the real str0m loop will implement to
//!   forward RTP packets from one peer to another with minimal copying.
//!
//! ## What's *not* implemented here
//!
//! The actual str0m event loop, ICE/DTLS/SRTP termination, RTP packet
//! forwarding, and congestion-aware simulcast/SVC routing are **out of scope
//! for P6**. The structural pieces above let `aero-server` reason about call
//! membership and route signaling without bringing in str0m yet.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use aero_common::{CallId, ParticipantId};
use parking_lot::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PeerRole {
    Publisher,
    Subscriber,
    Bidirectional,
}

/// In-memory roster of an active call. Cheap to clone (Arc inside).
#[derive(Default, Clone)]
pub struct SfuRouter {
    inner: Arc<RwLock<HashMap<CallId, CallState>>>,
}

#[derive(Default)]
struct CallState {
    peers: HashMap<ParticipantId, PeerRole>,
    tracks: HashMap<String, ParticipantId>,
    subscriptions: HashMap<ParticipantId, HashSet<String>>,
}

impl SfuRouter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_peer(&self, call: CallId, peer: ParticipantId, role: PeerRole) {
        let mut w = self.inner.write();
        let state = w.entry(call).or_default();
        state.peers.insert(peer, role);
    }

    pub fn remove_peer(&self, call: CallId, peer: ParticipantId) -> bool {
        let mut w = self.inner.write();
        let Some(state) = w.get_mut(&call) else { return false };
        state.peers.remove(&peer);
        state.subscriptions.remove(&peer);
        state.tracks.retain(|_, owner| *owner != peer);
        if state.peers.is_empty() {
            w.remove(&call);
            true
        } else {
            false
        }
    }

    pub fn add_track(&self, call: CallId, mid: &str, owner: ParticipantId) {
        let mut w = self.inner.write();
        let state = w.entry(call).or_default();
        state.tracks.insert(mid.to_owned(), owner);
    }

    pub fn add_subscription(&self, call: CallId, mid: &str, subscriber: ParticipantId) {
        let mut w = self.inner.write();
        let state = w.entry(call).or_default();
        state.subscriptions.entry(subscriber).or_default().insert(mid.to_owned());
    }

    #[must_use]
    pub fn subscribers_for(&self, call: CallId, mid: &str) -> Vec<ParticipantId> {
        let r = self.inner.read();
        let Some(state) = r.get(&call) else { return Vec::new() };
        state
            .subscriptions
            .iter()
            .filter_map(|(pid, set)| if set.contains(mid) { Some(*pid) } else { None })
            .collect()
    }

    #[must_use]
    pub fn participants(&self, call: CallId) -> Vec<ParticipantId> {
        let r = self.inner.read();
        r.get(&call)
            .map(|s| s.peers.keys().copied().collect())
            .unwrap_or_default()
    }

    #[must_use]
    pub fn call_count(&self) -> usize {
        self.inner.read().len()
    }
}

#[async_trait::async_trait]
pub trait MediaForwarder: Send + Sync {
    async fn forward_rtp(&self, call: CallId, mid: &str, packet: bytes::Bytes);
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NullForwarder;

#[async_trait::async_trait]
impl MediaForwarder for NullForwarder {
    async fn forward_rtp(&self, _call: CallId, _mid: &str, _packet: bytes::Bytes) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_remove_peer_clears_call_when_empty() {
        let r = SfuRouter::new();
        let call = CallId::new();
        let a = ParticipantId::new();
        let b = ParticipantId::new();
        r.add_peer(call, a, PeerRole::Publisher);
        r.add_peer(call, b, PeerRole::Subscriber);
        assert_eq!(r.call_count(), 1);
        assert!(!r.remove_peer(call, a));
        assert!(r.remove_peer(call, b));
        assert_eq!(r.call_count(), 0);
    }

    #[test]
    fn subscribers_for_routes_correctly() {
        let r = SfuRouter::new();
        let call = CallId::new();
        let pub_ = ParticipantId::new();
        let sub1 = ParticipantId::new();
        let sub2 = ParticipantId::new();
        r.add_peer(call, pub_, PeerRole::Publisher);
        r.add_peer(call, sub1, PeerRole::Subscriber);
        r.add_peer(call, sub2, PeerRole::Subscriber);
        r.add_track(call, "0", pub_);
        r.add_subscription(call, "0", sub1);
        r.add_subscription(call, "0", sub2);
        let s = r.subscribers_for(call, "0");
        assert_eq!(s.len(), 2);
        assert!(s.contains(&sub1));
        assert!(s.contains(&sub2));
    }

    #[test]
    fn track_dropped_when_owner_leaves() {
        let r = SfuRouter::new();
        let call = CallId::new();
        let pub_ = ParticipantId::new();
        r.add_peer(call, pub_, PeerRole::Publisher);
        r.add_track(call, "0", pub_);
        r.remove_peer(call, pub_);
        assert_eq!(r.call_count(), 0);
    }
}
