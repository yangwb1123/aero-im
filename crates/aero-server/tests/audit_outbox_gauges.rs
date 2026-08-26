//! Cross-crate and live-Postgres acceptance tests for the governance outbox
//! health sampler.
//!
//! The plain tests keep the SQL and Prometheus names pinned without services.
//! The ignored tests are deliberately self-isolating: they truncate the
//! throwaway outbox before seeding, compare the sampler with the aero-eng SQL
//! oracle, and snapshot mutable columns before/after to prove the sampler is
//! read-only.

use aero_ai::audit_outbox_health::{sample_outbox_health, set_audit_outbox_gauges};
use aero_common::metrics::{names, Registry};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::env;
use time::OffsetDateTime;
use uuid::Uuid;

const TABLE: &str = "audit_governance_outbox";

#[test]
fn sampler_sql_is_pinned_to_aero_eng_oracle() {
    assert_eq!(
        aero_ai::audit_outbox_health::Q3_SQL,
        aero_eng::audit_provision::Q3_SQL
    );
    assert_eq!(
        aero_ai::audit_outbox_health::Q4_SQL,
        aero_eng::audit_provision::Q4_SQL
    );
}

#[test]
fn five_gauge_names_are_pinned_to_the_contract() {
    assert_eq!(names::AUDIT_OUTBOX_PENDING, "aero_audit_outbox_pending");
    assert_eq!(names::AUDIT_OUTBOX_CLAIMED, "aero_audit_outbox_claimed");
    assert_eq!(names::AUDIT_OUTBOX_DELIVERED, "aero_audit_outbox_delivered");
    assert_eq!(names::AUDIT_OUTBOX_DEAD, "aero_audit_outbox_dead");
    assert_eq!(
        names::AUDIT_OUTBOX_OLDEST_PENDING_SECS,
        "aero_audit_outbox_oldest_pending_secs"
    );
}

async fn connect_live_db() -> PgPool {
    let url = env::var("DATABASE_URL")
        .expect("DATABASE_URL is required for the ignored audit outbox test");
    PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to the migrated throwaway database")
}

async fn reset_outbox(pool: &PgPool) {
    sqlx::query("TRUNCATE audit_governance_outbox")
        .execute(pool)
        .await
        .expect("reset governance outbox for an isolated test");
}

type RowSnapshot = (
    Uuid,
    i32,
    i64,
    Option<Uuid>,
    Option<OffsetDateTime>,
    Option<OffsetDateTime>,
    Option<String>,
    OffsetDateTime,
    OffsetDateTime,
);

async fn snapshot_rows(pool: &PgPool) -> Vec<RowSnapshot> {
    sqlx::query_as::<_, RowSnapshot>(
        "SELECT event_id, status, attempts, claim_token, lease_expires_at,
                delivered_at, last_error, available_at, created_at
           FROM audit_governance_outbox
          ORDER BY event_id",
    )
    .fetch_all(pool)
    .await
    .expect("snapshot governance outbox rows")
}

fn assert_gauge_value(rendered: &str, name: &str, expected: i64) {
    let expected_line = format!("{name} {expected}");
    assert!(
        rendered.lines().any(|line| line == expected_line),
        "missing gauge line {expected_line} in {rendered}"
    );
}

async fn seed_four_statuses(pool: &PgPool) {
    let pending = Uuid::new_v4();
    let claimed = Uuid::new_v4();
    let delivered = Uuid::new_v4();
    let dead = Uuid::new_v4();
    let payload = serde_json::json!({
        "event_id": "seed",
        "source_system": "aero-im.source",
        "action": "message.moderated"
    });

    sqlx::query(
        "INSERT INTO audit_governance_outbox
             (event_id, status, attempts, priority, payload, available_at, created_at)
         VALUES ($1, 0, 0, 10, $2, clock_timestamp() - interval '120 seconds',
                 clock_timestamp() - interval '120 seconds')",
    )
    .bind(pending)
    .bind(&payload)
    .execute(pool)
    .await
    .expect("seed pending row");

    sqlx::query(
        "INSERT INTO audit_governance_outbox
             (event_id, status, attempts, claim_token, lease_expires_at, payload)
         VALUES ($1, 1, 2, $2, clock_timestamp() + interval '30 seconds', $3)",
    )
    .bind(claimed)
    .bind(Uuid::new_v4())
    .bind(&payload)
    .execute(pool)
    .await
    .expect("seed claimed row");

    sqlx::query(
        "INSERT INTO audit_governance_outbox
             (event_id, status, attempts, delivered_at, payload)
         VALUES ($1, 2, 3, clock_timestamp(), $2)",
    )
    .bind(delivered)
    .bind(&payload)
    .execute(pool)
    .await
    .expect("seed delivered row");

    sqlx::query(
        "INSERT INTO audit_governance_outbox
             (event_id, status, attempts, last_error, payload)
         VALUES ($1, 3, 4, 'probe dead row', $2)",
    )
    .bind(dead)
    .bind(payload)
    .execute(pool)
    .await
    .expect("seed dead row");
}

#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn sampler_matches_sql_gauges_and_is_read_only() {
    let pool = connect_live_db().await;
    reset_outbox(&pool).await;
    seed_four_statuses(&pool).await;
    let before = snapshot_rows(&pool).await;

    let counts = sample_outbox_health(&pool)
        .await
        .expect("sample outbox health")
        .expect("0239 table exists in migrated test database");

    let q3 = aero_eng::audit_provision::Q3_SQL.replace("{table}", TABLE);
    let buckets: Vec<(i32, i64)> = sqlx::query_as(&q3)
        .fetch_all(&pool)
        .await
        .expect("run Q3 oracle");
    let mut expected = [0_i64; 4];
    for (status, count) in buckets {
        if let Ok(index) = usize::try_from(status) {
            if let Some(bucket) = expected.get_mut(index) {
                *bucket = count;
            }
        }
    }
    assert_eq!(
        [
            counts.pending,
            counts.claimed,
            counts.delivered,
            counts.dead
        ],
        expected
    );

    let q4 = aero_eng::audit_provision::Q4_SQL.replace("{table}", TABLE);
    let expected_oldest: Option<i64> = sqlx::query_scalar(&q4)
        .fetch_one(&pool)
        .await
        .expect("run Q4 oracle");
    assert_eq!(counts.oldest_pending_secs, expected_oldest);

    let registry = Registry::new();
    set_audit_outbox_gauges(&registry, &counts);
    let rendered = registry.render_prometheus();
    assert_gauge_value(&rendered, names::AUDIT_OUTBOX_PENDING, expected[0]);
    assert_gauge_value(&rendered, names::AUDIT_OUTBOX_CLAIMED, expected[1]);
    assert_gauge_value(&rendered, names::AUDIT_OUTBOX_DELIVERED, expected[2]);
    assert_gauge_value(&rendered, names::AUDIT_OUTBOX_DEAD, expected[3]);
    assert_gauge_value(
        &rendered,
        names::AUDIT_OUTBOX_OLDEST_PENDING_SECS,
        expected_oldest.unwrap_or(0),
    );

    let after = snapshot_rows(&pool).await;
    assert_eq!(before, after, "sampling must not mutate any outbox state");
    reset_outbox(&pool).await;
}

#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn t11_pending_attempts_never_look_dead() {
    const ROWS: i64 = 4;
    let pool = connect_live_db().await;
    reset_outbox(&pool).await;
    for attempts in 1..=ROWS {
        sqlx::query(
            "INSERT INTO audit_governance_outbox
                 (event_id, status, attempts, payload, available_at)
             VALUES ($1, 0, $2, $3, clock_timestamp() - interval '5 seconds')",
        )
        .bind(Uuid::new_v4())
        .bind(attempts)
        .bind(serde_json::json!({
            "event_id": format!("t11-{attempts}"),
            "source_system": "aero-im.source"
        }))
        .execute(&pool)
        .await
        .expect("seed T-11 pending row");
    }
    let before = snapshot_rows(&pool).await;
    let counts = sample_outbox_health(&pool)
        .await
        .expect("sample T-11 posture")
        .expect("0239 table exists in migrated test database");
    assert_eq!(counts.pending, ROWS);
    assert_eq!(counts.claimed, 0);
    assert_eq!(counts.delivered, 0);
    assert_eq!(counts.dead, 0);
    assert!(counts.oldest_pending_secs.is_some());

    let non_pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_governance_outbox WHERE status IN (1, 2, 3)",
    )
    .fetch_one(&pool)
    .await
    .expect("count non-pending T-11 rows");
    assert_eq!(non_pending, 0);
    assert_eq!(before, snapshot_rows(&pool).await);
    reset_outbox(&pool).await;
}
