//! Postgres pool and migrations.

use std::time::Duration;

use sqlx::postgres::PgPoolOptions;

pub type PgPool = sqlx::PgPool;

/// Default timeout for acquiring a connection from the pool.
///
/// Without this, callers block indefinitely when the pool is exhausted — a
/// single slow endpoint can cascade into a full request backlog. 30 s gives
/// upstream load-balancers enough time to detect the stall and retry on a
/// different node while keeping the slot available for bursty traffic.
const POOL_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn connect_pg(url: &str, max_conns: u32) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_conns)
        .acquire_timeout(POOL_ACQUIRE_TIMEOUT)
        .connect(url)
        .await
}

/// Runs embedded migrations from the workspace `migrations/` directory.
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("../../migrations").run(pool).await
}
