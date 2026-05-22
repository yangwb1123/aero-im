//! In-memory roster of participants currently joined to a single call.
//!
//! The WS layer holds one `CallRoster` per active `CallId` in a `DashMap`. The
//! roster is intentionally tiny — it stores membership and join timestamps only;
//! actual media flows peer-to-peer (P2P mode) or through the SFU (P6).

use std::collections::{BTreeMap, HashSet};

use aero_common::{CallId, ParticipantId};
use time::OffsetDateTime;

use crate::errors::SignalingError;

/// Membership tracker for an active call.
///
/// Cheap to construct, not `Clone` on purpose — there is exactly one roster per
/// call and the WS layer owns it behind a `Mutex`/`DashMap` entry.
#[derive(Debug)]
pub struct CallRoster {
    call_id: CallId,
    participants: HashSet<ParticipantId>,
    joined_at: BTreeMap<ParticipantId, OffsetDateTime>,
}

impl CallRoster {
    /// Create an empty roster for `call_id`.
    #[must_use]
    pub fn new(call_id: CallId) -> Self {
        Self {
            call_id,
            participants: HashSet::new(),
            joined_at: BTreeMap::new(),
        }
    }

    /// The call this roster belongs to.
    #[must_use]
    pub fn call_id(&self) -> CallId {
        self.call_id
    }

    /// Add a participant. Returns [`SignalingError::AlreadyJoined`] if the
    /// participant was already present.
    pub fn add(&mut self, pid: ParticipantId) -> Result<(), SignalingError> {
        if !self.participants.insert(pid) {
            return Err(SignalingError::AlreadyJoined);
        }
        self.joined_at.insert(pid, OffsetDateTime::now_utc());
        Ok(())
    }

    /// Remove a participant. Returns `true` when the roster is empty after the
    /// removal (signal to the caller that the call may be torn down).
    ///
    /// Removing a participant who was never in the roster is a no-op that still
    /// reports whether the roster is now empty.
    pub fn remove(&mut self, pid: ParticipantId) -> bool {
        self.participants.remove(&pid);
        self.joined_at.remove(&pid);
        self.participants.is_empty()
    }

    /// Whether `pid` is currently a member of this call.
    #[must_use]
    pub fn contains(&self, pid: ParticipantId) -> bool {
        self.participants.contains(&pid)
    }

    /// Number of joined participants.
    #[must_use]
    pub fn len(&self) -> usize {
        self.participants.len()
    }

    /// Whether the roster has no participants.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.participants.is_empty()
    }

    /// Snapshot of all joined participants (unordered).
    #[must_use]
    pub fn participants(&self) -> Vec<ParticipantId> {
        self.participants.iter().copied().collect()
    }

    /// When `pid` joined, if at all.
    #[must_use]
    pub fn joined_at(&self, pid: ParticipantId) -> Option<OffsetDateTime> {
        self.joined_at.get(&pid).copied()
    }
}
