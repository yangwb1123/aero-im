use aero_common::{ParticipantId, RoomId, TaskId, WorkspaceId};
use sqlx::PgPool;

use super::{
    action_item_batch_key_digest, ActionItemBatchError, TaskRepo, MAX_ACTION_ITEM_BATCH_KEY_LEN,
    MAX_ACTION_ITEM_BATCH_SIZE,
};

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
    room: RoomId,
    actor: ParticipantId,
}

async fn fixture(label: &str) -> Fixture {
    let pool = pool();
    let repo = TaskRepo::new(pool.clone());
    let actor = ParticipantId::new();
    let workspace = WorkspaceId::new();
    let room = RoomId::new();
    let mut tx = pool.begin().await.expect("begin action-item fixture");

    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(actor.to_uuid())
        .bind(format!("action-item-{label}-{actor}"))
        .execute(&mut *tx)
        .await
        .expect("insert participant");
    sqlx::query(
        "INSERT INTO workspaces (id, name, slug, created_by)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(workspace.to_uuid())
    .bind(format!("Action item {label} {workspace}"))
    .bind(format!("action-item-{label}-{workspace}"))
    .bind(actor.to_uuid())
    .execute(&mut *tx)
    .await
    .expect("insert workspace");
    sqlx::query(
        "INSERT INTO workspace_members
             (workspace_id, participant_id, role, joined_at)
         VALUES ($1, $2, 'owner', now())",
    )
    .bind(workspace.to_uuid())
    .bind(actor.to_uuid())
    .execute(&mut *tx)
    .await
    .expect("insert workspace owner");
    sqlx::query(
        "INSERT INTO rooms (id, kind, name, created_by, workspace_id)
         VALUES ($1, 'group', $2, $3, $4)",
    )
    .bind(room.to_uuid())
    .bind(format!("Action item {label} room"))
    .bind(actor.to_uuid())
    .bind(workspace.to_uuid())
    .execute(&mut *tx)
    .await
    .expect("insert room");
    sqlx::query(
        "INSERT INTO room_members
             (room_id, participant_id, role, joined_at)
         VALUES ($1, $2, 'owner', now())",
    )
    .bind(room.to_uuid())
    .bind(actor.to_uuid())
    .execute(&mut *tx)
    .await
    .expect("insert room owner");
    tx.commit().await.expect("commit action-item fixture");

    Fixture {
        pool,
        repo,
        room,
        actor,
    }
}

fn key(label: &str) -> String {
    format!("{label}-{}", uuid::Uuid::new_v4())
}

async fn batch_titles(f: &Fixture, batch_key: &str) -> Vec<String> {
    let batch_key = action_item_batch_key_digest(batch_key);
    sqlx::query_scalar(
        r"SELECT title
            FROM tasks
           WHERE creator_id = $1
             AND room_id = $2
             AND action_item_batch_key = $3
           ORDER BY action_item_batch_index",
    )
    .bind(f.actor.to_uuid())
    .bind(f.room.to_uuid())
    .bind(batch_key)
    .fetch_all(&f.pool)
    .await
    .expect("load batch titles")
}

async fn receipt_count(f: &Fixture, batch_key: &str) -> i64 {
    let batch_key = action_item_batch_key_digest(batch_key);
    sqlx::query_scalar(
        r"SELECT count(*)
            FROM task_action_item_batches
           WHERE participant_id = $1
             AND room_id = $2
             AND idempotency_key = $3",
    )
    .bind(f.actor.to_uuid())
    .bind(f.room.to_uuid())
    .bind(batch_key)
    .fetch_one(&f.pool)
    .await
    .expect("count batch receipts")
}

fn constraint(error: &sqlx::Error) -> Option<&str> {
    match error {
        sqlx::Error::Database(database) => database.constraint(),
        _ => None,
    }
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn action_item_batch_retry_returns_original_ids_and_keys_are_independent() {
    let f = fixture("retry").await;
    let first_key = key("first");
    let original_titles = vec!["Ship release".into(), "Notify support".into()];
    let original_ids = f
        .repo
        .create_action_item_batch(f.room, f.actor, &first_key, &original_titles)
        .await
        .unwrap();

    let retry_ids = f
        .repo
        .create_action_item_batch(
            f.room,
            f.actor,
            &first_key,
            &["Different model answer".into()],
        )
        .await
        .unwrap();
    assert_eq!(retry_ids, original_ids);
    assert_eq!(batch_titles(&f, &first_key).await, original_titles);
    assert_eq!(receipt_count(&f, &first_key).await, 1);
    let raw_key_rows: i64 = sqlx::query_scalar(
        r"SELECT
              (SELECT count(*)
                 FROM task_action_item_batches
                WHERE idempotency_key = $1)
            + (SELECT count(*)
                 FROM tasks
                WHERE action_item_batch_key = $1)",
    )
    .bind(&first_key)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        raw_key_rows, 0,
        "the caller's raw idempotency token must never be persisted"
    );

    // The receipt, rather than the current task projection, is authoritative.
    // A retry still returns every original id after one task is deleted.
    sqlx::query("DELETE FROM tasks WHERE id = $1")
        .bind(original_ids[0].to_uuid())
        .execute(&f.pool)
        .await
        .unwrap();
    let after_delete = f
        .repo
        .create_action_item_batch(f.room, f.actor, &first_key, &["Yet another answer".into()])
        .await
        .unwrap();
    assert_eq!(after_delete, original_ids);

    let second_key = key("second");
    let second_ids = f
        .repo
        .create_action_item_batch(f.room, f.actor, &second_key, &["Independent batch".into()])
        .await
        .unwrap();
    assert_ne!(second_ids, original_ids);
    assert_eq!(
        batch_titles(&f, &second_key).await,
        vec!["Independent batch".to_owned()]
    );

    let empty_key = key("empty");
    assert!(f
        .repo
        .create_action_item_batch(f.room, f.actor, &empty_key, &[])
        .await
        .unwrap()
        .is_empty());
    assert!(f
        .repo
        .create_action_item_batch(
            f.room,
            f.actor,
            &empty_key,
            &["A later non-empty answer".into()],
        )
        .await
        .unwrap()
        .is_empty());
    assert_eq!(receipt_count(&f, &empty_key).await, 1);
    assert!(batch_titles(&f, &empty_key).await.is_empty());
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn action_item_batch_concurrent_same_key_converges_without_mixing() {
    let f = fixture("concurrent").await;
    let batch_key = key("race");
    let left = vec!["Left one".into(), "Left two".into()];
    let right = vec!["Right one".into(), "Right two".into()];

    let (left_result, right_result) = tokio::join!(
        f.repo
            .create_action_item_batch(f.room, f.actor, &batch_key, &left),
        f.repo
            .create_action_item_batch(f.room, f.actor, &batch_key, &right),
    );
    let left_ids = left_result.unwrap();
    let right_ids = right_result.unwrap();
    assert_eq!(left_ids, right_ids);
    assert_eq!(left_ids.len(), 2);
    assert_eq!(receipt_count(&f, &batch_key).await, 1);

    let persisted = batch_titles(&f, &batch_key).await;
    assert!(
        persisted == left || persisted == right,
        "one complete contender wins; titles cannot mix across batches"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn action_item_batch_failure_rolls_back_receipt_and_every_task() {
    let f = fixture("rollback").await;
    let batch_key = key("rollback");
    let invalid_titles = vec!["valid first row".into(), "x".repeat(513)];
    let error = f
        .repo
        .create_action_item_batch(f.room, f.actor, &batch_key, &invalid_titles)
        .await
        .unwrap_err();
    assert!(matches!(error, ActionItemBatchError::Database(_)));
    assert_eq!(receipt_count(&f, &batch_key).await, 0);
    assert!(batch_titles(&f, &batch_key).await.is_empty());

    let recovered = f
        .repo
        .create_action_item_batch(f.room, f.actor, &batch_key, &["valid retry".into()])
        .await
        .unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(
        batch_titles(&f, &batch_key).await,
        vec!["valid retry".to_owned()]
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn action_item_batch_rechecks_access_before_create_or_replay() {
    let f = fixture("revocation").await;
    let batch_key = key("before-revoke");
    let original = f
        .repo
        .create_action_item_batch(
            f.room,
            f.actor,
            &batch_key,
            &["Created while authorized".into()],
        )
        .await
        .unwrap();
    assert_eq!(original.len(), 1);

    // Let revocation win the row-lock race, but do not commit it yet. The
    // canonical access helper must wait on that governance edge and then
    // re-evaluate the now-deleted membership; a stale pre-lock snapshot must
    // never replay the receipt's original ids.
    let mut revocation = f.pool.begin().await.unwrap();
    sqlx::query("DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
        .bind(f.room.to_uuid())
        .bind(f.actor.to_uuid())
        .execute(&mut *revocation)
        .await
        .unwrap();

    let retry_titles = vec!["Retry racing revocation".to_owned()];
    let mut replay =
        Box::pin(
            f.repo
                .create_action_item_batch(f.room, f.actor, &batch_key, &retry_titles),
        );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut replay)
            .await
            .is_err(),
        "replay must wait for the in-flight membership revocation"
    );
    revocation.commit().await.unwrap();
    let replay_result = tokio::time::timeout(std::time::Duration::from_secs(5), replay)
        .await
        .expect("replay resolves after revocation commits");
    assert!(matches!(
        replay_result,
        Err(ActionItemBatchError::ActorNotMember)
    ));

    let new_key = key("after-revoke");
    assert!(matches!(
        f.repo
            .create_action_item_batch(
                f.room,
                f.actor,
                &new_key,
                &["New batch after revocation".into()],
            )
            .await,
        Err(ActionItemBatchError::ActorNotMember)
    ));
    assert_eq!(receipt_count(&f, &batch_key).await, 1);
    assert_eq!(receipt_count(&f, &new_key).await, 0);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn deleted_batch_task_cannot_be_reinserted_after_access_revocation() {
    let f = fixture("deleted-reinsert-revoked").await;
    let batch_key = key("deleted-reinsert-revoked");
    let batch_key_digest = action_item_batch_key_digest(&batch_key);
    let ids = f
        .repo
        .create_action_item_batch(
            f.room,
            f.actor,
            &batch_key,
            &["Created while authorized".into()],
        )
        .await
        .unwrap();

    sqlx::query("DELETE FROM tasks WHERE id = $1")
        .bind(ids[0].to_uuid())
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
        .bind(f.room.to_uuid())
        .bind(f.actor.to_uuid())
        .execute(&f.pool)
        .await
        .unwrap();

    // The durable receipt is replay metadata, not an authorization capability:
    // after revocation, even an exact raw-SQL reconstruction of its deleted child
    // must fail before the receipt row is locked/consulted.
    let error = sqlx::query(
        r"INSERT INTO tasks (
              id,
              room_id,
              creator_id,
              title,
              action_item_batch_key,
              action_item_batch_index
          ) VALUES ($1, $2, $3, $4, $5, 0)",
    )
    .bind(ids[0].to_uuid())
    .bind(f.room.to_uuid())
    .bind(f.actor.to_uuid())
    .bind("Unauthorized reconstruction")
    .bind(batch_key_digest)
    .execute(&f.pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&error),
        Some("tasks_action_item_batch_effective_access_chk")
    );
    assert_eq!(receipt_count(&f, &batch_key).await, 1);
    assert!(batch_titles(&f, &batch_key).await.is_empty());
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn action_item_batch_sql_backstops_completeness_and_immutable_identity() {
    let f = fixture("backstop").await;
    let db_clock_key = key("db-clock");
    let db_clock_key_digest = action_item_batch_key_digest(&db_clock_key);
    let database_owned_created_at: bool = sqlx::query_scalar(
        r"INSERT INTO task_action_item_batches
              (participant_id, room_id, idempotency_key, task_ids, created_at)
           VALUES (
              $1,
              $2,
              $3,
              ARRAY[]::uuid[],
              TIMESTAMPTZ '2999-01-01 00:00:00+00'
           )
        RETURNING created_at < TIMESTAMPTZ '2100-01-01 00:00:00+00'",
    )
    .bind(f.actor.to_uuid())
    .bind(f.room.to_uuid())
    .bind(db_clock_key_digest)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(
        database_owned_created_at,
        "receipt trigger must overwrite caller-supplied created_at"
    );

    let incomplete_key = key("incomplete");
    let incomplete_key_digest = action_item_batch_key_digest(&incomplete_key);
    let expected_id = TaskId::new().to_uuid();
    let mut tx = f.pool.begin().await.unwrap();
    sqlx::query(
        r"INSERT INTO task_action_item_batches
              (participant_id, room_id, idempotency_key, task_ids)
           VALUES ($1, $2, $3, $4)",
    )
    .bind(f.actor.to_uuid())
    .bind(f.room.to_uuid())
    .bind(&incomplete_key_digest)
    .bind(vec![expected_id])
    .execute(&mut *tx)
    .await
    .unwrap();
    let incomplete = tx.commit().await.unwrap_err();
    assert_eq!(
        constraint(&incomplete),
        Some("task_action_item_batch_complete_chk")
    );
    assert_eq!(receipt_count(&f, &incomplete_key).await, 0);

    let complete_key = key("complete");
    let ids = f
        .repo
        .create_action_item_batch(
            f.room,
            f.actor,
            &complete_key,
            &["Immutable identity".into()],
        )
        .await
        .unwrap();
    let identity_error = sqlx::query(
        "UPDATE tasks
            SET action_item_batch_key = $2
          WHERE id = $1",
    )
    .bind(ids[0].to_uuid())
    .bind(action_item_batch_key_digest(&key("rewritten")))
    .execute(&f.pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&identity_error),
        Some("tasks_action_item_batch_identity_immutable_chk")
    );
}

#[tokio::test]
async fn action_item_batch_rejects_invalid_bounds_before_database_io() {
    let repo = TaskRepo::new(pool());
    let room = RoomId::new();
    let actor = ParticipantId::new();
    let too_many = vec!["item".to_owned(); MAX_ACTION_ITEM_BATCH_SIZE + 1];
    assert!(matches!(
        repo.create_action_item_batch(room, actor, "bounded", &too_many)
            .await,
        Err(ActionItemBatchError::TooManyItems)
    ));
    assert!(matches!(
        repo.create_action_item_batch(room, actor, "   ", &[]).await,
        Err(ActionItemBatchError::InvalidIdempotencyKey)
    ));
    let long_key = "k".repeat(MAX_ACTION_ITEM_BATCH_KEY_LEN + 1);
    assert!(matches!(
        repo.create_action_item_batch(room, actor, &long_key, &[])
            .await,
        Err(ActionItemBatchError::InvalidIdempotencyKey)
    ));
}

#[test]
fn action_item_batch_key_is_the_lowercase_sha256_digest() {
    assert_eq!(
        action_item_batch_key_digest("abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}
