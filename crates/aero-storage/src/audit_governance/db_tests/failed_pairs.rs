//! B5-1 DLQ compensation `db_tests` (migration 0244): enqueue shapes, replay
//! mechanics, and the ops-loop (`replay_all` / `count`).

use super::*;
use crate::audit_governance::tokens::{AUTH_LOGIN, OUTBOUND_AUTH_LOGIN};
use crate::audit_governance::FailedPairRepo;
use serde_json::json;

/// Seed one DLQ row directly (the exact shape a fail-open pair leaves:
/// original input + error sqlstate). Returns the row id.
///
/// The seed's audit target is unique to THIS test and its audit rows are
/// pre-cleaned at test start: `audit_events` accumulates across runs of the
/// same suite on one DB (module discipline — tests scope, never full-table),
/// so the replay-built audit row count must only ever see this run's row.
async fn seed_dlq_row(p: &PgPool, action: &str, target: &str) -> i64 {
    let mut tx = p.begin().await.expect("begin dlq seed");
    let id = FailedPairRepo::enqueue_in_tx(
        &mut tx,
        WorkspaceId::nil(),
        None,
        action,
        Some(target),
        json!({ "stage": "seed" }),
        OUTBOUND_AUTH_LOGIN,
        "23505",
        Some("seeded row"),
    )
    .await
    .expect("enqueue dlq row");
    tx.commit().await.expect("commit dlq seed");
    id
}

/// D9: replay rebuilds the pair with a NEW `AuditId` (the original pair never
/// committed — no idempotency conflict), stamps `replayed_at`, and a second
/// replay is a no-op. The DB state is the same as the fail-open path left:
/// 0 audit rows + 0 outbox rows + 1 DLQ row → after replay 1+1+1.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn failed_pair_replay_rebuilds_pair_and_stamps_replayed_at() {
    let p = pool();
    reset_governance_table(&p).await;
    // Pre-clean THIS test's own audit marker (see seed_dlq_row doc —
    // repeated runs of the suite on one DB accumulate audit rows).
    sqlx::query("DELETE FROM audit_events WHERE action = $1 AND target = $2")
        .bind(AUTH_LOGIN)
        .bind(REBUILD_TARGET)
        .execute(&p)
        .await
        .expect("pre-clean this test's audit rows");
    let id = seed_dlq_row(&p, AUTH_LOGIN, REBUILD_TARGET).await;

    let repo = FailedPairRepo::new(p.clone());
    let replayed = repo.replay(id).await.expect("replay succeeds");
    let audit_id = replayed.expect("pair rebuilt with a NEW AuditId");
    // 1 audit + 1 outbox row for the rebuilt pair (envelope pinned by the
    // auth parity test — here just the 1:1 + status), scoped by this test's
    // target marker.
    let audit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE action = $1 AND target = $2",
    )
    .bind(AUTH_LOGIN)
    .bind(REBUILD_TARGET)
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(audit, 1, "replay rebuilt exactly 1 audit row");
    let outbox: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_governance_outbox WHERE event_id = $1")
            .bind(audit_id.to_uuid())
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(outbox, 1, "replay rebuilt exactly 1 outbox row");
    let stamped: Option<time::OffsetDateTime> = sqlx::query_scalar(
        "SELECT replayed_at FROM audit_governance_failed_pairs WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&p)
    .await
    .unwrap();
    assert!(stamped.is_some(), "replayed_at stamped on success");

    // A second replay is a no-op (row already replayed).
    assert!(
        repo.replay(id).await.expect("second replay succeeds").is_none(),
        "already-replayed rows are not rebuilt twice"
    );
    let audit_after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_events WHERE action = $1 AND target = $2")
            .bind(AUTH_LOGIN)
            .bind(REBUILD_TARGET)
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(audit_after, 1, "no duplicate rebuild");
}

/// Unique audit-target marker for the rebuild test (see `seed_dlq_row`).
const REBUILD_TARGET: &str = "replay-rebuild-target";

/// Ops loop: `replay_all` replays every unreplayed row; `count` reports the
/// full DLQ depth (replayed rows stay — they are the audit trail of the
/// compensation).
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn failed_pair_replay_all_replays_unreplayed_rows() {
    let pool = pool();
    reset_governance_table(&pool).await;
    let first = seed_dlq_row(&pool, AUTH_LOGIN, "replay-all-target").await;
    let second = seed_dlq_row(&pool, AUTH_LOGIN, "replay-all-target").await;

    let repo = FailedPairRepo::new(pool.clone());
    assert_eq!(repo.count().await.expect("count"), 2, "DLQ depth = 2");
    let replayed = repo.replay_all(10).await.expect("replay_all");
    assert_eq!(replayed, 2, "both rows replayed");
    let unreplayed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_governance_failed_pairs WHERE replayed_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unreplayed, 0, "ops loop drains the unreplayed set");
    assert_eq!(repo.count().await.expect("count"), 2, "replayed rows remain (depth includes them)");
    // The ops-loop limit is honored (one row per bounded call; a drained
    // call replays nothing).
    let third = seed_dlq_row(&pool, AUTH_LOGIN, "replay-all-target").await;
    let fourth = seed_dlq_row(&pool, AUTH_LOGIN, "replay-all-target").await;
    assert_eq!(repo.replay_all(1).await.expect("limited replay"), 1);
    assert_eq!(repo.replay_all(1).await.expect("second limited replay"), 1);
    assert_eq!(repo.replay_all(1).await.expect("drained replay"), 0);
    let _ = (first, second, third, fourth);
}

/// Q2 (distributed-engineer blocker): the replay claim fence must be
/// CONCURRENCY-ATOMIC. Two overlapping `replay_all` runs over the same DLQ
/// rows must rebuild each row EXACTLY once — the claim now lives INSIDE the
/// replay tx (the `FOR UPDATE` lock spans the rebuild + status update), so a
/// concurrent claim SKIPs or blocks and re-checks `replayed_at` after the
/// winner commits. Pre-fix (autocommit claim + unguarded UPDATE), two runs
/// could both rebuild the same row: 1 DLQ row → 2 audit pairs + 2 outbox
/// rows (distinct event_ids — the sink double-delivers the compensation),
/// and both incremented `replay_attempts` (premature `dead` at ~half the
/// intended cap).
///
/// The assertion is on the INVARIANT, not a specific interleaving: spawn 8
/// concurrent `replay_all` runs over 4 rows and assert exactly 4 rebuilds
/// (4 audit rows, 4 outbox rows, 4× `replay_attempts = 1`). Red on the
/// pre-fix code whenever the race window is hit; deterministic-green on the
/// fix.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn failed_pair_replay_concurrent_runs_rebuild_each_row_once() {
    let p = pool();
    reset_governance_table(&p).await;
    // Pre-clean THIS test's own audit marker (repeated runs of the suite on
    // one DB accumulate audit rows — see seed_dlq_row doc).
    sqlx::query("DELETE FROM audit_events WHERE action = $1 AND target LIKE $2")
        .bind(AUTH_LOGIN)
        .bind("replay-concurrent-%")
        .execute(&p)
        .await
        .expect("pre-clean this test's audit rows");
    // 4 DLQ rows, all pointing at distinct targets under the marker prefix.
    let mut ids = Vec::new();
    for i in 0..4 {
        ids.push(seed_dlq_row(&p, AUTH_LOGIN, &format!("replay-concurrent-{i}")).await);
    }

    let repo = FailedPairRepo::new(p.clone());
    // 8 concurrent ops loops, each replaying up to 10 rows.
    let mut handles = Vec::new();
    for _ in 0..8 {
        let repo = repo.clone();
        handles.push(tokio::spawn(async move {
            repo.replay_all(10).await.expect("concurrent replay_all")
        }));
    }
    let mut total_replayed = 0usize;
    for handle in handles {
        total_replayed += handle.await.expect("join concurrent replay_all");
    }

    // Exactly 4 successful rebuilds across ALL runs — each row exactly once.
    assert_eq!(total_replayed, 4, "4 rows rebuilt exactly once across 8 concurrent runs");
    let audit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_events WHERE action = $1 AND target LIKE $2",
    )
    .bind(AUTH_LOGIN)
    .bind("replay-concurrent-%")
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(audit, 4, "exactly 4 audit rows — no duplicate rebuilds");
    let outbox: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_governance_outbox").fetch_one(&p).await.unwrap();
    assert_eq!(outbox, 4, "exactly 4 outbox rows — one pair per rebuilt row");
    // Every DLQ row stamped exactly once with replay_attempts == 1 (no
    // double-increment → no premature dead).
    let attempts: Vec<(i64, i32)> = sqlx::query_as(
        "SELECT id, replay_attempts FROM audit_governance_failed_pairs WHERE id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(&p)
    .await
    .unwrap();
    for (id, n) in attempts {
        assert_eq!(n, 1, "row {id} replayed exactly once (attempts == 1)");
    }
    let replayed_at: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_governance_failed_pairs WHERE replayed_at IS NOT NULL",
    )
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(replayed_at, 4, "all 4 rows stamped replayed_at");
}

/// F-4 third leg: `sweep_terminal_before` hard-deletes TERMINAL rows
/// (`status='dead'` or replayed) older than the cutoff and NEVER touches
/// never-replayed `pending` rows (the gauge's alert surface).
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn failed_pair_sweep_terminal_before_removes_only_terminal_rows() {
    let p = pool();
    reset_governance_table(&p).await;
    let now = time::OffsetDateTime::now_utc();
    let old = now.unix_timestamp() - 90 * 24 * 3600; // 90 days ago (past 30d cutoff)
    let recent = now.unix_timestamp() - 3600; // 1 hour ago (inside cutoff)

    // Seed directly (enqueue_in_tx then stamp) so `created_at`/`status`/
    // `replayed_at` are under test control.
    let seed = |action: String, target: String, status: String, created_at: i64, replayed: bool| {
        let p = p.clone();
        async move {
            let mut tx = p.begin().await.expect("begin sweep seed");
            let id = FailedPairRepo::enqueue_in_tx(
                &mut tx,
                WorkspaceId::nil(),
                None,
                &action,
                Some(target.as_str()),
                json!({ "stage": "seed" }),
                OUTBOUND_AUTH_LOGIN,
                "23505",
                Some("seeded row"),
            )
            .await
            .expect("enqueue dlq row");
            sqlx::query(
                "UPDATE audit_governance_failed_pairs
                    SET created_at = $2, status = $3,
                        replay_attempts = CASE WHEN $4 THEN 1 ELSE 0 END,
                        replayed_at = CASE WHEN $4 THEN created_at ELSE NULL END
                  WHERE id = $1",
            )
            .bind(id)
            .bind(time::OffsetDateTime::from_unix_timestamp(created_at).expect("ts"))
            .bind(status)
            .bind(replayed)
            .execute(&mut *tx)
            .await
            .expect("stamp sweep seed");
            tx.commit().await.expect("commit sweep seed");
            id
        }
    };
    seed(AUTH_LOGIN.to_string(), "sweep-old-replayed".to_string(), "pending".to_string(), old, true).await;
    seed(AUTH_LOGIN.to_string(), "sweep-old-dead".to_string(), "dead".to_string(), old, false).await;
    seed(AUTH_LOGIN.to_string(), "sweep-old-pending".to_string(), "pending".to_string(), old, false).await;
    seed(AUTH_LOGIN.to_string(), "sweep-recent-replayed".to_string(), "pending".to_string(), recent, true).await;

    let repo = FailedPairRepo::new(p.clone());
    let cutoff = now - time::Duration::days(30);
    let swept = repo
        .sweep_terminal_before(cutoff)
        .await
        .expect("sweep terminal rows");
    assert_eq!(swept, 2, "exactly the old replayed + old dead rows are terminal-past-cutoff");
    let remaining: Vec<String> = sqlx::query_scalar(
        "SELECT target FROM audit_governance_failed_pairs ORDER BY target",
    )
    .fetch_all(&p)
    .await
    .unwrap();
    assert_eq!(
        remaining,
        vec!["sweep-old-pending", "sweep-recent-replayed"],
        "pending (any age) and recent terminal rows survive"
    );
}
