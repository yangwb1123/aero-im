//! DB-integration tests for `ImService`.
//!
//! These tests require a running Postgres with the workspace migrations applied.
//! They are marked `#[ignore]` so `cargo test` stays hermetic; run explicitly with:
//!
//! ```bash
//! DATABASE_URL=postgres://aero:aero@localhost/aero_test \
//!   cargo test -p aero-im-core --lib -- --ignored
//! ```

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use aero_common::{
    Block, CallKind, CallMode, MessageId, ParticipantId, RoomId, RoomKind, WorkspaceRole,
};
use aero_storage::{
    db::PgPool, AiJobRepo, BlockRepo, CallRepo, DmRepo, GroupDmRepo, MessageRepo,
    NotificationBundleRepo, NotificationPrefsRepo, NotificationRepo, ParticipantRepo, ReactionRepo,
    ReceiptRepo, RoomRepo, ThreadMuteRepo, ThreadNotificationPrefsRepo, ThreadSubscriptionRepo,
    WorkspaceMuteRepo, WorkspaceRepo,
};

use crate::service::ImService;
use crate::test_util::MockBus;

mod auto_mod_tests;
mod notifications_tests;
mod recall_tests;
mod relay_tests;
mod room_kind_tests;

fn unique_email(prefix: &str) -> String {
    // Avoid pulling in `uuid` directly — IDs already give us ULIDs through `aero-common`.
    format!("{prefix}-{}@test.local", aero_common::ParticipantId::new())
}

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero@localhost/aero_test".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on well-formed URL")
}

fn service(pool: PgPool) -> ImService {
    let bus = Arc::new(MockBus::default());
    ImService::new(
        RoomRepo::new(pool.clone()),
        MessageRepo::new(pool.clone()),
        ParticipantRepo::new(pool.clone()),
        ReceiptRepo::new(pool.clone()),
        ReactionRepo::new(pool.clone()),
        CallRepo::new(pool.clone()),
        AiJobRepo::new(pool.clone()),
        bus,
    )
    // Workspace-scoped methods need the tenancy repo wired in (additive builder).
    .with_workspaces(WorkspaceRepo::new(pool.clone()))
    // Blocking guard for 1:1 call/DM (no-op unless a block exists).
    .with_block_repo(BlockRepo::new(pool))
}

/// Serializes the seed → dispatch → assert critical sections of the three tests
/// that create globally-claimable DUE side-effect jobs (AT-2, AT-6b, AT-7). The
/// relay loops claim GLOBAL state (`claim_due` without a message filter), so a
/// concurrent batch could steal another test's re-armed/seeded jobs mid-test and
/// skew exact return-value assertions (design §5.6). A std mutex held across
/// awaits is fine here: only these tests contend, there is no re-entrancy, and
/// the runbook additionally runs the suite with `--test-threads=1`.
static BATCH_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Fresh participants + a group room whose creator is `members[0]` and who adds
/// every other member. Returns (members, room).
async fn group_room(
    pool: &PgPool,
    svc: &ImService,
    prefix: &str,
    n: usize,
) -> (Vec<aero_common::Participant>, RoomId) {
    let participants = ParticipantRepo::new(pool.clone());
    let mut members = Vec::new();
    for i in 0..n {
        members.push(new_participant(&participants, &format!("{prefix}-p{i}")).await);
    }
    let room = svc
        .create_room(members[0].id, RoomKind::Group, None)
        .await
        .unwrap()
        .id;
    for member in &members[1..] {
        svc.add_member(members[0].id, room, member.id).await.unwrap();
    }
    (members, room)
}

/// Service with the full notification stack wired (mention/reply fan-out and
/// every suppression store). `bundles=false` omits `NotificationBundleRepo` so
/// reply notifications land immediately instead of deferring into bundles.
/// Returns the `MockBus` for publish assertions.
fn notification_service(pool: PgPool, bundles: bool) -> (ImService, Arc<MockBus>) {
    let bus = Arc::new(MockBus::default());
    let svc = ImService::new(
        RoomRepo::new(pool.clone()),
        MessageRepo::new(pool.clone()),
        ParticipantRepo::new(pool.clone()),
        ReceiptRepo::new(pool.clone()),
        ReactionRepo::new(pool.clone()),
        CallRepo::new(pool.clone()),
        AiJobRepo::new(pool.clone()),
        bus.clone(),
    )
    .with_workspaces(WorkspaceRepo::new(pool.clone()))
    .with_notifications(NotificationRepo::new(pool.clone()))
    .with_notification_prefs(NotificationPrefsRepo::new(pool.clone()))
    .with_block_repo(BlockRepo::new(pool.clone()))
    .with_workspace_mutes(WorkspaceMuteRepo::new(pool.clone()))
    .with_thread_subs(ThreadSubscriptionRepo::new(pool.clone()))
    .with_thread_mutes(ThreadMuteRepo::new(pool.clone()))
    .with_thread_notification_prefs(ThreadNotificationPrefsRepo::new(pool.clone()));
    let svc = if bundles {
        svc.with_notification_bundles(NotificationBundleRepo::new(pool))
    } else {
        svc
    };
    (svc, bus)
}

// ---------------------------------------------------------------------------
// Raw-SQL seeding + assertion helpers for the notification fan-out suite
// (design §3.2). Raw SQL is used only to seed state the public API cannot
// create: `NotificationPrefsRepo::mute`/`unmute` and
// `MessageSideEffectRepo::insert_in_tx` are `#[cfg(test)] pub(crate)` in
// aero-storage (unreachable from a dependency crate), so mute rows and
// synthetic/re-armed side-effect jobs are seeded here. Every count is
// message-scoped so the tests are immune to stray rows from other tests/rooms.
// ---------------------------------------------------------------------------

/// Seed a per-room mute (`channel_mutes`); mirrors `NotificationPrefsRepo::mute`.
async fn insert_channel_mute(pool: &PgPool, participant: ParticipantId, room: RoomId) {
    sqlx::query(
        "INSERT INTO channel_mutes (participant_id, room_id, created_at)
         VALUES ($1, $2, now())",
    )
    .bind(participant.to_uuid())
    .bind(room.to_uuid())
    .execute(pool)
    .await
    .unwrap();
}

/// Simulate a crash-redelivery of a completed side-effect job: re-arm the SAME
/// row (`message_side_effect_jobs` dedups `ON CONFLICT (message_id,
/// mutation_version, kind)`, so redelivery is a re-claim, never a re-insert).
/// Returns `rows_affected` — callers MUST assert 1: a re-arm matching 0 rows (job
/// already completed by the fast path) would otherwise let AT-2/AT-7 pass
/// without exercising the redelivery at all (vacuity guard, design §4 C3).
async fn rearm_side_effect_job(pool: &PgPool, job_id: uuid::Uuid) -> u64 {
    sqlx::query(
        "UPDATE message_side_effect_jobs
            SET completed_at = NULL,
                claimed_at = NULL,
                last_error = NULL,
                attempts = attempts + 1,
                available_at = now() - interval '1 minute'
          WHERE id = $1",
    )
    .bind(job_id)
    .execute(pool)
    .await
    .unwrap()
    .rows_affected()
}

/// Seed a synthetic due side-effect job (`kind` ∈ notifications|embed|moderate).
/// Plain INSERT (no ON CONFLICT): a UNIQUE (`message_id`, `mutation_version`, `kind`)
/// collision errors loudly instead of silently no-op'ing.
async fn insert_side_effect_job(
    pool: &PgPool,
    message_id: MessageId,
    mutation_version: i32,
    kind: &str,
) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO message_side_effect_jobs (id, message_id, mutation_version, kind)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(message_id.to_uuid())
    .bind(mutation_version)
    .bind(kind)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Seed a message row OUTSIDE `ImService` (no outbox row, no side-effect jobs) so
/// a test can install the *only* dispatch driver it wants (AT-6b). `version` 1;
/// `delivery_ordinal` is auto-assigned by the BEFORE-INSERT trigger (mig 0174).
async fn insert_message_row(
    pool: &PgPool,
    room: RoomId,
    sender: ParticipantId,
    blocks: Vec<Block>,
) -> MessageId {
    let id = MessageId::new();
    sqlx::query(
        "INSERT INTO messages (id, room_id, sender_id, blocks, version)
         VALUES ($1, $2, $3, $4::jsonb, 1)",
    )
    .bind(id.to_uuid())
    .bind(room.to_uuid())
    .bind(sender.to_uuid())
    .bind(serde_json::to_value(&blocks).unwrap())
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn count_notifications(
    pool: &PgPool,
    message_id: MessageId,
    participant: Option<ParticipantId>,
) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM notifications
          WHERE message_id = $1
            AND ($2::uuid IS NULL OR participant_id = $2)",
    )
    .bind(message_id.to_uuid())
    .bind(participant.map(|p| p.to_uuid()))
    .fetch_one(pool)
    .await
    .unwrap()
}

/// `event_kind = 'notify'` — lowercase, per the DB CHECK (mig 0163) and
/// `EventOutboxKind::Notify => "notify"` (design correction D1).
async fn count_notify_outbox(pool: &PgPool, message_id: MessageId) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM event_outbox
          WHERE message_id = $1 AND event_kind = 'notify'",
    )
    .bind(message_id.to_uuid())
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn count_bundles(pool: &PgPool, message_id: MessageId) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM notification_bundles WHERE message_id = $1")
        .bind(message_id.to_uuid())
        .fetch_one(pool)
        .await
        .unwrap()
}

/// `ai_jobs` has NO `message_id` column (mig 0002) — only `target_id`; filter on
/// `target_id` exactly as the production query does (`message_side_effect.rs:538`).
async fn count_ai_jobs(pool: &PgPool, message_id: MessageId) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM ai_jobs
          WHERE target_id = $1 AND kind IN ('embed', 'moderate')",
    )
    .bind(message_id.to_uuid())
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The not-yet-published Notify outbox row for a message: (`event_id`, `payload`).
/// For notify rows `event_id` IS the deterministic `delivery_id` — the insert
/// passes it as the idempotency key (`insert_idempotent_in_tx`, `event_outbox.rs`).
async fn pending_notify_outbox(
    pool: &PgPool,
    message_id: MessageId,
) -> Option<(uuid::Uuid, serde_json::Value)> {
    sqlx::query_as::<_, (uuid::Uuid, serde_json::Value)>(
        "SELECT event_id, payload FROM event_outbox
          WHERE message_id = $1 AND event_kind = 'notify' AND published_at IS NULL
          ORDER BY aggregate_version
          LIMIT 1",
    )
    .bind(message_id.to_uuid())
    .fetch_optional(pool)
    .await
    .unwrap()
}

/// (`completed_at`, `attempts`, `last_error`) for one side-effect job row.
async fn side_effect_job_state(
    pool: &PgPool,
    job_id: uuid::Uuid,
) -> (Option<time::OffsetDateTime>, i32, Option<String>) {
    sqlx::query_as::<_, (Option<time::OffsetDateTime>, i32, Option<String>)>(
        "SELECT completed_at, attempts, last_error FROM message_side_effect_jobs WHERE id = $1",
    )
    .bind(job_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn new_participant(participants: &ParticipantRepo, prefix: &str) -> aero_common::Participant {
    let participant = participants
        .create_human(aero_storage::participant::NewHuman {
            email: unique_email(prefix),
            display_name: prefix.into(),
            password_hash: "x".into(),
        })
        .await
        .unwrap();
    // Production registration enrolls every participant into the reserved
    // default workspace before they can create/use legacy rooms. Mirror that
    // invariant here so tests exercise the canonical room-access boundary
    // instead of relying on room membership alone.
    WorkspaceRepo::new(participants.pool().clone())
        .add_member(
            aero_common::WorkspaceId::from_uuid(uuid::Uuid::nil()),
            participant.id,
            WorkspaceRole::Member,
        )
        .await
        .unwrap();
    participant
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn create_room_and_send_message() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let alice = new_participant(&participants, "Alice").await;
    let bob = new_participant(&participants, "Bob").await;

    let svc = service(pool);
    let room = svc
        .create_room(alice.id, RoomKind::Group, Some("test".into()))
        .await
        .unwrap();
    svc.add_member(alice.id, room.id, bob.id).await.unwrap();

    let msg = svc
        .send_message(alice.id, room.id, vec![Block::text("hi")], None, None)
        .await
        .unwrap();
    assert_eq!(msg.room_id, room.id);

    let history = svc.history(bob.id, room.id, None, 50).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, msg.id);
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn pii_guard_blocks_message_with_sensitive_data() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let alice = new_participant(&participants, "alice").await;

    // Wire the PII guard onto the standard service (ROADMAP5 方向五).
    let svc = service(pool).with_pii_detector(std::sync::Arc::new(crate::PiiDetector::new(
        crate::PiiConfig::default(),
    )));
    let room = svc
        .create_room(alice.id, RoomKind::Group, Some("pii".into()))
        .await
        .unwrap();

    // A message carrying a Luhn-valid card number is rejected as Invalid before
    // it can reach the FTS index / embeddings / exports.
    let err = svc
        .send_message(
            alice.id,
            room.id,
            vec![Block::text("here is my card 4111 1111 1111 1111 thanks")],
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, aero_common::Error::Invalid(_)),
        "PII message must be blocked, got {err:?}"
    );

    // PII hidden in a Select option LABEL is also blocked — the guard scans the
    // same text that lands in the FTS index/embeddings (primary + extra
    // searchable text), not just Text blocks. Regression guard for the gap where
    // the scan missed `extra_searchable_text`.
    let err = svc
        .send_message(
            alice.id,
            room.id,
            vec![aero_common::Block::Select {
                action_id: "pick".into(),
                placeholder: Some("choose".into()),
                options: vec![aero_common::SelectOption {
                    value: "a".into(),
                    label: "card 4111 1111 1111 1111".into(),
                }],
            }],
            None,
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, aero_common::Error::Invalid(_)),
        "PII in a Select option label must be blocked, got {err:?}"
    );

    // A clean message still sends, and nothing was persisted for the blocked ones.
    let ok = svc
        .send_message(
            alice.id,
            room.id,
            vec![Block::text("ship it at 3pm")],
            None,
            None,
        )
        .await
        .expect("clean message sends");
    assert_eq!(ok.room_id, room.id);
    let history = svc.history(alice.id, room.id, None, 50).await.unwrap();
    assert_eq!(history.len(), 1, "only the clean message persisted");
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn non_member_cannot_send_message() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let alice = new_participant(&participants, "Alice").await;
    let intruder = new_participant(&participants, "Eve").await;

    let svc = service(pool);
    let room = svc
        .create_room(alice.id, RoomKind::Group, Some("test".into()))
        .await
        .unwrap();

    let err = svc
        .send_message(intruder.id, room.id, vec![Block::text("hi")], None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, aero_common::Error::Forbidden(_)));
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn send_message_publishes_envelope() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let alice = new_participant(&participants, "Alice").await;

    let bus = Arc::new(MockBus::default());
    let svc = ImService::new(
        RoomRepo::new(pool.clone()),
        MessageRepo::new(pool.clone()),
        ParticipantRepo::new(pool.clone()),
        ReceiptRepo::new(pool.clone()),
        ReactionRepo::new(pool.clone()),
        CallRepo::new(pool.clone()),
        AiJobRepo::new(pool.clone()),
        bus.clone(),
    )
    .with_workspaces(WorkspaceRepo::new(pool));
    let room = svc
        .create_room(alice.id, RoomKind::Group, Some("publish-test".into()))
        .await
        .unwrap();
    let _ = svc
        .send_message(alice.id, room.id, vec![Block::text("hi")], None, None)
        .await
        .unwrap();

    let log = bus.published.lock().unwrap();
    // Expect at least one publish to the per-room subject.
    assert!(
        log.iter()
            .any(|(s, _)| s == &ImService::room_subject(room.id)),
        "no publish on im.room.{room_id}: {log:?}",
        room_id = room.id
    );
}

// ---------------------------------------------------------------------------
// Workspace-scoped tenancy methods (additive). These document and compile-verify
// the new RoomRepo/WorkspaceRepo/ImService behavior; they run only against a live
// Postgres with the 0006 workspace migration applied.
// ---------------------------------------------------------------------------

fn unique_slug(prefix: &str) -> String {
    format!("{prefix}-{}", aero_common::WorkspaceId::new())
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn create_room_in_workspace_member_succeeds_room_carries_workspace() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let workspaces = WorkspaceRepo::new(pool.clone());
    let rooms = RoomRepo::new(pool.clone());

    let alice = new_participant(&participants, "alice").await;
    // create() enrolls alice as workspace owner.
    let ws = workspaces
        .create("Acme".into(), unique_slug("acme"), alice.id)
        .await
        .unwrap();

    let svc = service(pool);
    let room = svc
        .create_room_in_workspace(alice.id, ws.id, RoomKind::Channel, Some("general".into()))
        .await
        .unwrap();

    // The room must be stamped with its owning workspace.
    assert_eq!(rooms.room_workspace(room.id).await.unwrap(), Some(ws.id));
    // And it must appear in the workspace-scoped listing for alice.
    let scoped = rooms.rooms_for_in_workspace(alice.id, ws.id).await.unwrap();
    assert!(scoped.iter().any(|r| r.id == room.id));
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn create_room_in_workspace_non_member_forbidden() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let workspaces = WorkspaceRepo::new(pool.clone());

    let owner = new_participant(&participants, "owner").await;
    let outsider = new_participant(&participants, "outsider").await;
    let ws = workspaces
        .create("Acme".into(), unique_slug("acme"), owner.id)
        .await
        .unwrap();

    let svc = service(pool);
    let err = svc
        .create_room_in_workspace(outsider.id, ws.id, RoomKind::Channel, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, aero_common::Error::Forbidden(_)),
        "got {err:?}"
    );
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn create_room_in_workspace_guest_forbidden() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let workspaces = WorkspaceRepo::new(pool.clone());

    let owner = new_participant(&participants, "owner").await;
    let guest = new_participant(&participants, "guest").await;
    let ws = workspaces
        .create("Acme".into(), unique_slug("acme"), owner.id)
        .await
        .unwrap();
    workspaces
        .add_member(ws.id, guest.id, WorkspaceRole::Guest)
        .await
        .unwrap();

    let svc = service(pool);
    let err = svc
        .create_room_in_workspace(guest.id, ws.id, RoomKind::Channel, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, aero_common::Error::Forbidden(_)),
        "got {err:?}"
    );
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn assert_room_access_allows_workspace_and_room_member() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let workspaces = WorkspaceRepo::new(pool.clone());

    let alice = new_participant(&participants, "alice").await;
    let ws = workspaces
        .create("Acme".into(), unique_slug("acme"), alice.id)
        .await
        .unwrap();

    let svc = service(pool);
    let room = svc
        .create_room_in_workspace(alice.id, ws.id, RoomKind::Channel, None)
        .await
        .unwrap();

    // alice is both a workspace member and (as creator/owner) a room member.
    svc.assert_room_access(alice.id, room.id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn assert_room_access_denies_non_room_member_in_same_workspace() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let workspaces = WorkspaceRepo::new(pool.clone());

    let alice = new_participant(&participants, "alice").await;
    let bob = new_participant(&participants, "bob").await;
    let ws = workspaces
        .create("Acme".into(), unique_slug("acme"), alice.id)
        .await
        .unwrap();
    // bob is in the workspace but NOT in the room alice creates.
    workspaces
        .add_member(ws.id, bob.id, WorkspaceRole::Member)
        .await
        .unwrap();

    let svc = service(pool);
    let room = svc
        .create_room_in_workspace(alice.id, ws.id, RoomKind::Channel, None)
        .await
        .unwrap();

    let err = svc.assert_room_access(bob.id, room.id).await.unwrap_err();
    assert!(
        matches!(err, aero_common::Error::Forbidden(_)),
        "got {err:?}"
    );
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn assert_room_access_unknown_room_is_not_found() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let alice = new_participant(&participants, "alice").await;

    let svc = service(pool);
    let err = svc
        .assert_room_access(alice.id, aero_common::RoomId::new())
        .await
        .unwrap_err();
    assert!(
        matches!(err, aero_common::Error::NotFound(_)),
        "got {err:?}"
    );
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn update_member_role_updates_existing_members_idempotently() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let workspaces = WorkspaceRepo::new(pool.clone());

    let owner = new_participant(&participants, "owner").await;
    let member = new_participant(&participants, "member").await;
    let ws = workspaces
        .create("Acme".into(), unique_slug("acme"), owner.id)
        .await
        .unwrap();

    // The low-level role writer must not resurrect a membership that a
    // concurrent deprovision removed.
    assert!(!workspaces
        .update_member_role(ws.id, member.id, WorkspaceRole::Member)
        .await
        .unwrap());
    assert_eq!(
        workspaces.member_role(ws.id, member.id).await.unwrap(),
        None
    );

    workspaces
        .add_member(ws.id, member.id, WorkspaceRole::Member)
        .await
        .unwrap();
    assert_eq!(
        workspaces.member_role(ws.id, member.id).await.unwrap(),
        Some(WorkspaceRole::Member)
    );

    // Update path: overwrite existing role (what add_member's DO NOTHING couldn't do).
    workspaces
        .update_member_role(ws.id, member.id, WorkspaceRole::Admin)
        .await
        .unwrap();
    assert_eq!(
        workspaces.member_role(ws.id, member.id).await.unwrap(),
        Some(WorkspaceRole::Admin)
    );

    // Idempotent re-apply.
    workspaces
        .update_member_role(ws.id, member.id, WorkspaceRole::Admin)
        .await
        .unwrap();
    assert_eq!(
        workspaces.member_role(ws.id, member.id).await.unwrap(),
        Some(WorkspaceRole::Admin)
    );
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn rooms_for_in_workspace_is_tenant_scoped() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let workspaces = WorkspaceRepo::new(pool.clone());
    let rooms = RoomRepo::new(pool.clone());

    let alice = new_participant(&participants, "alice").await;
    let ws_a = workspaces
        .create("A".into(), unique_slug("a"), alice.id)
        .await
        .unwrap();
    let ws_b = workspaces
        .create("B".into(), unique_slug("b"), alice.id)
        .await
        .unwrap();

    let svc = service(pool);
    let room_a = svc
        .create_room_in_workspace(alice.id, ws_a.id, RoomKind::Channel, None)
        .await
        .unwrap();
    let room_b = svc
        .create_room_in_workspace(alice.id, ws_b.id, RoomKind::Channel, None)
        .await
        .unwrap();

    let in_a = rooms
        .rooms_for_in_workspace(alice.id, ws_a.id)
        .await
        .unwrap();
    assert!(in_a.iter().any(|r| r.id == room_a.id));
    assert!(
        !in_a.iter().any(|r| r.id == room_b.id),
        "workspace A listing must not leak workspace B's room"
    );
}

/// A 1:1 direct-message call must not connect users who have blocked each other
/// (either direction), closing the gap where a pre-existing DM could be used to
/// ring someone after a block (notification suppression can't catch a live call).
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn blocked_user_cannot_call_in_direct_room() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let blocks = BlockRepo::new(pool.clone());
    let svc = service(pool.clone());

    let alice = new_participant(&participants, "alice-callblk").await;
    let bob = new_participant(&participants, "bob-callblk").await;
    let charlie = new_participant(&participants, "charlie-callblk").await;
    let dana = new_participant(&participants, "dana-callblk").await;

    let room = DmRepo::new(pool.clone())
        .find_or_create_in_workspace(
            aero_common::WorkspaceId::from_uuid(uuid::Uuid::nil()),
            alice.id,
            bob.id,
        )
        .await
        .unwrap();
    let add_error = svc
        .add_member(alice.id, room.id, charlie.id)
        .await
        .expect_err("generic membership cannot append a third direct participant");
    assert!(matches!(add_error, aero_common::Error::Conflict(_)));
    let direct_members: i64 =
        sqlx::query_scalar("SELECT count(*) FROM room_members WHERE room_id = $1")
            .bind(room.id.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(direct_members, 2);
    let group_dm = GroupDmRepo::new(pool.clone())
        .find_or_create_in_workspace(
            aero_common::WorkspaceId::from_uuid(uuid::Uuid::nil()),
            &[alice.id, bob.id, charlie.id],
            alice.id,
        )
        .await
        .unwrap();
    let group_add_error = svc
        .add_member(alice.id, group_dm.id, dana.id)
        .await
        .expect_err("generic membership cannot mutate a marker-backed group DM");
    assert!(matches!(group_add_error, aero_common::Error::Conflict(_)));
    let group_members: i64 =
        sqlx::query_scalar("SELECT count(*) FROM room_members WHERE room_id = $1")
            .bind(group_dm.id.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(group_members, 3);

    blocks.block(alice.id, bob.id).await.unwrap();

    // Neither direction may ring the other while the block stands.
    let err = svc
        .start_call(
            bob.id,
            room.id,
            CallKind::Audio,
            CallMode::P2p,
            "sdp".into(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, aero_common::Error::Forbidden(_)),
        "blocked → forbidden, got {err:?}"
    );
    let err2 = svc
        .start_call(
            alice.id,
            room.id,
            CallKind::Audio,
            CallMode::P2p,
            "sdp".into(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err2, aero_common::Error::Forbidden(_)));

    // After unblock the call is permitted again (reaches the call machinery).
    blocks.unblock(alice.id, bob.id).await.unwrap();
    svc.start_call(
        bob.id,
        room.id,
        CallKind::Audio,
        CallMode::P2p,
        "sdp".into(),
    )
    .await
    .expect("call allowed after unblock");
}

/// Optimistic-lock edit (migration 0157): a client that supplies the version
/// it last read gets a real lost-update guard — an edit against a stale
/// `expected_version` is rejected with `Conflict` (409) rather than silently
/// overwriting a newer edit, and the version visible to clients increments by
/// exactly one per successful edit.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn edit_message_with_stale_expected_version_conflicts() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let svc = service(pool.clone());

    let alice = new_participant(&participants, "alice-verlock").await;
    let room = svc
        .create_room(alice.id, RoomKind::Group, Some("verlock".into()))
        .await
        .unwrap();

    let msg = svc
        .send_message(alice.id, room.id, vec![Block::text("v1")], None, None)
        .await
        .unwrap();
    assert_eq!(msg.version, 1, "a freshly sent message starts at version 1");

    // First edit, correctly supplying the version just read: succeeds and
    // bumps the version exactly once.
    let edited = svc
        .edit_message(alice.id, msg.id, vec![Block::text("v2")], Some(msg.version))
        .await
        .unwrap();
    assert_eq!(edited.version, 2);

    // Second edit racing against the SAME stale version (as if a second
    // client had loaded the message before the first edit landed): rejected.
    let err = svc
        .edit_message(
            alice.id,
            msg.id,
            vec![Block::text("v3-stale")],
            Some(msg.version),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, aero_common::Error::Conflict(_)),
        "stale version → Conflict, got {err:?}"
    );

    // The message itself is unchanged by the rejected edit.
    let current = svc.messages.get(msg.id).await.unwrap().unwrap();
    assert_eq!(current.version, 2);
    assert_eq!(current.searchable_text(), "v2");

    // A retry with the now-current version succeeds.
    let edited2 = svc
        .edit_message(
            alice.id,
            msg.id,
            vec![Block::text("v3")],
            Some(edited.version),
        )
        .await
        .unwrap();
    assert_eq!(edited2.version, 3);
}

/// A caller that omits `expected_version` (not yet updated to the versioned
/// contract) keeps working exactly as before versioning existed: the edit
/// always succeeds against whatever the current version is, and the returned
/// message still reports an incremented version for callers that DO check it.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn edit_message_without_expected_version_is_unprotected_but_succeeds() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let svc = service(pool.clone());

    let alice = new_participant(&participants, "alice-verlegacy").await;
    let room = svc
        .create_room(alice.id, RoomKind::Group, Some("verlegacy".into()))
        .await
        .unwrap();

    let msg = svc
        .send_message(alice.id, room.id, vec![Block::text("v1")], None, None)
        .await
        .unwrap();

    let edited = svc
        .edit_message(alice.id, msg.id, vec![Block::text("v2")], None)
        .await
        .unwrap();
    assert_eq!(
        edited.version, 2,
        "legacy no-version edits still succeed and bump the counter"
    );
}

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn departed_author_cannot_edit_or_delete_by_global_message_id() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let workspaces = WorkspaceRepo::new(pool.clone());
    let owner = new_participant(&participants, "departed-author-owner").await;
    let alice = new_participant(&participants, "departed-author").await;
    let unique = aero_common::WorkspaceId::new();
    let workspace = workspaces
        .create(
            "Departed author tenant".into(),
            format!("departed-author-{unique}"),
            owner.id,
        )
        .await
        .unwrap();
    workspaces
        .add_member(workspace.id, alice.id, aero_common::WorkspaceRole::Member)
        .await
        .unwrap();
    let svc = service(pool.clone());
    let room = svc
        .create_room_in_workspace(owner.id, workspace.id, RoomKind::Channel, None)
        .await
        .unwrap();
    svc.add_member(owner.id, room.id, alice.id).await.unwrap();
    let message = svc
        .send_message(
            alice.id,
            room.id,
            vec![Block::text("tenant secret")],
            None,
            None,
        )
        .await
        .unwrap();

    RoomRepo::new(pool)
        .remove_member(room.id, alice.id)
        .await
        .unwrap();

    let edit_error = svc
        .edit_message(
            alice.id,
            message.id,
            vec![Block::text("should not land")],
            Some(message.version),
        )
        .await
        .unwrap_err();
    assert!(matches!(edit_error, aero_common::Error::Forbidden(_)));
    let delete_error = svc.delete_message(alice.id, message.id).await.unwrap_err();
    assert!(matches!(delete_error, aero_common::Error::Forbidden(_)));
    let current = svc.messages.get(message.id).await.unwrap().unwrap();
    assert!(current.deleted_at.is_none());
    assert_eq!(current.searchable_text(), "tenant secret");
}
