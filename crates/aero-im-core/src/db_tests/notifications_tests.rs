//! DB-gated integration tests for the notification fan-out pipeline
//! (`ImService::dispatch_notifications`): AT-1…AT-5 of
//! `docs/design/2026-08-06-aero-im-core-notification-fanout-tests.design.md`.
//!
//! Everything is driven through the public service APIs (`send_message`, the
//! post-commit side-effect fast path); raw SQL is used only to seed state the
//! public API cannot create (mutes, re-armed jobs) and to assert row counts.
//! Every test is `#[ignore]`-gated and runs against a throwaway Postgres with
//! the workspace migrations applied (runbook: design §7 / `scripts/test-notification-fanout.sh`).

use aero_common::{Block, NotificationKind, ParticipantId, WorkspaceId};
use aero_storage::{
    BlockRepo, KeywordAlertRepo, NotificationPrefsRepo, NotificationRepo, ParticipantRepo,
    ThreadMuteRepo, ThreadNotificationPrefsRepo, ThreadSubscriptionRepo, UserGroupRepo,
    WorkspaceMuteRepo,
};
use fred::prelude::ClientLike;

use super::{
    count_bundles, count_notifications, count_notify_outbox, group_room, insert_channel_mute,
    new_participant, notification_service, pending_notify_outbox, pool, side_effect_job_state,
};
use crate::service::orig::{notify_delivery_id, NotifyBatchKind};

async fn notification_rows(
    pool: &sqlx::PgPool,
    message_id: aero_common::MessageId,
) -> Vec<(ParticipantId, NotificationKind, Option<ParticipantId>)> {
    sqlx::query_as::<_, (uuid::Uuid, String, Option<uuid::Uuid>)>(
        "SELECT participant_id, kind, actor_id
           FROM notifications
          WHERE message_id = $1
          ORDER BY participant_id",
    )
    .bind(message_id.to_uuid())
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|(participant, kind, actor)| {
        (
            ParticipantId::from_uuid(participant),
            NotificationKind::from_str_lenient(&kind),
            actor.map(ParticipantId::from_uuid),
        )
    })
    .collect()
}

/// Build the nil-workspace fixture used by the user-group acceptance tests. The
/// extra participant is enrolled in the workspace but deliberately not added to
/// the room, proving that group membership alone cannot widen room visibility.
async fn group_mention_fixture(
    prefix: &str,
) -> (
    sqlx::PgPool,
    crate::service::ImService,
    Vec<aero_common::Participant>,
    aero_common::RoomId,
    String,
) {
    let pool = super::pool();
    let (svc, _bus) = super::notification_service(pool.clone(), false);
    let (members, room) = super::group_room(&pool, &svc, prefix, 4).await;
    let eve = super::new_participant(
        &aero_storage::ParticipantRepo::new(pool.clone()),
        &format!("{prefix}-eve"),
    )
    .await;
    let groups = UserGroupRepo::new(pool.clone());
    let workspace = WorkspaceId::from_uuid(uuid::Uuid::nil());
    // The notification suite intentionally shares one throwaway database
    // across tests. The nil workspace is the room fixture's tenant, so the
    // handle must be unique per fixture or AT-1b..AT-1e collide on the
    // user_groups_workspace_id_handle_key constraint after AT-1a.
    let handle = format!("team-{}", uuid::Uuid::new_v4().simple());
    let group = groups
        .create_authorized(workspace, &handle, "Team", members[0].id)
        .await
        .unwrap();
    for participant in [members[1].id, members[2].id, eve.id] {
        groups
            .add_member_authorized(workspace, group.id, participant, members[0].id)
            .await
            .unwrap();
    }
    (pool, svc, [members, vec![eve]].concat(), room, handle)
}

/// Build a `PresenceStore` against `REDIS_URL`, or `None` when the variable is
/// UNSET (manual ad-hoc runs only). A set-but-unreachable Redis PANICS — a green
/// run must never be indistinguishable from "AT-4c never ran" (design §5.8). No
/// localhost default: an unset var must stay distinguishable from a broken one.
async fn try_presence_store() -> Option<aero_storage::PresenceStore> {
    let Ok(url) = std::env::var("REDIS_URL") else {
        eprintln!("skipping presence sub-check (REDIS_URL unset)");
        return None;
    };
    let client = fred::prelude::RedisClient::new(
        fred::types::RedisConfig::from_url(&url)
            .unwrap_or_else(|err| panic!("REDIS_URL set but invalid ({url}): {err}")),
        None,
        None,
        None,
    );
    client.connect();
    tokio::time::timeout(std::time::Duration::from_secs(5), client.wait_for_connect())
        .await
        .unwrap_or_else(|_| {
            panic!("REDIS_URL set but unreachable ({url}) — the presence leg is required")
        })
        .unwrap_or_else(|err| panic!("REDIS_URL set but unreachable ({url}): {err}"));
    Some(aero_storage::PresenceStore::new(client))
}

/// AT-1: an @mention fan-out writes exactly one inbox row per mentioned member
/// (kind `mention`, actor = sender, sender excluded) plus ONE Notify outbox row
/// whose `event_id` equals the deterministic v5 `notify_delivery_id` and whose
/// payload decodes to the matching `NotifyBatch`.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at1_mention_fans_out_to_inbox_and_deterministic_notify_outbox() {
    let pool = pool();
    let (svc, _bus) = notification_service(pool.clone(), false);
    let (members, room) = group_room(&pool, &svc, "at1", 3).await;
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

    // 1. Inbox rows: exactly 2, one per mentioned member, actor = alice;
    //    no row for the sender (self-exclusion) or a non-mentioned member.
    assert_eq!(count_notifications(&pool, msg.id, None).await, 2);
    assert_eq!(count_notifications(&pool, msg.id, Some(alice.id)).await, 0);
    let inbox = NotificationRepo::new(pool.clone());
    for recipient in [bob, carol] {
        let rows = inbox.list(recipient.id, None, false, None).await.unwrap();
        let row = rows
            .iter()
            .find(|n| n.message_id == msg.id)
            .unwrap_or_else(|| panic!("{}'s inbox has the mention", recipient.id));
        assert_eq!(row.kind, NotificationKind::Mention);
        assert_eq!(row.actor_id, Some(alice.id));
    }

    // 2. Exactly one Notify outbox row whose event_id is the deterministic id.
    let delivery_id = notify_delivery_id(msg.id, NotifyBatchKind::Mention);
    assert_eq!(count_notify_outbox(&pool, msg.id).await, 1);
    let (event_id, payload) = pending_notify_outbox(&pool, msg.id)
        .await
        .expect("notify outbox row pending after the send fast path");
    assert_eq!(event_id, delivery_id);

    // 3. The payload decodes to the matching NotifyBatch.
    let aero_common::RoomEvent::NotifyBatch {
        room_id,
        message_id,
        by,
        delivery_id: payload_delivery_id,
        recipients,
    } = serde_json::from_value(payload).unwrap()
    else {
        panic!("notify outbox payload is a NotifyBatch");
    };
    assert_eq!(room_id, room);
    assert_eq!(message_id, msg.id);
    assert_eq!(by, alice.id);
    assert_eq!(payload_delivery_id, delivery_id);
    let mut recipient_ids: Vec<ParticipantId> = recipients.iter().map(|r| r.participant).collect();
    recipient_ids.sort();
    assert_eq!(recipient_ids, [bob.id, carol.id]);
    assert!(recipients
        .iter()
        .all(|r| r.kind == NotificationKind::Mention));
}

/// AT-2: redelivery with the same deterministic `delivery_id` de-dups. Simulate a
/// crash-before-receipt by re-arming the completed `notifications` job, then
/// redispatched: row counts stay identical (2 inbox / 1 outbox) and the job
/// completes with `last_error = None`. The ON CONFLICT dedup (`delivery_id`,
/// `participant_id`) (mig 0137) + ON CONFLICT (`event_id`) (mig 0162) collapse the
/// replay — the collapse is invisible at the service layer (design correction
/// D5), so assertions are row-count + job-state based, never return values.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
#[allow(clippy::await_holding_lock)]
async fn at2_redelivery_with_same_delivery_id_dedups() {
    let pool = pool();
    let (svc, _bus) = notification_service(pool.clone(), false);
    let (members, room) = group_room(&pool, &svc, "at2", 3).await;
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
    assert_eq!(count_notifications(&pool, msg.id, None).await, 2);
    assert_eq!(count_notify_outbox(&pool, msg.id).await, 1);

    let job_id: uuid::Uuid = sqlx::query_scalar(
        "SELECT id FROM message_side_effect_jobs WHERE message_id = $1 AND kind = 'notifications'",
    )
    .bind(msg.id.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();

    // The re-arm → dispatch → assert section runs under BATCH_SERIAL: the
    // re-armed job is globally claimable and AT-6b/AT-7's global batch could
    // otherwise claim it mid-test (design §5.6).
    let _serial = super::batch_serial();
    assert_eq!(
        super::rearm_side_effect_job(&pool, job_id).await,
        1,
        "re-arm must hit exactly one live row (vacuity guard)"
    );
    assert_eq!(
        side_effect_job_state(&pool, job_id).await.1,
        2,
        "re-arm flips attempts"
    );
    assert_eq!(
        svc.dispatch_message_side_effects_for(msg.id).await.unwrap(),
        1
    );

    // Counts unchanged — the second projection collapsed on the idempotency keys.
    assert_eq!(count_notifications(&pool, msg.id, None).await, 2);
    assert_eq!(count_notifications(&pool, msg.id, Some(bob.id)).await, 1);
    assert_eq!(count_notify_outbox(&pool, msg.id).await, 1);
    let (completed_at, _attempts, last_error) = side_effect_job_state(&pool, job_id).await;
    assert!(completed_at.is_some(), "redelivered job completed");
    assert!(
        last_error.is_none(),
        "redelivery must not fail: {last_error:?}"
    );
}

/// AT-2b: the empty-target completion branch. Every plain, unmentioned message —
/// and a broadcast filtered to zero recipients — must complete its
/// `notifications` claim WITHOUT inserting rows or outbox events; a regression
/// would park the job in retry forever with zero visible output (orig.rs:1006).
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at2b_empty_target_message_completes_claim_without_rows() {
    let pool = pool();
    let (svc, _bus) = notification_service(pool.clone(), false);
    let (members, room) = group_room(&pool, &svc, "at2b", 4).await;
    let (alice, bob, carol, dave) = (&members[0], &members[1], &members[2], &members[3]);

    // Leg 1: plain text — zero targets.
    let plain = svc
        .send_message(alice.id, room, vec![Block::text("just text")], None, None)
        .await
        .unwrap();
    assert_eq!(count_notifications(&pool, plain.id, None).await, 0);
    assert_eq!(count_notify_outbox(&pool, plain.id).await, 0);
    assert_eq!(count_bundles(&pool, plain.id).await, 0);
    let plain_job: uuid::Uuid = sqlx::query_scalar(
        "SELECT id FROM message_side_effect_jobs WHERE message_id = $1 AND kind = 'notifications'",
    )
    .bind(plain.id.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    let (completed_at, _, last_error) = side_effect_job_state(&pool, plain_job).await;
    assert!(completed_at.is_some(), "plain-message claim completed");
    assert!(
        last_error.is_none(),
        "plain-message claim must not fail: {last_error:?}"
    );
    assert_eq!(
        svc.dispatch_message_side_effects_for(plain.id)
            .await
            .unwrap(),
        0,
        "nothing due for the plain message"
    );

    // Leg 2: @everyone where every member blocks the sender → the broadcast
    // expands targets, then the block filter removes all of them → zero targets.
    for blocker in [bob.id, carol.id, dave.id] {
        BlockRepo::new(pool.clone())
            .block(blocker, alice.id)
            .await
            .unwrap();
    }
    let muted = svc
        .send_message(alice.id, room, vec![Block::text("@everyone")], None, None)
        .await
        .unwrap();
    assert_eq!(count_notifications(&pool, muted.id, None).await, 0);
    assert_eq!(count_notify_outbox(&pool, muted.id).await, 0);
    let muted_job: uuid::Uuid = sqlx::query_scalar(
        "SELECT id FROM message_side_effect_jobs WHERE message_id = $1 AND kind = 'notifications'",
    )
    .bind(muted.id.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    let (completed_at, _, last_error) = side_effect_job_state(&pool, muted_job).await;
    assert!(completed_at.is_some(), "filtered-to-empty claim completed");
    assert!(
        last_error.is_none(),
        "filtered-to-empty claim must not fail: {last_error:?}"
    );
    assert_eq!(
        svc.dispatch_message_side_effects_for(muted.id)
            .await
            .unwrap(),
        0
    );
}

/// AT-3: reply notifications route to bundles when wired, immediate otherwise;
/// mention and reply batches derive DISTINCT deterministic delivery ids.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at3_reply_notifications_bundle_vs_immediate_routing() {
    let pool = pool();

    // --- Immediate (bundles=false): reply → inbox row + Notify outbox. ---
    let (svc, _bus) = notification_service(pool.clone(), false);
    let (members, room) = group_room(&pool, &svc, "at3a", 2).await;
    let (alice, bob) = (&members[0], &members[1]);
    let root_msg = svc
        .send_message(alice.id, room, vec![Block::text("root")], None, None)
        .await
        .unwrap();
    let reply = svc
        .send_message(
            bob.id,
            room,
            vec![Block::text("reply")],
            Some(root_msg.id),
            None,
        )
        .await
        .unwrap();
    assert_eq!(count_notifications(&pool, reply.id, None).await, 1);
    assert_eq!(
        count_notifications(&pool, reply.id, Some(alice.id)).await,
        1
    );
    assert_eq!(count_notify_outbox(&pool, reply.id).await, 1);
    assert_eq!(count_bundles(&pool, reply.id).await, 0);
    let (event_id, _) = pending_notify_outbox(&pool, reply.id)
        .await
        .expect("immediate reply has an outbox row");
    assert_eq!(
        event_id,
        notify_delivery_id(reply.id, NotifyBatchKind::Reply)
    );

    // --- Bundled (bundles=true): reply defers into notification_bundles. ---
    let (svc, _bus) = notification_service(pool.clone(), true);
    let (members, room) = group_room(&pool, &svc, "at3b", 2).await;
    let (alice, bob) = (&members[0], &members[1]);
    let root_msg = svc
        .send_message(alice.id, room, vec![Block::text("root")], None, None)
        .await
        .unwrap();
    let reply = svc
        .send_message(
            bob.id,
            room,
            vec![Block::text("reply")],
            Some(root_msg.id),
            None,
        )
        .await
        .unwrap();
    assert_eq!(count_notifications(&pool, reply.id, None).await, 0);
    assert_eq!(count_notify_outbox(&pool, reply.id).await, 0);
    assert_eq!(count_bundles(&pool, reply.id).await, 1);
    let (bundle_participant, bundle_kind, bundle_root, bundle_delivery_id): (
        uuid::Uuid,
        String,
        Option<uuid::Uuid>,
        Option<uuid::Uuid>,
    ) = sqlx::query_as(
        "SELECT participant_id, kind, thread_root, delivery_id
           FROM notification_bundles WHERE message_id = $1",
    )
    .bind(reply.id.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(bundle_participant, alice.id.to_uuid());
    assert_eq!(bundle_kind, "reply");
    assert_eq!(bundle_root, Some(root_msg.id.to_uuid()));
    assert_eq!(
        bundle_delivery_id,
        Some(notify_delivery_id(reply.id, NotifyBatchKind::Reply))
    );
    let job_id: uuid::Uuid = sqlx::query_scalar(
        "SELECT id FROM message_side_effect_jobs WHERE message_id = $1 AND kind = 'notifications'",
    )
    .bind(reply.id.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    let (completed_at, _, last_error) = side_effect_job_state(&pool, job_id).await;
    assert!(completed_at.is_some(), "bundle insert completed the claim");
    assert!(
        last_error.is_none(),
        "bundle insert must not fail: {last_error:?}"
    );

    // --- Mixed mention + reply with bundles wired. ---
    let carol = new_participant(&ParticipantRepo::new(pool.clone()), "at3c-carol").await;
    svc.add_member(alice.id, room, carol.id).await.unwrap();
    let mixed = svc
        .send_message(
            bob.id,
            room,
            vec![
                Block::text("mixed"),
                Block::Mention {
                    participant: carol.id,
                },
            ],
            Some(root_msg.id),
            None,
        )
        .await
        .unwrap();
    // Mention → notifications (immediate); reply → notification_bundles.
    assert_eq!(count_notifications(&pool, mixed.id, None).await, 1);
    assert_eq!(
        count_notifications(&pool, mixed.id, Some(carol.id)).await,
        1
    );
    assert_eq!(count_bundles(&pool, mixed.id).await, 1);
    // Exactly ONE outbox notify row (the mention batch): the bundle insert
    // writes NO outbox row — only flush() does (design correction C2).
    assert_eq!(count_notify_outbox(&pool, mixed.id).await, 1);
    let (event_id, _) = pending_notify_outbox(&pool, mixed.id)
        .await
        .expect("mention batch has an outbox row");
    let mention_id = notify_delivery_id(mixed.id, NotifyBatchKind::Mention);
    let reply_id = notify_delivery_id(mixed.id, NotifyBatchKind::Reply);
    assert_eq!(event_id, mention_id);
    let (_, _, _, bundle_delivery_id): (
        uuid::Uuid,
        String,
        Option<uuid::Uuid>,
        Option<uuid::Uuid>,
    ) = sqlx::query_as(
        "SELECT participant_id, kind, thread_root, delivery_id
           FROM notification_bundles WHERE message_id = $1",
    )
    .bind(mixed.id.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(bundle_delivery_id, Some(reply_id));
    assert_ne!(mention_id, reply_id, "mention and reply ids never collide");
}

/// AT-4a: `@everyone` fans out to every member except the sender.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at4a_everyone_targets_all_members_except_sender() {
    let pool = pool();
    let (svc, _bus) = notification_service(pool.clone(), false);
    let (members, room) = group_room(&pool, &svc, "at4a", 4).await;
    let (alice, bob, carol, dave) = (&members[0], &members[1], &members[2], &members[3]);

    let msg = svc
        .send_message(alice.id, room, vec![Block::text("@everyone")], None, None)
        .await
        .unwrap();
    assert_eq!(count_notifications(&pool, msg.id, None).await, 3);
    assert_eq!(count_notifications(&pool, msg.id, Some(alice.id)).await, 0);
    for member in [bob.id, carol.id, dave.id] {
        assert_eq!(count_notifications(&pool, msg.id, Some(member)).await, 1);
    }
}

/// AT-4b: `@here` WITHOUT a presence store keeps the legacy fail-open: the full
/// member set (orig.rs:1020 `let Some(presence) = … else { return member_set }`).
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at4b_here_without_presence_falls_back_to_all_members() {
    let pool = pool();
    let (svc, _bus) = notification_service(pool.clone(), false);
    let (members, room) = group_room(&pool, &svc, "at4b", 4).await;
    let (alice, bob, carol, dave) = (&members[0], &members[1], &members[2], &members[3]);

    let msg = svc
        .send_message(alice.id, room, vec![Block::text("@here")], None, None)
        .await
        .unwrap();
    assert_eq!(count_notifications(&pool, msg.id, None).await, 3);
    for member in [bob.id, carol.id, dave.id] {
        assert_eq!(count_notifications(&pool, msg.id, Some(member)).await, 1);
    }
}

/// AT-4c: `@here` narrows to the online member subset when a presence store is
/// wired — and stays fail-open (full member set) for an empty roster or a
/// roster disjoint from the room membership. Redis-gated: skips ONLY when
/// `REDIS_URL` is unset; a set-but-unreachable Redis fails the test loudly.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at4c_here_narrows_to_online_members_and_fails_open() {
    let Some(presence) = try_presence_store().await else {
        return;
    };
    let pool = pool();
    let (svc, _bus) = notification_service(pool.clone(), false);
    let svc = svc.with_presence(presence.clone());

    // Narrowing: only bob is online in room A → exactly {bob}.
    let (members, room_a) = group_room(&pool, &svc, "at4c-a", 4).await;
    let (alice, bob, carol, dave) = (&members[0], &members[1], &members[2], &members[3]);
    presence.join(room_a, bob.id).await.unwrap();
    let msg = svc
        .send_message(alice.id, room_a, vec![Block::text("@here")], None, None)
        .await
        .unwrap();
    assert_eq!(count_notifications(&pool, msg.id, None).await, 1);
    assert_eq!(count_notifications(&pool, msg.id, Some(bob.id)).await, 1);
    assert_eq!(count_notifications(&pool, msg.id, Some(carol.id)).await, 0);
    assert_eq!(count_notifications(&pool, msg.id, Some(dave.id)).await, 0);

    // Empty roster (nobody online): fail-open to the full member set.
    let (members_b, room_b) = group_room(&pool, &svc, "at4c-b", 4).await;
    let alice_b = &members_b[0];
    let msg_b = svc
        .send_message(alice_b.id, room_b, vec![Block::text("@here")], None, None)
        .await
        .unwrap();
    assert_eq!(count_notifications(&pool, msg_b.id, None).await, 3);

    // Disjoint roster: an OUTSIDER (not a member) is online in room C → the
    // online ∩ members intersection is empty → fail-open to the full set
    // (orig.rs:1041–1045 `intersected.is_empty()` fallback).
    let (members, room_c) = group_room(&pool, &svc, "at4c-c", 4).await;
    let (alice, bob, carol, dave) = (&members[0], &members[1], &members[2], &members[3]);
    let outsider = new_participant(&ParticipantRepo::new(pool.clone()), "at4c-outsider").await;
    presence.join(room_c, outsider.id).await.unwrap();
    let msg_c = svc
        .send_message(alice.id, room_c, vec![Block::text("@here")], None, None)
        .await
        .unwrap();
    assert_eq!(count_notifications(&pool, msg_c.id, None).await, 3);
    for member in [bob.id, carol.id, dave.id] {
        assert_eq!(count_notifications(&pool, msg_c.id, Some(member)).await, 1);
    }
}

/// AT-5a: a per-room mute removes exactly the muted member.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at5a_room_mute_suppresses_only_the_muted_member() {
    let pool = pool();
    let (svc, _bus) = notification_service(pool.clone(), false);
    let (members, room) = group_room(&pool, &svc, "at5a", 4).await;
    let (alice, bob, carol, dave) = (&members[0], &members[1], &members[2], &members[3]);

    insert_channel_mute(&pool, bob.id, room).await;
    let msg = svc
        .send_message(alice.id, room, vec![Block::text("@everyone")], None, None)
        .await
        .unwrap();
    assert_eq!(count_notifications(&pool, msg.id, None).await, 2);
    assert_eq!(count_notifications(&pool, msg.id, Some(bob.id)).await, 0);
    assert_eq!(count_notifications(&pool, msg.id, Some(carol.id)).await, 1);
    assert_eq!(count_notifications(&pool, msg.id, Some(dave.id)).await, 1);
}

/// AT-5b: a full-day DND window (0..1440 — end-exclusive, hole-free by
/// construction, design correction D3) removes exactly the DND'd member.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at5b_dnd_suppresses_only_the_dnd_member() {
    let pool = pool();
    let (svc, _bus) = notification_service(pool.clone(), false);
    let (members, room) = group_room(&pool, &svc, "at5b", 4).await;
    let (alice, bob, carol, dave) = (&members[0], &members[1], &members[2], &members[3]);

    NotificationPrefsRepo::new(pool.clone())
        .set_dnd(bob.id, Some(0), Some(1440))
        .await
        .unwrap();
    let msg = svc
        .send_message(alice.id, room, vec![Block::text("@everyone")], None, None)
        .await
        .unwrap();
    assert_eq!(count_notifications(&pool, msg.id, None).await, 2);
    assert_eq!(count_notifications(&pool, msg.id, Some(bob.id)).await, 0);
    assert_eq!(count_notifications(&pool, msg.id, Some(carol.id)).await, 1);
    assert_eq!(count_notifications(&pool, msg.id, Some(dave.id)).await, 1);
}

/// AT-5c: a one-off snooze (10-minute margin kills boundary flake — `is_snoozed`
/// is `now < until`) removes exactly the snoozing member.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at5c_snooze_suppresses_only_the_snoozing_member() {
    let pool = pool();
    let (svc, _bus) = notification_service(pool.clone(), false);
    let (members, room) = group_room(&pool, &svc, "at5c", 4).await;
    let (alice, bob, carol, dave) = (&members[0], &members[1], &members[2], &members[3]);

    NotificationPrefsRepo::new(pool.clone())
        .set_snooze(
            bob.id,
            Some(time::OffsetDateTime::now_utc() + time::Duration::minutes(10)),
        )
        .await
        .unwrap();
    let msg = svc
        .send_message(alice.id, room, vec![Block::text("@everyone")], None, None)
        .await
        .unwrap();
    assert_eq!(count_notifications(&pool, msg.id, None).await, 2);
    assert_eq!(count_notifications(&pool, msg.id, Some(bob.id)).await, 0);
    assert_eq!(count_notifications(&pool, msg.id, Some(carol.id)).await, 1);
    assert_eq!(count_notifications(&pool, msg.id, Some(dave.id)).await, 1);
}

/// AT-5d: a user block removes exactly the blocker (`blockers_of(sender)`).
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at5d_block_suppresses_only_the_blocker() {
    let pool = pool();
    let (svc, _bus) = notification_service(pool.clone(), false);
    let (members, room) = group_room(&pool, &svc, "at5d", 4).await;
    let (alice, bob, carol, dave) = (&members[0], &members[1], &members[2], &members[3]);

    BlockRepo::new(pool.clone())
        .block(bob.id, alice.id)
        .await
        .unwrap();
    let msg = svc
        .send_message(alice.id, room, vec![Block::text("@everyone")], None, None)
        .await
        .unwrap();
    assert_eq!(count_notifications(&pool, msg.id, None).await, 2);
    assert_eq!(count_notifications(&pool, msg.id, Some(bob.id)).await, 0);
    assert_eq!(count_notifications(&pool, msg.id, Some(carol.id)).await, 1);
    assert_eq!(count_notifications(&pool, msg.id, Some(dave.id)).await, 1);
}

/// AT-5e: a workspace mute removes exactly the muted member (legacy rooms land
/// in the nil workspace, so muting the nil workspace applies).
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at5e_workspace_mute_suppresses_only_the_muted_member() {
    let pool = pool();
    let (svc, _bus) = notification_service(pool.clone(), false);
    let (members, room) = group_room(&pool, &svc, "at5e", 4).await;
    let (alice, bob, carol, dave) = (&members[0], &members[1], &members[2], &members[3]);

    WorkspaceMuteRepo::new(pool.clone())
        .mute(bob.id, WorkspaceId::from_uuid(uuid::Uuid::nil()))
        .await
        .unwrap();
    let msg = svc
        .send_message(alice.id, room, vec![Block::text("@everyone")], None, None)
        .await
        .unwrap();
    assert_eq!(count_notifications(&pool, msg.id, None).await, 2);
    assert_eq!(count_notifications(&pool, msg.id, Some(bob.id)).await, 0);
    assert_eq!(count_notifications(&pool, msg.id, Some(carol.id)).await, 1);
    assert_eq!(count_notifications(&pool, msg.id, Some(dave.id)).await, 1);
}

/// AT-5f: a thread mute removes exactly the muter from reply fan-out (root
/// author + thread subscribers still get notified).
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at5f_thread_mute_suppresses_only_the_muter() {
    let pool = pool();
    let (svc, _bus) = notification_service(pool.clone(), false);
    let (members, room) = group_room(&pool, &svc, "at5f", 4).await;
    let (alice, bob, carol, dave) = (&members[0], &members[1], &members[2], &members[3]);

    let root_msg = svc
        .send_message(alice.id, room, vec![Block::text("root")], None, None)
        .await
        .unwrap();
    let subs = ThreadSubscriptionRepo::new(pool.clone());
    subs.subscribe_authorized(carol.id, root_msg.id)
        .await
        .unwrap();
    subs.subscribe_authorized(dave.id, root_msg.id)
        .await
        .unwrap();
    ThreadMuteRepo::new(pool.clone())
        .mute_authorized(carol.id, root_msg.id)
        .await
        .unwrap();

    let reply = svc
        .send_message(
            bob.id,
            room,
            vec![Block::text("reply")],
            Some(root_msg.id),
            None,
        )
        .await
        .unwrap();
    assert_eq!(count_notifications(&pool, reply.id, None).await, 2);
    assert_eq!(
        count_notifications(&pool, reply.id, Some(alice.id)).await,
        1
    );
    assert_eq!(
        count_notifications(&pool, reply.id, Some(carol.id)).await,
        0
    );
    assert_eq!(count_notifications(&pool, reply.id, Some(dave.id)).await, 1);
}

/// AT-5g: per-thread notification levels — "none" always drops; "mentions"
/// drops a subscriber who is not @-mentioned but delivers when mentioned;
/// the default "all" (no row) always delivers.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at5g_thread_level_suppresses_per_recipient() {
    let pool = pool();
    let (svc, _bus) = notification_service(pool.clone(), false);
    let (members, room) = group_room(&pool, &svc, "at5g", 4).await;
    let (alice, bob, carol, dave) = (&members[0], &members[1], &members[2], &members[3]);

    let root_msg = svc
        .send_message(alice.id, room, vec![Block::text("root")], None, None)
        .await
        .unwrap();
    let subs = ThreadSubscriptionRepo::new(pool.clone());
    subs.subscribe_authorized(carol.id, root_msg.id)
        .await
        .unwrap();
    subs.subscribe_authorized(dave.id, root_msg.id)
        .await
        .unwrap();
    let levels = ThreadNotificationPrefsRepo::new(pool.clone());
    levels
        .set_level_authorized(dave.id, root_msg.id, "none")
        .await
        .unwrap();
    levels
        .set_level_authorized(carol.id, root_msg.id, "mentions")
        .await
        .unwrap();

    // Reply WITHOUT mentioning carol: "mentions" drops her, "none" drops dave,
    // alice (root author, default "all") keeps the notification.
    let quiet = svc
        .send_message(
            bob.id,
            room,
            vec![Block::text("quiet reply")],
            Some(root_msg.id),
            None,
        )
        .await
        .unwrap();
    assert_eq!(count_notifications(&pool, quiet.id, None).await, 1);
    assert_eq!(
        count_notifications(&pool, quiet.id, Some(alice.id)).await,
        1
    );
    assert_eq!(
        count_notifications(&pool, quiet.id, Some(carol.id)).await,
        0
    );
    assert_eq!(count_notifications(&pool, quiet.id, Some(dave.id)).await, 0);

    // Positive control: a reply MENTIONING carol delivers to her ("mentions"
    // delivers on mention), dave stays suppressed ("none" beats the mention).
    let loud = svc
        .send_message(
            bob.id,
            room,
            vec![
                Block::text("loud"),
                Block::Mention {
                    participant: carol.id,
                },
            ],
            Some(root_msg.id),
            None,
        )
        .await
        .unwrap();
    assert_eq!(count_notifications(&pool, loud.id, None).await, 2);
    assert_eq!(count_notifications(&pool, loud.id, Some(alice.id)).await, 1);
    assert_eq!(count_notifications(&pool, loud.id, Some(carol.id)).await, 1);
    assert_eq!(count_notifications(&pool, loud.id, Some(dave.id)).await, 0);
}

/// AT-1a: a user-group handle expands only to the intersection of group members
/// and room members, excluding the sender.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at1a_user_group_mention_intersects_room_members() {
    let (pool, svc, members, room, handle) = group_mention_fixture("at1a-group").await;
    let (alice, bob, carol, dave, eve) = (
        &members[0],
        &members[1],
        &members[2],
        &members[3],
        &members[4],
    );
    let message = svc
        .send_message(
            alice.id,
            room,
            vec![Block::text(format!("hello @{handle}"))],
            None,
            None,
        )
        .await
        .unwrap();
    let rows = notification_rows(&pool, message.id).await;
    assert_eq!(rows.len(), 2);
    assert!(rows.contains(&(bob.id, NotificationKind::Mention, Some(alice.id))));
    assert!(rows.contains(&(carol.id, NotificationKind::Mention, Some(alice.id))));
    assert!(!rows.iter().any(|(p, _, _)| *p == alice.id));
    assert!(!rows.iter().any(|(p, _, _)| *p == dave.id));
    assert!(!rows.iter().any(|(p, _, _)| *p == eve.id));
}

/// AT-1b: reply-author notifications retain the stronger `Reply` kind when the
/// same recipient is also reached through a group handle; direct + group mention
/// paths remain idempotently deduplicated.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at1b_user_group_does_not_downgrade_reply_or_duplicate_mentions() {
    let (pool, svc, members, room, handle) = group_mention_fixture("at1b-group").await;
    let (alice, bob, carol) = (&members[0], &members[1], &members[2]);
    let root_message = svc
        .send_message(bob.id, room, vec![Block::text("root")], None, None)
        .await
        .unwrap();
    let reply = svc
        .send_message(
            alice.id,
            room,
            vec![Block::text(format!("@{handle}"))],
            Some(root_message.id),
            None,
        )
        .await
        .unwrap();
    let rows = notification_rows(&pool, reply.id).await;
    assert_eq!(rows.len(), 2);
    assert!(rows.contains(&(bob.id, NotificationKind::Reply, Some(alice.id))));
    assert!(rows.contains(&(carol.id, NotificationKind::Mention, Some(alice.id))));

    let direct_and_group = svc
        .send_message(
            alice.id,
            room,
            vec![
                Block::Mention {
                    participant: bob.id,
                },
                Block::text(format!("@{handle}")),
            ],
            None,
            None,
        )
        .await
        .unwrap();
    let rows = notification_rows(&pool, direct_and_group.id).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter().filter(|(p, _, _)| *p == bob.id).count(),
        1,
        "direct and group paths deduplicate the same recipient"
    );
    assert!(rows.contains(&(bob.id, NotificationKind::Mention, Some(alice.id))));
    assert!(rows.contains(&(carol.id, NotificationKind::Mention, Some(alice.id))));
}

/// AT-1c: the existing block filter applies equally to a group-expanded target.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at1c_user_group_respects_block_filter() {
    let (pool, svc, members, room, handle) = group_mention_fixture("at1c-group").await;
    let (alice, bob, carol) = (&members[0], &members[1], &members[2]);
    BlockRepo::new(pool.clone())
        .block(bob.id, alice.id)
        .await
        .unwrap();
    let message = svc
        .send_message(
            alice.id,
            room,
            vec![Block::text(format!("@{handle}"))],
            None,
            None,
        )
        .await
        .unwrap();
    let rows = notification_rows(&pool, message.id).await;
    assert_eq!(rows.len(), 1);
    assert!(rows.contains(&(carol.id, NotificationKind::Mention, Some(alice.id))));
    assert!(!rows.iter().any(|(p, _, _)| *p == bob.id));
}

/// AT-1d: a full-day DND window suppresses a group-expanded recipient without
/// affecting the other group member.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at1d_user_group_respects_dnd_filter() {
    let (pool, svc, members, room, handle) = group_mention_fixture("at1d-group").await;
    let (alice, bob, carol) = (&members[0], &members[1], &members[2]);
    NotificationPrefsRepo::new(pool.clone())
        .set_dnd(bob.id, Some(0), Some(1440))
        .await
        .unwrap();
    let message = svc
        .send_message(
            alice.id,
            room,
            vec![Block::text(format!("@{handle}"))],
            None,
            None,
        )
        .await
        .unwrap();
    let rows = notification_rows(&pool, message.id).await;
    assert_eq!(rows.len(), 1);
    assert!(rows.contains(&(carol.id, NotificationKind::Mention, Some(alice.id))));
    assert!(!rows.iter().any(|(p, _, _)| *p == bob.id));
}

/// AT-1e: a room mute suppresses a group-expanded recipient while preserving the
/// notification for the unmuted member.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at1e_user_group_respects_channel_mute() {
    let (pool, svc, members, room, handle) = group_mention_fixture("at1e-group").await;
    let (alice, bob, carol) = (&members[0], &members[1], &members[2]);
    super::insert_channel_mute(&pool, bob.id, room).await;
    let message = svc
        .send_message(
            alice.id,
            room,
            vec![Block::text(format!("@{handle}"))],
            None,
            None,
        )
        .await
        .unwrap();
    let rows = notification_rows(&pool, message.id).await;
    assert_eq!(rows.len(), 1);
    assert!(rows.contains(&(carol.id, NotificationKind::Mention, Some(alice.id))));
    assert!(!rows.iter().any(|(p, _, _)| *p == bob.id));
}

/// AT-2: keyword alerts add exactly the authorized, room-member subscriber;
/// sender, non-room members, and non-matching keywords are excluded.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at2_keyword_alert_fanout_is_room_scoped_and_case_insensitive() {
    let pool = super::pool();
    let (svc, _bus) = super::notification_service(pool.clone(), false);
    let (members, room) = super::group_room(&pool, &svc, "at2-keyword", 4).await;
    let (alice, bob, carol, dave) = (&members[0], &members[1], &members[2], &members[3]);
    let eve = super::new_participant(
        &aero_storage::ParticipantRepo::new(pool.clone()),
        "at2-keyword-eve",
    )
    .await;
    let workspace = WorkspaceId::from_uuid(uuid::Uuid::nil());
    let alerts = KeywordAlertRepo::new(pool.clone());
    alerts
        .add_authorized(bob.id, workspace, "invoice")
        .await
        .unwrap();
    alerts
        .add_authorized(alice.id, workspace, "invoice")
        .await
        .unwrap();
    alerts
        .add_authorized(eve.id, workspace, "invoice")
        .await
        .unwrap();
    alerts
        .add_authorized(carol.id, workspace, "payroll")
        .await
        .unwrap();

    let matched = svc
        .send_message(
            alice.id,
            room,
            vec![Block::text("send the INVOICE now")],
            None,
            None,
        )
        .await
        .unwrap();
    let rows = notification_rows(&pool, matched.id).await;
    assert_eq!(rows.len(), 1);
    assert!(rows.contains(&(bob.id, NotificationKind::Mention, Some(alice.id))));
    assert!(!rows.iter().any(|(p, _, _)| *p == alice.id));
    assert!(!rows.iter().any(|(p, _, _)| *p == eve.id));
    assert!(!rows.iter().any(|(p, _, _)| *p == carol.id));
    assert!(!rows.iter().any(|(p, _, _)| *p == dave.id));

    let unmatched = svc
        .send_message(
            alice.id,
            room,
            vec![Block::text("nothing relevant here")],
            None,
            None,
        )
        .await
        .unwrap();
    assert!(notification_rows(&pool, unmatched.id).await.is_empty());
}

/// AT-4: the service-level flush timer consumes an expired reply bundle,
/// materializes one deterministic Reply notification and Notify outbox row, and
/// remains idempotent on a second invocation.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn at4_notification_bundle_flush_materializes_durable_event() {
    let pool = super::pool();
    let (svc, _bus) = super::notification_service(pool.clone(), true);
    let (members, room) = super::group_room(&pool, &svc, "at4-flush", 2).await;
    let (alice, bob) = (&members[0], &members[1]);
    let root_message = svc
        .send_message(alice.id, room, vec![Block::text("root")], None, None)
        .await
        .unwrap();
    let reply = svc
        .send_message(
            bob.id,
            room,
            vec![Block::text("reply")],
            Some(root_message.id),
            None,
        )
        .await
        .unwrap();
    let job_id: uuid::Uuid = sqlx::query_scalar(
        "SELECT id FROM message_side_effect_jobs
          WHERE message_id = $1 AND kind = 'notifications'",
    )
    .bind(reply.id.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    let (completed_at, _, last_error) = side_effect_job_state(&pool, job_id).await;
    assert!(completed_at.is_some());
    assert!(
        last_error.is_none(),
        "bundle source completed: {last_error:?}"
    );
    assert_eq!(count_notifications(&pool, reply.id, None).await, 0);
    assert_eq!(count_notify_outbox(&pool, reply.id).await, 0);
    assert_eq!(count_bundles(&pool, reply.id).await, 1);

    let bundle_ids = super::bundle_delivery_ids(&pool, reply.id).await;
    assert_eq!(bundle_ids.len(), 1);
    let expected =
        super::expected_bundle_delivery_id(alice.id, room, Some(root_message.id), &bundle_ids);
    super::backdate_bundle(&pool, reply.id).await;
    svc.flush_notification_bundles().await;

    assert_eq!(count_bundles(&pool, reply.id).await, 0);
    assert_eq!(count_notifications(&pool, reply.id, None).await, 1);
    assert_eq!(count_notify_outbox(&pool, reply.id).await, 1);
    let (kind, delivery_id): (String, uuid::Uuid) =
        sqlx::query_as("SELECT kind, delivery_id FROM notifications WHERE message_id = $1")
            .bind(reply.id.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(kind, NotificationKind::Reply.as_str());
    assert_eq!(delivery_id, expected);
    let (event_id, payload) = super::pending_notify_outbox(&pool, reply.id)
        .await
        .expect("flush creates one pending Notify outbox row");
    assert_eq!(event_id, expected);
    let aero_common::RoomEvent::NotifyBatch {
        message_id,
        delivery_id: payload_delivery_id,
        recipients,
        ..
    } = serde_json::from_value(payload).unwrap()
    else {
        panic!("flush outbox payload is NotifyBatch");
    };
    assert_eq!(message_id, reply.id);
    assert_eq!(payload_delivery_id, expected);
    assert_eq!(recipients.len(), 1);
    assert_eq!(recipients[0].participant, alice.id);
    assert_eq!(recipients[0].kind, NotificationKind::Reply);

    svc.flush_notification_bundles().await;
    assert_eq!(count_bundles(&pool, reply.id).await, 0);
    assert_eq!(count_notifications(&pool, reply.id, None).await, 1);
    assert_eq!(count_notify_outbox(&pool, reply.id).await, 1);
    let (completed_at, _, last_error) = side_effect_job_state(&pool, job_id).await;
    assert!(completed_at.is_some());
    assert!(last_error.is_none());
}
