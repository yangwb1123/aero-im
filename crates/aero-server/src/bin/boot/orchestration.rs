//! Cross-node call orchestrator + bridge supervisor.
use std::sync::Arc;
use tracing::info;
use aero_live_webrtc::{MediaForwarder, SfuRouter};
use aero_server::call_bridge_supervisor::{
    BridgeSubscriberRegistry, CallBridgeSupervisor, NodeRtpPullerFactory, UpstreamFactory,
};
use aero_im_call::CallOrchestrator;
use aero_storage::{CallRepo, CallRouteRegistry};

pub(crate) struct Orchestration {
    pub(crate) call_orchestrator: Arc<CallOrchestrator>,
    pub(crate) call_supervisor: Arc<CallBridgeSupervisor>,
    pub(crate) bridge_subscribers: BridgeSubscriberRegistry,
}

pub(crate) fn build(
    calls: CallRepo,
    sfu_router: SfuRouter,
    sfu_forwarder: Arc<dyn MediaForwarder>,
    call_routes: Arc<CallRouteRegistry>,
    public_base_url: String,
    default_host: String,
) -> Orchestration {
    let call_orchestrator = Arc::new(
        CallOrchestrator::new(calls.clone())
            .with_sfu(sfu_router.clone())
            .with_call_routes(call_routes.clone(), public_base_url.clone()),
    );

    // Fall back to the configured server host (NOT a hardcoded "localhost"): in a
    // multi-node cluster a peer pulls cross-node call-bridge RTP from this address,
    // and "localhost" would point it at itself. The original boot used
    // `cfg.server.host`; the bin split had regressed this to "localhost".
    let bridge_advertise_host = std::env::var("AERO_BRIDGE_ADVERTISE_HOST")
        .or_else(|_| std::env::var("AERO_INGEST_HOST"))
        .unwrap_or(default_host);
    let bridge_secret = std::env::var("AERO_INTERNAL_BRIDGE_SECRET")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let bridge_factory: Arc<dyn UpstreamFactory> =
        Arc::new(NodeRtpPullerFactory::new(bridge_advertise_host, bridge_secret));

    let bridge_subscribers = BridgeSubscriberRegistry::default();
    let call_supervisor = Arc::new(CallBridgeSupervisor::new(
        sfu_router.clone(),
        sfu_forwarder,
        bridge_factory,
        bridge_subscribers.clone(),
    ));

    info!(
        node = %public_base_url,
        "call-bridge supervisor + orchestrator ready (dormant until cross-node group calls form)"
    );

    Orchestration {
        call_orchestrator,
        call_supervisor,
        bridge_subscribers,
    }
}
