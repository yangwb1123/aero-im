use super::*;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

/// Create a throwaway sender + room so the test is self-contained.
async fn fixture(p: &PgPool) -> (RoomId, ParticipantId) {
    let sender = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(sender.to_uuid())
        .bind(format!("recurring-sender-{sender}"))
        .execute(p)
        .await
        .expect("insert participant");
    let room = RoomId::new();
    // `rooms.workspace_id` is NOT NULL (migration 0006); reuse the reserved
    // all-zero default workspace, guaranteed to exist by that migration's
    // backfill, so the fixture row satisfies the FK + NOT NULL constraint.
    sqlx::query(
        "INSERT INTO rooms (id, kind, name, created_by, workspace_id)
         VALUES ($1, 'group', $2, $3, '00000000-0000-0000-0000-000000000000')",
    )
    .bind(room.to_uuid())
    .bind(format!("recurring-room-{room}"))
    .bind(sender.to_uuid())
    .execute(p)
    .await
    .expect("insert room");
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ('00000000-0000-0000-0000-000000000000', $1, 'member')
         ON CONFLICT (workspace_id, participant_id) DO NOTHING",
    )
    .bind(sender.to_uuid())
    .execute(p)
    .await
    .expect("insert workspace membership");
    sqlx::query(
        "INSERT INTO room_members (room_id, participant_id, role)
         VALUES ($1, $2, 'owner')
         ON CONFLICT (room_id, participant_id) DO NOTHING",
    )
    .bind(room.to_uuid())
    .bind(sender.to_uuid())
    .execute(p)
    .await
    .expect("insert room membership");
    (room, sender)
}

fn blocks() -> serde_json::Value {
    serde_json::json!([{ "type": "text", "text": "standup time" }])
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn recurring_create_list_for_room() {
    let p = pool();
    let repo = RecurringMessageRepo::new(p.clone());
    let (room, sender) = fixture(&p).await;

    let first = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let id = repo
        .create(room, sender, &blocks(), "daily", first)
        .await
        .unwrap();

    let listed = repo.list_for_room(room).await.unwrap();
    assert!(listed.iter().any(|m| m.id == id), "list_for_room shows it");
    let found = listed.iter().find(|m| m.id == id).expect("present");
    assert_eq!(found.cadence, "daily");
    assert!(found.active, "newly created series is active");

    sqlx::query("DELETE FROM recurring_messages WHERE room_id = $1")
        .bind(room.to_uuid())
        .execute(&p)
        .await
        .ok();
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn recurring_claim_due_respects_next_run() {
    let p = pool();
    let repo = RecurringMessageRepo::new(p.clone());
    let (room, sender) = fixture(&p).await;

    let past = time::OffsetDateTime::now_utc() - time::Duration::minutes(5);
    let future = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let due_id = repo
        .create(room, sender, &blocks(), "hourly", past)
        .await
        .unwrap();
    let pending_id = repo
        .create(room, sender, &blocks(), "hourly", future)
        .await
        .unwrap();

    let now = time::OffsetDateTime::now_utc();
    let due = repo
        .claim_due(now, time::Duration::minutes(5), 10)
        .await
        .unwrap();
    assert!(
        due.iter().any(|claim| claim.series.id == due_id),
        "past next_run is due"
    );
    assert!(
        !due.iter().any(|claim| claim.series.id == pending_id),
        "future next_run is not due"
    );

    sqlx::query("DELETE FROM recurring_messages WHERE room_id = $1")
        .bind(room.to_uuid())
        .execute(&p)
        .await
        .ok();
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn recurring_failure_retries_and_fenced_success_advances() {
    let p = pool();
    let repo = RecurringMessageRepo::new(p.clone());
    let (room, sender) = fixture(&p).await;

    let past = time::OffsetDateTime::now_utc() - time::Duration::minutes(5);
    let id = repo
        .create(room, sender, &blocks(), "hourly", past)
        .await
        .unwrap();

    let now = time::OffsetDateTime::now_utc();
    let first = repo
        .claim_due(now, time::Duration::seconds(1), 10)
        .await
        .unwrap()
        .into_iter()
        .find(|claim| claim.series.id == id)
        .unwrap();
    assert_eq!(first.attempt, 1);
    assert_eq!(first.payload, blocks());
    assert!(
        !repo.cancel(id, sender).await.unwrap(),
        "active claim fences cancellation"
    );

    let retry_at = now + time::Duration::minutes(2);
    assert_eq!(
        repo.record_failure(
            id,
            first.claim_token,
            first.delivery_key,
            first.attempt,
            retry_at,
            "temporary",
            true,
            8,
        )
        .await
        .unwrap(),
        RecurringFailureDisposition::RetryScheduled
    );
    assert!(repo
        .claim_due(
            now + time::Duration::minutes(1),
            time::Duration::minutes(5),
            10,
        )
        .await
        .unwrap()
        .iter()
        .all(|claim| claim.series.id != id));

    let second = repo
        .claim_due(retry_at, time::Duration::minutes(5), 10)
        .await
        .unwrap()
        .into_iter()
        .find(|claim| claim.series.id == id)
        .unwrap();
    assert_eq!(second.delivery_key, first.delivery_key);
    assert_eq!(second.payload, first.payload);
    assert_eq!(second.attempt, 2);
    let next = next_occurrence("hourly", now).expect("known cadence");
    assert!(!repo
        .confirm_sent(
            id,
            first.claim_token,
            first.delivery_key,
            first.attempt,
            next,
            now,
        )
        .await
        .unwrap());
    assert!(repo
        .confirm_sent(
            id,
            second.claim_token,
            second.delivery_key,
            second.attempt,
            next,
            now,
        )
        .await
        .unwrap());
    let state: (time::OffsetDateTime, Option<time::OffsetDateTime>, i32) = sqlx::query_as(
        "SELECT next_run, last_sent_at, delivery_attempts
           FROM recurring_messages WHERE id = $1",
    )
    .bind(id.to_uuid())
    .fetch_one(&p)
    .await
    .unwrap();
    // PostgreSQL timestamps round-trip at microsecond precision, while
    // `OffsetDateTime` may carry nanoseconds.
    assert!((state.0.unix_timestamp_nanos() - next.unix_timestamp_nanos()).abs() < 1_000);
    assert!(state.1.is_some());
    assert_eq!(state.2, 0);

    sqlx::query("DELETE FROM recurring_messages WHERE room_id = $1")
        .bind(room.to_uuid())
        .execute(&p)
        .await
        .ok();
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn recurring_concurrent_workers_and_expired_lease_are_fenced() {
    let p = pool();
    let repo = RecurringMessageRepo::new(p.clone());
    let (room, sender) = fixture(&p).await;
    let now = time::OffsetDateTime::now_utc();
    for text in ["left", "right"] {
        repo.create(
            room,
            sender,
            &serde_json::json!([{"kind":"text","content":text}]),
            "daily",
            now - time::Duration::minutes(1),
        )
        .await
        .unwrap();
    }
    let (left, right) = tokio::join!(
        repo.claim_due(now, time::Duration::seconds(1), 1),
        repo.claim_due(now, time::Duration::seconds(1), 1)
    );
    let left = left.unwrap().pop().unwrap();
    let right = right.unwrap().pop().unwrap();
    assert_ne!(left.series.id, right.series.id);

    let reclaimed_claims = repo
        .claim_due(
            now + time::Duration::seconds(2),
            time::Duration::minutes(5),
            10,
        )
        .await
        .unwrap();
    let reclaimed = reclaimed_claims
        .iter()
        .find(|claim| claim.series.id == left.series.id)
        .unwrap()
        .clone();
    let reclaimed_right = reclaimed_claims
        .iter()
        .find(|claim| claim.series.id == right.series.id)
        .unwrap()
        .clone();
    assert_eq!(reclaimed.delivery_key, left.delivery_key);
    assert_eq!(reclaimed.attempt, left.attempt + 1);
    assert_ne!(reclaimed.claim_token, left.claim_token);
    assert_eq!(
        repo.record_failure(
            left.series.id,
            left.claim_token,
            left.delivery_key,
            left.attempt,
            now,
            "stale",
            true,
            8,
        )
        .await
        .unwrap(),
        RecurringFailureDisposition::FenceLost
    );

    assert_eq!(
        repo.record_failure(
            right.series.id,
            right.claim_token,
            right.delivery_key,
            right.attempt,
            now,
            "membership revoked",
            false,
            8,
        )
        .await
        .unwrap(),
        RecurringFailureDisposition::FenceLost
    );
    assert_eq!(
        repo.record_failure(
            reclaimed_right.series.id,
            reclaimed_right.claim_token,
            reclaimed_right.delivery_key,
            reclaimed_right.attempt,
            now,
            "membership revoked",
            false,
            8,
        )
        .await
        .unwrap(),
        RecurringFailureDisposition::Dead
    );
    let visible_dead = repo
        .list_for_room(room)
        .await
        .unwrap()
        .into_iter()
        .find(|series| series.id == right.series.id)
        .expect("terminal failure remains visible");
    assert!(!visible_dead.active);
    assert!(visible_dead.dead_at.is_some());
    assert_eq!(
        visible_dead.last_error.as_deref(),
        Some("membership revoked")
    );
    assert!(repo.cancel(right.series.id, sender).await.unwrap());

    let after_exhaustion = repo
        .claim_due_with_policy(
            now + time::Duration::minutes(6),
            time::Duration::minutes(5),
            2,
            10,
        )
        .await
        .unwrap();
    assert!(after_exhaustion
        .iter()
        .all(|claim| claim.series.id != reclaimed.series.id));
    let crash_dead: (bool, Option<time::OffsetDateTime>) =
        sqlx::query_as("SELECT active, dead_at FROM recurring_messages WHERE id = $1")
            .bind(reclaimed.series.id.to_uuid())
            .fetch_one(&p)
            .await
            .unwrap();
    assert!(!crash_dead.0);
    assert!(crash_dead.1.is_some());
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn recurring_cancel_owner_scoped() {
    let p = pool();
    let repo = RecurringMessageRepo::new(p.clone());
    let (room, sender) = fixture(&p).await;
    let stranger = ParticipantId::new();

    let first = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let id = repo
        .create(room, sender, &blocks(), "weekly", first)
        .await
        .unwrap();

    // A stranger cannot cancel; the owner can, once.
    assert!(
        !repo.cancel(id, stranger).await.unwrap(),
        "stranger cannot cancel another user's series"
    );
    assert!(repo.cancel(id, sender).await.unwrap(), "owner cancels");
    assert!(
        !repo.cancel(id, sender).await.unwrap(),
        "second cancel is a no-op"
    );

    // Cancelled series leaves the room list.
    assert!(
        !repo
            .list_for_room(room)
            .await
            .unwrap()
            .iter()
            .any(|m| m.id == id),
        "cancelled series leaves list_for_room"
    );

    // A claimed occurrence cannot race cancellation, but once the failed
    // attempt is re-parked the owner can terminate the series instead of
    // being trapped in an endless retry.
    let due_id = repo
        .create(
            room,
            sender,
            &blocks(),
            "hourly",
            time::OffsetDateTime::now_utc() - time::Duration::minutes(1),
        )
        .await
        .unwrap();
    let now = time::OffsetDateTime::now_utc();
    let claim = repo
        .claim_due(now, time::Duration::minutes(5), 10)
        .await
        .unwrap()
        .into_iter()
        .find(|claim| claim.series.id == due_id)
        .unwrap();
    assert!(!repo.cancel(due_id, sender).await.unwrap());
    assert_eq!(
        repo.record_failure(
            due_id,
            claim.claim_token,
            claim.delivery_key,
            claim.attempt,
            now + time::Duration::hours(1),
            "permanent until user fixes access",
            true,
            8,
        )
        .await
        .unwrap(),
        RecurringFailureDisposition::RetryScheduled
    );
    assert!(repo.cancel(due_id, sender).await.unwrap());
    let canceled_state: (bool, i32, Option<uuid::Uuid>) = sqlx::query_as(
        "SELECT active, delivery_attempts, delivery_key
           FROM recurring_messages
          WHERE id = $1",
    )
    .bind(due_id.to_uuid())
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(canceled_state, (false, 0, None));

    sqlx::query("DELETE FROM recurring_messages WHERE room_id = $1")
        .bind(room.to_uuid())
        .execute(&p)
        .await
        .ok();
}
