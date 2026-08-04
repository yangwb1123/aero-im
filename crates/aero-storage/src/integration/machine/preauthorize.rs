//! Read-only ingress authorization before an expensive upload body is read.

use aero_common::Error;
use uuid::Uuid;

use super::{authorized_installation_in_tx, prepare_target_in_tx, PreparedIntegrationTarget};
use crate::integration::{IntegrationRepo, IntegrationTarget};

impl IntegrationRepo {
    /// Check mutable installation and target policy without creating a request
    /// row or direct room. The post-body claim and commit repeat these checks.
    pub async fn preauthorize_target(
        &self,
        installation_id: Uuid,
        issuer: &str,
        client_id: &str,
        target: &IntegrationTarget,
    ) -> Result<PreparedIntegrationTarget, Error> {
        let mut tx = self.pool.begin().await?;
        let installation =
            authorized_installation_in_tx(&mut tx, installation_id, issuer, client_id).await?;
        let prepared = prepare_target_in_tx(&mut tx, installation, target).await?;
        tx.commit().await?;
        Ok(prepared)
    }
}
