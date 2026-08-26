//! Audit-scope provisioning seam for the B5-4 relay.
//!
//! Participant JWTs deliberately do not carry application scopes.  The
//! `audit:event:write` capability is therefore represented by this injected,
//! fail-closed predicate instead.  The relay and future machine-token
//! endpoints can share the same decision without coupling identity claims to
//! the operational health of the audit sink.

use std::sync::Arc;

use async_trait::async_trait;

use crate::relay_gate::SharedRelayProvisionGate;

/// Answers whether the audit relay is currently provisioned for
/// `audit:event:write`.
///
/// Implementations must collapse database/query failures to `false`; the
/// trait intentionally has no error channel so callers cannot accidentally
/// turn an unavailable provisioning source into an allow decision.
#[async_trait]
pub trait RelayScopeProvisioner: Send + Sync {
    async fn audit_event_write_provisioned(&self) -> bool;
}

/// Cheap-to-clone provisioner handle shared by auth and relay consumers.
pub type SharedRelayScopeProvisioner = Arc<dyn RelayScopeProvisioner>;

/// Database-backed Q0 provisioner.  Query failures are converted to `false`
/// by the trait implementation, preserving the fail-closed contract.
#[derive(Debug, Clone)]
pub struct PgRelayScopeProvisioner {
    repo: aero_storage::RelayScopeRepo,
}

impl PgRelayScopeProvisioner {
    #[must_use]
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self {
            repo: aero_storage::RelayScopeRepo::new(pool),
        }
    }
}

#[async_trait]
impl RelayScopeProvisioner for PgRelayScopeProvisioner {
    async fn audit_event_write_provisioned(&self) -> bool {
        match self.repo.q0_provisioned().await {
            Ok(provisioned) => provisioned,
            Err(error) => {
                tracing::warn!(
                    ?error,
                    "audit scope provisioning query failed; denying capability"
                );
                false
            }
        }
    }
}

/// Merged B5-4 capability: the commercial Q0 predicate and the durable relay
/// heartbeat must both be true.  Either leg failing denies the scope.
pub struct ComposedProvisioner {
    q0: PgRelayScopeProvisioner,
    heartbeat: SharedRelayProvisionGate,
}

impl ComposedProvisioner {
    #[must_use]
    pub fn new(q0: PgRelayScopeProvisioner, heartbeat: SharedRelayProvisionGate) -> Self {
        Self { q0, heartbeat }
    }
}

#[async_trait]
impl RelayScopeProvisioner for ComposedProvisioner {
    async fn audit_event_write_provisioned(&self) -> bool {
        self.q0.audit_event_write_provisioned().await && self.heartbeat.verified().await
    }
}
