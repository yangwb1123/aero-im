use super::*;
use aero_common::{Block, RoomKind};

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

// Create a throwaway sender + room so the test is self-contained.
async fn fixture(p: &PgPool) -> (RoomId, ParticipantId) {
    let sender = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(sender.to_uuid())
        .bind(format!("sched-sender-{sender}"))
        .execute(p)
        .await
        .expect("insert participant");
    let room = RoomId::new();
    // `rooms.workspace_id` is NOT NULL (migration 0006); reuse the reserved
    // all-zero default workspace, guaranteed to exist by that migration's
    // backfill, so the fixture row satisfies the FK + NOT NULL constraint.
    sqlx::query(
        "INSERT INTO rooms (id, kind, created_by, workspace_id)
         VALUES ($1, $2, $3, '00000000-0000-0000-0000-000000000000'::uuid)",
    )
    .bind(room.to_uuid())
    .bind(format!("{:?}", RoomKind::Group).to_lowercase())
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

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn scheduled_create_list_cancel() {
    let p = pool();
    let repo = ScheduledRepo::new(p.clone());
    let (room, sender) = fixture(&p).await;

    let future = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let blocks = vec![Block::text("send later")];
    let id = repo
        .create(room, sender, &blocks, None, future)
        .await
        .unwrap();

    // Appears in the sender's pending list (both unfiltered and room-scoped).
    let all = repo.list_pending_for_sender(sender, None).await.unwrap();
    assert!(
        all.iter().any(|m| m.id == id),
        "pending list includes the new message"
    );
    let scoped = repo
        .list_pending_for_sender(sender, Some(room))
        .await
        .unwrap();
    assert!(
        scoped.iter().any(|m| m.id == id),
        "room-scoped pending list includes it"
    );

    // A different sender sees none of it.
    let other = ParticipantId::new();
    let theirs = repo.list_pending_for_sender(other, None).await.unwrap();
    assert!(
        !theirs.iter().any(|m| m.id == id),
        "pending list is sender-scoped"
    );

    // Cancel is sender-scoped: a stranger can't cancel it; the owner can, once.
    assert!(
        !repo.cancel(id, other).await.unwrap(),
        "stranger cannot cancel"
    );
    assert!(repo.cancel(id, sender).await.unwrap(), "owner cancels");
    assert!(
        !repo.cancel(id, sender).await.unwrap(),
        "second cancel is a no-op"
    );

    let after = repo.list_pending_for_sender(sender, None).await.unwrap();
    assert!(
        !after.iter().any(|m| m.id == id),
        "canceled message leaves the pending list"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn scheduled_claim_requires_fenced_confirmation() {
    let p = pool();
    let repo = ScheduledRepo::new(p.clone());
    let (room, sender) = fixture(&p).await;

    // Due in the past ⇒ immediately claimable.
    let past = time::OffsetDateTime::now_utc() - time::Duration::minutes(1);
    let id = repo
        .create(room, sender, &[Block::text("due now")], None, past)
        .await
        .unwrap();

    let now = time::OffsetDateTime::now_utc();
    let first = repo.claim_due(now, 100).await.unwrap();
    assert!(
        first.iter().any(|claim| claim.message.id == id),
        "claim_due returns the due message"
    );
    let claim = first
        .iter()
        .find(|claim| claim.message.id == id)
        .expect("claim");
    assert_eq!(claim.attempt, 1);
    assert!(
        claim.message.delivered_at.is_none(),
        "claiming work must not pretend delivery succeeded"
    );

    // A second worker cannot take an unexpired lease.
    let second = repo
        .claim_due(time::OffsetDateTime::now_utc(), 100)
        .await
        .unwrap();
    assert!(
        !second.iter().any(|claim| claim.message.id == id),
        "an unexpired lease is exclusive"
    );

    assert!(
        repo.confirm_delivered(
            id,
            claim.claim_token,
            claim.attempt,
            claim.generation,
            time::OffsetDateTime::now_utc(),
        )
        .await
        .unwrap(),
        "owner confirms"
    );
    assert!(
        !repo
            .confirm_delivered(
                id,
                claim.claim_token,
                claim.attempt,
                claim.generation,
                time::OffsetDateTime::now_utc(),
            )
            .await
            .unwrap(),
        "confirmation is one-shot"
    );

    let pending = repo.list_pending_for_sender(sender, None).await.unwrap();
    assert!(
        !pending.iter().any(|m| m.id == id),
        "delivered message leaves the pending list"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn scheduled_update_edits_pending_and_noops_after_claim() {
    let p = pool();
    let repo = ScheduledRepo::new(p.clone());
    let (room, sender) = fixture(&p).await;

    let t1 = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let id = repo
        .create(room, sender, &[Block::text("v1")], None, t1)
        .await
        .unwrap();

    // Edit blocks + time; the listing reflects the new values.
    let t2 = time::OffsetDateTime::now_utc() + time::Duration::hours(3);
    let new_blocks = vec![Block::text("v2-edited")];
    assert!(
        repo.update(id, sender, t2, &new_blocks, None)
            .await
            .unwrap(),
        "owner edits a pending message"
    );
    let pending = repo.list_pending_for_sender(sender, None).await.unwrap();
    let edited = pending.iter().find(|m| m.id == id).expect("still pending");
    assert!(
        matches!(edited.blocks.first(), Some(Block::Text { content, .. }) if content == "v2-edited"),
        "blocks reflect the edit"
    );
    assert!(edited.scheduled_at > t1, "scheduled_at moved later");

    // A stranger cannot edit it.
    let stranger = ParticipantId::new();
    assert!(
        !repo
            .update(id, stranger, t2, &new_blocks, None)
            .await
            .unwrap(),
        "update is sender-scoped"
    );

    // Once claimed (delivered), update is a no-op.
    let claimed = repo
        .claim_due(
            time::OffsetDateTime::now_utc() + time::Duration::hours(4),
            10,
        )
        .await
        .unwrap();
    assert!(
        claimed.iter().any(|claim| claim.message.id == id),
        "claimed the now-due message"
    );
    assert!(
        !repo
            .update(id, sender, t2, &new_blocks, None)
            .await
            .unwrap(),
        "update of an already-claimed row is a no-op"
    );
    assert!(
        !repo.cancel(id, sender).await.unwrap(),
        "cancellation cannot steal an active delivery claim"
    );
    let admin_payload_rewrite =
        sqlx::query("UPDATE scheduled_messages SET blocks = $2 WHERE id = $1")
            .bind(id.to_uuid())
            .bind(serde_json::json!([{"kind": "text", "content": "admin-rewrite"}]))
            .execute(&p)
            .await;
    assert!(
        matches!(
            admin_payload_rewrite,
            Err(sqlx::Error::Database(ref error))
                if error.code().as_deref() == Some("23514")
        ),
        "database fence blocks payload rewrites after an attempt"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn scheduled_multi_worker_claims_are_disjoint() {
    let p = pool();
    let repo = ScheduledRepo::new(p.clone());
    let (room, sender) = fixture(&p).await;
    let past = time::OffsetDateTime::now_utc() - time::Duration::minutes(1);
    let first_id = repo
        .create(room, sender, &[Block::text("first")], None, past)
        .await
        .unwrap();
    let second_id = repo
        .create(room, sender, &[Block::text("second")], None, past)
        .await
        .unwrap();
    let now = time::OffsetDateTime::now_utc();

    let (left, right) = tokio::join!(
        repo.claim_due_with_policy(now, 1, 300, 8),
        repo.claim_due_with_policy(now, 1, 300, 8)
    );
    let left = left.unwrap();
    let right = right.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(right.len(), 1);
    let claimed = [left[0].message.id, right[0].message.id];
    assert_ne!(claimed[0], claimed[1]);
    assert!(claimed.contains(&first_id));
    assert!(claimed.contains(&second_id));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn scheduled_expired_claim_reclaim_fences_stale_worker() {
    let p = pool();
    let repo = ScheduledRepo::new(p.clone());
    let (room, sender) = fixture(&p).await;
    let due = time::OffsetDateTime::now_utc() - time::Duration::minutes(1);
    let id = repo
        .create(room, sender, &[Block::text("crash-window")], None, due)
        .await
        .unwrap();
    let claimed_at = time::OffsetDateTime::now_utc();
    let first = repo
        .claim_due_with_policy(claimed_at, 10, 1, 8)
        .await
        .unwrap()
        .into_iter()
        .find(|claim| claim.message.id == id)
        .unwrap();

    let second = repo
        .claim_due_with_policy(claimed_at + time::Duration::seconds(2), 10, 30, 8)
        .await
        .unwrap()
        .into_iter()
        .find(|claim| claim.message.id == id)
        .unwrap();
    assert_eq!(second.attempt, first.attempt + 1);
    assert_ne!(second.claim_token, first.claim_token);
    assert!(
        !repo
            .confirm_delivered(
                id,
                first.claim_token,
                first.attempt,
                first.generation,
                time::OffsetDateTime::now_utc(),
            )
            .await
            .unwrap(),
        "expired owner is fenced"
    );
    assert!(repo
        .confirm_delivered(
            id,
            second.claim_token,
            second.attempt,
            second.generation,
            time::OffsetDateTime::now_utc(),
        )
        .await
        .unwrap());
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn scheduled_failure_backoff_dead_letter_and_aba_fencing() {
    let p = pool();
    let repo = ScheduledRepo::new(p.clone());
    let (room, sender) = fixture(&p).await;
    let now = time::OffsetDateTime::now_utc();
    let id = repo
        .create(
            room,
            sender,
            &[Block::text("retry-me")],
            None,
            now - time::Duration::minutes(1),
        )
        .await
        .unwrap();
    let first = repo
        .claim_due_with_policy(now, 10, 30, 3)
        .await
        .unwrap()
        .into_iter()
        .find(|claim| claim.message.id == id)
        .unwrap();
    let retry_at = now + time::Duration::minutes(2);
    assert_eq!(
        repo.record_failure(
            id,
            first.claim_token,
            first.attempt,
            first.generation,
            "temporary",
            retry_at,
            true,
            3,
        )
        .await
        .unwrap(),
        ScheduledFailureDisposition::RetryScheduled
    );
    assert!(
        repo.claim_due_with_policy(now + time::Duration::minutes(1), 10, 30, 3)
            .await
            .unwrap()
            .iter()
            .all(|claim| claim.message.id != id),
        "backoff delays retry"
    );
    let second = repo
        .claim_due_with_policy(retry_at, 10, 30, 3)
        .await
        .unwrap()
        .into_iter()
        .find(|claim| claim.message.id == id)
        .unwrap();
    assert_eq!(second.attempt, 2);
    assert_eq!(
        repo.record_failure(
            id,
            first.claim_token,
            first.attempt,
            first.generation,
            "stale ABA settlement",
            retry_at,
            true,
            3,
        )
        .await
        .unwrap(),
        ScheduledFailureDisposition::FenceLost
    );
    assert_eq!(
        repo.record_failure(
            id,
            second.claim_token,
            second.attempt,
            second.generation,
            "permanent",
            retry_at,
            false,
            3,
        )
        .await
        .unwrap(),
        ScheduledFailureDisposition::Dead
    );
    let state: (String, Option<time::OffsetDateTime>) =
        sqlx::query_as("SELECT delivery_status, dead_at FROM scheduled_messages WHERE id = $1")
            .bind(id.to_uuid())
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(state.0, "dead");
    assert!(state.1.is_some());

    let visible = repo
        .list_actionable_for_sender(sender, Some(room))
        .await
        .unwrap()
        .into_iter()
        .find(|message| message.id == id)
        .expect("dead delivery stays owner-visible");
    assert_eq!(visible.delivery_status, "dead");
    assert_eq!(visible.last_error.as_deref(), Some("permanent"));

    assert!(repo.retry_dead(id, sender, now).await.unwrap());
    let explicit_retry = repo
        .claim_due_with_policy(now, 10, 30, 3)
        .await
        .unwrap()
        .into_iter()
        .find(|claim| claim.message.id == id)
        .expect("owner retry requeues immediately");
    assert_eq!(explicit_retry.attempt, 1);
    assert_eq!(explicit_retry.generation, second.generation + 1);
    assert!(
        !repo
            .confirm_delivered(
                id,
                second.claim_token,
                second.attempt,
                second.generation,
                now,
            )
            .await
            .unwrap(),
        "pre-retry generation remains fenced"
    );
    assert_eq!(
        repo.record_failure(
            id,
            explicit_retry.claim_token,
            explicit_retry.attempt,
            explicit_retry.generation,
            "still permanent",
            now,
            false,
            3,
        )
        .await
        .unwrap(),
        ScheduledFailureDisposition::Dead
    );
    assert!(repo.cancel(id, sender).await.unwrap());
    assert!(
        repo.list_actionable_for_sender(sender, Some(room))
            .await
            .unwrap()
            .iter()
            .all(|message| message.id != id),
        "terminal cancel removes the failed delivery from owner view"
    );

    let exhausted_id = repo
        .create(
            room,
            sender,
            &[Block::text("expire-once")],
            None,
            now - time::Duration::minutes(1),
        )
        .await
        .unwrap();
    let exhausted = repo
        .claim_due_with_policy(now, 10, 1, 1)
        .await
        .unwrap()
        .into_iter()
        .find(|claim| claim.message.id == exhausted_id)
        .unwrap();
    assert_eq!(exhausted.attempt, 1);
    let after_expiry = repo
        .claim_due_with_policy(now + time::Duration::seconds(2), 10, 30, 1)
        .await
        .unwrap();
    assert!(after_expiry
        .iter()
        .all(|claim| claim.message.id != exhausted_id));
    let exhausted_state: String =
        sqlx::query_scalar("SELECT delivery_status FROM scheduled_messages WHERE id = $1")
            .bind(exhausted_id.to_uuid())
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(exhausted_state, "dead");
}
