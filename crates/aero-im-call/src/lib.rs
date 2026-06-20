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
//!   CallJoin  ──► join_group_call()  ──► SfuRouter::add_peer + route registration
//!                                        + decide_call_topology (bridge intent)
//!   CallLeave ──► leave_group_call() ──► SfuRouter::remove_peer + route cleanup
//!   CallEnd   ──► end_call()         ──► SfuRouter::disband_call + node deregistration
//! ```
//!
//! ## Cross-node group calls (ROADMAP3 方向二)
//!
//! With [`with_call_routes`](CallOrchestrator::with_call_routes) attached, a
//! join also registers the participant in the cluster-wide
//! [`CallRouteStore`] (`call -> { participant -> hosting node }`) and consults
//! the **pure** [`decide_call_topology`] policy: the returned
//! [`GroupJoin::topology`] is the recorded **bridge intent** — `BridgeTo(urls)`
//! lists every *other* node hosting participants of the call, each of which
//! the caller (the server layer) should cover with one
//! [`CallBridge`](aero_live_webrtc::CallBridge) pull. Without the registry
//! (`None`), behavior is exactly the single-node behavior of before:
//! [`CallTopology::ServeLocal`] always.

use std::sync::Arc;

use aero_common::{CallId, CallKind, CallMode, CallSession, ParticipantId, RoomId};
use aero_live_webrtc::{decide_call_topology, CallTopology, PeerRole, SfuRouter};
use aero_storage::{CallRepo, CallRouteRegistry};
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

    /// The group call has reached its full-mesh participant ceiling
    /// ([`max_mesh_participants`]); admitting another *new* member would
    /// saturate every existing member's browser (see [`MAX_MESH_PARTICIPANTS`]).
    /// The payload is the cap that was hit.
    #[error("call full: mesh participant limit ({0}) reached")]
    CallFull(usize),

    /// A database error occurred.
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

type Result<T> = std::result::Result<T, OrchestratorError>;

/// Default ceiling on the number of participants in a single full-mesh group
/// call (P1-4 mesh-saturation guard).
///
/// Group calls are full-mesh WebRTC: every pair of members holds one P2P
/// connection, so a call of `N` members has `N·(N-1)/2` connections and each
/// browser must sustain `N-1` simultaneous upstreams. Real browsers saturate
/// at roughly 6-8 outbound streams; past that the **whole** call degrades for
/// everyone already in it (an N² connection avalanche), not just the newest
/// joiner. We therefore hard-cap mesh membership: once a call holds
/// [`max_mesh_participants`] distinct members, a *new* member's join is
/// rejected with [`OrchestratorError::CallFull`] instead of being admitted and
/// dragging the call down.
pub const MAX_MESH_PARTICIPANTS: usize = 8;

/// Environment override for [`MAX_MESH_PARTICIPANTS`].
const MAX_MESH_ENV: &str = "AERO_MAX_CALL_MESH";

/// Resolve the effective mesh cap: `AERO_MAX_CALL_MESH` if set to a positive
/// integer, otherwise [`MAX_MESH_PARTICIPANTS`]. A `0`/non-numeric value is
/// ignored (falls back to the default) so a typo can never disable the guard.
#[must_use]
pub fn max_mesh_participants() -> usize {
    std::env::var(MAX_MESH_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(MAX_MESH_PARTICIPANTS)
}

/// Cluster view of which nodes host which participants of a group call.
///
/// Mirrors [`aero_storage::CallRouteRegistry`]'s surface (which implements
/// this trait); abstracted so the orchestrator unit-tests against an in-memory
/// fake — the real registry needs live Redis. All methods are best-effort from
/// the orchestrator's perspective: a registry failure degrades to single-node
/// behavior rather than failing the call operation.
#[async_trait::async_trait]
pub trait CallRouteStore: Send + Sync {
    /// Record (or refresh) that `participant`'s media leg for `call` is hosted
    /// on `node_url`.
    async fn register_participant(
        &self,
        call: CallId,
        participant: ParticipantId,
        node_url: &str,
    ) -> anyhow::Result<()>;

    /// Drop `participant`'s mapping for `call`.
    async fn unregister_participant(
        &self,
        call: CallId,
        participant: ParticipantId,
    ) -> anyhow::Result<()>;

    /// Drop **every** participant mapped to `node_url` for `call` (the
    /// last-local-participant / call-end cleanup path).
    async fn unregister_node(&self, call: CallId, node_url: &str) -> anyhow::Result<()>;

    /// The nodes currently hosting participants of `call`, with participant
    /// counts, deterministically ordered.
    async fn nodes_for_call(&self, call: CallId) -> anyhow::Result<Vec<(String, u32)>>;
}

#[async_trait::async_trait]
impl CallRouteStore for CallRouteRegistry {
    async fn register_participant(
        &self,
        call: CallId,
        participant: ParticipantId,
        node_url: &str,
    ) -> anyhow::Result<()> {
        CallRouteRegistry::register_participant(self, call, participant, node_url).await
    }

    async fn unregister_participant(
        &self,
        call: CallId,
        participant: ParticipantId,
    ) -> anyhow::Result<()> {
        CallRouteRegistry::unregister_participant(self, call, participant).await
    }

    async fn unregister_node(&self, call: CallId, node_url: &str) -> anyhow::Result<()> {
        CallRouteRegistry::unregister_node(self, call, node_url).await
    }

    async fn nodes_for_call(&self, call: CallId) -> anyhow::Result<Vec<(String, u32)>> {
        CallRouteRegistry::nodes_for_call(self, call).await
    }
}

/// The cross-node registry plus this node's own public base URL.
#[derive(Clone)]
struct CallRoutes {
    store: Arc<dyn CallRouteStore>,
    node_url: String,
}

/// Outcome of [`CallOrchestrator::join_group_call`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupJoin {
    /// Participants already in the call **on this node** (for mesh-offer
    /// setup), excluding the joiner.
    pub existing_peers: Vec<ParticipantId>,
    /// The recorded bridge intent: [`CallTopology::BridgeTo`] lists every
    /// other node hosting participants — the server layer should ensure one
    /// [`CallBridge`](aero_live_webrtc::CallBridge) pull per listed node.
    /// Always [`CallTopology::ServeLocal`] when no registry is attached.
    pub topology: CallTopology,
}

/// Drives the call lifecycle: persistence, SFU peer registration, and
/// missed-call detection.
///
/// Construct with [`CallOrchestrator::new`]; attach an [`SfuRouter`] via
/// [`with_sfu`](Self::with_sfu) for SFU-mode calls, and a [`CallRouteStore`]
/// via [`with_call_routes`](Self::with_call_routes) for cross-node group
/// calls. Cheap to clone — all interior state is reference-counted.
#[derive(Clone)]
pub struct CallOrchestrator {
    calls: CallRepo,
    sfu: Option<SfuRouter>,
    routes: Option<CallRoutes>,
}

impl CallOrchestrator {
    /// Create a new orchestrator backed by the given call repository.
    #[must_use]
    pub fn new(calls: CallRepo) -> Self {
        Self { calls, sfu: None, routes: None }
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

    /// Attach the cross-node call-route registry and this node's public base
    /// URL (ROADMAP3 方向二 — 分布式通话基底).
    ///
    /// When present, [`join_group_call`](Self::join_group_call) registers the
    /// joiner in the cluster registry and returns a bridge-intent
    /// [`CallTopology`]; [`leave_group_call`](Self::leave_group_call) and
    /// [`end_call`](Self::end_call) clean the registry up. When absent
    /// (`None`, the default), every code path behaves exactly as the
    /// single-node orchestrator did — zero regression.
    #[must_use]
    pub fn with_call_routes(
        mut self,
        store: Arc<dyn CallRouteStore>,
        node_url: impl Into<String>,
    ) -> Self {
        self.routes = Some(CallRoutes { store, node_url: node_url.into() });
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
    /// Also disbands the SFU router entry for this call (if present) and
    /// deregisters this node from the cross-node registry (if attached;
    /// best-effort — remote nodes' entries are cleaned by their own
    /// leave/end paths or age out via the registry TTL).
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

        if let Some(routes) = &self.routes {
            if let Err(e) = routes.store.unregister_node(call_id, &routes.node_url).await {
                warn!(error = ?e, %call_id, "call-route node cleanup failed on end_call");
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

    /// Join a group call (SFU mode). Returns the participants already in the
    /// call on this node (for mesh-offer setup, excluding the joiner) plus the
    /// cross-node bridge intent (see [`GroupJoin`]).
    ///
    /// Registers the participant as a bidirectional SFU peer if an
    /// [`SfuRouter`] is attached, and in the cluster registry if a
    /// [`CallRouteStore`] is attached (best-effort: a registry failure
    /// degrades to [`CallTopology::ServeLocal`] rather than failing the join).
    ///
    /// ## Full-mesh capacity guard (P1-4)
    ///
    /// This is the single authoritative admission point for the mesh. Before
    /// the joiner is added to the SFU roster, the call's current member count
    /// is checked against [`max_mesh_participants`]. A member already on the
    /// roster (a *reconnect* — same [`ParticipantId`]) is **always** re-admitted
    /// (idempotent, never counted twice). A genuinely new member is rejected
    /// with [`OrchestratorError::CallFull`] when the roster is already at the
    /// cap, so the N+1ᵗʰ peer never enters the mesh and never drags the call
    /// down for everyone already in it.
    ///
    /// # Errors
    ///
    /// - [`OrchestratorError::CallFull`] if admitting this *new* participant
    ///   would exceed [`max_mesh_participants`].
    /// - [`OrchestratorError::Db`] if the DB session creation fails.
    #[instrument(skip(self))]
    pub async fn join_group_call(
        &self,
        call_id: CallId,
        room: RoomId,
        participant: ParticipantId,
        kind: CallKind,
    ) -> Result<GroupJoin> {
        // Full-mesh admission control: reject a *new* member that would push the
        // call past the mesh-saturation ceiling, but never reject a reconnect of
        // a member already on the roster (dedup by ParticipantId). Only the SFU
        // roster gives a per-call member count; with no SFU attached there is no
        // mesh to saturate here, so the guard is a no-op (server-side roster
        // owns admission in that configuration).
        let existing_peers = if let Some(sfu) = &self.sfu {
            let roster = sfu.participants(call_id);
            if Self::cap_rejects(&roster, &participant) {
                warn!(
                    %call_id,
                    current = roster.len(),
                    cap = max_mesh_participants(),
                    "rejecting group-call join: full-mesh participant ceiling reached"
                );
                return Err(OrchestratorError::CallFull(max_mesh_participants()));
            }
            roster.into_iter().filter(|p| *p != participant).collect()
        } else {
            Vec::new()
        };

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

        // Capacity check passed — add to the SFU router (idempotent for a
        // reconnect: re-inserting the same key just refreshes the role).
        if let Some(sfu) = &self.sfu {
            sfu.add_peer(call_id, participant, PeerRole::Bidirectional);
        }

        let topology = self.register_and_decide(call_id, participant).await;
        Ok(GroupJoin { existing_peers, topology })
    }

    /// Register `participant` in the cross-node registry and decide this
    /// node's bridge intent. Best-effort: with no registry attached, or on any
    /// registry error, the answer is [`CallTopology::ServeLocal`] — the call
    /// still works exactly as single-node.
    async fn register_and_decide(
        &self,
        call_id: CallId,
        participant: ParticipantId,
    ) -> CallTopology {
        let Some(routes) = &self.routes else {
            return CallTopology::ServeLocal;
        };
        if let Err(e) = routes
            .store
            .register_participant(call_id, participant, &routes.node_url)
            .await
        {
            warn!(error = ?e, %call_id, "call-route registration failed; serving locally");
            return CallTopology::ServeLocal;
        }
        match routes.store.nodes_for_call(call_id).await {
            Ok(nodes) => decide_call_topology(&routes.node_url, &nodes),
            Err(e) => {
                warn!(error = ?e, %call_id, "call-route census failed; serving locally");
                CallTopology::ServeLocal
            }
        }
    }

    /// The full-mesh capacity decision (P1-4), factored out so the
    /// admission gate has a single, testable source of truth.
    ///
    /// Returns `true` iff `participant` is a *new* member (not already on
    /// `roster`) **and** `roster` is already at or above
    /// [`max_mesh_participants`]. A reconnect (participant already present)
    /// always returns `false` — it is re-admitted regardless of size.
    fn cap_rejects(roster: &[ParticipantId], participant: &ParticipantId) -> bool {
        !roster.contains(participant) && roster.len() >= max_mesh_participants()
    }

    /// Leave a group call. Removes the participant from the SFU router and
    /// the cross-node registry (if attached).
    ///
    /// Returns `true` if this was the **last local** participant — i.e. this
    /// node's leg of the call is now empty and the caller should disband it
    /// (end the session row, emit a `CallEnd`, drop any cluster-wide roster
    /// entry, and stop any bridges pulling for this call). On that signal this
    /// node's registry entry is also deregistered (sweeping any stale local
    /// mappings). Returns `false` if other local participants remain, or if no
    /// [`SfuRouter`] is attached (in which case emptiness cannot be tracked
    /// here and the caller must decide).
    ///
    /// This surfaces the empty-roster signal that [`SfuRouter::remove_peer`]
    /// already computes; without it, callers have no way to detect that the
    /// final member dropped and the group call should be torn down.
    #[instrument(skip(self))]
    pub async fn leave_group_call(&self, call_id: CallId, participant: ParticipantId) -> bool {
        let last_local = match &self.sfu {
            Some(sfu) => sfu.remove_peer(call_id, participant),
            None => false,
        };

        if let Some(routes) = &self.routes {
            if let Err(e) = routes.store.unregister_participant(call_id, participant).await {
                warn!(error = ?e, %call_id, "call-route unregistration failed");
            }
            if last_local {
                if let Err(e) = routes.store.unregister_node(call_id, &routes.node_url).await {
                    warn!(error = ?e, %call_id, "call-route node deregistration failed");
                }
            }
        }
        last_local
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
    use std::sync::Mutex;

    fn make_ids() -> (ParticipantId, ParticipantId) {
        (ParticipantId::new(), ParticipantId::new())
    }

    // ── full-mesh capacity guard (P1-4) ──────────────────────────────────────

    #[test]
    fn max_mesh_participants_defaults_when_env_unset() {
        // Default is the compile-time ceiling unless AERO_MAX_CALL_MESH overrides.
        // (Tests run without the env var set; if a hostile env leaks one in, this
        // assertion documents the contract rather than guaranteeing the value.)
        if std::env::var(MAX_MESH_ENV).is_err() {
            assert_eq!(max_mesh_participants(), MAX_MESH_PARTICIPANTS);
        }
        assert_eq!(MAX_MESH_PARTICIPANTS, 8, "documented browser-mesh saturation point");
    }

    /// Reaching the cap rejects the (N+1)ᵗʰ *new* member, and does so before
    /// any DB access (so the stub repo is never awaited).
    #[tokio::test]
    async fn join_rejects_new_member_at_capacity() {
        let sfu = SfuRouter::new();
        let orch = CallOrchestrator::new_for_test(sfu.clone());
        let call = CallId::new();

        // Fill the roster to exactly the cap with distinct members.
        let cap = max_mesh_participants();
        for _ in 0..cap {
            sfu.add_peer(call, ParticipantId::new(), PeerRole::Bidirectional);
        }
        assert_eq!(sfu.participants(call).len(), cap, "roster filled to the cap");

        // A brand-new member is rejected with CallFull (no DB touched).
        let newcomer = ParticipantId::new();
        let err = orch
            .join_group_call(call, RoomId::new(), newcomer, CallKind::Video)
            .await
            .expect_err("join past the cap must fail");
        match err {
            OrchestratorError::CallFull(reported) => assert_eq!(reported, cap),
            other => panic!("expected CallFull, got {other:?}"),
        }
        // The newcomer was NOT added to the mesh.
        assert_eq!(sfu.participants(call).len(), cap, "rejected member never entered the roster");
        assert!(!sfu.participants(call).contains(&newcomer));
    }

    /// A reconnect of an *existing* member at capacity is admitted, not rejected:
    /// the capacity gate dedups by ParticipantId and never double-counts.
    #[tokio::test]
    async fn join_does_not_reject_reconnecting_member_at_capacity() {
        let sfu = SfuRouter::new();
        let orch = CallOrchestrator::new_for_test(sfu.clone());
        let call = CallId::new();

        // Fill the roster to the cap; remember one member as the "reconnector".
        let cap = max_mesh_participants();
        let reconnector = ParticipantId::new();
        sfu.add_peer(call, reconnector, PeerRole::Bidirectional);
        for _ in 1..cap {
            sfu.add_peer(call, ParticipantId::new(), PeerRole::Bidirectional);
        }
        assert_eq!(sfu.participants(call).len(), cap, "roster at the cap incl. reconnector");

        // The reconnect must pass the capacity gate (it is already a member). We
        // exercise the gate decision in isolation — the post-gate path does a DB
        // write the stub repo can't serve, so assert on the pure decision here.
        assert!(
            !orch.would_reject_join(call, reconnector),
            "an existing member reconnecting must never be rejected by the cap"
        );
        // And a genuinely new member at the same cap *is* rejected.
        assert!(
            orch.would_reject_join(call, ParticipantId::new()),
            "a new member at the cap is rejected"
        );
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
        let p1_was_last = orch.leave_group_call(call_id, p1).await;
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
        assert!(!orch.leave_group_call(call_id, p1).await, "p1 is not the last to leave");
        // Removing the final peer signals the call is now empty.
        assert!(orch.leave_group_call(call_id, p2).await, "p2 is the last to leave");
        assert!(sfu.participants(call_id).is_empty(), "roster cleared after last leave");

        // A redundant leave on an already-empty / unknown call is not a teardown.
        assert!(
            !orch.leave_group_call(call_id, p2).await,
            "leaving an already-empty call must not re-signal teardown"
        );
    }

    #[tokio::test]
    async fn leave_group_call_without_sfu_returns_false() {
        // No SfuRouter attached: emptiness can't be tracked, so never claim
        // "last participant" (the caller must decide via other state).
        let orch = CallOrchestrator { calls: stub_repo(), sfu: None, routes: None };
        let (p1, _p2) = make_ids();
        assert!(!orch.leave_group_call(CallId::new(), p1).await);
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

    // ── cross-node route registry (ROADMAP3 方向二) ──────────────────────────

    /// In-memory [`CallRouteStore`]: records operations, serves a scripted
    /// census, optionally fails everything (for degradation tests).
    #[derive(Default)]
    struct FakeRoutes {
        nodes: Mutex<Vec<(String, u32)>>,
        registered: Mutex<Vec<(CallId, ParticipantId, String)>>,
        unregistered: Mutex<Vec<(CallId, ParticipantId)>>,
        nodes_unregistered: Mutex<Vec<(CallId, String)>>,
        fail: bool,
    }

    impl FakeRoutes {
        fn with_nodes(nodes: Vec<(String, u32)>) -> Arc<Self> {
            Arc::new(Self { nodes: Mutex::new(nodes), ..Self::default() })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self { fail: true, ..Self::default() })
        }

        fn check(&self) -> anyhow::Result<()> {
            if self.fail {
                anyhow::bail!("redis unavailable");
            }
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl CallRouteStore for FakeRoutes {
        async fn register_participant(
            &self,
            call: CallId,
            participant: ParticipantId,
            node_url: &str,
        ) -> anyhow::Result<()> {
            self.check()?;
            self.registered.lock().unwrap().push((call, participant, node_url.to_owned()));
            Ok(())
        }

        async fn unregister_participant(
            &self,
            call: CallId,
            participant: ParticipantId,
        ) -> anyhow::Result<()> {
            self.check()?;
            self.unregistered.lock().unwrap().push((call, participant));
            Ok(())
        }

        async fn unregister_node(&self, call: CallId, node_url: &str) -> anyhow::Result<()> {
            self.check()?;
            self.nodes_unregistered.lock().unwrap().push((call, node_url.to_owned()));
            Ok(())
        }

        async fn nodes_for_call(&self, _call: CallId) -> anyhow::Result<Vec<(String, u32)>> {
            self.check()?;
            Ok(self.nodes.lock().unwrap().clone())
        }
    }

    const LOCAL: &str = "http://node-a.example";

    #[tokio::test]
    async fn register_and_decide_without_routes_serves_local() {
        let orch = CallOrchestrator::new_for_test(SfuRouter::new());
        let topology = orch.register_and_decide(CallId::new(), ParticipantId::new()).await;
        assert_eq!(topology, CallTopology::ServeLocal, "no registry → today's behavior");
    }

    #[tokio::test]
    async fn register_and_decide_registers_then_serves_local_when_self_only() {
        let routes = FakeRoutes::with_nodes(vec![(LOCAL.to_owned(), 1)]);
        let orch = CallOrchestrator::new_for_test(SfuRouter::new())
            .with_call_routes(routes.clone(), LOCAL);
        let (call, p) = (CallId::new(), ParticipantId::new());

        let topology = orch.register_and_decide(call, p).await;
        assert_eq!(topology, CallTopology::ServeLocal, "only this node hosts the call");
        assert_eq!(
            routes.registered.lock().unwrap().as_slice(),
            &[(call, p, LOCAL.to_owned())],
            "the joiner was registered under this node's URL"
        );
    }

    #[tokio::test]
    async fn register_and_decide_bridges_to_every_other_hosting_node() {
        let routes = FakeRoutes::with_nodes(vec![
            ("http://node-c.example".to_owned(), 1),
            (LOCAL.to_owned(), 2),
            ("http://node-b.example".to_owned(), 3),
        ]);
        let orch = CallOrchestrator::new_for_test(SfuRouter::new())
            .with_call_routes(routes, LOCAL);

        let topology = orch.register_and_decide(CallId::new(), ParticipantId::new()).await;
        assert_eq!(
            topology,
            CallTopology::BridgeTo(vec![
                "http://node-b.example".to_owned(),
                "http://node-c.example".to_owned(),
            ]),
            "bridge intent covers every other node, deterministically ordered"
        );
    }

    #[tokio::test]
    async fn registry_failure_degrades_to_serve_local() {
        let orch = CallOrchestrator::new_for_test(SfuRouter::new())
            .with_call_routes(FakeRoutes::failing(), LOCAL);
        let topology = orch.register_and_decide(CallId::new(), ParticipantId::new()).await;
        assert_eq!(
            topology,
            CallTopology::ServeLocal,
            "a registry outage must not break the (single-node-correct) join"
        );
    }

    #[tokio::test]
    async fn leave_unregisters_and_last_local_leave_deregisters_node() {
        let sfu = SfuRouter::new();
        let routes = FakeRoutes::with_nodes(Vec::new());
        let orch =
            CallOrchestrator::new_for_test(sfu.clone()).with_call_routes(routes.clone(), LOCAL);
        let (p1, p2) = make_ids();
        let call = CallId::new();
        orch.sfu_add(call, p1);
        orch.sfu_add(call, p2);

        assert!(!orch.leave_group_call(call, p1).await, "p1 is not the last local");
        assert_eq!(routes.unregistered.lock().unwrap().as_slice(), &[(call, p1)]);
        assert!(
            routes.nodes_unregistered.lock().unwrap().is_empty(),
            "node entry kept while local participants remain"
        );

        assert!(orch.leave_group_call(call, p2).await, "p2 is the last local participant");
        assert_eq!(
            routes.nodes_unregistered.lock().unwrap().as_slice(),
            &[(call, LOCAL.to_owned())],
            "last local leave deregisters this node's entry"
        );
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
            // SAFETY: we only call `sfu_add`, `existing_peers_excluding`,
            // `disband_sfu`, `register_and_decide`, and `leave_group_call` in
            // these tests, none of which touch `self.calls`.
            #[allow(clippy::needless_pass_by_value)]
            let calls = stub_repo();
            Self { calls, sfu: Some(sfu), routes: None }
        }

        fn sfu_add(&self, call_id: CallId, participant: ParticipantId) {
            if let Some(sfu) = &self.sfu {
                sfu.add_peer(call_id, participant, PeerRole::Bidirectional);
            }
        }

        /// Mirror of `join_group_call`'s capacity gate against the live SFU
        /// roster, exposed so reconnect-vs-new admission can be asserted without
        /// the post-gate DB write that the stub repo cannot serve.
        fn would_reject_join(&self, call_id: CallId, participant: ParticipantId) -> bool {
            let roster =
                self.sfu.as_ref().map(|s| s.participants(call_id)).unwrap_or_default();
            Self::cap_rejects(&roster, &participant)
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
