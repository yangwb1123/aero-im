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

#[cfg(test)]
mod tests {
    use aero_audit_connector::heartbeat::HeartbeatRecorder as _;
    use aero_storage::audit_relay_provision::AuditRelayProvisionRepo;

    use super::PgHeartbeatRecorder;

    fn pool(url: &str) -> sqlx::PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(url)
            .expect("well-formed DATABASE_URL")
    }

    /// Self-isolating start/end: the singleton row is global (shared
    /// throwaway DB) — a re-run or a crashed sibling test must never leak a
    /// stale/absent state into the freshness assertions.
    async fn reset(pool: &sqlx::PgPool) {
        sqlx::query("DELETE FROM audit_relay_provisioning")
            .execute(pool)
            .await
            .expect("reset the provisioning singleton");
    }

    /// QA F-2 — the bootstrap arm end-to-end over a real DB: the adapter's
    /// `record_heartbeat()` (the server's one-shot bootstrap at relay spawn)
    /// must land a row that `provision_check` reads back as Verified — the
    /// settle fence is armed before the first event. The std→time `Duration`
    /// conversion is exercised by construction (300s in both units).
    #[tokio::test]
    #[ignore = "requires live Postgres (DATABASE_URL)"]
    async fn bootstrap_record_heartbeat_arms_the_fence() {
        let url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL must point at a throwaway Postgres");
        let pool = pool(&url);
        reset(&pool).await;
        let recorder = PgHeartbeatRecorder::new(
            AuditRelayProvisionRepo::new(pool.clone()),
            std::time::Duration::from_secs(300),
        );
        assert!(
            !recorder.heartbeat_fresh().await.expect("freshness check"),
            "no row yet → fail-closed NotVerified (fence closed before bootstrap)"
        );
        recorder
            .record_heartbeat()
            .await
            .expect("bootstrap heartbeat must land");
        assert!(
            recorder.heartbeat_fresh().await.expect("freshness check"),
            "a landed bootstrap heartbeat arms the settle fence"
        );
        reset(&pool).await;
    }

    /// QA F-2 — missing-table variant (bootstrap could not land, e.g. the
    /// 0248 migration never ran): the adapter stays fail-closed —
    /// `heartbeat_fresh()` is false even though the row is absent, and a
    /// `record_heartbeat()` attempt does not fabricate freshness. The table
    /// is restored exactly (self-isolation) so sibling tests are unaffected.
    #[tokio::test]
    #[ignore = "requires live Postgres (DATABASE_URL)"]
    async fn missing_table_heartbeat_stays_fail_closed() {
        let url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL must point at a throwaway Postgres");
        let pool = pool(&url);
        sqlx::query("DROP TABLE IF EXISTS audit_relay_provisioning")
            .execute(&pool)
            .await
            .expect("drop the provisioning table");
        let recorder = PgHeartbeatRecorder::new(
            AuditRelayProvisionRepo::new(pool.clone()),
            std::time::Duration::from_secs(300),
        );
        assert!(
            !recorder.heartbeat_fresh().await.expect("freshness check"),
            "missing table → fail-closed false (never a fabricated green)"
        );
        assert!(
            recorder.record_heartbeat().await.is_err(),
            "record on a missing table must error (the tick logs + retries, FM-B)"
        );
        // Restore the 0248 DDL exactly (self-isolation for sibling tests).
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS audit_relay_provisioning (
                singleton       BOOLEAN      PRIMARY KEY DEFAULT TRUE CHECK (singleton),
                verified_at     TIMESTAMPTZ  NOT NULL,
                provision_state TEXT         NOT NULL DEFAULT 'provisioned',
                updated_at      TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp()
            )",
        )
        .execute(&pool)
        .await
        .expect("restore the provisioning table");
        reset(&pool).await;
    }
}
