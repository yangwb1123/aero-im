use super::{action_item_batch_key_digest, TaskRepo};
use aero_common::{ParticipantId, RoomId, TaskId, WorkspaceId};
use sqlx::{PgPool, Postgres, Transaction};

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

struct Fixture {
    pool: PgPool,
    repo: TaskRepo,
    workspace: WorkspaceId,
    room: RoomId,
    actor: ParticipantId,
}

async fn fixture(label: &str) -> Fixture {
    let pool = pool();
    let repo = TaskRepo::new(pool.clone());
    let actor = ParticipantId::new();
    let workspace = WorkspaceId::new();
    let room = RoomId::new();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(actor.to_uuid())
        .bind(format!("action-item-erasure-{label}-{actor}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspaces (id, name, slug, created_by)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(workspace.to_uuid())
    .bind(format!("Action item erasure {label}"))
    .bind(format!("action-item-erasure-{label}-{workspace}"))
    .bind(actor.to_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspace_members
             (workspace_id, participant_id, role, joined_at)
         VALUES ($1, $2, 'owner', now())",
    )
    .bind(workspace.to_uuid())
    .bind(actor.to_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO rooms (id, kind, name, created_by, workspace_id)
         VALUES ($1, 'group', $2, $3, $4)",
    )
    .bind(room.to_uuid())
    .bind(format!("Action item erasure {label} room"))
    .bind(actor.to_uuid())
    .bind(workspace.to_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO room_members
             (room_id, participant_id, role, joined_at)
         VALUES ($1, $2, 'owner', now())",
    )
    .bind(room.to_uuid())
    .bind(actor.to_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    Fixture {
        pool,
        repo,
        workspace,
        room,
        actor,
    }
}

async fn add_member(f: &Fixture, label: &str, workspace_role: &str) -> ParticipantId {
    let participant = ParticipantId::new();
    let mut tx = f.pool.begin().await.unwrap();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(participant.to_uuid())
        .bind(format!("action-item-{label}-{participant}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspace_members
             (workspace_id, participant_id, role, joined_at)
         VALUES ($1, $2, $3, now())",
    )
    .bind(f.workspace.to_uuid())
    .bind(participant.to_uuid())
    .bind(workspace_role)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO room_members
             (room_id, participant_id, role, joined_at)
         VALUES ($1, $2, 'member', now())",
    )
    .bind(f.room.to_uuid())
    .bind(participant.to_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    participant
}

async fn make_actor_erasable(f: &Fixture) -> ParticipantId {
    let governor = add_member(f, "governor", "owner").await;
    sqlx::query(
        "UPDATE workspace_members
            SET role = 'member'
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(f.workspace.to_uuid())
    .bind(f.actor.to_uuid())
    .execute(&f.pool)
    .await
    .unwrap();
    governor
}

fn key(label: &str) -> String {
    format!("{label}-{}", uuid::Uuid::new_v4())
}

async fn receipt_count(
    pool: &PgPool,
    participant: ParticipantId,
    room: RoomId,
    raw_key: &str,
) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*)
           FROM task_action_item_batches
          WHERE participant_id = $1
            AND room_id = $2
            AND idempotency_key = $3",
    )
    .bind(participant.to_uuid())
    .bind(room.to_uuid())
    .bind(action_item_batch_key_digest(raw_key))
    .fetch_one(pool)
    .await
    .unwrap()
}

type TaskProjection = (
    uuid::Uuid,
    uuid::Uuid,
    uuid::Uuid,
    String,
    String,
    Option<String>,
    Option<i16>,
);

async fn task_projection(pool: &PgPool, task: TaskId) -> TaskProjection {
    sqlx::query_as(
        "SELECT id, room_id, creator_id, title, status,
                action_item_batch_key, action_item_batch_index
           FROM tasks
          WHERE id = $1",
    )
    .bind(task.to_uuid())
    .fetch_one(pool)
    .await
    .unwrap()
}

fn constraint(error: &sqlx::Error) -> Option<&str> {
    match error {
        sqlx::Error::Database(database) => database.constraint(),
        _ => None,
    }
}

async fn erasure_tx(pool: &PgPool, actor: ParticipantId) -> Transaction<'static, Postgres> {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('aero.participant_erasure_actor', $1, true)")
        .bind(actor.to_uuid().to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    tx
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn participant_erasure_preserves_tasks_and_erases_only_batch_metadata() {
    let f = fixture("preserve").await;
    let erased_key = key("erased");
    let erased_digest = action_item_batch_key_digest(&erased_key);
    let erased_ids = f
        .repo
        .create_action_item_batch(
            f.room,
            f.actor,
            &erased_key,
            &["Keep first task".into(), "Keep second task".into()],
        )
        .await
        .unwrap();
    sqlx::query("UPDATE tasks SET status = 'in_progress' WHERE id = $1")
        .bind(erased_ids[0].to_uuid())
        .execute(&f.pool)
        .await
        .unwrap();
    let mut before = Vec::with_capacity(erased_ids.len());
    for task in erased_ids.iter().copied() {
        before.push(task_projection(&f.pool, task).await);
    }

    let governor = make_actor_erasable(&f).await;
    let other_key = key("other");
    let other_ids = f
        .repo
        .create_action_item_batch(
            f.room,
            governor,
            &other_key,
            &["Other participant task".into()],
        )
        .await
        .unwrap();
    let other_before = task_projection(&f.pool, other_ids[0]).await;

    assert!(crate::ParticipantRepo::new(f.pool.clone())
        .delete_participant(f.actor)
        .await
        .unwrap());

    assert_eq!(
        receipt_count(&f.pool, f.actor, f.room, &erased_key).await,
        0
    );
    let digest_rows: i64 = sqlx::query_scalar(
        "SELECT
             (SELECT count(*) FROM task_action_item_batches
               WHERE idempotency_key = $1)
           + (SELECT count(*) FROM tasks
               WHERE action_item_batch_key = $1)",
    )
    .bind(&erased_digest)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(digest_rows, 0, "receipt digest and task linkage are erased");

    for (task, original) in erased_ids.iter().copied().zip(before) {
        let preserved = task_projection(&f.pool, task).await;
        assert_eq!(
            (
                preserved.0,
                preserved.1,
                preserved.2,
                preserved.3.as_str(),
                preserved.4.as_str(),
            ),
            (
                original.0,
                original.1,
                original.2,
                original.3.as_str(),
                original.4.as_str(),
            ),
            "shared task identity, room, creator, title, and status survive"
        );
        assert_eq!(preserved.5, None);
        assert_eq!(preserved.6, None);
    }
    let tombstone: (String, bool) = sqlx::query_as(
        "SELECT display_name, deleted_at IS NOT NULL FROM participants WHERE id = $1",
    )
    .bind(f.actor.to_uuid())
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(tombstone, ("[deleted]".into(), true));

    assert_eq!(
        receipt_count(&f.pool, governor, f.room, &other_key).await,
        1,
        "another participant's receipt is untouched"
    );
    assert_eq!(
        task_projection(&f.pool, other_ids[0]).await,
        other_before,
        "another participant's batch task is untouched"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn action_item_erasure_context_allows_only_exact_detach_then_receipt_delete() {
    let f = fixture("raw-context").await;
    let batch_key = key("raw-context");
    let digest = action_item_batch_key_digest(&batch_key);
    let task = f
        .repo
        .create_action_item_batch(
            f.room,
            f.actor,
            &batch_key,
            &["Immutable shared task".into()],
        )
        .await
        .unwrap()[0];
    let original = task_projection(&f.pool, task).await;

    let no_actor = sqlx::query(
        "UPDATE tasks
            SET action_item_batch_key = NULL,
                action_item_batch_index = NULL
          WHERE id = $1",
    )
    .bind(task.to_uuid())
    .execute(&f.pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&no_actor),
        Some("tasks_action_item_batch_erasure_actor_chk")
    );

    let mut active_creator = erasure_tx(&f.pool, f.actor).await;
    let active_creator_error = sqlx::query(
        "UPDATE tasks
            SET action_item_batch_key = NULL,
                action_item_batch_index = NULL
          WHERE id = $1",
    )
    .bind(task.to_uuid())
    .execute(&mut *active_creator)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&active_creator_error),
        Some("tasks_action_item_batch_erasure_tombstone_chk")
    );
    active_creator.rollback().await.unwrap();

    let wrong_actor = make_actor_erasable(&f).await;
    sqlx::query(
        "UPDATE participants
            SET deleted_at = clock_timestamp(),
                display_name = '[deleted]',
                avatar_url = NULL
          WHERE id = $1",
    )
    .bind(f.actor.to_uuid())
    .execute(&f.pool)
    .await
    .unwrap();

    let mut wrong_context = erasure_tx(&f.pool, wrong_actor).await;
    let wrong_context_error = sqlx::query(
        "UPDATE tasks
            SET action_item_batch_key = NULL,
                action_item_batch_index = NULL
          WHERE id = $1",
    )
    .bind(task.to_uuid())
    .execute(&mut *wrong_context)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&wrong_context_error),
        Some("tasks_action_item_batch_erasure_actor_chk")
    );
    wrong_context.rollback().await.unwrap();

    let mut hitchhike = erasure_tx(&f.pool, f.actor).await;
    let hitchhike_error = sqlx::query(
        "UPDATE tasks
            SET action_item_batch_key = NULL,
                action_item_batch_index = NULL,
                title = 'tampered during detach'
          WHERE id = $1",
    )
    .bind(task.to_uuid())
    .execute(&mut *hitchhike)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&hitchhike_error),
        Some("tasks_action_item_batch_erasure_exact_chk")
    );
    hitchhike.rollback().await.unwrap();

    let mut premature_delete = erasure_tx(&f.pool, f.actor).await;
    let premature_delete_error = sqlx::query(
        "DELETE FROM task_action_item_batches
          WHERE participant_id = $1 AND room_id = $2 AND idempotency_key = $3",
    )
    .bind(f.actor.to_uuid())
    .bind(f.room.to_uuid())
    .bind(&digest)
    .execute(&mut *premature_delete)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&premature_delete_error),
        Some("task_action_item_batch_delete_detached_chk")
    );
    premature_delete.rollback().await.unwrap();

    let mut detach = erasure_tx(&f.pool, f.actor).await;
    assert_eq!(
        sqlx::query(
            "UPDATE tasks
                SET action_item_batch_key = NULL,
                    action_item_batch_index = NULL
              WHERE id = $1",
        )
        .bind(task.to_uuid())
        .execute(&mut *detach)
        .await
        .unwrap()
        .rows_affected(),
        1
    );
    detach.commit().await.unwrap();
    let detached = task_projection(&f.pool, task).await;
    assert_eq!(
        (
            &detached.0,
            &detached.1,
            &detached.2,
            &detached.3,
            &detached.4
        ),
        (
            &original.0,
            &original.1,
            &original.2,
            &original.3,
            &original.4
        )
    );
    assert_eq!((detached.5, detached.6), (None, None));

    let no_delete_actor = sqlx::query(
        "DELETE FROM task_action_item_batches
          WHERE participant_id = $1 AND room_id = $2 AND idempotency_key = $3",
    )
    .bind(f.actor.to_uuid())
    .bind(f.room.to_uuid())
    .bind(&digest)
    .execute(&f.pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&no_delete_actor),
        Some("task_action_item_batch_delete_actor_chk")
    );

    let mut erase_receipt = erasure_tx(&f.pool, f.actor).await;
    assert_eq!(
        sqlx::query(
            "DELETE FROM task_action_item_batches
              WHERE participant_id = $1 AND room_id = $2 AND idempotency_key = $3",
        )
        .bind(f.actor.to_uuid())
        .bind(f.room.to_uuid())
        .bind(&digest)
        .execute(&mut *erase_receipt)
        .await
        .unwrap()
        .rows_affected(),
        1
    );
    erase_receipt.commit().await.unwrap();
    assert_eq!(receipt_count(&f.pool, f.actor, f.room, &batch_key).await, 0);
    assert_eq!(task_projection(&f.pool, task).await.3, original.3);

    let resurrection = sqlx::query(
        "UPDATE tasks
            SET action_item_batch_key = $2,
                action_item_batch_index = 0
          WHERE id = $1",
    )
    .bind(task.to_uuid())
    .bind(digest)
    .execute(&f.pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&resurrection),
        Some("tasks_action_item_batch_identity_immutable_chk")
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn action_item_receipt_guard_preserves_participant_and_room_cascades() {
    let f = fixture("parent-cascades").await;
    let member = add_member(&f, "empty-batch-member", "member").await;
    let empty_key = key("empty-cascade");
    assert!(f
        .repo
        .create_action_item_batch(f.room, member, &empty_key, &[])
        .await
        .unwrap()
        .is_empty());
    sqlx::query("DELETE FROM participants WHERE id = $1")
        .bind(member.to_uuid())
        .execute(&f.pool)
        .await
        .unwrap();
    assert_eq!(receipt_count(&f.pool, member, f.room, &empty_key).await, 0);

    let room_key = key("room-cascade");
    let task = f
        .repo
        .create_action_item_batch(f.room, f.actor, &room_key, &["Room-owned task".into()])
        .await
        .unwrap()[0];
    sqlx::query("DELETE FROM rooms WHERE id = $1")
        .bind(f.room.to_uuid())
        .execute(&f.pool)
        .await
        .unwrap();
    let task_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tasks WHERE id = $1)")
        .bind(task.to_uuid())
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert!(!task_exists);
    assert_eq!(receipt_count(&f.pool, f.actor, f.room, &room_key).await, 0);
}
