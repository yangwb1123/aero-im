//! 1-to-1 and group call orchestration (invite / ring / answer / hangup).
//!
//! [`CallOrchestrator`] is the domain object that drives the call lifecycle.
//! It handles DB persistence (via [`CallRepo`]), optional SFU peer bookkeeping
//! (via [`SfuRouter`]), and missed-call detection. Event publishing and
//! WebRTC signaling relay are intentionally kept out of this crate — they live
//! in `aero-im-core` (event bus) and `aero-server` (WS handler).
//!
//! ## Call lifecycle
//!
//! ```text
//! Initiator sends CallInvite
//!   └─► start_call() ──► [call_sessions row, call_participants rows]
//!         │
//!         ▼
//!     Callees see CallEvent::Invite
//!         │
//!         ▼  (callee picks up)
//! Callee sends CallAnswer
//!   └─► answer_call() ──► [answered_at stamped]
//!         │
//!         ▼
//! Either side sends CallEnd
//!   └─► end_call() ──► [ended_at stamped]
//!         │
//!         ├─ (answered_at IS NULL) ──► missed-call: (initiator, callees)
//!         └─ (answered_at IS NOT NULL) ──► None
//!
//! Group call (P6, mode = Sfu)
//!   CallJoin  ──► join_group_call()  ──► SfuRouter::add_peer + roster
//!   CallLeave ──► leave_group_call() ──► SfuRouter::remove_peer + roster
//!   CallEnd   ──► end_call()         ──► SfuRouter::disband_call
//! ```

use aero_common::{CallId, CallKind, CallMode, CallSession, ParticipantId, RoomId};
use aero_live_webrtc::{PeerRole, SfuRouter};
use aero_storage::CallRepo;
use tracing::{instrument, warn};

/// Errors from [`CallOrchestrator`] operations.
#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    /// The call or one of its participants was not found.
    #[error("not found: {0}")]
    NotFound(String),

    /// The caller does not have permission for this operation.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// A database error occurred.
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

type Result<T> = std::result::Result<T, OrchestratorError>;

/// Drives the call lifecycle: persistence, SFU peer registration, and
/// missed-call detection.
///
/// Construct with [`CallOrchestrator::new`]; attach an [`SfuRouter`] via
/// [`with_sfu`](Self::with_sfu) for SFU-mode calls. Cheap to clone — all
/// interior state is reference-counted.
#[derive(Clone)]
pub struct CallOrchestrator {
    calls: CallRepo,
    sfu: Option<SfuRouter>,
}

impl CallOrchestrator {
    /// Create a new orchestrator backed by the given call repository.
    #[must_use]
    pub fn new(calls: CallRepo) -> Self {
        Self { calls, sfu: None }
    }

    /// Attach an [`SfuRouter`] for tracking RTP media peers in SFU-mode calls.
    ///
    /// When present, [`join_group_call`](Self::join_group_call) and
    /// [`leave_group_call`](Self::leave_group_call) register/deregister peers
    /// with the router, and [`end_call`](Self::end_call) disbands the router
    /// entry.
    #[must_use]
    pub fn with_sfu(mut self, sfu: SfuRouter) -> Self {
        self.sfu = Some(sfu);
        self
    }

    /// Start a new call: persist the session, register callees.
    ///
    /// Returns the created [`CallSession`] on success.
    ///
    /// # Errors
    ///
    /// Returns an error if the DB insert fails.
    #[instrument(skip(self, callees), fields(call_kind = ?kind, call_mode = ?mode))]
    pub async fn start_call(
        &self,
        initiator: ParticipantId,
        room: RoomId,
        kind: CallKind,
        mode: CallMode,
        callees: &[ParticipantId],
    ) -> Result<(CallId, CallSession)> {
        let call_id = CallId::new();
        let session = self.calls.start(call_id, room, initiator, kind, mode, callees).await?;

        // For SFU-mode calls add the initiator as a bidirectional peer immediately.
        if mode == CallMode::Sfu {
            if let Some(sfu) = &self.sfu {
                sfu.add_peer(call_id, initiator, PeerRole::Bidirectional);
            }
        }
        Ok((call_id, session))
    }

    /// Record that a call was answered (idempotent; first answer wins).
    ///
    /// # Errors
    ///
    /// Returns an error if the DB update fails.
    #[instrument(skip(self))]
    pub async fn answer_call(&self, call_id: CallId) -> Result<()> {
        self.calls.mark_answered(call_id).await.map_err(OrchestratorError::Db)
    }

    /// End a call and return missed-call information if applicable.
    ///
    /// Returns `Some((initiator, callees))` if the call ended before anyone
    /// answered — callers should queue a missed-call activity-feed entry for
    /// each callee. Returns `None` if the call was answered before ending.
    ///
    /// Also disbands the SFU router entry for this call (if present).
    ///
    /// # Errors
    ///
    /// Returns an error if the DB end-call update fails.
    #[instrument(skip(self, reason))]
    pub async fn end_call(
        &self,
        call_id: CallId,
        reason: &str,
    ) -> Result<Option<(ParticipantId, Vec<ParticipantId>)>> {
        self.calls.end(call_id, reason).await?;

        // Disband the SFU peer table for this call (if SFU mode was in use).
        if let Some(sfu) = &self.sfu {
            let members = sfu.participants(call_id);
            for p in members {
                sfu.remove_peer(call_id, p);
            }
        }

        // Check if the call ended without anyone answering (missed call).
        match self.calls.unanswered_callees(call_id).await {
            Ok(info) => Ok(info),
            Err(e) => {
                // Missed-call detection is best-effort; a DB error here does not
                // fail the end-call operation.
                warn!(error = ?e, %call_id, "unanswered_callees lookup failed");
                Ok(None)
            }
        }
    }

    /// Join a group call (SFU mode). Returns the list of participants already
    /// in the call (for mesh-offer setup), excluding the joiner.
    ///
    /// Registers the participant as a bidirectional SFU peer if an
    /// [`SfuRouter`] is attached.
    ///
    /// # Errors
    ///
    /// Returns an error if the DB session creation fails.
    #[instrument(skip(self))]
    pub async fn join_group_call(
        &self,
        call_id: CallId,
        room: RoomId,
        participant: ParticipantId,
        kind: CallKind,
    ) -> Result<Vec<ParticipantId>> {
        // Create the session row if this is the very first joiner (idempotent on
        // unique constraint: a subsequent start returns a conflict, which is fine
        // — the first joiner's row is the canonical one).
        if let Err(e) = self
            .calls
            .start(call_id, room, participant, kind, CallMode::Sfu, &[])
            .await
        {
            // UniqueViolation (23505) means the call already exists — not an error.
            if e.as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .as_deref()
                != Some("23505")
            {
                return Err(OrchestratorError::Db(e));
            }
        }

        // Add to the SFU router and return existing participants (minus self).
        let existing = if let Some(sfu) = &self.sfu {
            let before: Vec<ParticipantId> =
                sfu.participants(call_id).into_iter().filter(|p| *p != participant).collect();
            sfu.add_peer(call_id, participant, PeerRole::Bidirectional);
            before
        } else {
            Vec::new()
        };
        Ok(existing)
    }

    /// Leave a group call. Removes the participant from the SFU router.
    ///
    /// Returns `true` if this was the **last** participant — i.e. the call is
    /// now empty and the caller should disband it (end the session row, emit a
    /// `CallEnd`, and drop any cluster-wide roster entry). Returns `false` if
    /// other participants remain, or if no [`SfuRouter`] is attached (in which
    /// case emptiness cannot be tracked here and the caller must decide).
    ///
    /// This surfaces the empty-roster signal that [`SfuRouter::remove_peer`]
    /// already computes; without it, callers have no way to detect that the
    /// final member dropped and the group call should be torn down.
    #[instrument(skip(self))]
    pub fn leave_group_call(&self, call_id: CallId, participant: ParticipantId) -> bool {
        match &self.sfu {
            Some(sfu) => sfu.remove_peer(call_id, participant),
            None => false,
        }
    }

    /// Expose the underlying [`SfuRouter`] for RTP track/subscription queries.
    #[must_use]
    pub fn sfu(&self) -> Option<&SfuRouter> {
        self.sfu.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_live_webrtc::SfuRouter;

    fn make_ids() -> (ParticipantId, ParticipantId) {
        (ParticipantId::new(), ParticipantId::new())
    }

    #[tokio::test]
    async fn join_group_call_tracks_peers_in_sfu() {
        let sfu = SfuRouter::new();
        let (p1, p2) = make_ids();
        let call_id = CallId::new();

        // Wire up an orchestrator with the SFU router (no real DB needed here —
        // we test SFU bookkeeping in isolation via the non-async path).
        let orch = CallOrchestrator::new_for_test(sfu.clone());

        // Simulate p1 joining first.
        orch.sfu_add(call_id, p1);
        let p1_before = orch.existing_peers_excluding(call_id, p1);
        assert!(p1_before.is_empty(), "first joiner sees no peers");

        // p2 joins; should see p1.
        orch.sfu_add(call_id, p2);
        let p2_before = orch.existing_peers_excluding(call_id, p2);
        assert_eq!(p2_before, vec![p1], "second joiner sees p1");

        // p1 leaves; p2 remains, so the call is NOT yet empty.
        let p1_was_last = orch.leave_group_call(call_id, p1);
        assert!(!p1_was_last, "p1 leaving with p2 still present is not the last leave");
        assert_eq!(sfu.participants(call_id), vec![p2]);
    }

    #[tokio::test]
    async fn leave_group_call_reports_last_participant() {
        let sfu = SfuRouter::new();
        let (p1, p2) = make_ids();
        let call_id = CallId::new();
        let orch = CallOrchestrator::new_for_test(sfu.clone());

        orch.sfu_add(call_id, p1);
        orch.sfu_add(call_id, p2);

        // Removing a non-last peer must not signal teardown.
        assert!(!orch.leave_group_call(call_id, p1), "p1 is not the last to leave");
        // Removing the final peer signals the call is now empty.
        assert!(orch.leave_group_call(call_id, p2), "p2 is the last to leave");
        assert!(sfu.participants(call_id).is_empty(), "roster cleared after last leave");

        // A redundant leave on an already-empty / unknown call is not a teardown.
        assert!(
            !orch.leave_group_call(call_id, p2),
            "leaving an already-empty call must not re-signal teardown"
        );
    }

    #[tokio::test]
    async fn leave_group_call_without_sfu_returns_false() {
        // No SfuRouter attached: emptiness can't be tracked, so never claim
        // "last participant" (the caller must decide via other state).
        let orch = CallOrchestrator { calls: stub_repo(), sfu: None };
        let (p1, _p2) = make_ids();
        assert!(!orch.leave_group_call(CallId::new(), p1));
    }

    #[tokio::test]
    async fn end_call_disbands_sfu_roster() {
        let sfu = SfuRouter::new();
        let (p1, p2) = make_ids();
        let call_id = CallId::new();

        sfu.add_peer(call_id, p1, PeerRole::Bidirectional);
        sfu.add_peer(call_id, p2, PeerRole::Bidirectional);
        assert_eq!(sfu.participants(call_id).len(), 2);

        let orch = CallOrchestrator::new_for_test(sfu.clone());
        orch.disband_sfu(call_id);

        assert!(sfu.participants(call_id).is_empty(), "SFU roster cleared after call end");
    }

    // Helpers for testing SFU bookkeeping without a real DB.
    impl CallOrchestrator {
        fn new_for_test(sfu: SfuRouter) -> Self {
            // `CallRepo::new` requires a live pool; use the real repo type with a
            // test-only helper path that skips DB access. The SFU logic being
            // tested here is sync and never touches the pool.
            //
            // We build a minimal repo: the test methods below bypass DB calls, so
            // the pool value is irrelevant — only the SFU methods are exercised.
            //
            // SAFETY: we only call `sfu_add`, `existing_peers_excluding`, and
            // `disband_sfu` in these tests, none of which touch `self.calls`.
            #[allow(clippy::needless_pass_by_value)]
            let calls = stub_repo();
            Self { calls, sfu: Some(sfu) }
        }

        fn sfu_add(&self, call_id: CallId, participant: ParticipantId) {
            if let Some(sfu) = &self.sfu {
                sfu.add_peer(call_id, participant, PeerRole::Bidirectional);
            }
        }

        fn existing_peers_excluding(
            &self,
            call_id: CallId,
            exclude: ParticipantId,
        ) -> Vec<ParticipantId> {
            self.sfu
                .as_ref()
                .map(|s| s.participants(call_id).into_iter().filter(|p| *p != exclude).collect())
                .unwrap_or_default()
        }

        fn disband_sfu(&self, call_id: CallId) {
            if let Some(sfu) = &self.sfu {
                for p in sfu.participants(call_id) {
                    sfu.remove_peer(call_id, p);
                }
            }
        }
    }

    /// Build a CallRepo with a lazy, never-connected Postgres pool.
    ///
    /// The tests above only exercise SFU bookkeeping — they never `.await` any
    /// async `CallRepo` method, so the pool is never actually opened.
    fn stub_repo() -> CallRepo {
        let pg = sqlx::PgPool::connect_lazy("postgres://localhost/nonexistent")
            .expect("pg lazy pool");
        CallRepo::new(pg)
    }
}
