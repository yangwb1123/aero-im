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

use std::collections::HashMap;
use std::sync::Arc;

use aero_common::{
    CallId, CallKind, CallMode, CallSession, Error as CommonError, ParticipantId, RoomId,
};
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

    /// The canonical call exists, but its lifecycle/mode no longer admits the
    /// requested transition.
    #[error("conflict: {0}")]
    Conflict(String),

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

impl From<CommonError> for OrchestratorError {
    fn from(error: CommonError) -> Self {
        match error {
            CommonError::NotFound(message) => Self::NotFound(message),
            CommonError::Forbidden(message) | CommonError::Unauthorized(message) => {
                Self::Forbidden(message)
            }
            CommonError::Conflict(message) | CommonError::Invalid(message) => {
                Self::Conflict(message)
            }
            CommonError::Database(error) => Self::Db(error),
            other => Self::Conflict(other.to_string()),
        }
    }
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

    /// Generation-fenced variant used by current SFU joins. `false` means a
    /// newer durable leg already owns the participant route.
    async fn register_participant_generation(
        &self,
        call: CallId,
        participant: ParticipantId,
        node_url: &str,
        generation: i64,
    ) -> anyhow::Result<bool> {
        let _ = generation;
        self.register_participant(call, participant, node_url)
            .await?;
        Ok(true)
    }

    /// Drop `participant`'s mapping for `call`.
    async fn unregister_participant(
        &self,
        call: CallId,
        participant: ParticipantId,
    ) -> anyhow::Result<()>;

    /// Remove only the route written by `generation`.
    async fn unregister_participant_generation(
        &self,
        call: CallId,
        participant: ParticipantId,
        node_url: &str,
        generation: i64,
    ) -> anyhow::Result<bool> {
        let _ = (node_url, generation);
        self.unregister_participant(call, participant).await?;
        Ok(true)
    }

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

    async fn register_participant_generation(
        &self,
        call: CallId,
        participant: ParticipantId,
        node_url: &str,
        generation: i64,
    ) -> anyhow::Result<bool> {
        CallRouteRegistry::register_participant_generation(
            self,
            call,
            participant,
            node_url,
            generation,
        )
        .await
    }

    async fn unregister_participant(
        &self,
        call: CallId,
        participant: ParticipantId,
    ) -> anyhow::Result<()> {
        CallRouteRegistry::unregister_participant(self, call, participant).await
    }

    async fn unregister_participant_generation(
        &self,
        call: CallId,
        participant: ParticipantId,
        node_url: &str,
        generation: i64,
    ) -> anyhow::Result<bool> {
        CallRouteRegistry::unregister_participant_generation(
            self,
            call,
            participant,
            node_url,
            generation,
        )
        .await
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
    /// Durable incarnation of this participant's logical call leg.
    pub leg_generation: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupLeave {
    /// This exact generation won the durable active→left transition.
    pub transitioned: bool,
    /// Removing this node-local generation left no local SFU peers.
    pub call_empty: bool,
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
    local_leg_generations: Arc<tokio::sync::RwLock<HashMap<(CallId, ParticipantId), i64>>>,
}

impl CallOrchestrator {
    /// Create a new orchestrator backed by the given call repository.
    #[must_use]
    pub fn new(calls: CallRepo) -> Self {
        Self {
            calls,
            sfu: None,
            routes: None,
            local_leg_generations: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        }
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
        self.routes = Some(CallRoutes {
            store,
            node_url: node_url.into(),
        });
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
        let session = self
            .calls
            .start(call_id, room, initiator, kind, mode, callees)
            .await?;

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
        self.calls
            .mark_answered(call_id)
            .await
            .map_err(OrchestratorError::Db)
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
        self.cleanup_ended_call(call_id).await;

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

    /// Tear down the local SFU/router and cross-node route state for a call
    /// whose durable End transition has already committed.
    ///
    /// This intentionally performs no call-session write. The IM service owns
    /// the fail-closed DB transition before it publishes `CallEnd`; both the
    /// originating WS handler and every bus consumer can then invoke this
    /// idempotent cleanup even if the database becomes temporarily unavailable.
    pub async fn cleanup_ended_call(&self, call_id: CallId) {
        self.local_leg_generations
            .write()
            .await
            .retain(|(active_call, _), _| *active_call != call_id);
        // Disband the SFU peer table for this call (if SFU mode was in use).
        if let Some(sfu) = &self.sfu {
            let members = sfu.participants(call_id);
            for p in members {
                sfu.remove_peer(call_id, p);
            }
        }

        if let Some(routes) = &self.routes {
            if let Err(e) = routes
                .store
                .unregister_node(call_id, &routes.node_url)
                .await
            {
                warn!(error = ?e, %call_id, "call-route node cleanup failed on end_call");
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
        // Resolve the canonical row before touching any live topology. If two
        // creators race on the same id, only a conflict that can be re-read as
        // the same active SFU call is accepted; a blanket 23505 success would
        // let a caller attach an arbitrary room to somebody else's call id.
        let call = match self.calls.get(call_id).await? {
            Some(call) => call,
            None => {
                self.calls
                    .start_group_authorized(call_id, room, participant, kind)
                    .await?
            }
        };
        validate_group_call(&call, room, kind)?;

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

        // Persist the active leg before exposing it through any live topology.
        // The conditional upsert refuses an already-ended call; a canonical
        // re-read disambiguates that case from an idempotent reconnect.
        let (before_mutation, leg_generation) = self
            .calls
            .join_participant_authorized_generation(call_id, participant, room, kind)
            .await?;
        validate_group_call(&before_mutation, room, kind)?;

        let topology = self
            .register_and_decide(call_id, participant, leg_generation)
            .await?;
        self.local_leg_generations
            .write()
            .await
            .insert((call_id, participant), leg_generation);

        // Capacity check passed — add to the SFU router (idempotent for a
        // reconnect: re-inserting the same key just refreshes the role).
        if let Some(sfu) = &self.sfu {
            sfu.add_peer(call_id, participant, PeerRole::Bidirectional);
        }

        // `end_call` can race the awaits above (including from another node).
        // Re-read after every live mutation. If the canonical row changed or
        // ended, reliably remove the local peer and best-effort unregister its
        // cross-node route before the server is allowed to touch Hub/Redis.
        let post_mutation = self.calls.get(call_id).await;
        let post_error = match post_mutation {
            Ok(Some(call)) => validate_group_call(&call, room, kind).err(),
            Ok(None) => Some(OrchestratorError::NotFound(format!("call {call_id}"))),
            Err(error) => Some(OrchestratorError::Db(error)),
        };
        if let Some(error) = post_error {
            if let Err(rollback_error) = self.leave_group_call(call_id, participant).await {
                warn!(
                    ?rollback_error,
                    %call_id,
                    %participant,
                    "failed to persist group-call join rollback"
                );
            }
            return Err(error);
        }

        Ok(GroupJoin {
            existing_peers,
            topology,
            leg_generation,
        })
    }

    /// Register `participant` in the cross-node registry and decide this
    /// node's bridge intent. Best-effort: with no registry attached, or on any
    /// registry error, the answer is [`CallTopology::ServeLocal`] — the call
    /// still works exactly as single-node.
    async fn register_and_decide(
        &self,
        call_id: CallId,
        participant: ParticipantId,
        leg_generation: i64,
    ) -> Result<CallTopology> {
        let Some(routes) = &self.routes else {
            return Ok(CallTopology::ServeLocal);
        };
        match routes
            .store
            .register_participant_generation(call_id, participant, &routes.node_url, leg_generation)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return Err(OrchestratorError::Conflict(
                    "a newer call participant incarnation is already routed".into(),
                ));
            }
            Err(e) => {
                warn!(error = ?e, %call_id, "call-route registration failed; serving locally");
                return Ok(CallTopology::ServeLocal);
            }
        }
        Ok(self
            .current_topology(call_id)
            .await
            .unwrap_or(CallTopology::ServeLocal))
    }

    /// Recompute this node's bridge intent from the current cluster census.
    ///
    /// Unlike the join-time helper, this does not mutate participant routing.
    /// It is used by remote Join/Publisher events and the periodic reconciler so
    /// an already-present node discovers peers that joined later and retries a
    /// bridge whose transport previously failed. `None` distinguishes a
    /// transient registry failure from a genuine single-node
    /// [`CallTopology::ServeLocal`] result; callers must retain existing bridges
    /// on `None`.
    pub async fn current_topology(&self, call_id: CallId) -> Option<CallTopology> {
        let Some(routes) = &self.routes else {
            return Some(CallTopology::ServeLocal);
        };
        match routes.store.nodes_for_call(call_id).await {
            Ok(nodes) => Some(decide_call_topology(&routes.node_url, &nodes)),
            Err(e) => {
                warn!(error = ?e, %call_id, "call-route census failed; retaining current bridges");
                None
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

    /// Leave a group call. Persists the active leg as left before removing the
    /// participant from the SFU router and cross-node registry.
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
    pub async fn leave_group_call(
        &self,
        call_id: CallId,
        participant: ParticipantId,
    ) -> Result<bool> {
        if let Some(generation) = self.local_leg_generation(call_id, participant).await {
            return Ok(self
                .leave_group_call_generation(call_id, participant, generation)
                .await?
                .call_empty);
        }
        self.calls
            .leave_participant_if_active(call_id, participant)
            .await?;
        Ok(self
            .cleanup_group_call_participant(call_id, participant)
            .await)
    }

    /// Leave and clean one exact durable participant incarnation.
    ///
    /// A stale generation still removes only matching node-local state, but
    /// cannot mark a replacement database leg left or delete its Redis route.
    pub async fn leave_group_call_generation(
        &self,
        call_id: CallId,
        participant: ParticipantId,
        generation: i64,
    ) -> Result<GroupLeave> {
        let transitioned = self
            .calls
            .leave_participant_if_generation(call_id, participant, generation)
            .await?;
        let call_empty = self
            .cleanup_group_call_participant_generation(call_id, participant, generation)
            .await;
        Ok(GroupLeave {
            transitioned,
            call_empty,
        })
    }

    #[must_use]
    pub async fn local_leg_generation(
        &self,
        call_id: CallId,
        participant: ParticipantId,
    ) -> Option<i64> {
        self.local_leg_generations
            .read()
            .await
            .get(&(call_id, participant))
            .copied()
    }

    /// Remove an exact node-local incarnation after its durable leave was
    /// already committed (for example by `ImService::leave_call`).
    pub async fn cleanup_group_call_participant_generation(
        &self,
        call_id: CallId,
        participant: ParticipantId,
        generation: i64,
    ) -> bool {
        let owned = {
            let mut generations = self.local_leg_generations.write().await;
            if generations.get(&(call_id, participant)) == Some(&generation) {
                generations.remove(&(call_id, participant));
                true
            } else {
                false
            }
        };
        if !owned {
            return false;
        }

        let last_local = match &self.sfu {
            Some(sfu) => sfu.remove_peer(call_id, participant),
            None => false,
        };
        if let Some(routes) = &self.routes {
            if let Err(e) = routes
                .store
                .unregister_participant_generation(
                    call_id,
                    participant,
                    &routes.node_url,
                    generation,
                )
                .await
            {
                warn!(error = ?e, %call_id, %participant, generation, "call-route generation cleanup failed");
            }
            if last_local {
                if let Err(e) = routes
                    .store
                    .unregister_node(call_id, &routes.node_url)
                    .await
                {
                    warn!(error = ?e, %call_id, "call-route node deregistration failed");
                }
            }
        }
        last_local
    }

    /// Remove an already-persisted left leg from this node's live topology.
    ///
    /// This second phase performs no database write and is idempotent.
    pub async fn cleanup_group_call_participant(
        &self,
        call_id: CallId,
        participant: ParticipantId,
    ) -> bool {
        self.local_leg_generations
            .write()
            .await
            .remove(&(call_id, participant));
        let last_local = match &self.sfu {
            Some(sfu) => sfu.remove_peer(call_id, participant),
            None => false,
        };

        if let Some(routes) = &self.routes {
            if let Err(e) = routes
                .store
                .unregister_participant(call_id, participant)
                .await
            {
                warn!(error = ?e, %call_id, "call-route unregistration failed");
            }
            if last_local {
                if let Err(e) = routes
                    .store
                    .unregister_node(call_id, &routes.node_url)
                    .await
                {
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

fn validate_group_call(call: &CallSession, room: RoomId, kind: CallKind) -> Result<()> {
    if call.room_id != room {
        return Err(OrchestratorError::Forbidden(
            "call does not belong to the claimed room".into(),
        ));
    }
    if call.mode != CallMode::Sfu {
        return Err(OrchestratorError::Forbidden(
            "group join requires an SFU call".into(),
        ));
    }
    if call.kind != kind {
        return Err(OrchestratorError::Forbidden(
            "call kind does not match the canonical call".into(),
        ));
    }
    if call.ended_at.is_some() {
        return Err(OrchestratorError::Forbidden(
            "cannot join an ended call".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
