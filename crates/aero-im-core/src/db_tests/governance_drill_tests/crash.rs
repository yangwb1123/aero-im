//! None-workspace guard + crash-window drills (R9–R14) — see the parent
//! module doc for the slot map and the load-bearing invariants.

use aero_audit_connector::stub::{DuplicateBehavior, SinkBehavior, StubSink};
use aero_common::{Error, RoomKind};
use serde_json::Value;
use uuid::Uuid;

use super::shared::*;
use super::*;

// ---------------------------------------------------------------------------
// R9 — moderate_delete(None) refuses (sec-1a / db-3.3, production guard).
// ---------------------------------------------------------------------------

/// The None-workspace refusal is gate-INDEPENDENT (enforcement OFF): the
/// R-D1 guard alone must reject the un-audited delete — message stays live,
/// zero audit / governance / Deleted-broadcast rows. The workspace-ful
/// control half (same call, enforcement ON) commits 1+1, proving the guard
/// is the only difference.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_moderate_delete_none_workspace_refuses_like_rd1() {
    let pool = pool();
    let svc = service(pool.clone());
    self_isolate(&pool).await;
    // Enforcement OFF (fresh-DB default; defensively re-assert — the global
    // singleton may have been left ON by a panicked earlier drill).
    restore_enforcement_disabled(&pool).await;
    let (ws, owner) = workspace_fixture(&pool, "drill-r9").await;
    let room = svc
        .create_room_in_workspace(owner, ws, RoomKind::Channel, Some("drill-r9-room".into()))
        .await
        .expect("create room")
        .id;
    let msg = svc
        .send_message(
            owner,
            room,
            vec![Block::text("drill r9 message")],
            None,
            None,
        )
        .await
        .expect("send message")
        .id;

    // The seam's own room.create + message.create rows (0245/0242) are
    // enqueued by this setup — scrub them so the refusal half below can
    // assert a pristine zero-governance table (the drill scopes to the
    // moderation lane).
    scrub_seam_rows(&pool).await;

    // Refusal half (enforcement OFF ⇒ only the R-D1 guard can reject).
    let err = svc
        .moderate_delete(msg, None, "drill", "drill-digest")
        .await
        .expect_err("None-workspace moderation must refuse");
    assert!(
        matches!(err, Error::Invalid(_)),
        "R-D1 parity: Err(Invalid), got {err:?}"
    );

    // Message still live, blocks intact — no invisible removal.
    let (deleted_at, blocks): (Option<time::OffsetDateTime>, Value) =
        sqlx::query_as("SELECT deleted_at, blocks FROM messages WHERE id = $1")
            .bind(msg.to_uuid())
            .fetch_one(&pool)
            .await
            .expect("message row");
    assert!(deleted_at.is_none(), "message not deleted");
    assert_eq!(
        blocks.as_array().map_or(0, Vec::len),
        1,
        "blocks intact (the reviewable content survives)"
    );

    // Zero audit rows, zero governance rows, zero Deleted broadcast.
    let audit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_events
          WHERE target = $1::text AND action = 'message.moderated'",
    )
    .bind(msg.to_string())
    .fetch_one(&pool)
    .await
    .expect("count audit rows");
    assert_eq!(audit, 0, "zero audit rows — nothing un-audited escaped");
    let governance: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::bigint FROM audit_governance_outbox")
            .fetch_one(&pool)
            .await
            .expect("count governance rows");
    assert_eq!(governance, 0, "zero governance rows (TRUNCATE-clean start)");
    let deleted_frames: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM event_outbox
          WHERE message_id = $1 AND event_kind = 'deleted'",
    )
    .bind(msg.to_uuid())
    .fetch_one(&pool)
    .await
    .expect("count Deleted frames");
    assert_eq!(
        deleted_frames, 0,
        "no Deleted broadcast for the refused delete"
    );

    // Control half: the SAME call with the workspace commits 1 audit + 1
    // governance row (status 0 / class 'admin' / priority 100).
    let source = seed_governance_enforcement(&pool, ws).await;
    assert_eq!(source, format!("source-{ws}"));
    svc.moderate_delete(msg, Some(ws), "drill", "drill-digest")
        .await
        .expect("control half commits");
    let audit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM audit_events
          WHERE target = $1::text AND action = 'message.moderated'",
    )
    .bind(msg.to_string())
    .fetch_one(&pool)
    .await
    .expect("count audit rows");
    assert_eq!(audit, 1, "control half appends exactly one audit row");
    let row: (i32, String, i16) =
        sqlx::query_as("SELECT status, class, priority FROM audit_governance_outbox")
            .fetch_one(&pool)
            .await
            .expect("governance row");
    assert_eq!(row.0, 0, "status 0");
    assert_eq!(row.1, "admin", "class 'admin'");
    assert_eq!(row.2, 100, "priority 100");

    // Cleanup.
    sqlx::query("DELETE FROM audit_governance_outbox")
        .execute(&pool)
        .await
        .expect("clean outbox");
    let audit_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM audit_events
          WHERE target = $1::text AND action = 'message.moderated'",
    )
    .bind(msg.to_string())
    .fetch_one(&pool)
    .await
    .expect("audit id");
    sqlx::query("DELETE FROM audit_events WHERE id = $1")
        .bind(audit_id)
        .execute(&pool)
        .await
        .expect("clean audit rows");
    sqlx::query("DELETE FROM event_outbox WHERE message_id = $1")
        .bind(msg.to_uuid())
        .execute(&pool)
        .await
        .expect("clean event outbox");
    restore_enforcement_disabled(&pool).await;
}

// ---------------------------------------------------------------------------
// R10b — payload source ≠ relay source dead-letters, documented (sec-3).
// ---------------------------------------------------------------------------

/// The v2 relay is single-credential/single-source: a workspace binding
/// whose `source_system` ≠ `AERO_AUDIT_SOURCE_SYSTEM` dead-letters
/// PERMANENTLY (`PayloadGuard` fires BEFORE the token plane and any POST).
/// This pins the "one relay per source system" ops contract as a tested,
/// loud outcome — never delivered to the wrong lane, never silently dropped.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_source_mismatch_dead_letters_documented() {
    let pool = pool();
    let svc = service(pool.clone());
    let rows = write_path_rows(&pool, &svc, "drill-r10", 2).await;

    let stub = StubSink::start().await.expect("start stub");
    let (relay, _repo, _config) = relay_for(&pool, &stub, "aero-im.source").await;
    assert_ne!(
        rows.source, "aero-im.source",
        "the ops misconfiguration under test (fixture seeds source-{{ws}})"
    );

    assert_eq!(
        relay.dispatch_batch().await.expect("round 1"),
        2,
        "round 1 claims both rows"
    );
    for id in &rows.event_ids {
        let row = outbox_row(&pool, *id).await;
        assert_eq!(row.status, 0, "attempt 1 requeues (≤1 retry)");
        assert_eq!(row.attempts, 1);
        assert!(
            row.last_error
                .as_deref()
                .is_some_and(|e| e.contains("PayloadGuard")),
            "last_error names the payload guard (got {:?})",
            row.last_error
        );
    }
    assert_eq!(stub.posts(), 0, "the payload guard fires before any POST");
    assert_eq!(
        stub.token_requests(),
        0,
        "the guard precedes the token plane"
    );

    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    assert_eq!(
        relay.dispatch_batch().await.expect("round 2"),
        2,
        "round 2 reclaims after the backoff"
    );
    for id in &rows.event_ids {
        let row = outbox_row(&pool, *id).await;
        assert_eq!(row.status, 3, "dead-lettered — permanent, documented");
        assert_eq!(row.attempts, 2);
        assert!(
            row.last_error
                .as_deref()
                .is_some_and(|e| e.contains("PayloadGuard")),
            "last_error keeps the dead cause (got {:?})",
            row.last_error
        );
    }
    assert_eq!(
        count_status(&pool, &rows.event_ids, 2).await,
        0,
        "zero delivered — never the wrong lane"
    );
    assert_eq!(stub.posts(), 0);
    assert_eq!(stub.token_requests(), 0);

    cleanup_drill_rows(&pool, &rows).await;
}

// ---------------------------------------------------------------------------
// R11–R14 — crash-window reclaim terminals (rel-1 / rel-4c / rel-5a/b).
// ---------------------------------------------------------------------------

/// The at-least-once crash window (claim → 202 → crash before settle →
/// lease expiry → reclaim with attempts=2 and a rotated token → redeliver
/// with the SAME Idempotency-Key): a CONFORMING sink replays the original
/// receipt, `validate_audit_receipt` passes value-level, and the row SETTLES
/// (status 2). Also the rel-5a header pin: both POSTs carry the base32
/// `AuditId` spelling — a regression respelling the header fails here.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_crash_window_reclaim_replays_original_receipt_and_settles() {
    let pool = pool();
    let svc = service(pool.clone());
    let rows = write_path_rows(&pool, &svc, "drill-r11", 1).await;
    let event_id = rows.event_ids[0];

    let stub = StubSink::start().await.expect("start stub");
    stub.set_behavior(SinkBehavior {
        duplicate: DuplicateBehavior::ReplayOriginalReceipt,
        ..SinkBehavior::default()
    })
    .await;
    let (relay, repo, config) = relay_for(&pool, &stub, &rows.source).await;

    // The crash window: claim (attempts=1) → real 202 + receipt → claim
    // dropped without settle → lease forced past expiry.
    let key = crash_after_delivery(&pool, &repo, &config, event_id).await;
    assert_eq!(stub.posts(), 1, "exactly one POST before the crash");

    // Reclaim + redeliver through the REAL relay: the stub replays the
    // original receipt → settle.
    assert_eq!(
        relay.dispatch_batch().await.expect("reclaim"),
        1,
        "the reclaimed row is claimed again"
    );
    let row = outbox_row(&pool, event_id).await;
    assert_eq!(
        row.status, 2,
        "the conforming sink's replayed receipt settles the crash window"
    );
    assert!(row.delivered_at.is_some(), "delivered_at stamped");
    assert!(row.last_error.is_none(), "no error recorded on settle");
    assert_eq!(
        row.attempts, 2,
        "reclaim increments attempts (claim budget)"
    );
    assert!(row.claim_token.is_none(), "fencing token cleared on settle");
    assert!(row.lease_expires_at.is_none(), "lease cleared on settle");
    assert_eq!(stub.posts(), 2, "one real POST per claim");

    // rel-5a header pin: both POSTs carried the SAME base32 spelling.
    assert_eq!(
        stub.seen_idempotency_keys().await,
        vec![key.clone(), key],
        "Idempotency-Key header = AuditId base32, stable across the reclaim"
    );

    cleanup_drill_rows(&pool, &rows).await;
}

/// Crash window + sink flipped to 422: the reclaim eats the retry budget
/// (`PERMANENT_DEAD_AT` counts CLAIMS, not POSTs — pg.rs's own caveat), so the
/// genuine 422 on the first real POST after the reclaim deads straight away:
/// status 3, attempts 2, zero retries between the reclaim and the dead.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_crash_window_reclaim_422_deads_at_attempt_two() {
    let pool = pool();
    let svc = service(pool.clone());
    let rows = write_path_rows(&pool, &svc, "drill-r12", 1).await;
    let event_id = rows.event_ids[0];

    let stub = StubSink::start().await.expect("start stub");
    let (relay, repo, config) = relay_for(&pool, &stub, &rows.source).await;
    crash_after_delivery(&pool, &repo, &config, event_id).await;

    // The sink flips to 422 after the crash (e.g. validation drift mid-roll).
    stub.set_behavior(SinkBehavior {
        events_status: 422,
        ..SinkBehavior::default()
    })
    .await;
    assert_eq!(
        relay.dispatch_batch().await.expect("reclaim"),
        1,
        "the reclaimed row is claimed again"
    );
    let row = outbox_row(&pool, event_id).await;
    assert_eq!(
        row.status, 3,
        "dead at attempt 2 — the claim budget is spent"
    );
    assert_eq!(
        row.attempts, 2,
        "budget counts claims, not POSTs: one real success + one rejected redelivery"
    );
    assert!(row.delivered_at.is_none());
    assert!(
        row.last_error
            .as_deref()
            .is_some_and(|e| e.contains("Unprocessable")),
        "last_error names the class (got {:?})",
        row.last_error
    );
    assert_eq!(stub.posts(), 2, "one success before the crash + one 422");

    cleanup_drill_rows(&pool, &rows).await;
}

/// Crash window + sink flipped to 403: the Forbidden arm deads IMMEDIATELY
/// (T-11) — no `is_dead_at` gate, regardless of the reclaim-eaten budget.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_crash_window_reclaim_403_deads_immediately() {
    let pool = pool();
    let svc = service(pool.clone());
    let rows = write_path_rows(&pool, &svc, "drill-r13", 1).await;
    let event_id = rows.event_ids[0];

    let stub = StubSink::start().await.expect("start stub");
    let (relay, repo, config) = relay_for(&pool, &stub, &rows.source).await;
    crash_after_delivery(&pool, &repo, &config, event_id).await;

    stub.set_behavior(SinkBehavior {
        events_status: 403,
        ..SinkBehavior::default()
    })
    .await;
    assert_eq!(
        relay.dispatch_batch().await.expect("reclaim"),
        1,
        "the reclaimed row is claimed again"
    );
    let row = outbox_row(&pool, event_id).await;
    assert_eq!(row.status, 3, "403 → immediate dead (T-11)");
    assert_eq!(
        row.attempts, 2,
        "attempts still grow (the reclaim happened)"
    );
    assert_eq!(stub.posts(), 2, "one success before the crash + one 403");
    assert_eq!(
        row.last_error.as_deref(),
        Some("audit sink rejected the service identity (HTTP 403)"),
        "exact Forbidden-arm string"
    );
    assert!(row.delivered_at.is_none());

    cleanup_drill_rows(&pool, &rows).await;
}

/// Duplicate conflict signals (409 / `conflict:true`) are classified
/// PERMANENT by the connector (v1-parity decision, D-W1): the row deads
/// after ≤1 retry — documented, never silent, never settled on an
/// unverifiable signal (no receipt matched in either twin). If a later
/// decision settles on `conflict:true`-with-matching-event_id, this drill is
/// where the behavior flip lands.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_duplicate_conflict_signal_deads_per_documented_policy() {
    for (mode, fragment) in [
        (DuplicateBehavior::Conflict409, "Conflict"),
        (DuplicateBehavior::ConflictFlag, "ReceiptMismatch"),
    ] {
        let pool = pool();
        let svc = service(pool.clone());
        let rows = write_path_rows(&pool, &svc, "drill-r14", 1).await;
        let event_id = rows.event_ids[0];

        let stub = StubSink::start().await.expect("start stub");
        stub.set_behavior(SinkBehavior {
            duplicate: mode,
            ..SinkBehavior::default()
        })
        .await;
        let (relay, repo, config) = relay_for(&pool, &stub, &rows.source).await;
        crash_after_delivery(&pool, &repo, &config, event_id).await;

        assert_eq!(
            relay.dispatch_batch().await.expect("reclaim"),
            1,
            "the reclaimed row is claimed again"
        );
        let row = outbox_row(&pool, event_id).await;
        assert_eq!(
            row.status, 3,
            "dedup signal → dead after ≤1 retry (documented)"
        );
        assert_eq!(row.attempts, 2);
        assert_eq!(
            stub.posts(),
            2,
            "one original + one dedup-signal redelivery"
        );
        assert!(
            row.last_error
                .as_deref()
                .is_some_and(|e| e.contains(fragment)),
            "last_error names {fragment} (got {:?})",
            row.last_error
        );
        assert!(
            row.delivered_at.is_none(),
            "never settled on a conflict signal"
        );

        cleanup_drill_rows(&pool, &rows).await;
    }
}

// ---------------------------------------------------------------------------
// F1 — panic-leaves-ON: the failure mode is REAL (there is NO Drop guard —
// async sqlx restore cannot run in sync Drop, and harness kills skip Drop
// anyway), so a panicked drill leaves the global
// `snaplink_commercial_runtime.enabled` singleton ON, and every later
// unbound message INSERT raises P0001 (0235 metering) — empirically the
// `message_reports::db_tests` 3/3 cascade. The start-of-test defensive
// re-assert every drill runs is the ONLY backstop. This drill makes both
// halves real with an ACTUAL panic: (a) the panic really leaves the switch
// ON and the unbound INSERT really raises P0001; (b) the exact backstop
// statement (`restore_enforcement_disabled`, the first statement of every
// later test) really closes the cascade — the same INSERT then commits.
// ---------------------------------------------------------------------------

/// F1 — a genuine panic mid-drill (after the singleton flip, before any
/// restore) leaves enforcement ON; the next test's start-of-test re-assert
/// is what keeps the shared DB green — proven here, not assumed.
#[tokio::test]
#[ignore = "requires live Postgres (DATABASE_URL)"]
async fn drill_panic_leaves_enforcement_on_backstopped_by_start_reassert() {
    let pool = pool();
    self_isolate(&pool).await;
    // The very statement every later test runs first (the backstop under
    // test, exercised at the bottom too).
    restore_enforcement_disabled(&pool).await;
    let (ws, owner) = workspace_fixture(&pool, "drill-f1").await;
    let svc = service(pool.clone());
    let room = svc
        .create_room_in_workspace(owner, ws, RoomKind::Channel, Some("drill-f1-room".into()))
        .await
        .expect("create room")
        .id;

    // The crash: a drill flips the global singleton ON as its first action,
    // then panics before any restore. No Drop guard runs (module doc).
    sqlx::query(
        "UPDATE snaplink_commercial_runtime
            SET enabled = TRUE, updated_at = clock_timestamp()
          WHERE singleton",
    )
    .execute(&pool)
    .await
    .expect("flip singleton ON (the crash's first action)");
    let fired = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        panic!("F1: simulated drill panic before restore");
    }));
    assert!(fired.is_err(), "the simulated panic fired");

    // The panic really left the switch ON — the failure mode is real.
    let enabled: bool =
        sqlx::query_scalar("SELECT enabled FROM snaplink_commercial_runtime WHERE singleton")
            .fetch_one(&pool)
            .await
            .expect("read singleton");
    assert!(enabled, "panic left enforcement ON (the F1 hazard is real)");

    // The leaked-ON state IS the P0001 cascade: an unbound message INSERT
    // (this workspace has NO binding) raises 'commercial binding is
    // unavailable' (0235 metering) — the exact failure that empirically
    // took down message_reports::db_tests 3/3.
    let err = sqlx::query(
        "INSERT INTO messages (id, room_id, sender_id, blocks)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(MessageId::new().to_uuid())
    .bind(room.to_uuid())
    .bind(owner.to_uuid())
    .bind(serde_json::json!([{ "type": "text", "text": "unbound while leaked ON" }]))
    .execute(&pool)
    .await
    .expect_err("unbound message INSERT must raise P0001 while enforcement is ON");
    assert_eq!(
        err.as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("P0001"),
        "0235 metering raises P0001 (got {err:?})"
    );

    // The backstop: the next test's start-of-test defensive re-assert (the
    // exact statement above) heals the singleton; the same INSERT then
    // commits. The cascade is closed BY the backstop, not by luck.
    restore_enforcement_disabled(&pool).await;
    let enabled: bool =
        sqlx::query_scalar("SELECT enabled FROM snaplink_commercial_runtime WHERE singleton")
            .fetch_one(&pool)
            .await
            .expect("read singleton");
    assert!(!enabled, "backstop restored the fresh-DB default");
    sqlx::query(
        "INSERT INTO messages (id, room_id, sender_id, blocks)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(MessageId::new().to_uuid())
    .bind(room.to_uuid())
    .bind(owner.to_uuid())
    .bind(serde_json::json!([{ "type": "text", "text": "post-backstop insert" }]))
    .execute(&pool)
    .await
    .expect("the same INSERT commits after the backstop");
}
