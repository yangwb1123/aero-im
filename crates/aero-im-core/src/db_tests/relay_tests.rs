//! DB-gated integration tests for the two durable relay loops:
//! `dispatch_event_outbox_batch` (AT-6a) and `dispatch_message_side_effect_batch`
//! (AT-6b, AT-7) — design §4 AT-6/AT-7 of
//! `docs/design/2026-08-06-aero-im-core-notification-fanout-tests.design.md`.
//!
//! The global claimers (`claim_due` + `FOR UPDATE SKIP LOCKED`) operate on
//! GLOBAL state, so the seed → dispatch → assert sections of AT-6b/AT-7 run
//! under `BATCH_SERIAL` (design §5.6) and the runbook executes the suite with
//! `--test-threads=1`.

use aero_common::{Block, ParticipantId};

use super::{
    count_ai_jobs, count_notifications, group_room, insert_message_row, insert_side_effect_job,
    notification_service, pending_notify_outbox, pool, rearm_side_effect_job,
    side_effect_job_state,
};
use crate::service::orig::{notify_delivery_id, NotifyBatchKind};
use crate::service::ImService;

/// AT-6a: the outbox relay claims and publishes the pending Notify row: exactly
/// one `im.room.{room}` frame with `event_id` == `delivery_id` and a numeric
/// per-subject seq stamp, then marks the row published (`pending_for_message` →
/// None). The claimer takes up to `limit` rows per tick in due order, so the
/// test pumps the relay until our row is published — mirroring the real
/// timer-driven relay.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at6a_outbox_relay_publishes_and_marks_notify_row() {
    let pool = pool();
    let (svc, bus) = notification_service(pool.clone(), false);
    let (members, room) = group_room(&pool, &svc, "at6a", 3).await;
    let (alice, bob, carol) = (&members[0], &members[1], &members[2]);

    let msg = svc
        .send_message(
            alice.id,
            room,
            vec![
                Block::text("hi"),
                Block::Mention {
                    participant: bob.id,
                },
                Block::Mention {
                    participant: carol.id,
                },
            ],
            None,
            None,
        )
        .await
        .unwrap();

    // Self-seeded precondition (AT-1's dropped pending-state assertion is
    // re-covered here): the Notify row is pending before the relay run.
    let delivery_id = notify_delivery_id(msg.id, NotifyBatchKind::Mention);
    let (event_id, _payload) = pending_notify_outbox(&pool, msg.id)
        .await
        .expect("notify outbox row pending before the relay run");
    assert_eq!(event_id, delivery_id);

    // Pump the relay until OUR row is published. The claimer takes up to
    // `limit` rows per tick in available_at/created_at order, and earlier
    // tests' pending Notify rows share the same timestamps, so our row may
    // need a second tick — exactly how the real timer-driven relay behaves.
    let mut ticks = 0;
    while pending_notify_outbox(&pool, msg.id).await.is_some() {
        ticks += 1;
        assert!(ticks <= 10, "Notify row not published after 10 relay ticks");
        let published = svc.dispatch_event_outbox_batch(10).await.unwrap();
        assert!(
            published >= 1,
            "the relay must publish at least one pending row per tick"
        );
    }

    // Row marked published.
    assert!(
        pending_notify_outbox(&pool, msg.id).await.is_none(),
        "the Notify row is published after the relay"
    );

    // Exactly one frame on this room's subject carries event_id == delivery_id;
    // it has a numeric seq stamp and decodes to the same NotifyBatch.
    let subject = ImService::room_subject(room);
    let delivery_id_str = delivery_id.to_string();
    let frames: Vec<serde_json::Value> = bus
        .published
        .lock()
        .unwrap()
        .iter()
        .filter(|(s, _)| s == &subject)
        .filter_map(|(_, bytes)| serde_json::from_slice::<serde_json::Value>(bytes).ok())
        .filter(|value| {
            value
                .get("event_id")
                .and_then(serde_json::Value::as_str)
                == Some(delivery_id_str.as_str())
        })
        .collect();
    assert_eq!(
        frames.len(),
        1,
        "exactly one NotifyBatch frame per delivery_id"
    );
    let frame = &frames[0];
    assert!(
        frame.get("seq").is_some_and(serde_json::Value::is_number),
        "notify frame carries a numeric seq stamp"
    );
    let aero_common::RoomEvent::NotifyBatch {
        message_id,
        by,
        recipients,
        ..
    } = serde_json::from_value(frame.clone()).unwrap()
    else {
        panic!("relay frame decodes to a NotifyBatch");
    };
    assert_eq!(message_id, msg.id);
    assert_eq!(by, alice.id);
    let mut ids: Vec<ParticipantId> = recipients.iter().map(|r| r.participant).collect();
    ids.sort();
    assert_eq!(ids, [bob.id, carol.id]);
}

/// AT-6b: the side-effect relay claims seeded due jobs: returns 1 → job
/// completed → inbox rows written; the second call returns 0 (nothing due). A
/// deleted message completes its claim without rows (no crash).
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
#[allow(clippy::await_holding_lock)]
async fn at6b_side_effect_relay_completes_seeded_claims() {
    let pool = pool();
    let (svc, _bus) = notification_service(pool.clone(), false);
    let (members, room) = group_room(&pool, &svc, "at6b", 2).await;
    let (alice, bob) = (&members[0], &members[1]);

    // A message row created OUTSIDE ImService has no outbox row and no
    // side-effect jobs — the seeded notifications job is the ONLY driver.
    let msg = insert_message_row(&pool, room, alice.id, vec![Block::text("@everyone")]).await;
    let job = insert_side_effect_job(&pool, msg, 1, "notifications").await;

    let _serial = super::BATCH_SERIAL.lock().unwrap();
    assert_eq!(
        svc.dispatch_message_side_effect_batch(10).await.unwrap(),
        1,
        "the seeded job is claimed and completed"
    );
    let (completed_at, _, last_error) = side_effect_job_state(&pool, job).await;
    assert!(completed_at.is_some(), "seeded job completed");
    assert!(last_error.is_none(), "seeded job must not fail: {last_error:?}");
    assert_eq!(count_notifications(&pool, msg, None).await, 1);
    assert_eq!(count_notifications(&pool, msg, Some(bob.id)).await, 1);
    assert_eq!(
        svc.dispatch_message_side_effect_batch(10).await.unwrap(),
        0,
        "nothing due after the claim completed"
    );

    // Negative control: a deleted message completes the claim without rows.
    let deleted_msg = insert_message_row(&pool, room, alice.id, vec![Block::text("gone")]).await;
    sqlx::query("UPDATE messages SET deleted_at = now() WHERE id = $1")
        .bind(deleted_msg.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    let deleted_job = insert_side_effect_job(&pool, deleted_msg, 1, "notifications").await;
    assert_eq!(
        svc.dispatch_message_side_effect_batch(10).await.unwrap(),
        1,
        "the deleted-message job is completed, not failed"
    );
    let (completed_at, _, last_error) = side_effect_job_state(&pool, deleted_job).await;
    assert!(completed_at.is_some(), "deleted-message claim completed");
    assert!(last_error.is_none(), "deleted-message claim must not fail: {last_error:?}");
    assert_eq!(count_notifications(&pool, deleted_msg, None).await, 0);
    assert_eq!(
        svc.dispatch_message_side_effect_batch(10).await.unwrap(),
        0
    );
}

/// AT-7: a stale (pre-edit) side-effect job completes WITHOUT spending on AI:
/// after editing v1 → v2, the re-dispatched stale v1 Embed job is completed by
/// the version guard with `last_error = NULL`, while the fresh v2 Embed /
/// Moderate jobs enqueue — the `ai_jobs` delta is exactly 2, not 3.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
#[allow(clippy::await_holding_lock)]
async fn at7_stale_side_effect_version_completes_without_spending() {
    let pool = pool();
    let (svc, _bus) = notification_service(pool.clone(), false);
    let (members, room) = group_room(&pool, &svc, "at7", 1).await;
    let alice = &members[0];

    let msg = svc
        .send_message(alice.id, room, vec![Block::text("v1")], None, None)
        .await
        .unwrap();
    let edited = svc
        .edit_message(alice.id, msg.id, vec![Block::text("v2")], None)
        .await
        .unwrap();
    assert_eq!(edited.version, 2);

    // Poll to quiescence: every side-effect job for this message must be
    // completed BEFORE re-arming (design §9.5 — correct under both the awaited
    // fast path and a hypothetical fire-and-forget one; a re-arm racing the
    // background dispatch would clobber a live claim and go nondeterministic).
    for _ in 0..100 {
        let pending: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM message_side_effect_jobs
              WHERE message_id = $1 AND completed_at IS NULL",
        )
        .bind(msg.id.to_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
        if pending == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let still_pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM message_side_effect_jobs
          WHERE message_id = $1 AND completed_at IS NULL",
    )
    .bind(msg.id.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(still_pending, 0, "side effects quiesced before re-arm");

    let ai_before = count_ai_jobs(&pool, msg.id).await;
    assert_eq!(
        ai_before, 4,
        "v1 send + v2 edit each enqueued one Embed + one Moderate"
    );

    let pool_for_jobs = pool.clone();
    let job_for = |kind: &'static str, version: i32| {
        let pool = pool_for_jobs.clone();
        let message_id = msg.id;
        async move {
            sqlx::query_scalar::<_, uuid::Uuid>(
                "SELECT id FROM message_side_effect_jobs
                  WHERE message_id = $1 AND kind = $2 AND mutation_version = $3",
            )
            .bind(message_id.to_uuid())
            .bind(kind)
            .bind(version)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let stale_v1_embed = job_for("embed", 1).await;
    let v2_embed = job_for("embed", 2).await;
    let v2_moderate = job_for("moderate", 2).await;

    let _serial = super::BATCH_SERIAL.lock().unwrap();
    // Vacuity guard: every re-arm must hit exactly one live row (1/1/1) and
    // flip attempts, or the redelivery never actually happened.
    assert_eq!(rearm_side_effect_job(&pool, stale_v1_embed).await, 1);
    assert_eq!(rearm_side_effect_job(&pool, v2_embed).await, 1);
    assert_eq!(rearm_side_effect_job(&pool, v2_moderate).await, 1);
    assert_eq!(side_effect_job_state(&pool, stale_v1_embed).await.1, 2);
    assert_eq!(side_effect_job_state(&pool, v2_embed).await.1, 2);
    assert_eq!(side_effect_job_state(&pool, v2_moderate).await.1, 2);

    assert_eq!(
        svc.dispatch_message_side_effect_batch(10).await.unwrap(),
        3,
        "the re-armed trio is claimed and completed"
    );

    // The stale v1 job completed (version guard) without failing…
    let (completed_at, _, last_error) = side_effect_job_state(&pool, stale_v1_embed).await;
    assert!(completed_at.is_some(), "stale v1 job completed");
    assert!(last_error.is_none(), "stale v1 job must not fail: {last_error:?}");
    // …and the fresh v2 pair enqueued: delta exactly 2, not 3 (a removed
    // version guard would spend on the stale text and yield delta 3).
    assert_eq!(count_ai_jobs(&pool, msg.id).await - ai_before, 2);
    let (v2_embed_done, _, _) = side_effect_job_state(&pool, v2_embed).await;
    let (v2_moderate_done, _, _) = side_effect_job_state(&pool, v2_moderate).await;
    assert!(v2_embed_done.is_some(), "v2 embed completed");
    assert!(v2_moderate_done.is_some(), "v2 moderate completed");
}
