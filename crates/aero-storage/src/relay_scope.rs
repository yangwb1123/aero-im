//! Q0 audit-scope provisioning predicate.
//!
//! This is deliberately separate from the durable relay heartbeat repository:
//! Q0 answers whether the commercial runtime has an enabled binding at all,
//! while the heartbeat answers whether a relay recently proved liveness.

use sqlx::PgPool;

/// Read-only repository for the B5-4 Q0 relay-health predicate.
#[derive(Debug, Clone)]
pub struct RelayScopeRepo {
    pool: PgPool,
}

impl RelayScopeRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Return `(runtime.enabled && enabled_bindings > 0)`.  A missing row or
    /// schema error is returned to the caller, which must fail closed.
    pub async fn q0_provisioned(&self) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (bool, i64)>(
            // Keep this text in lock-step with aero-eng's Q0_SQL.  The query
            // intentionally does not use SnaplinkCommercialRepo::ready().
            r"SELECT runtime.enabled,
                      (SELECT count(*) FROM snaplink_commercial_bindings
                        WHERE enabled)
                 FROM snaplink_commercial_runtime runtime
                WHERE runtime.singleton",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some_and(|(enabled, bindings)| enabled && bindings > 0))
    }
}

#[cfg(test)]
mod tests {
    use super::RelayScopeRepo;

    #[tokio::test]
    async fn q0_repo_can_be_constructed_without_eager_db_connection() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://aero:aero_dev_pw@localhost:5432/aero")
            .expect("well-formed URL");
        let _repo = RelayScopeRepo::new(pool);
    }
}
