//! Bounded PostgreSQL coverage for the source-owned revision primitives.
//!
//! The account-summary endpoint still uses its legacy wall-clock version until
//! Aero IM has an owner-approved mapping (or the approved canonical-state
//! fingerprint implementation) from the opaque signed target account ID to
//! source mutations. These tests therefore cover only the exact-key durable
//! allocator and do not claim that endpoint integration is complete.

use std::time::Duration;

use sqlx::{postgres::PgPoolOptions, PgPool};

async fn connect_pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL must point at a disposable migrated Aero IM database");
    PgPoolOptions::new()
        .max_connections(16)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&url)
        .await
        .expect("connect to migrated Aero IM database")
}

async fn revision(pool: &PgPool, account_id: &str) -> i64 {
    sqlx::query_scalar("SELECT revision FROM account_summary_revisions WHERE account_id = $1")
        .bind(account_id)
        .fetch_one(pool)
        .await
        .expect("read durable summary revision")
}

#[tokio::test]
#[ignore = "requires a disposable migrated Aero IM PostgreSQL database"]
async fn exact_opaque_account_allocator_is_monotonic_across_restart_and_clock_rollback() {
    let pool = connect_pool().await;
    let account = "opaque-account:not-a-participant-uuid";
    let calls = 12;
    let mut tasks = Vec::with_capacity(calls);
    for _ in 0..calls {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            sqlx::query_scalar::<_, i64>("SELECT next_account_summary_revision($1)")
                .bind(account)
                .fetch_one(&pool)
                .await
                .expect("allocate concurrent revision")
        }));
    }
    let mut allocated = Vec::with_capacity(calls);
    for task in tasks {
        allocated.push(task.await.expect("allocator task"));
    }
    allocated.sort_unstable();
    assert_eq!(allocated, (1..=calls as i64).collect::<Vec<_>>());

    // updated_at is informational and must not influence the revision.
    sqlx::query(
        "UPDATE account_summary_revisions SET updated_at = '1970-01-01'
         WHERE account_id = $1",
    )
    .bind(account)
    .execute(&pool)
    .await
    .expect("simulate source clock rollback");

    // A new pool has no process-local allocator state.
    pool.close().await;
    let restarted = connect_pool().await;
    let next: i64 = sqlx::query_scalar("SELECT next_account_summary_revision($1)")
        .bind(account)
        .fetch_one(&restarted)
        .await
        .expect("allocate after restart");
    assert_eq!(next, calls as i64 + 1);
    assert_eq!(revision(&restarted, account).await, next);
}

#[test]
fn migration_and_source_keep_signed_account_identity_opaque() {
    let migration = include_str!("../../../migrations/0249_account_summary_durable_revisions.sql");
    let source = include_str!("../src/integrations/account_summary.rs");

    assert!(migration.contains("account_id TEXT PRIMARY KEY"));
    assert!(migration.contains("ON CONFLICT (account_id) DO UPDATE"));
    assert!(source.contains("\"source_account_id\": target.account_id"));
    assert!(!migration.contains("participant_id::text"));
}
