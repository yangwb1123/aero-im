//! B5-1 L1 `auth.login.failure` aggregation `db_test` (landing design §6
//! AC-2 companion; connector design §6 AC3).

use super::*;
use crate::audit_governance::outbox::AuditGovernanceOutboxRepo;
use crate::audit_governance::tokens::{L1_AUTH_LOGIN_FAILURE_ACTION, OUTBOUND_AUTH_LOGIN_FAILURE};
use uuid::Uuid;

const WINDOW: i64 = 60;

/// Deterministic v5 `event_id` recomputation (G2 closure — the same formula the
/// aggregator uses; the test never inlines a literal UUID).
fn l1_key(bucket_start_epoch: i64) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!(
            "aero.im.audit.l1:{}:{}:{}",
            WorkspaceId::nil().to_uuid(),
            L1_AUTH_LOGIN_FAILURE_ACTION,
            bucket_start_epoch
        )
        .as_bytes(),
    )
}

/// Seed `n` `login_failures` rows at `at` (explicit `created_at` — the table's
/// default is `now()`, the repo never binds it, so explicit seeding is the only
/// way to place rows in closed buckets).
async fn seed_failures(p: &PgPool, at: time::OffsetDateTime, n: i64) {
    for i in 0..n {
        sqlx::query(
            "INSERT INTO login_failures (account, ip, user_agent, created_at)
                 VALUES ($1, $2, NULL, $3)",
        )
        .bind(format!("l1-auth-{at}-{i}"))
        .bind("203.0.113.1")
        .bind(at)
        .execute(p)
        .await
        .expect("seed login failure");
    }
}

/// AC-3 companion: seed N=3 same-bucket + 2 different-bucket rows (closed
/// buckets only) → aggregate → exactly 2 outbox rows (1 per closed bucket,
/// counts 3/2), class 'message', status 0, priority 10, deterministic v5
/// `event_id`, `payload->>'aggregated' = 'true'` at the ENVELOPE TOP LEVEL,
/// rerun idempotent; an open bucket is never aggregated; base rows stay
/// (forensic retention — the timer never deletes them).
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn login_failure_l1_aggregation_n_to_one() {
    let p = pool();
    reset_governance_table(&p).await;
    let now = time::OffsetDateTime::now_utc();
    let now_epoch = now.unix_timestamp();

    // Two closed buckets (bucket_end <= now - window) + one open bucket.
    // Bucket floors computed off `now` so boundary drift can never move a
    // seeded row across a bucket edge.
    let bucket_a = (now_epoch - 150).div_euclid(WINDOW) * WINDOW; // rows at +30s inside
    let bucket_b = (now_epoch - 210).div_euclid(WINDOW) * WINDOW;
    let open_bucket = (now_epoch - 30).div_euclid(WINDOW) * WINDOW;
    let ts = |bucket: i64| {
        time::OffsetDateTime::from_unix_timestamp(bucket + 30).expect("bucket ts")
    };
    seed_failures(&p, ts(bucket_a), 3).await;
    seed_failures(&p, ts(bucket_b), 2).await;
    seed_failures(&p, ts(open_bucket), 1).await; // open — never aggregated

    let mut repo = AuditGovernanceOutboxRepo::new(p.clone());
    let inserted = repo
        .aggregate_login_failure_buckets(WINDOW)
        .await
        .expect("aggregate closed buckets");
    assert_eq!(inserted, 2, "exactly one row per CLOSED bucket (3 and 2 → 2 rows)");

    let rows: Vec<(String, i32, String, i16, serde_json::Value)> = sqlx::query_as(
        "SELECT event_id::text, status, class, priority, payload
           FROM audit_governance_outbox",
    )
    .fetch_all(&p)
    .await
    .expect("read aggregation rows");
    assert_eq!(rows.len(), 2, "exactly 2 aggregation rows");
    for (event_id, status, class, priority, payload) in &rows {
        assert_eq!(status, &0, "status 0 = enqueued");
        assert_eq!(
            class, GOVERNANCE_CLASS_MESSAGE,
            "class 'message' (D2 outcome A — the aggregation row is message-domain)"
        );
        assert_eq!(priority, &10, "priority 10 = GOVERNANCE_PRIORITY_BACKLOG");
        assert_eq!(
            payload["aggregated"], serde_json::Value::Bool(true),
            "payload->>'aggregated' = 'true' at the ENVELOPE TOP LEVEL (0242-shared parity-exemption key — never inside detail)"
        );
        assert_eq!(payload["action"], OUTBOUND_AUTH_LOGIN_FAILURE);
        assert_eq!(payload["source_system"], crate::audit_governance::tokens::AUTH_SOURCE_SYSTEM);
        let window_start: i64 = payload["payload"]["window_start_epoch"]
            .as_i64()
            .expect("window_start_epoch");
        let count: i64 = payload["payload"]["count"].as_i64().expect("count");
        assert_eq!(
            event_id,
            &l1_key(window_start).to_string(),
            "deterministic v5 event_id (recomputed, never inline)"
        );
        assert_eq!(payload["payload"]["window_secs"], serde_json::json!(WINDOW));
        // The bucket of this row's count: 3 (bucket_a) or 2 (bucket_b).
        assert!(
            count == 3 || count == 2,
            "count is the closed bucket's N (3 or 2), got {count}"
        );
        assert_eq!(
            window_start,
            if count == 3 { bucket_a } else { bucket_b },
            "window_start_epoch = the bucket floor"
        );
        // No audit_events row (D7 exemption — no 0236 v1 side effect, no
        // default-workspace audit-view pollution). Scoped by the synthetic
        // event ids: `audit_events` accumulates across the run (module
        // discipline — tests scope, never full-table).
        let synthetic: Vec<uuid::Uuid> = rows
            .iter()
            .map(|(event_id, ..)| uuid::Uuid::parse_str(event_id).expect("event_id uuid"))
            .collect();
        let audit_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE id = ANY($1)")
                .bind(&synthetic)
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(
            audit_rows, 0,
            "aggregation rows never create audit_events rows (D7)"
        );
    }

    // Rerun: idempotent — same 2 rows, counts unchanged (ON CONFLICT + closed
    // buckets can no longer receive rows).
    let inserted = repo
        .aggregate_login_failure_buckets(WINDOW)
        .await
        .expect("rerun aggregation");
    assert_eq!(inserted, 0, "rerun inserts nothing (idempotent)");
    let rows_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_governance_outbox")
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(rows_after, 2, "rerun leaves exactly the same 2 rows");

    // Base rows were never deleted (forensic retention — the timer owns no
    // deletion; the retention sweep does).
    let base: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM login_failures")
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(base, 6, "aggregation never deletes login_failures base rows");

    // Hygiene: remove the seeded base rows (shared-DB discipline — later
    // runs must not re-aggregate this test's buckets).
    sqlx::query("DELETE FROM login_failures WHERE ip = '203.0.113.1'")
        .execute(&p)
        .await
        .expect("cleanup seeded failures");
    let _ = open_bucket; // referenced by the seed placement above
}
