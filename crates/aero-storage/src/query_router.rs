//! Explicit Postgres query-consistency routing.
//!
//! The router does not infer safety from SQL text. Call sites must choose
//! [`QueryConsistency::Strong`] for authorization, security state, writes,
//! cross-room queries, reconnect convergence, and read-after-write paths.
//! [`QueryConsistency::Eventual`] is reserved for staleness-tolerant reads after
//! the caller has already passed the relevant authorization checks on primary.

use sqlx::PgPool;

/// Consistency contract required by a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryConsistency {
    /// Route to primary. Required for security and read-after-write correctness.
    Strong,
    /// Route to the replica when configured, otherwise naturally to primary.
    Eventual,
}

/// Physical target selected for a consistency contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryTarget {
    Primary,
    Replica,
}

/// Small, clone-cheap router over the primary pool and an optional read replica.
#[derive(Clone)]
pub struct QueryRouter {
    primary: PgPool,
    replica: Option<PgPool>,
}

impl QueryRouter {
    #[must_use]
    pub fn new(primary: PgPool, replica: Option<PgPool>) -> Self {
        Self { primary, replica }
    }

    /// Return the selected shared pool by reference.
    #[must_use]
    pub fn pool(&self, consistency: QueryConsistency) -> &PgPool {
        match self.target(consistency) {
            QueryTarget::Primary => &self.primary,
            QueryTarget::Replica => self
                .replica
                .as_ref()
                .expect("replica target is selected only when configured"),
        }
    }

    /// Clone the selected pool for constructing a repository.
    ///
    /// `PgPool::clone` shares the pool internals; this does not open another
    /// connection pool.
    #[must_use]
    pub fn repo_pool(&self, consistency: QueryConsistency) -> PgPool {
        self.pool(consistency).clone()
    }

    /// Report the physical target without exposing either pool.
    #[must_use]
    pub fn target(&self, consistency: QueryConsistency) -> QueryTarget {
        if consistency == QueryConsistency::Eventual && self.replica.is_some() {
            QueryTarget::Replica
        } else {
            QueryTarget::Primary
        }
    }

    #[must_use]
    pub fn has_replica(&self) -> bool {
        self.replica.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lazy_pool(database: &str) -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .connect_lazy(&format!("postgres://user:password@localhost/{database}"))
            .expect("test URL is valid")
    }

    #[tokio::test]
    async fn absent_replica_routes_both_contracts_to_primary() {
        let router = QueryRouter::new(lazy_pool("primary"), None);
        assert_eq!(
            router.target(QueryConsistency::Strong),
            QueryTarget::Primary
        );
        assert_eq!(
            router.target(QueryConsistency::Eventual),
            QueryTarget::Primary
        );
        assert!(!router.has_replica());
    }

    #[tokio::test]
    async fn configured_replica_is_used_only_for_eventual_reads() {
        let router = QueryRouter::new(lazy_pool("primary"), Some(lazy_pool("read_replica")));
        assert_eq!(
            router.target(QueryConsistency::Strong),
            QueryTarget::Primary
        );
        assert_eq!(
            router.target(QueryConsistency::Eventual),
            QueryTarget::Replica
        );
        assert!(router.has_replica());
    }
}
