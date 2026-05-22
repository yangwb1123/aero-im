//! Postgres pool and migrations.

use sqlx::postgres::PgPoolOptions;

pub type PgPool = sqlx::PgPool;

pub async fn connect_pg(url: &str, max_conns: u32) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_conns)
        .connect(url)
        .await
}

/// Runs embedded migrations from the workspace `migrations/` directory.
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("../../migrations").run(pool).await
}
