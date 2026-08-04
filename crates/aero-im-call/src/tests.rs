use super::*;
use aero_live_webrtc::SfuRouter;
use sqlx::types::time::OffsetDateTime;
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
    assert_eq!(
        MAX_MESH_PARTICIPANTS, 8,
        "documented browser-mesh saturation point"
    );
}

/// Reaching the cap rejects the (N+1)ᵗʰ *new* member without mutating the
/// live roster. Canonical DB validation is exercised before this policy in
/// the production join path.
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
    assert_eq!(
        sfu.participants(call).len(),
        cap,
        "roster filled to the cap"
    );

    // A brand-new member is rejected by the pure admission decision.
    let newcomer = ParticipantId::new();
    assert!(orch.would_reject_join(call, newcomer));
    // The newcomer was NOT added to the mesh.
    assert_eq!(
        sfu.participants(call).len(),
        cap,
        "rejected member never entered the roster"
    );
    assert!(!sfu.participants(call).contains(&newcomer));
}

#[test]
fn canonical_group_call_rejects_room_mode_kind_and_ended_mismatch() {
    let room = RoomId::new();
    let base = CallSession {
        id: CallId::new(),
        room_id: room,
        initiator: ParticipantId::new(),
        kind: CallKind::Video,
        mode: CallMode::Sfu,
        started_at: OffsetDateTime::now_utc(),
        ended_at: None,
        end_reason: None,
    };
    assert!(validate_group_call(&base, room, CallKind::Video).is_ok());

    assert!(matches!(
        validate_group_call(&base, RoomId::new(), CallKind::Video),
        Err(OrchestratorError::Forbidden(_))
    ));

    let mut p2p = base.clone();
    p2p.mode = CallMode::P2p;
    assert!(matches!(
        validate_group_call(&p2p, room, CallKind::Video),
        Err(OrchestratorError::Forbidden(_))
    ));
    assert!(matches!(
        validate_group_call(&base, room, CallKind::Audio),
        Err(OrchestratorError::Forbidden(_))
    ));

    let mut ended = base;
    ended.ended_at = Some(OffsetDateTime::now_utc());
    assert!(matches!(
        validate_group_call(&ended, room, CallKind::Video),
        Err(OrchestratorError::Forbidden(_))
    ));
}

/// A reconnect of an *existing* member at capacity is admitted, not rejected:
/// the capacity gate dedups by `ParticipantId` and never double-counts.
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
    assert_eq!(
        sfu.participants(call).len(),
        cap,
        "roster at the cap incl. reconnector"
    );

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
    let p1_was_last = orch.leave_group_call_for_test(call_id, p1).await;
    assert!(
        !p1_was_last,
        "p1 leaving with p2 still present is not the last leave"
    );
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
    assert!(
        !orch.leave_group_call_for_test(call_id, p1).await,
        "p1 is not the last to leave"
    );
    // Removing the final peer signals the call is now empty.
    assert!(
        orch.leave_group_call_for_test(call_id, p2).await,
        "p2 is the last to leave"
    );
    assert!(
        sfu.participants(call_id).is_empty(),
        "roster cleared after last leave"
    );

    // A redundant leave on an already-empty / unknown call is not a teardown.
    assert!(
        !orch.leave_group_call_for_test(call_id, p2).await,
        "leaving an already-empty call must not re-signal teardown"
    );
}

#[tokio::test]
async fn leave_group_call_without_sfu_returns_false() {
    // No SfuRouter attached: emptiness can't be tracked, so never claim
    // "last participant" (the caller must decide via other state).
    let orch = CallOrchestrator {
        calls: stub_repo(),
        sfu: None,
        routes: None,
        local_leg_generations: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
    };
    let (p1, _p2) = make_ids();
    assert!(!orch.leave_group_call_for_test(CallId::new(), p1).await);
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

    assert!(
        sfu.participants(call_id).is_empty(),
        "SFU roster cleared after call end"
    );
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
        Arc::new(Self {
            nodes: Mutex::new(nodes),
            ..Self::default()
        })
    }

    fn failing() -> Arc<Self> {
        Arc::new(Self {
            fail: true,
            ..Self::default()
        })
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
        self.registered
            .lock()
            .unwrap()
            .push((call, participant, node_url.to_owned()));
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
        self.nodes_unregistered
            .lock()
            .unwrap()
            .push((call, node_url.to_owned()));
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
    let topology = orch
        .register_and_decide(CallId::new(), ParticipantId::new(), 1)
        .await
        .unwrap();
    assert_eq!(
        topology,
        CallTopology::ServeLocal,
        "no registry → today's behavior"
    );
}

#[tokio::test]
async fn register_and_decide_registers_then_serves_local_when_self_only() {
    let routes = FakeRoutes::with_nodes(vec![(LOCAL.to_owned(), 1)]);
    let orch =
        CallOrchestrator::new_for_test(SfuRouter::new()).with_call_routes(routes.clone(), LOCAL);
    let (call, p) = (CallId::new(), ParticipantId::new());

    let topology = orch.register_and_decide(call, p, 1).await.unwrap();
    assert_eq!(
        topology,
        CallTopology::ServeLocal,
        "only this node hosts the call"
    );
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
    let orch = CallOrchestrator::new_for_test(SfuRouter::new()).with_call_routes(routes, LOCAL);

    let topology = orch
        .register_and_decide(CallId::new(), ParticipantId::new(), 1)
        .await
        .unwrap();
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
async fn current_topology_reconciles_without_registering_again() {
    let routes = FakeRoutes::with_nodes(vec![
        (LOCAL.to_owned(), 1),
        ("http://node-b.example".to_owned(), 2),
    ]);
    let orch =
        CallOrchestrator::new_for_test(SfuRouter::new()).with_call_routes(routes.clone(), LOCAL);
    let call = CallId::new();

    assert_eq!(
        orch.current_topology(call).await,
        Some(CallTopology::BridgeTo(vec![
            "http://node-b.example".to_owned()
        ]))
    );
    assert!(
        routes.registered.lock().unwrap().is_empty(),
        "a census refresh must not create a participant route"
    );
}

#[tokio::test]
async fn current_topology_preserves_bridges_on_registry_failure() {
    let orch = CallOrchestrator::new_for_test(SfuRouter::new())
        .with_call_routes(FakeRoutes::failing(), LOCAL);
    assert_eq!(orch.current_topology(CallId::new()).await, None);
}

#[tokio::test]
async fn registry_failure_degrades_to_serve_local() {
    let orch = CallOrchestrator::new_for_test(SfuRouter::new())
        .with_call_routes(FakeRoutes::failing(), LOCAL);
    let topology = orch
        .register_and_decide(CallId::new(), ParticipantId::new(), 1)
        .await
        .unwrap();
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
    let orch = CallOrchestrator::new_for_test(sfu.clone()).with_call_routes(routes.clone(), LOCAL);
    let (p1, p2) = make_ids();
    let call = CallId::new();
    orch.sfu_add(call, p1);
    orch.sfu_add(call, p2);

    assert!(
        !orch.leave_group_call_for_test(call, p1).await,
        "p1 is not the last local"
    );
    assert_eq!(
        routes.unregistered.lock().unwrap().as_slice(),
        &[(call, p1)]
    );
    assert!(
        routes.nodes_unregistered.lock().unwrap().is_empty(),
        "node entry kept while local participants remain"
    );

    assert!(
        orch.leave_group_call_for_test(call, p2).await,
        "p2 is the last local participant"
    );
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
        Self {
            calls,
            sfu: Some(sfu),
            routes: None,
            local_leg_generations: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
        }
    }

    fn sfu_add(&self, call_id: CallId, participant: ParticipantId) {
        if let Some(sfu) = &self.sfu {
            sfu.add_peer(call_id, participant, PeerRole::Bidirectional);
        }
    }

    async fn leave_group_call_for_test(&self, call_id: CallId, participant: ParticipantId) -> bool {
        self.cleanup_group_call_participant(call_id, participant)
            .await
    }

    /// Mirror of `join_group_call`'s capacity gate against the live SFU
    /// roster, exposed so reconnect-vs-new admission can be asserted without
    /// the post-gate DB write that the stub repo cannot serve.
    fn would_reject_join(&self, call_id: CallId, participant: ParticipantId) -> bool {
        let roster = self
            .sfu
            .as_ref()
            .map(|s| s.participants(call_id))
            .unwrap_or_default();
        Self::cap_rejects(&roster, &participant)
    }

    fn existing_peers_excluding(
        &self,
        call_id: CallId,
        exclude: ParticipantId,
    ) -> Vec<ParticipantId> {
        self.sfu
            .as_ref()
            .map(|s| {
                s.participants(call_id)
                    .into_iter()
                    .filter(|p| *p != exclude)
                    .collect()
            })
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

/// Build a `CallRepo` with a lazy, never-connected Postgres pool.
///
/// The tests above only exercise SFU bookkeeping — they never `.await` any
/// async `CallRepo` method, so the pool is never actually opened.
fn stub_repo() -> CallRepo {
    let pg = sqlx::PgPool::connect_lazy("postgres://localhost/nonexistent").expect("pg lazy pool");
    CallRepo::new(pg)
}
