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
    let p = pool();
    reset_governance_table(&p).await;
    let a = seed_dlq_row(&p, AUTH_LOGIN, "replay-all-target").await;
    let b = seed_dlq_row(&p, AUTH_LOGIN, "replay-all-target").await;

    let repo = FailedPairRepo::new(p.clone());
    assert_eq!(repo.count().await.expect("count"), 2, "DLQ depth = 2");
    let replayed = repo.replay_all(10).await.expect("replay_all");
    assert_eq!(replayed, 2, "both rows replayed");
    let unreplayed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_governance_failed_pairs WHERE replayed_at IS NULL",
    )
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(unreplayed, 0, "ops loop drains the unreplayed set");
    assert_eq!(repo.count().await.expect("count"), 2, "replayed rows remain (depth includes them)");
    // The ops-loop limit is honored (one row per bounded call; a drained
    // call replays nothing).
    let c = seed_dlq_row(&p, AUTH_LOGIN, "replay-all-target").await;
    let d = seed_dlq_row(&p, AUTH_LOGIN, "replay-all-target").await;
    assert_eq!(repo.replay_all(1).await.expect("limited replay"), 1);
    assert_eq!(repo.replay_all(1).await.expect("second limited replay"), 1);
    assert_eq!(repo.replay_all(1).await.expect("drained replay"), 0);
    let _ = (a, b, c, d);
}
