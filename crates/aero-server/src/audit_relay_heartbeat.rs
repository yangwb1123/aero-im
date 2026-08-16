//! Server-side [`HeartbeatRecorder`] adapter (B5-4 R6): drives the audit
//! relay's settle-face freshness gate from the durable provisioning row.
//!
//! The relay cannot depend on aero-server, so the connector defines the
//! [`HeartbeatRecorder`] trait and this adapter implements it over
//! [`AuditRelayProvisionRepo`]. The freshness window comes from
//! `RelayConfig.provision_freshness` (std `Duration`); the adapter converts
//! to the `time::Duration` the storage repo's `provision_check` takes.

use aero_audit_connector::heartbeat::HeartbeatRecorder;
use aero_storage::audit_relay_provision::{AuditRelayProvisionRepo, ProvisionCheck};

/// Durable-liveness recorder for the audit relay settle gate.
pub struct PgHeartbeatRecorder {
    repo: AuditRelayProvisionRepo,
    freshness: time::Duration,
}

impl PgHeartbeatRecorder {
    /// `freshness` is the connector's `std::time::Duration` (the clock/type
    /// seam — the storage repo speaks `time::Duration`).
    #[must_use]
    pub fn new(repo: AuditRelayProvisionRepo, freshness: std::time::Duration) -> Self {
        let secs = i64::try_from(freshness.as_secs()).unwrap_or(86_400);
        Self {
            repo,
            freshness: time::Duration::seconds(secs),
        }
    }
}

impl std::fmt::Debug for PgHeartbeatRecorder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PgHeartbeatRecorder")
            .field("freshness", &self.freshness)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl HeartbeatRecorder for PgHeartbeatRecorder {
    async fn record_heartbeat(&self) -> Result<(), sqlx::Error> {
        self.repo.record_heartbeat().await
    }

    async fn heartbeat_fresh(&self) -> Result<bool, sqlx::Error> {
        Ok(matches!(
            self.repo.provision_check(self.freshness).await,
            Ok(ProvisionCheck::Verified(_))
        ))
    }
}
