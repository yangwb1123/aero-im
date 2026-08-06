use std::collections::HashMap;
use std::sync::Arc;

use aero_common::WorkspaceId;
use aero_storage::{
    ProjectionOutcome, SnaplinkCommercialRepo, SnaplinkDeliveryClaim, SnaplinkEntitlementProjection,
};
use anyhow::Context;
use futures::{stream, StreamExt};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{info, warn};

use super::config::{CommercialConfig, CommercialMode};
use super::http::{MachineBinding, SnaplinkHttpClient};

const DELIVERY_BACKLOG_METRIC: &str = "aero_snaplink_delivery_outbox_backlog";

pub struct SnaplinkCommercialRuntime {
    repo: SnaplinkCommercialRepo,
    config: CommercialConfig,
    http: SnaplinkHttpClient,
    bindings: HashMap<WorkspaceId, Arc<MachineBinding>>,
}

impl std::fmt::Debug for SnaplinkCommercialRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SnaplinkCommercialRuntime")
            .field("config", &self.config)
            .field("bindings", &self.bindings.len())
            .finish_non_exhaustive()
    }
}

impl SnaplinkCommercialRuntime {
    /// Load desired state and synchronize the database enforcement switch.
    ///
    /// An absent env setting may not silently disable a database previously
    /// activated by another replica. An explicit `false` is required for an
    /// operator-controlled rollback.
    pub async fn from_env(
        pool: sqlx::PgPool,
    ) -> anyhow::Result<Option<Arc<SnaplinkCommercialRuntime>>> {
        let repo = SnaplinkCommercialRepo::new(pool);
        let config = match CommercialMode::from_env()? {
            CommercialMode::Unspecified => {
                repo.require_disabled().await.context(
                    "commercial desired state is absent while database enforcement is active",
                )?;
                return Ok(None);
            }
            CommercialMode::Disabled => {
                repo.configure_disabled()
                    .await
                    .context("disable Snaplink commercial enforcement")?;
                return Ok(None);
            }
            CommercialMode::Enabled(config) => *config,
        };
        let desired = config
            .bindings
            .iter()
            .map(super::config::CommercialBinding::storage_spec)
            .collect::<Vec<_>>();
        repo.configure_enabled(&desired)
            .await
            .context("activate Snaplink commercial binding desired state")?;
        let http = SnaplinkHttpClient::new(config.clone())?;
        let bindings = config
            .bindings
            .iter()
            .cloned()
            .map(|binding| (binding.workspace_id, Arc::new(MachineBinding::new(binding))))
            .collect();
        let runtime = Arc::new(Self {
            repo,
            config,
            http,
            bindings,
        });
        runtime.reconcile_current_usage().await?;
        runtime.refresh_all_entitlements().await;
        Ok(Some(runtime))
    }

    pub async fn ready(&self) -> Result<bool, sqlx::Error> {
        self.repo.ready().await
    }

    pub fn spawn(self: &Arc<Self>, tracker: &TaskTracker, cancel: CancellationToken) {
        register_metrics();
        let projector = self.clone();
        let projector_cancel = cancel.clone();
        tracker.spawn(async move {
            projector.run_projector(projector_cancel).await;
        });
        let relay = self.clone();
        tracker.spawn(async move {
            relay.run_delivery_relay(cancel).await;
        });
        info!(
            bindings = self.bindings.len(),
            "Snaplink commercial projection and delivery workers enabled"
        );
    }

    async fn run_projector(self: Arc<Self>, cancel: CancellationToken) {
        let start = tokio::time::Instant::now() + self.config.projection_interval;
        let mut tick = tokio::time::interval_at(start, self.config.projection_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                _ = tick.tick() => self.refresh_all_entitlements().await,
            }
        }
    }

    async fn refresh_all_entitlements(&self) {
        let enabled = self
            .bindings
            .values()
            .filter(|binding| binding.binding.enabled)
            .cloned()
            .collect::<Vec<_>>();
        let concurrency = self.config.concurrency;
        let results = stream::iter(enabled)
            .map(|binding| async move {
                let result = self.refresh_entitlement(&binding).await;
                (binding, result)
            })
            .buffer_unordered(concurrency)
            .collect::<Vec<_>>()
            .await;
        for (binding, result) in results {
            let workspace_ref = self
                .config
                .log_ref("workspace", &binding.binding.workspace_id.to_string());
            let tenant_ref = self.config.log_ref("tenant", &binding.binding.tenant_id);
            match result {
                Ok(ProjectionOutcome::Applied) => info!(
                    workspace_ref,
                    tenant_ref, "Snaplink entitlement projection advanced"
                ),
                Ok(ProjectionOutcome::Unchanged) => {}
                Ok(ProjectionOutcome::Stale) => warn!(
                    workspace_ref,
                    tenant_ref, "ignored stale Snaplink entitlement projection"
                ),
                Err(error) => warn!(
                    workspace_ref,
                    tenant_ref,
                    %error,
                    "Snaplink entitlement refresh failed; retaining durable local projection"
                ),
            }
        }
    }

    async fn refresh_entitlement(
        &self,
        binding: &MachineBinding,
    ) -> anyhow::Result<ProjectionOutcome> {
        let projection: SnaplinkEntitlementProjection =
            self.http.fetch_entitlement(binding).await?;
        self.repo
            .project_entitlement(&projection)
            .await
            .context("persist Snaplink entitlement projection")
    }

    async fn run_delivery_relay(self: Arc<Self>, cancel: CancellationToken) {
        let mut tick = tokio::time::interval(self.config.delivery_poll_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    self.shutdown_drain().await;
                    self.sample_backlog().await;
                    return;
                },
                _ = tick.tick() => {
                    if let Err(error) = self.dispatch_batch().await {
                        warn!(?error, "Snaplink delivery claim failed; durable rows remain queued");
                    }
                    self.sample_backlog().await;
                }
            }
        }
    }

    async fn shutdown_drain(&self) {
        let drained = tokio::time::timeout(self.config.shutdown_drain, async {
            let mut total = 0;
            loop {
                let count = self.dispatch_batch().await?;
                total += count;
                if count == 0 {
                    return Ok::<_, sqlx::Error>(total);
                }
            }
        })
        .await;
        match drained {
            Ok(Ok(count)) => info!(count, "Snaplink delivery shutdown drain complete"),
            Ok(Err(error)) => warn!(?error, "Snaplink delivery shutdown drain failed"),
            Err(_) => warn!("Snaplink delivery shutdown drain timed out; rows remain durable"),
        }
    }

    async fn dispatch_batch(&self) -> Result<usize, sqlx::Error> {
        self.repo.reconcile_usage(self.config.batch_size).await?;
        self.repo.reconcile_audit(self.config.batch_size).await?;
        let claims = self
            .repo
            .claim_due(
                time::OffsetDateTime::now_utc(),
                self.config.delivery_lease,
                i64::from(self.config.batch_size),
            )
            .await?;
        let count = claims.len();
        stream::iter(claims)
            .for_each_concurrent(self.config.concurrency, |claim| async move {
                self.deliver_claim(claim).await;
            })
            .await;
        Ok(count)
    }

    async fn reconcile_current_usage(&self) -> anyhow::Result<()> {
        loop {
            let count = self
                .repo
                .reconcile_usage(self.config.batch_size)
                .await
                .context("reconcile pre-activation Snaplink usage")?;
            if count < self.config.batch_size {
                return Ok(());
            }
        }
    }

    async fn deliver_claim(&self, claim: SnaplinkDeliveryClaim) {
        let delivery_ref = self.config.log_ref("delivery", &claim.delivery_id);
        let result = self.binding_for_claim(&claim).cloned();
        let result = match result {
            Ok(binding) => self.http.deliver(&claim, &binding).await,
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => match self.repo.mark_delivered(&claim).await {
                Ok(true) => {}
                Ok(false) => warn!(
                    delivery_ref,
                    "Snaplink delivery lost its lease before acknowledgement"
                ),
                Err(error) => warn!(
                    delivery_ref,
                    ?error,
                    "Snaplink delivery succeeded but acknowledgement failed; idempotent retry will recover"
                ),
            },
            Err(error) => {
                let parked = self
                    .repo
                    .mark_failed(&claim, &error.to_string(), time::OffsetDateTime::now_utc())
                    .await;
                if !matches!(parked, Ok(true)) {
                    warn!(
                        delivery_ref,
                        %error,
                        ?parked,
                        "Snaplink delivery failed and explicit re-park lost its fence; lease expiry will reclaim"
                    );
                }
            }
        }
    }

    fn binding_for_claim(
        &self,
        claim: &SnaplinkDeliveryClaim,
    ) -> anyhow::Result<&Arc<MachineBinding>> {
        let binding = self
            .bindings
            .get(&claim.workspace_id)
            .context("claim workspace has no configured service identity")?;
        let expected_client = match claim.destination {
            aero_storage::SnaplinkDeliveryDestination::Usage => &binding.binding.billing_client_id,
            aero_storage::SnaplinkDeliveryDestination::Audit => &binding.binding.audit_client_id,
        };
        if binding.binding.tenant_id != claim.tenant_id
            || expected_client != &claim.client_id
            || binding.binding.source_system != claim.source_system
        {
            anyhow::bail!("claim identity does not match its trusted service binding");
        }
        Ok(binding)
    }

    async fn sample_backlog(&self) {
        match self.repo.pending_count().await {
            Ok(count) => {
                #[allow(clippy::cast_precision_loss)]
                aero_common::metrics::global().set_gauge(DELIVERY_BACKLOG_METRIC, count as f64);
            }
            Err(error) => warn!(?error, "sample Snaplink delivery backlog failed"),
        }
    }
}

fn register_metrics() {
    aero_common::metrics::global().register_help(
        DELIVERY_BACKLOG_METRIC,
        aero_common::metrics::MetricKind::Gauge,
        "Usage and audit facts durably waiting for Snaplink delivery.",
    );
}
