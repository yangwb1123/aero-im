//! WebRTC Selective Forwarding Unit (SFU) — group calls + interactive live.
//!
//! ## Scope
//!
//! A real [`str0m`](https://docs.rs/str0m)-backed SFU: each participant is a
//! sans-IO `str0m` [`Rtc`](str0m::Rtc) instance ([`SfuPeer`]); one publisher's
//! RTP is forwarded to every subscriber that subscribed to that track, with
//! per-subscriber sequence-number / timestamp remapping.
//!
//! - [`SfuRouter`] — keyed by call id, tracks publishers and subscribers with
//!   simple add/remove semantics. Holds the per-call media routing topology.
//! - [`PeerRole`] — `Publisher` / `Subscriber` / `Bidirectional`.
//! - [`SfuPeer`] — wraps a `str0m` `Rtc`: SDP offer/answer + the
//!   `poll_output`/`handle_input` loop exposed as [`peer::PeerProgress`].
//! - [`SfuForwarder`] — the real [`MediaForwarder`]: routes inbound RTP from a
//!   publisher to subscriber peers, writing remapped RTP onto each subscriber's
//!   matching outbound `str0m` stream.
//! - [`remap`] — the **pure**, fully unit-tested routing + RTP header-remap
//!   bookkeeping (`ForwardTable`, `RtpRemapper`), with no IO dependency.
//! - [`codec`] — codec-aware keyframe detection ([`Codec`] +
//!   [`payload_is_keyframe`]) dispatching to the pure payload-descriptor
//!   parsers in [`h264`], [`vp8`] and [`vp9`].
//!
//! ## What's verified vs. pending
//!
//! Compiles against `str0m` 0.19 (RTP mode, pure-Rust crypto backend) and the
//! pure routing/remap logic is unit-tested. The actual ICE/DTLS/SRTP handshake
//! and end-to-end browser forwarding require a live UDP socket + real browsers
//! and are **not** runtime-verifiable here; the server wiring of the UDP loop is
//! intentionally out of scope (the loop is provided as a clean API on
//! [`SfuPeer`]).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use aero_common::{CallId, ParticipantId};
use parking_lot::RwLock;
use thiserror::Error;

pub mod codec;
pub mod forward;
pub mod h264;
pub mod peer;
pub mod remap;
pub mod rtcp_feedback;
pub mod simulcast;
pub mod vp8;
pub mod vp9;

pub use codec::{payload_is_keyframe, Codec};
pub use forward::SfuForwarder;
pub use peer::{InboundRtp, KeyframeReq, PeerProgress, SfuPeer};
pub use remap::{ForwardTable, ForwardTarget, RemappedRtp, RtpKey, RtpRemapper};
pub use rtcp_feedback::{KeyframeGate, ParsedFeedback, PendingKeyframeRequest};
pub use simulcast::{ForwardDecision, LayerKind, LayerSelector, LayerSelectorTable, LayerSet, SimulcastLayer};

/// Errors surfaced by the `str0m`-backed peer / forwarder.
#[derive(Debug, Error)]
pub enum SfuError {
    /// SDP offer/answer parsing or negotiation failed.
    #[error("sdp error: {0}")]
    Sdp(String),
    /// Inbound datagram could not be parsed as STUN/DTLS/RTP.
    #[error("net error: {0}")]
    Net(String),
    /// The underlying `str0m` `Rtc` returned an error.
    #[error("rtc error: {0}")]
    Rtc(String),
}

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
    /// `published_mid -> owning publisher`.
    tracks: HashMap<String, ParticipantId>,
    /// `subscriber -> set of published mids they receive`.
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
        let Some(state) = w.get_mut(&call) else {
            return false;
        };
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
        state
            .subscriptions
            .entry(subscriber)
            .or_default()
            .insert(mid.to_owned());
    }

    /// The publisher that owns `mid`, if known.
    #[must_use]
    pub fn owner_of(&self, call: CallId, mid: &str) -> Option<ParticipantId> {
        let r = self.inner.read();
        r.get(&call).and_then(|s| s.tracks.get(mid).copied())
    }

    #[must_use]
    pub fn subscribers_for(&self, call: CallId, mid: &str) -> Vec<ParticipantId> {
        let r = self.inner.read();
        let Some(state) = r.get(&call) else {
            return Vec::new();
        };
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

/// Abstraction the SFU loop uses to fan a publisher's RTP out to subscribers.
///
/// The real implementation is [`SfuForwarder`] (which also exposes the
/// header-aware [`SfuForwarder::on_rtp`]); [`NullForwarder`] is the test/no-op.
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
    fn owner_of_resolves_publisher() {
        let r = SfuRouter::new();
        let call = CallId::new();
        let pub_ = ParticipantId::new();
        r.add_track(call, "v0", pub_);
        assert_eq!(r.owner_of(call, "v0"), Some(pub_));
        assert_eq!(r.owner_of(call, "missing"), None);
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
