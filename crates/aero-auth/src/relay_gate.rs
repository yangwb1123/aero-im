//! Durable relay-heartbeat verification leg of the B5-4 scope capability.
//!
//! The scope grant is composed in [`crate::relay_scope::ComposedProvisioner`]
//! with the Q0 commercial-binding predicate.  This module intentionally owns
//! only the heartbeat trait and its `PostgreSQL` adapter; the single
//! `AuthService::assert_audit_scope_provisioned` rejection point remains in
//! the `relay_scope` seam.

use std::sync::Arc;

use async_trait::async_trait;

/// Answers whether the durable relay heartbeat is present and fresh.
#[async_trait]
pub trait RelayProvisionGate: Send + Sync {
    async fn verified(&self) -> bool;
}

/// Shared heartbeat gate handle.
pub type SharedRelayProvisionGate = Arc<dyn RelayProvisionGate>;

/// `PostgreSQL` implementation over the durable heartbeat singleton.
#[derive(Debug, Clone)]
pub struct PgRelayProvisionGate {
    repo: aero_storage::AuditRelayProvisionRepo,
    freshness: time::Duration,
}

impl PgRelayProvisionGate {
    #[must_use]
    pub fn new(pool: sqlx::PgPool, freshness: time::Duration) -> Self {
        Self {
            repo: aero_storage::AuditRelayProvisionRepo::new(pool),
            freshness,
        }
    }
}

#[async_trait]
impl RelayProvisionGate for PgRelayProvisionGate {
    async fn verified(&self) -> bool {
        matches!(
            self.repo.provision_check(self.freshness).await,
            Ok(aero_storage::ProvisionCheck::Verified(_))
        )
    }
}
