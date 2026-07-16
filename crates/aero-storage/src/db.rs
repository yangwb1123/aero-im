//! Postgres pool and migrations.
//!
//! Resilience improvements (roadmap 9th analysis 方向二):
//! - `min_connections(2)` — warm start, no cold-start acquire latency
//! - `test_before_acquire(true)` — verify connection health before handing it out
//! - `after_connect` callback — sets `statement_timeout = '10s'` on each connection
//!   to prevent a single slow query from occupying the slot indefinitely

use std::time::Duration;

use sqlx::postgres::PgPoolOptions;

pub type PgPool = sqlx::PgPool;

/// Default timeout for acquiring a connection from the pool.
const POOL_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

/// Idle timeout: a connection sitting unused for this long is closed and the
/// slot released back to Postgres.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// Minimum idle connections kept warm. Avoids cold-start acquire latency on
/// the first request after a period of low traffic.
const POOL_MIN_CONNECTIONS: u32 = 2;

/// Per-query statement timeout set on every connection via `after_connect`.
/// Prevents a single slow query from occupying a pool slot for minutes.
/// 10 seconds covers the vast majority of normal queries; the search path
/// (which may run slower trigram scans) currently has its own code-level
/// guard (MAX_SEARCH_LIMIT) so it stays within this window.
const STATEMENT_TIMEOUT_SQL: &str = "SET statement_timeout = '10000'";

pub async fn connect_pg(url: &str, max_conns: u32) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_conns)
        .min_connections(POOL_MIN_CONNECTIONS)
        .acquire_timeout(POOL_ACQUIRE_TIMEOUT)
        .idle_timeout(POOL_IDLE_TIMEOUT)
        .test_before_acquire(true)
        .after_connect(|conn, _meta| Box::pin(async move {
            sqlx::query(STATEMENT_TIMEOUT_SQL).execute(conn).await?;
            Ok(())
        }))
        .connect(url)
        .await
}

/// Runs embedded migrations from the workspace `migrations/` directory.
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("../../migrations").run(pool).await
}
