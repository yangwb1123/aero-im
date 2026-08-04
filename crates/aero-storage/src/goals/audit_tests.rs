use super::*;
use crate::{NewStream, RoomRepo, StreamRepo};
use aero_common::{RoomId, RoomKind, StreamProtocol, WorkspaceId, WorkspaceRole};

#[test]
fn metric_validation() {
    assert!(is_valid_metric("gifts"));
    assert!(is_valid_metric("viewers"));
    assert!(is_valid_metric("points"));
    assert!(!is_valid_metric("subs"));
    assert!(!is_valid_metric(""));
}

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

async fn creator_stream(pool: &PgPool) -> (ParticipantId, Ulid) {
    let owner = participant(pool, "stream-owner").await;
    let stream = StreamRepo::new(pool.clone())
        .create(NewStream {
            owner_id: owner,
            room_id: None,
            title: format!("goal-audit-stream-{owner}"),
            protocol: StreamProtocol::Rtmp,
            stream_key: None,
        })
        .await
        .expect("create stream");
    (owner, stream.id)
}

async fn participant(pool: &PgPool, label: &str) -> ParticipantId {
    let owner = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
        .bind(owner.to_uuid())
        .bind(format!("goal-audit-{label}-{owner}"))
        .execute(pool)
        .await
        .expect("insert participant");
    owner
}

async fn room_linked_stream(
    pool: &PgPool,
) -> (ParticipantId, ParticipantId, WorkspaceId, RoomId, Ulid) {
    let manager = participant(pool, "manager").await;
    let owner = participant(pool, "room-stream-owner").await;
    let workspace = WorkspaceId::new();
    let mut tx = pool.begin().await.expect("begin workspace fixture");
    sqlx::query("INSERT INTO workspaces (id, name, slug, created_by) VALUES ($1, $2, $3, $4)")
        .bind(workspace.to_uuid())
        .bind(format!("goal-audit-workspace-{workspace}"))
        .bind(format!("goal-audit-{workspace}"))
        .bind(manager.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert workspace");
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, $3), ($1, $4, $5)",
    )
    .bind(workspace.to_uuid())
    .bind(manager.to_uuid())
    .bind(WorkspaceRole::Owner.as_str())
    .bind(owner.to_uuid())
    .bind(WorkspaceRole::Member.as_str())
    .execute(&mut *tx)
    .await
    .expect("insert workspace members");
    tx.commit().await.expect("commit workspace fixture");

    let room = RoomRepo::new(pool.clone())
        .create_in_workspace(
            workspace,
            RoomKind::Group,
            Some("goal audit room".into()),
            owner,
        )
        .await
        .expect("create owner room");
    let stream = StreamRepo::new(pool.clone())
        .create(NewStream {
            owner_id: owner,
            room_id: Some(room.id),
            title: format!("goal-audit-room-stream-{owner}"),
            protocol: StreamProtocol::Rtmp,
            stream_key: None,
        })
        .await
        .expect("create stream");
    (manager, owner, workspace, room.id, stream.id)
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations through 0222"]
async fn goals_use_wall_clock_after_waiting_on_resource_locks() {
    let pool = pool();
    let repo = GoalRepo::new(pool.clone());
    let (owner, stream) = creator_stream(&pool).await;

    // A rolling writer starts while its expiry is valid, then waits until after
    // it behind the canonical stream lock.
    let mut stream_guard = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM streams WHERE id = $1 FOR UPDATE")
        .bind(Uuid::from_u128(stream.0))
        .execute(&mut *stream_guard)
        .await
        .unwrap();
    let raw_goal = GoalId::new();
    let raw_expiry = time::OffsetDateTime::now_utc() + time::Duration::milliseconds(700);
    let raw_pool = pool.clone();
    let mut raw_insert = tokio::spawn(async move {
        sqlx::query(
            r"INSERT INTO goals
                  (id, stream_id, creator_id, title, metric_type, target, expires_at)
               VALUES ($1, $2, $3, 'waited raw goal', 'gifts', 10, $4)",
        )
        .bind(raw_goal.to_uuid())
        .bind(Uuid::from_u128(stream.0))
        .bind(owner.to_uuid())
        .bind(raw_expiry)
        .execute(&raw_pool)
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !raw_insert.is_finished(),
        "raw insert waits on the canonical stream"
    );
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    stream_guard.commit().await.unwrap();
    let raw_error = tokio::time::timeout(std::time::Duration::from_secs(3), &mut raw_insert)
        .await
        .expect("raw insert unblocked")
        .expect("raw insert task")
        .expect_err("wall-clock expiry must reject the waited insert");
    assert_eq!(
        raw_error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("22023")
    );

    // Waiting before a progress UPDATE similarly cannot reserve pre-expiry time.
    let goal = repo
        .create_goal_authorized(
            stream,
            owner,
            "waited progress",
            None,
            "points",
            10,
            Some(time::OffsetDateTime::now_utc() + time::Duration::milliseconds(700)),
        )
        .await
        .unwrap();
    let mut goal_guard = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM goals WHERE id = $1 FOR UPDATE")
        .bind(goal.to_uuid())
        .execute(&mut *goal_guard)
        .await
        .unwrap();
    let progress_repo = repo.clone();
    let mut progress = tokio::spawn(async move { progress_repo.add_progress(goal, 1).await });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!progress.is_finished(), "progress waits on the goal row");
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    goal_guard.commit().await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(3), &mut progress)
            .await
            .expect("progress unblocked")
            .expect("progress task")
            .expect("progress query")
            .is_none()
    );
    assert_eq!(repo.get_goal(goal).await.unwrap().unwrap().current_tally, 0);
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations through 0222"]
async fn goals_legacy_oversized_text_does_not_block_lifecycle_updates() {
    let pool = pool();
    let (owner, stream) = creator_stream(&pool).await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("CREATE TEMP TABLE legacy_goal_row (LIKE goals INCLUDING ALL) ON COMMIT DROP")
        .execute(&mut *tx)
        .await
        .unwrap();
    let goal = GoalId::new();
    sqlx::query(
        r"INSERT INTO legacy_goal_row
              (id, stream_id, creator_id, title, metric_type, target)
           VALUES ($1, $2, $3, $4, 'gifts', 10)",
    )
    .bind(goal.to_uuid())
    .bind(Uuid::from_u128(stream.0))
    .bind(owner.to_uuid())
    .bind("x".repeat(MAX_GOAL_TITLE_CHARS + 1))
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        r"CREATE TRIGGER legacy_goal_write_fence
             BEFORE INSERT OR UPDATE ON legacy_goal_row
             FOR EACH ROW EXECUTE FUNCTION goal_write_fence()",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("UPDATE legacy_goal_row SET current_tally = 1 WHERE id = $1")
        .bind(goal.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("unchanged legacy text must not block progress");
    sqlx::query("UPDATE legacy_goal_row SET status = 'cancelled' WHERE id = $1")
        .bind(goal.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("unchanged legacy text must not block cancellation");
    tx.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations through 0222"]
async fn legacy_goal_audit_validation_fails_closed_on_ambiguous_rows() {
    let pool = pool();

    // Recreate a pre-0222 row that could not survive the new write fence. The
    // migration diagnostic must name the bad event instead of silently deleting
    // or converting its negative contribution.
    let invalid_event = Uuid::new_v4();
    let mut invalid_delta = pool.begin().await.unwrap();
    sqlx::query("ALTER TABLE goal_events DISABLE TRIGGER goal_event_write_fence_trigger")
        .execute(&mut *invalid_delta)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE goal_events DROP CONSTRAINT goal_events_positive_delta")
        .execute(&mut *invalid_delta)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO goal_events (id, goal_id, delta)
         VALUES ($1, $2, -1)",
    )
    .bind(invalid_event)
    .bind(Uuid::new_v4())
    .execute(&mut *invalid_delta)
    .await
    .unwrap();
    let error = sqlx::query("SELECT goal_validate_legacy_audit()")
        .execute(&mut *invalid_delta)
        .await
        .expect_err("negative legacy audit deltas must stop migration");
    assert_eq!(
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("goal_events_legacy_invalid_delta")
    );
    assert!(
        error.to_string().contains(&invalid_event.to_string()),
        "diagnostic identifies the legacy event requiring explicit repair"
    );
    invalid_delta.rollback().await.unwrap();

    // A positive event can still be ambiguous when it exceeds the retained
    // tally. Leave both rows untouched and require an operator to decide which
    // historical value is authoritative.
    let repo = GoalRepo::new(pool.clone());
    let (owner, stream) = creator_stream(&pool).await;
    let goal = repo
        .create_goal_authorized(
            stream,
            owner,
            "ambiguous legacy total",
            None,
            "points",
            10,
            None,
        )
        .await
        .unwrap();
    let mut excessive_total = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO goal_events (id, goal_id, delta)
         VALUES ($1, $2, 1)",
    )
    .bind(Uuid::new_v4())
    .bind(goal.to_uuid())
    .execute(&mut *excessive_total)
    .await
    .unwrap();
    let error = sqlx::query("SELECT goal_validate_legacy_audit()")
        .execute(&mut *excessive_total)
        .await
        .expect_err("legacy event totals above the tally must stop migration");
    assert_eq!(
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("goal_events_legacy_total_exceeds_tally")
    );
    assert!(
        error.to_string().contains(&goal.to_uuid().to_string()),
        "diagnostic identifies the legacy goal requiring explicit repair"
    );
    excessive_total.rollback().await.unwrap();

    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM goal_events WHERE goal_id = $1")
            .bind(goal.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap(),
        0,
        "the failed validation fixture rolls back without audit loss"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations through 0222"]
async fn goal_creation_requires_current_effective_stream_access() {
    let pool = pool();
    let repo = GoalRepo::new(pool.clone());

    let (manager, owner, workspace, _room, linked_stream) = room_linked_stream(&pool).await;
    sqlx::query(
        "INSERT INTO workspace_deactivations
             (workspace_id, participant_id, deactivated_by)
         VALUES ($1, $2, $3)",
    )
    .bind(workspace.to_uuid())
    .bind(owner.to_uuid())
    .bind(manager.to_uuid())
    .execute(&pool)
    .await
    .expect("deactivate room-linked owner");

    assert!(matches!(
        repo.create_goal_authorized(
            linked_stream,
            owner,
            "revoked repository goal",
            None,
            "gifts",
            10,
            None,
        )
        .await,
        Err(GoalCreateError::NotAuthorized)
    ));
    let linked_raw = sqlx::query(
        r"INSERT INTO goals
              (id, stream_id, creator_id, title, metric_type, target)
           VALUES ($1, $2, $3, 'revoked raw goal', 'gifts', 10)",
    )
    .bind(GoalId::new().to_uuid())
    .bind(Uuid::from_u128(linked_stream.0))
    .bind(owner.to_uuid())
    .execute(&pool)
    .await
    .expect_err("raw goal must recheck effective room access");
    assert_eq!(
        linked_raw
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("42501")
    );

    let (roomless_owner, roomless_stream) = creator_stream(&pool).await;
    sqlx::query("UPDATE participants SET deleted_at = clock_timestamp() WHERE id = $1")
        .bind(roomless_owner.to_uuid())
        .execute(&pool)
        .await
        .expect("soft-delete roomless owner");
    assert!(matches!(
        repo.create_goal_authorized(
            roomless_stream,
            roomless_owner,
            "deleted repository goal",
            None,
            "points",
            10,
            None,
        )
        .await,
        Err(GoalCreateError::NotAuthorized)
    ));
    let roomless_raw = sqlx::query(
        r"INSERT INTO goals
              (id, stream_id, creator_id, title, metric_type, target)
           VALUES ($1, $2, $3, 'deleted raw goal', 'points', 10)",
    )
    .bind(GoalId::new().to_uuid())
    .bind(Uuid::from_u128(roomless_stream.0))
    .bind(roomless_owner.to_uuid())
    .execute(&pool)
    .await
    .expect_err("raw goal must reject a deleted roomless owner");
    assert_eq!(
        roomless_raw
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .as_deref(),
        Some("42501")
    );
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations through 0222"]
async fn goal_cancel_uses_current_owner_and_current_effective_access() {
    let pool = pool();
    let repo = GoalRepo::new(pool.clone());

    let (manager, original_owner, _workspace, room, stream) = room_linked_stream(&pool).await;
    let transferred_goal = repo
        .create_goal_authorized(
            stream,
            original_owner,
            "transferred stream",
            None,
            "viewers",
            10,
            None,
        )
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO room_members (room_id, participant_id, role)
         VALUES ($1, $2, 'member')
         ON CONFLICT (room_id, participant_id) DO NOTHING",
    )
    .bind(room.to_uuid())
    .bind(manager.to_uuid())
    .execute(&pool)
    .await
    .expect("grant the new owner room access");
    sqlx::query("UPDATE streams SET owner_id = $2 WHERE id = $1")
        .bind(Uuid::from_u128(stream.0))
        .bind(manager.to_uuid())
        .execute(&pool)
        .await
        .expect("transfer canonical stream owner");

    assert_eq!(
        repo.cancel_goal(transferred_goal, original_owner)
            .await
            .unwrap(),
        GoalCancelOutcome::NotFound,
        "historical goal creator loses control after stream transfer"
    );
    assert_eq!(
        repo.cancel_goal(transferred_goal, manager).await.unwrap(),
        GoalCancelOutcome::Cancelled,
        "current canonical owner controls the goal"
    );
    assert_eq!(
        repo.cancel_goal(transferred_goal, manager).await.unwrap(),
        GoalCancelOutcome::AlreadyCancelled,
        "only the authorized current owner sees idempotent retry state"
    );

    let (deactivator, revoked_owner, workspace, _room, revoked_stream) =
        room_linked_stream(&pool).await;
    let revoked_goal = repo
        .create_goal_authorized(
            revoked_stream,
            revoked_owner,
            "revoked cancellation",
            None,
            "gifts",
            10,
            None,
        )
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspace_deactivations
             (workspace_id, participant_id, deactivated_by)
         VALUES ($1, $2, $3)",
    )
    .bind(workspace.to_uuid())
    .bind(revoked_owner.to_uuid())
    .bind(deactivator.to_uuid())
    .execute(&pool)
    .await
    .expect("revoke room-linked owner");
    assert_eq!(
        repo.cancel_goal(revoked_goal, revoked_owner).await.unwrap(),
        GoalCancelOutcome::NotFound
    );
    assert_eq!(
        repo.get_goal(revoked_goal).await.unwrap().unwrap().status,
        "active"
    );

    let (deleted_owner, roomless_stream) = creator_stream(&pool).await;
    let roomless_goal = repo
        .create_goal_authorized(
            roomless_stream,
            deleted_owner,
            "deleted cancellation",
            None,
            "points",
            10,
            None,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE participants SET deleted_at = clock_timestamp() WHERE id = $1")
        .bind(deleted_owner.to_uuid())
        .execute(&pool)
        .await
        .expect("soft-delete roomless owner");
    assert_eq!(
        repo.cancel_goal(roomless_goal, deleted_owner)
            .await
            .unwrap(),
        GoalCancelOutcome::NotFound
    );
    assert_eq!(
        repo.get_goal(roomless_goal).await.unwrap().unwrap().status,
        "active"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations through 0222"]
async fn goal_progress_saturates_and_audit_is_consistent_and_append_only() {
    let pool = pool();
    let repo = GoalRepo::new(pool.clone());
    let (owner, stream) = creator_stream(&pool).await;
    let goal = repo
        .create_goal_authorized(
            stream,
            owner,
            "bigint ceiling",
            None,
            "points",
            i64::MAX,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        repo.add_progress(goal, i64::MAX - 1).await.unwrap(),
        Some((i64::MAX - 1, false))
    );
    assert_eq!(
        repo.add_progress(goal, 2).await.unwrap(),
        Some((i64::MAX, true)),
        "overflowing contribution saturates at the representable ceiling"
    );
    let deltas = sqlx::query_scalar::<_, i64>(
        "SELECT delta FROM goal_events WHERE goal_id = $1 ORDER BY delta",
    )
    .bind(goal.to_uuid())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(deltas, vec![1, i64::MAX - 1]);
    let audit_total = sqlx::query_scalar::<_, String>(
        "SELECT COALESCE(SUM(delta::numeric), 0)::text
           FROM goal_events
          WHERE goal_id = $1",
    )
    .bind(goal.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audit_total, i64::MAX.to_string());

    let (raw_owner, raw_stream) = creator_stream(&pool).await;
    let raw_goal = repo
        .create_goal_authorized(
            raw_stream,
            raw_owner,
            "raw consistency",
            None,
            "gifts",
            10,
            None,
        )
        .await
        .unwrap();
    let unpaired_tally = sqlx::query("UPDATE goals SET current_tally = 1 WHERE id = $1")
        .bind(raw_goal.to_uuid())
        .execute(&pool)
        .await
        .expect_err("a tally cannot commit without its audit delta");
    assert_eq!(
        unpaired_tally
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("goal_audit_tally_consistency")
    );
    assert_eq!(
        repo.get_goal(raw_goal)
            .await
            .unwrap()
            .unwrap()
            .current_tally,
        0
    );

    let unpaired_event = sqlx::query(
        "INSERT INTO goal_events (id, goal_id, delta)
         VALUES ($1, $2, 1)",
    )
    .bind(Uuid::new_v4())
    .bind(raw_goal.to_uuid())
    .execute(&pool)
    .await
    .expect_err("an audit delta cannot commit without its tally change");
    assert_eq!(
        unpaired_event
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("goal_audit_tally_consistency")
    );

    let raw_event = Uuid::new_v4();
    let mut paired = pool.begin().await.unwrap();
    sqlx::query("UPDATE goals SET current_tally = 1 WHERE id = $1")
        .bind(raw_goal.to_uuid())
        .execute(&mut *paired)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO goal_events (id, goal_id, delta, created_at)
         VALUES ($1, $2, 1, $3)",
    )
    .bind(raw_event)
    .bind(raw_goal.to_uuid())
    .bind(time::OffsetDateTime::UNIX_EPOCH)
    .execute(&mut *paired)
    .await
    .unwrap();
    paired
        .commit()
        .await
        .expect("paired tally and event commit");
    let event_created_at = sqlx::query_scalar::<_, time::OffsetDateTime>(
        "SELECT created_at FROM goal_events WHERE id = $1",
    )
    .bind(raw_event)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        event_created_at > time::OffsetDateTime::UNIX_EPOCH + time::Duration::days(1),
        "database replaces caller-supplied audit time"
    );

    for statement in [
        "UPDATE goal_events SET delta = delta + 1 WHERE id = $1",
        "DELETE FROM goal_events WHERE id = $1",
    ] {
        let error = sqlx::query(statement)
            .bind(raw_event)
            .execute(&pool)
            .await
            .expect_err("goal audit rows are immutable");
        assert_eq!(
            error
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::constraint),
            Some("goal_events_append_only")
        );
    }
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations through 0222"]
async fn goal_event_failure_rolls_back_tally_update() {
    let pool = pool();
    let repo = GoalRepo::new(pool.clone());
    let (owner, stream) = creator_stream(&pool).await;
    let goal = repo
        .create_goal_authorized(stream, owner, "audit rollback", None, "gifts", 10, None)
        .await
        .unwrap();
    let suffix = goal.to_uuid().simple().to_string();
    let function_name = format!("aero_test_fail_goal_event_{suffix}");
    let trigger_name = format!("zz_aero_test_fail_goal_event_{suffix}");
    sqlx::query(&format!(
        "CREATE FUNCTION {function_name}()
         RETURNS trigger
         LANGUAGE plpgsql
         AS $$
         BEGIN
             RAISE EXCEPTION 'forced goal event failure';
         END;
         $$"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER {trigger_name}
         BEFORE INSERT ON goal_events
         FOR EACH ROW
         WHEN (NEW.goal_id = '{}'::uuid)
         EXECUTE FUNCTION {function_name}()",
        goal.to_uuid()
    ))
    .execute(&pool)
    .await
    .unwrap();

    assert!(
        repo.add_progress(goal, 3).await.is_err(),
        "forced event failure surfaces from the shared transaction"
    );
    sqlx::query(&format!("DROP TRIGGER {trigger_name} ON goal_events"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("DROP FUNCTION {function_name}()"))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(repo.get_goal(goal).await.unwrap().unwrap().current_tally, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM goal_events WHERE goal_id = $1")
            .bind(goal.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}
