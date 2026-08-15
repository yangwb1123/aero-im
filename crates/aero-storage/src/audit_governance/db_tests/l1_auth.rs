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

/// Poll the aggregator's boundary expression (the exact SQL
/// `aggregate_login_failure_buckets` scans with) until it reaches `target`.
/// Waits for a REAL condition — the DB clock crossing a bucket edge — never
/// a hard sleep (testing-spec discipline). The boundary window where
/// `boundary == target` is one full `window_secs` wide, so the caller's
/// aggregation call (milliseconds after this returns) runs inside it.
///
/// Bounded: a stuck clock or a preempted test panics instead of hanging the
/// suite.
async fn wait_for_boundary(p: &PgPool, window_secs: i64, target: i64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        let boundary: i64 = sqlx::query_scalar(
            "SELECT floor(extract(epoch FROM clock_timestamp()) / $1)::bigint * $1 - $1",
        )
        .bind(window_secs)
        .fetch_one(p)
        .await
        .expect("boundary poll");
        if boundary >= target {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "DB clock boundary did not advance to {target} within 90s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Async-reviewer Critical #1 regression (design gate run 386d075c): rows
/// that land in a bucket while it is still OPEN must be counted exactly once
/// when the bucket closes — across SEVERAL boot phases — never permanently
/// lost to a watermark that advanced past them.
///
/// The pre-fix watermark advanced to the global `MAX(created_at)` (an
/// open-bucket timestamp) BEFORE the insert loop: every bucket permanently
/// lost its first `(boot mod W)` seconds — `ON CONFLICT DO NOTHING` froze the
/// undercount and a restart did not heal it (the partial-count outbox row
/// already existed). The fix (629c5e9) advances the watermark to the
/// closed-bucket BOUNDARY, AFTER the insert loop, so a row written into a
/// bucket that is still open is scanned at the first tick whose boundary
/// passes its bucket end.
///
/// Timeline (w = 3s buckets, tick 1 at boundary B1):
///   tick 1: rows seeded at B1 + w/2 land in the OPEN bucket [B1, B1+w) —
///           skipped by the scan; wm := B1
///   tick 2: the DB clock closes [B1, B1+w) → counted (count 2); a second
///           cohort seeded at B1 + w + w/2 lands in the open [B1+w, B1+2w)
///   tick 3: the second bucket closes → counted (count 3)
/// final: exactly 2 outbox rows (counts 2 and 3), deterministic v5 keys,
/// message class, zero `audit_events` rows (D7), base rows retained, rerun
/// idempotent. The pre-fix code yields ZERO rows here (wm permanently skips
/// every cohort) — this test is red on the bug, green on the fix.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn l1_auth_multi_tick_rollover_never_loses_rows() {
    const W: i64 = 3;
    let p = pool();
    reset_governance_table(&p).await;
    // DB-clock domain (single clock discipline): the seed timestamps and the
    // expected bucket floors derive from the same boundary expression the
    // aggregator scans with.
    let b1: i64 = sqlx::query_scalar(
        "SELECT floor(extract(epoch FROM clock_timestamp()) / $1)::bigint * $1 - $1",
    )
    .bind(W)
    .fetch_one(&p)
    .await
    .expect("initial boundary");
    let ts = |at: i64| time::OffsetDateTime::from_unix_timestamp(at).expect("bucket ts");

    // Cohort 1: written into the bucket [b1, b1+w) while it is STILL OPEN
    // (created_at = b1 + w/2 < b1 + w — the tick-1 scan requires
    // `created_at < boundary = b1`, so these are skipped).
    seed_failures(&p, ts(b1 + W / 2), 2).await;

    let mut repo = AuditGovernanceOutboxRepo::new(p.clone());
    let inserted = repo
        .aggregate_login_failure_buckets(W)
        .await
        .expect("tick 1 aggregation");
    assert_eq!(inserted, 0, "tick 1: the open bucket is never aggregated");

    // Cohort 2: written while [b1+w, b1+2w) is still open, after the clock
    // has closed the first bucket.
    wait_for_boundary(&p, W, b1 + W).await;
    seed_failures(&p, ts(b1 + W + W / 2), 3).await;
    let inserted = repo
        .aggregate_login_failure_buckets(W)
        .await
        .expect("tick 2 aggregation");
    assert_eq!(
        inserted, 1,
        "tick 2 closes [b1, b1+w): exactly one new row (count 2) — cohort 1 counted"
    );

    // Third boot phase: close the second bucket.
    wait_for_boundary(&p, W, b1 + 2 * W).await;
    let inserted = repo
        .aggregate_login_failure_buckets(W)
        .await
        .expect("tick 3 aggregation");
    assert_eq!(
        inserted, 1,
        "tick 3 closes [b1+w, b1+2w): exactly one new row (count 3) — cohort 2 counted"
    );

    // Final state: one row per closed bucket, counts {2, 3}, deterministic
    // v5 keys (recomputed, never inline), class message, status 0.
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT event_id::text,
                (payload->'payload'->>'count')::bigint,
                (payload->'payload'->>'window_start_epoch')::bigint
           FROM audit_governance_outbox
          ORDER BY payload->'payload'->>'window_start_epoch'",
    )
    .fetch_all(&p)
    .await
    .expect("read rollover rows");
    assert_eq!(rows.len(), 2, "exactly one row per closed bucket (2 + 3 → 2 rows)");
    for (idx, (event_id, count, window_start)) in rows.iter().enumerate() {
        let (expected_start, expected_count) = if idx == 0 { (b1, 2) } else { (b1 + W, 3) };
        assert_eq!(window_start, &expected_start, "window_start_epoch = bucket floor");
        assert_eq!(count, &expected_count, "count = the closed bucket's N");
        assert_eq!(
            event_id,
            &l1_key(expected_start).to_string(),
            "deterministic v5 event_id (recomputed, never inline)"
        );
    }
    // No audit_events rows (D7 — synthetic ids, no 0236 v1 side effect).
    let synthetic: Vec<uuid::Uuid> = rows
        .iter()
        .map(|(e, _, _)| uuid::Uuid::parse_str(e).expect("event_id uuid"))
        .collect();
    let audit_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE id = ANY($1)")
            .bind(&synthetic)
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(audit_rows, 0, "aggregation rows never create audit_events rows (D7)");
    // Base rows retained (forensic retention) + rerun idempotent.
    let base: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM login_failures")
        .fetch_one(&p)
        .await
        .unwrap();
    assert_eq!(base, 5, "all 5 seeded rows retained (the timer never deletes)");
    assert_eq!(
        repo.aggregate_login_failure_buckets(W).await.expect("rerun"),
        0,
        "rerun inserts nothing (idempotent)"
    );

    // Hygiene: remove this test's seeded base rows (shared-DB discipline).
    sqlx::query("DELETE FROM login_failures WHERE ip = '203.0.113.1'")
        .execute(&p)
        .await
        .expect("cleanup seeded failures");
}

/// Async-reviewer High #2 regression (design gate run 386d075c): a mid-loop
/// insert failure must leave the watermark BEHIND the scanned rows so the
/// next tick rescans them (deduped by `ON CONFLICT DO NOTHING`) and
/// completes the batch — the watermark advance happens AFTER the insert
/// loop, never before it.
///
/// The pre-fix code advanced the watermark unconditionally BEFORE the insert
/// loop: a transient failure (pool timeout under a credential storm — the
/// exact scenario the feature exists for) permanently lost the whole tick's
/// buckets ("it never drops rows" was false; only a process restart
/// recovered, and the restart did not fix the rollover undercount).
///
/// Fixture: a temporary BEFORE INSERT trigger raises for exactly the second
/// bucket's deterministic v5 `event_id` (the older bucket inserts first —
/// `ORDER BY bucket_start`). After the failure the trigger is dropped and
/// the same repo re-aggregates immediately (no wall-clock wait needed — the
/// watermark never moved): bucket A is deduped, bucket B completes.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn l1_auth_watermark_not_advanced_on_mid_loop_failure() {
    const W: i64 = 60;
    let p = pool();
    reset_governance_table(&p).await;
    let b0: i64 = sqlx::query_scalar(
        "SELECT floor(extract(epoch FROM clock_timestamp()) / $1)::bigint * $1 - $1",
    )
    .bind(W)
    .fetch_one(&p)
    .await
    .expect("initial boundary");
    let ts = |at: i64| time::OffsetDateTime::from_unix_timestamp(at).expect("bucket ts");
    // Two long-closed buckets (both < the tick-1 boundary): A is older and
    // aggregates first, B is the one whose insert the trigger fails.
    let bucket_a = b0 - 2 * W;
    let bucket_b = b0 - W;
    seed_failures(&p, ts(bucket_a + W / 2), 2).await;
    seed_failures(&p, ts(bucket_b + W / 2), 2).await;

    // Forcing fixture: a unique trigger raising for exactly bucket B's v5
    // event_id. **Fixture hygiene (code-architecture-reviewer)**: the names
    // are unique PER RUN (v5 key + a per-run random suffix) — deterministic
    // names derived from the bucket key collided across parallel runs on one
    // DB and left residue that poisoned reruns within the same bucket window
    // ("already exists"). A crashed run's residue can never collide with a
    // later run; the DROP IF EXISTS hygiene below stays best-effort.
    let v5_b = l1_key(bucket_b);
    // Hyphen-stripped uuid so the name is a valid unquoted SQL identifier
    // (a hyphenated name would parse as subtraction).
    let fn_suffix = v5_b.to_string().replace('-', "");
    let run_suffix = uuid::Uuid::new_v4().to_string().replace('-', "");
    let fn_name = format!("raise_for_l1_bucket_{fn_suffix}_{run_suffix}");
    let tg_name = format!("tg_l1_fail_{fn_suffix}_{run_suffix}");
    sqlx::query(&format!(
        "CREATE FUNCTION {fn_name}() RETURNS trigger AS $$
           BEGIN
             IF NEW.event_id = '{v5_b}'::uuid THEN
               RAISE EXCEPTION 'forced mid-loop L1 insert failure (regression fixture)';
             END IF;
             RETURN NEW;
           END
         $$ LANGUAGE plpgsql"
    ))
    .execute(&p)
    .await
    .expect("create forcing function");
    sqlx::query(&format!(
        "CREATE TRIGGER {tg_name} BEFORE INSERT ON audit_governance_outbox
           FOR EACH ROW EXECUTE FUNCTION {fn_name}()"
    ))
    .execute(&p)
    .await
    .expect("create forcing trigger");

    let mut repo = AuditGovernanceOutboxRepo::new(p.clone());
    let res = repo.aggregate_login_failure_buckets(W).await;
    assert!(
        res.is_err(),
        "the mid-loop insert failure must propagate as Err (never swallowed)"
    );
    let inserted_after_failure: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_governance_outbox")
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(
        inserted_after_failure, 1,
        "bucket A (older) committed before the failure; bucket B has no row"
    );

    // Drop the forcing trigger; the same repo re-aggregates immediately.
    // The watermark never advanced (it moves only AFTER a successful insert
    // loop), so the rescan covers both buckets again — A dedupes, B lands.
    sqlx::query(&format!("DROP TRIGGER IF EXISTS {tg_name} ON audit_governance_outbox"))
        .execute(&p)
        .await
        .expect("drop forcing trigger");
    sqlx::query(&format!("DROP FUNCTION IF EXISTS {fn_name}()"))
        .execute(&p)
        .await
        .expect("drop forcing function");

    let inserted = repo
        .aggregate_login_failure_buckets(W)
        .await
        .expect("rerun after the failure");
    assert_eq!(
        inserted, 1,
        "the rerun completes exactly bucket B (A is deduped) — nothing lost"
    );
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT event_id::text,
                (payload->'payload'->>'count')::bigint,
                (payload->'payload'->>'window_start_epoch')::bigint
           FROM audit_governance_outbox
          ORDER BY payload->'payload'->>'window_start_epoch'",
    )
    .fetch_all(&p)
    .await
    .expect("read recovery rows");
    assert_eq!(rows.len(), 2, "both buckets recovered exactly once");
    for (idx, (event_id, count, window_start)) in rows.iter().enumerate() {
        let (expected_start, expected_count) =
            if idx == 0 { (bucket_a, 2) } else { (bucket_b, 2) };
        assert_eq!(window_start, &expected_start);
        assert_eq!(count, &expected_count, "counts are the closed buckets' N");
        assert_eq!(
            event_id,
            &l1_key(expected_start).to_string(),
            "deterministic v5 event_id (recomputed, never inline)"
        );
    }

    // Hygiene: remove this test's seeded base rows + any trigger residue.
    sqlx::query("DELETE FROM login_failures WHERE ip = '203.0.113.1'")
        .execute(&p)
        .await
        .expect("cleanup seeded failures");
    sqlx::query(&format!("DROP TRIGGER IF EXISTS {tg_name} ON audit_governance_outbox"))
        .execute(&p)
        .await
        .expect("cleanup trigger");
    sqlx::query(&format!("DROP FUNCTION IF EXISTS {fn_name}()"))
        .execute(&p)
        .await
        .expect("cleanup function");
}

/// Q3 (distributed-engineer blocker, F-C skew bound): a DELIBERATELY
/// SKEWED / out-of-band row — created_at inserted into a bucket whose outbox
/// row ALREADY exists (backfill/restore/manual) — must NOT change the frozen
/// count. `login_failures.created_at` is DB-stamped (`DEFAULT now()`) and
/// `LoginFailureRepo::record` never binds it, so producer and aggregator
/// share one clock domain; the documented freeze residual is exactly this
/// out-of-band insert. The freeze is exercised under RESTART semantics: a
/// FRESH repo (watermark resets to `UNIX_EPOCH`) rescans the whole closed
/// window, re-sees the backfilled row, and `ON CONFLICT DO NOTHING` cannot
/// merge it into the existing row (there is no merge arm for the auth
/// pull-scan — the 0242 trigger's `DO UPDATE … WHERE status = 0` merge is
/// message-lane only). A future-dated row lands in an OPEN bucket and is not
/// aggregated until its bucket closes (the rollover test pins that eventual
/// counting).
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn l1_auth_backfilled_row_into_aggregated_bucket_keeps_count_frozen() {
    const W: i64 = 60;
    let p = pool();
    reset_governance_table(&p).await;
    let now_epoch = time::OffsetDateTime::now_utc().unix_timestamp();
    // Two closed buckets (≥2-window margin per QA F-3 — never open at run
    // time) + one open bucket. Bucket floors computed off `now` so boundary
    // drift can never move a seeded row across a bucket edge.
    let bucket_a = (now_epoch - 150).div_euclid(W) * W; // rows at +30s inside
    let bucket_b = (now_epoch - 210).div_euclid(W) * W;
    let open_bucket = (now_epoch - 30).div_euclid(W) * W;
    let ts = |bucket: i64| {
        time::OffsetDateTime::from_unix_timestamp(bucket + 30).expect("bucket ts")
    };
    seed_failures(&p, ts(bucket_a), 3).await;
    seed_failures(&p, ts(bucket_b), 2).await;
    seed_failures(&p, ts(open_bucket), 1).await; // open — never aggregated

    let mut repo = AuditGovernanceOutboxRepo::new(p.clone());
    let inserted = repo
        .aggregate_login_failure_buckets(W)
        .await
        .expect("aggregate closed buckets");
    assert_eq!(inserted, 2, "exactly one row per CLOSED bucket (3 and 2 → 2 rows)");

    // **Deliberately skewed backfill**: insert ONE more row into bucket A —
    // a bucket whose outbox row ALREADY exists (out-of-band restore/backfill).
    seed_failures(&p, ts(bucket_a), 1).await;

    // RESTART rescan: a FRESH repo (watermark = UNIX_EPOCH — the boot
    // restart / new-instance shape) re-scans the entire closed window. The
    // backfilled row IS scanned (created_at ≥ UNIX_EPOCH and < boundary) and
    // collides with bucket A's existing outbox row — `ON CONFLICT DO NOTHING`
    // keeps the count FROZEN at 3 (no merge arm). The open bucket stays out.
    let mut fresh = AuditGovernanceOutboxRepo::new(p.clone());
    let inserted = fresh
        .aggregate_login_failure_buckets(W)
        .await
        .expect("fresh-repo rescan after backfill");
    assert_eq!(inserted, 0, "rescan inserts nothing — the backfilled row cannot merge");
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT event_id::text,
                (payload->'payload'->>'count')::bigint,
                (payload->'payload'->>'window_start_epoch')::bigint
           FROM audit_governance_outbox
          ORDER BY payload->'payload'->>'window_start_epoch'",
    )
    .fetch_all(&p)
    .await
    .expect("read aggregation rows");
    assert_eq!(rows.len(), 2, "still exactly 2 aggregation rows");
    for (event_id, count, window_start) in &rows {
        let (expected_start, expected_count) =
            if *window_start == bucket_a { (bucket_a, 3) } else { (bucket_b, 2) };
        assert_eq!(
            window_start, &expected_start,
            "window_start_epoch = the bucket floor"
        );
        assert_eq!(
            count, &expected_count,
            "count FROZEN — the backfilled row is never merged into bucket A"
        );
        assert_eq!(
            event_id,
            &l1_key(expected_start).to_string(),
            "deterministic v5 event_id (recomputed, never inline)"
        );
    }

    // Hygiene: remove the seeded base rows (shared-DB discipline).
    sqlx::query("DELETE FROM login_failures WHERE ip = '203.0.113.1'")
        .execute(&p)
        .await
        .expect("cleanup seeded failures");
    let _ = open_bucket; // referenced by the seed placement above
}

/// Q1 residual (distributed-engineer): a row whose `created_at` is EXACTLY
/// a bucket boundary (a whole-second multiple of W) must be counted when its
/// bucket closes — the pre-fix strict `created_at > watermark` excluded it
/// from the scan that closed its bucket AND from every later scan (permanent
/// leak). The fix is `>= watermark`: the boundary-exact row is picked up by
/// the first scan whose boundary passes its bucket end. The initial watermark
/// is `UNIX_EPOCH`, so `>=` is safe from the first tick.
///
/// Timeline (w = 3s buckets, row seeded at exactly the boundary that tick 1
/// advanced the watermark to):
///   tick 1: nothing seeded yet → 0 rows; wm := b2 (read back from the DB
///           clock immediately after — the boundary tick 1 used)
///   seed:   1 row at created_at == b2 EXACTLY (the now-current watermark)
///   tick 2: the bucket [b2, b2+w) closed → the row at exactly b2 is scanned
///           only with `>= watermark` (strict `>` excludes it forever → the
///           leak). Red on the pre-fix `>` predicate, green on `>=`.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn l1_auth_boundary_exact_row_is_counted_once() {
    const W: i64 = 3;
    let p = pool();
    reset_governance_table(&p).await;

    let mut repo = AuditGovernanceOutboxRepo::new(p.clone());
    let inserted = repo
        .aggregate_login_failure_buckets(W)
        .await
        .expect("tick 1 aggregation");
    assert_eq!(inserted, 0, "tick 1: no rows seeded yet");

    // Read back the boundary tick 1 advanced the watermark to (no edge
    // crosses in the milliseconds between the aggregation's boundary fetch
    // and this read — the wait_for_boundary gate below guarantees the
    // seeded bucket is closed before tick 2).
    let b2: i64 = sqlx::query_scalar(
        "SELECT floor(extract(epoch FROM clock_timestamp()) / $1)::bigint * $1 - $1",
    )
    .bind(W)
    .fetch_one(&p)
    .await
    .expect("boundary read");
    // Seed a row at EXACTLY b2 — the boundary value the watermark now holds.
    seed_failures(
        &p,
        time::OffsetDateTime::from_unix_timestamp(b2).expect("boundary ts"),
        1,
    )
    .await;

    // Close [b2, b2+w): the row at exactly b2 must be counted NOW (only the
    // `>= watermark` predicate includes it — strict `>` leaks it forever).
    wait_for_boundary(&p, W, b2 + W).await;
    let inserted = repo
        .aggregate_login_failure_buckets(W)
        .await
        .expect("tick 2 aggregation");
    assert_eq!(
        inserted, 1,
        "tick 2: the boundary-exact row is counted when its bucket closes (>= watermark)"
    );
    let row: (String, i64) = sqlx::query_as(
        "SELECT event_id::text, (payload->'payload'->>'count')::bigint
           FROM audit_governance_outbox",
    )
    .fetch_one(&p)
    .await
    .expect("read boundary row");
    assert_eq!(row.0, l1_key(b2).to_string(), "deterministic v5 key for bucket b2");
    assert_eq!(row.1, 1, "count 1 for the boundary-exact row");
    // Rerun idempotent; base row retained.
    assert_eq!(
        repo.aggregate_login_failure_buckets(W).await.expect("rerun"),
        0,
        "rerun inserts nothing"
    );

    // Hygiene.
    sqlx::query("DELETE FROM login_failures WHERE ip = '203.0.113.1'")
        .execute(&p)
        .await
        .expect("cleanup seeded failures");
}
