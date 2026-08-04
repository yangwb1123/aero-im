//! PostgreSQL-backed search-impression proof and authorization tests.

use std::time::Duration;

use aero_common::{RoomKind, WorkspaceRole};
use sqlx::PgPool;

use super::*;
use crate::{RoomRepo, WorkspaceRepo};

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect_lazy(&url)
        .expect("valid DATABASE_URL")
}

async fn participant(pool: &PgPool, label: &str) -> ParticipantId {
    let participant = ParticipantId::new();
    sqlx::query(
        "INSERT INTO participants (id, kind, display_name)
         VALUES ($1, 'human', $2)",
    )
    .bind(participant.to_uuid())
    .bind(format!("{label}-{participant}"))
    .execute(pool)
    .await
    .unwrap();
    participant
}

struct Fixture {
    pool: PgPool,
    owner: ParticipantId,
    actor: ParticipantId,
    outsider: ParticipantId,
    workspace: WorkspaceId,
    room: RoomId,
    first: MessageId,
    second: MessageId,
}

impl Fixture {
    async fn new(label: &str) -> Self {
        let pool = pool();
        let owner = participant(&pool, &format!("{label}-owner")).await;
        let actor = participant(&pool, &format!("{label}-actor")).await;
        let outsider = participant(&pool, &format!("{label}-outsider")).await;
        let workspace = WorkspaceRepo::new(pool.clone())
            .create(
                format!("{label}-{owner}"),
                format!("{label}-{owner}").to_ascii_lowercase(),
                owner,
            )
            .await
            .unwrap()
            .id;
        let room = RoomRepo::new(pool.clone())
            .create_in_workspace(
                workspace,
                RoomKind::Group,
                Some(format!("{label}-room")),
                owner,
            )
            .await
            .unwrap()
            .id;
        WorkspaceRepo::new(pool.clone())
            .add_member(workspace, actor, WorkspaceRole::Member)
            .await
            .unwrap();
        RoomRepo::new(pool.clone())
            .add_member(room, actor)
            .await
            .unwrap();
        let first = message(&pool, room, owner, &format!("{label}-first")).await;
        let second = message(&pool, room, owner, &format!("{label}-second")).await;
        Self {
            pool,
            owner,
            actor,
            outsider,
            workspace,
            room,
            first,
            second,
        }
    }

    async fn cleanup(&self) {
        sqlx::query("DELETE FROM search_click_events WHERE workspace_id = $1")
            .bind(self.workspace.to_uuid())
            .execute(&self.pool)
            .await
            .ok();
        sqlx::query("DELETE FROM search_impressions WHERE workspace_id = $1")
            .bind(self.workspace.to_uuid())
            .execute(&self.pool)
            .await
            .ok();
        sqlx::query("DELETE FROM workspace_deactivations WHERE workspace_id = $1")
            .bind(self.workspace.to_uuid())
            .execute(&self.pool)
            .await
            .ok();
        sqlx::query("DELETE FROM messages WHERE room_id = $1")
            .bind(self.room.to_uuid())
            .execute(&self.pool)
            .await
            .ok();
        sqlx::query("DELETE FROM room_members WHERE room_id = $1")
            .bind(self.room.to_uuid())
            .execute(&self.pool)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(self.room.to_uuid())
            .execute(&self.pool)
            .await
            .ok();
        sqlx::query("DELETE FROM workspace_members WHERE workspace_id = $1")
            .bind(self.workspace.to_uuid())
            .execute(&self.pool)
            .await
            .ok();
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(self.workspace.to_uuid())
            .execute(&self.pool)
            .await
            .ok();
        for participant in [self.owner, self.actor, self.outsider] {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(participant.to_uuid())
                .execute(&self.pool)
                .await
                .ok();
        }
    }
}

async fn message(pool: &PgPool, room: RoomId, sender: ParticipantId, label: &str) -> MessageId {
    let message = MessageId::new();
    sqlx::query(
        "INSERT INTO messages
             (id, room_id, sender_id, blocks, searchable_text, created_at)
         VALUES (
             $1,
             $2,
             $3,
             jsonb_build_array(jsonb_build_object('type', 'text', 'content', $4)),
             $4,
             clock_timestamp()
         )",
    )
    .bind(message.to_uuid())
    .bind(room.to_uuid())
    .bind(sender.to_uuid())
    .bind(label)
    .execute(pool)
    .await
    .unwrap();
    message
}

fn constraint(error: &sqlx::Error) -> Option<&str> {
    error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::constraint)
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn legal_roundtrip_is_rank_derived_idempotent_owned_and_expiring() {
    let fixture = Fixture::new("search-proof-roundtrip").await;
    let repo = SearchFeedbackRepo::new(fixture.pool.clone());
    let full_query = format!(
        "from:@{} in:{} before:{} after:{} since:2026-01-01 until:2026-12-31 alpha beta",
        fixture.owner, fixture.room, fixture.second, fixture.first
    );
    let impression = repo
        .create_impression(
            fixture.actor,
            fixture.workspace,
            &full_query,
            &[fixture.second, fixture.first],
        )
        .await
        .unwrap();

    let first = repo
        .record_impression_click(fixture.actor, impression.id, fixture.first)
        .await
        .unwrap();
    assert_eq!(first.result_rank, 1);
    let retry = repo
        .record_impression_click(fixture.actor, impression.id, fixture.first)
        .await
        .unwrap();
    assert_eq!(retry, first, "same retry returns the stable receipt");

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM search_click_events WHERE impression_id = $1")
            .bind(impression.id)
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert_eq!(count, 1, "retry cannot pollute click analytics");
    let recorded_query: String =
        sqlx::query_scalar("SELECT query_text FROM search_click_events WHERE impression_id = $1")
            .bind(impression.id)
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert_eq!(
        recorded_query, full_query,
        "proof and click retain the complete normalized query including operators"
    );
    assert!(matches!(
        repo.record_impression_click(fixture.actor, impression.id, fixture.second)
            .await,
        Err(Error::Conflict(_))
    ));

    let unconsumed = repo
        .create_impression(
            fixture.actor,
            fixture.workspace,
            "alpha beta",
            &[fixture.first],
        )
        .await
        .unwrap();
    assert!(matches!(
        repo.record_impression_click(fixture.actor, unconsumed.id, MessageId::new())
            .await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        repo.record_impression_click(fixture.outsider, unconsumed.id, fixture.first)
            .await,
        Err(Error::NotFound(_))
    ));

    let recently_expired: uuid::Uuid = sqlx::query_scalar(
        r"WITH instant AS (SELECT clock_timestamp() AS now)
          INSERT INTO search_impressions
              (participant_id, workspace_id, query_text, result_ids,
               created_at, expires_at)
          SELECT $1, $2, 'expired proof', $3,
                 instant.now - INTERVAL '16 minutes',
                 instant.now - INTERVAL '1 minute'
            FROM instant
          RETURNING id",
    )
    .bind(fixture.actor.to_uuid())
    .bind(fixture.workspace.to_uuid())
    .bind(vec![fixture.first.to_uuid()])
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert!(matches!(
        repo.record_impression_click(fixture.actor, recently_expired, fixture.first)
            .await,
        Err(Error::Conflict(_))
    ));
    let raw_backdate = sqlx::query(
        r"UPDATE search_impressions
              SET clicked_result_id = $2,
                  clicked_at = expires_at - INTERVAL '1 second'
            WHERE id = $1",
    )
    .bind(recently_expired)
    .bind(fixture.first.to_uuid())
    .execute(&fixture.pool)
    .await
    .expect_err("raw SQL cannot backdate consumption of an expired proof");
    assert_eq!(
        constraint(&raw_backdate),
        Some("search_impressions_click_proof_chk")
    );
    let ancient_expired: uuid::Uuid = sqlx::query_scalar(
        r"WITH instant AS (SELECT clock_timestamp() AS now)
          INSERT INTO search_impressions
              (participant_id, workspace_id, query_text, result_ids,
               created_at, expires_at)
          SELECT $1, $2, 'ancient expired proof', $3,
                 instant.now - INTERVAL '100 days',
                 instant.now - INTERVAL '100 days' + INTERVAL '10 minutes'
            FROM instant
          RETURNING id",
    )
    .bind(fixture.actor.to_uuid())
    .bind(fixture.workspace.to_uuid())
    .bind(vec![fixture.first.to_uuid()])
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    repo.sweep_before(time::OffsetDateTime::now_utc() - time::Duration::days(90))
        .await
        .unwrap();
    let (recent_exists, ancient_exists): (bool, bool) = sqlx::query_as(
        r"SELECT EXISTS(SELECT 1 FROM search_impressions WHERE id = $1),
                  EXISTS(SELECT 1 FROM search_impressions WHERE id = $2)",
    )
    .bind(recently_expired)
    .bind(ancient_expired)
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert!(
        recent_exists,
        "expiry ends mutation authority but preserves the CTR denominator"
    );
    assert!(
        !ancient_exists,
        "the configured analytics cutoff bounds impression retention"
    );

    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn committed_retry_survives_revocation_message_deletion_and_expiry() {
    let fixture = Fixture::new("search-proof-stable-retry").await;
    let repo = SearchFeedbackRepo::new(fixture.pool.clone());
    let impression_id: uuid::Uuid = sqlx::query_scalar(
        r"INSERT INTO search_impressions
             (participant_id, workspace_id, query_text, result_ids, expires_at)
           VALUES ($1, $2, 'stable retry', $3,
                   statement_timestamp() + INTERVAL '3 seconds')
           RETURNING id",
    )
    .bind(fixture.actor.to_uuid())
    .bind(fixture.workspace.to_uuid())
    .bind(vec![fixture.first.to_uuid()])
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    let first = repo
        .record_impression_click(fixture.actor, impression_id, fixture.first)
        .await
        .unwrap();

    sqlx::query(
        r"INSERT INTO workspace_deactivations
              (workspace_id, participant_id, deactivated_by)
           VALUES ($1, $2, $3)",
    )
    .bind(fixture.workspace.to_uuid())
    .bind(fixture.actor.to_uuid())
    .bind(fixture.owner.to_uuid())
    .execute(&fixture.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE messages SET deleted_at = clock_timestamp() WHERE id = $1")
        .bind(fixture.first.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(3_100)).await;

    let retry = repo
        .record_impression_click(fixture.actor, impression_id, fixture.first)
        .await
        .unwrap();
    assert_eq!(
        retry, first,
        "an identical retry returns the committed receipt, not current authority state"
    );
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM search_click_events WHERE impression_id = $1")
            .bind(impression_id)
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);

    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn workspace_cascade_order_does_not_deadlock_with_click() {
    let fixture = Fixture::new("search-proof-workspace-order").await;
    let repo = SearchFeedbackRepo::new(fixture.pool.clone());
    let impression = repo
        .create_impression(
            fixture.actor,
            fixture.workspace,
            "workspace cascade order",
            &[fixture.first],
        )
        .await
        .unwrap();

    let mut erasure = fixture.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(fixture.workspace.to_uuid())
        .execute(&mut *erasure)
        .await
        .unwrap();
    let click = tokio::spawn({
        let repo = repo.clone();
        async move {
            repo.record_impression_click(fixture.actor, impression.id, fixture.first)
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !click.is_finished(),
        "click waits at the workspace boundary"
    );

    tokio::time::timeout(
        Duration::from_secs(1),
        sqlx::query("DELETE FROM search_impressions WHERE id = $1")
            .bind(impression.id)
            .execute(&mut *erasure),
    )
    .await
    .expect("workspace-owner transaction must not wait on an impression-first click")
    .unwrap();
    erasure.commit().await.unwrap();

    assert!(matches!(click.await.unwrap(), Err(Error::NotFound(_))));
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn message_delete_race_waits_then_records_zero_clicks() {
    let fixture = Fixture::new("search-proof-message-delete").await;
    let repo = SearchFeedbackRepo::new(fixture.pool.clone());
    let impression = repo
        .create_impression(
            fixture.actor,
            fixture.workspace,
            "message deletion race",
            &[fixture.first],
        )
        .await
        .unwrap();

    let mut deletion = fixture.pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE messages
            SET deleted_at = clock_timestamp()
          WHERE id = $1
            AND deleted_at IS NULL",
    )
    .bind(fixture.first.to_uuid())
    .execute(&mut *deletion)
    .await
    .unwrap();
    let click = tokio::spawn({
        let repo = repo.clone();
        async move {
            repo.record_impression_click(fixture.actor, impression.id, fixture.first)
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !click.is_finished(),
        "click waits behind the message row fence"
    );
    deletion.commit().await.unwrap();

    assert!(matches!(click.await.unwrap(), Err(Error::NotFound(_))));
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM search_click_events WHERE impression_id = $1")
            .bind(impression.id)
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn unrelated_result_deletion_does_not_poison_a_valid_click() {
    let fixture = Fixture::new("search-proof-unrelated-delete").await;
    let repo = SearchFeedbackRepo::new(fixture.pool.clone());
    let impression = repo
        .create_impression(
            fixture.actor,
            fixture.workspace,
            "unrelated result deletion",
            &[fixture.first, fixture.second],
        )
        .await
        .unwrap();

    sqlx::query("UPDATE messages SET deleted_at = clock_timestamp() WHERE id = $1")
        .bind(fixture.second.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();

    let click = repo
        .record_impression_click(fixture.actor, impression.id, fixture.first)
        .await
        .expect("only the selected result is revalidated at click time");
    assert_eq!(click.result_rank, 0);

    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn revocation_race_waits_then_records_zero_clicks() {
    let fixture = Fixture::new("search-proof-revoke").await;
    let repo = SearchFeedbackRepo::new(fixture.pool.clone());
    let impression = repo
        .create_impression(
            fixture.actor,
            fixture.workspace,
            "revocation race",
            &[fixture.first],
        )
        .await
        .unwrap();

    let mut revoke = fixture.pool.begin().await.unwrap();
    sqlx::query_scalar::<_, uuid::Uuid>("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(fixture.workspace.to_uuid())
        .fetch_one(&mut *revoke)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspace_deactivations
             (workspace_id, participant_id, deactivated_by)
         VALUES ($1, $2, $3)",
    )
    .bind(fixture.workspace.to_uuid())
    .bind(fixture.actor.to_uuid())
    .bind(fixture.owner.to_uuid())
    .execute(&mut *revoke)
    .await
    .unwrap();

    let click = tokio::spawn({
        let repo = repo.clone();
        async move {
            repo.record_impression_click(fixture.actor, impression.id, fixture.first)
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !click.is_finished(),
        "click waits behind the workspace revocation fence"
    );
    revoke.commit().await.unwrap();

    assert!(matches!(click.await.unwrap(), Err(Error::Forbidden(_))));
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM search_click_events WHERE impression_id = $1")
            .bind(impression.id)
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert_eq!(count, 0, "revoked click leaves no analytics row");

    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn raw_sql_rejects_forged_result_rank_query_identity_and_mutation() {
    let fixture = Fixture::new("search-proof-raw").await;
    let repo = SearchFeedbackRepo::new(fixture.pool.clone());
    let impression = repo
        .create_impression(
            fixture.actor,
            fixture.workspace,
            "canonical query",
            &[fixture.first, fixture.second],
        )
        .await
        .unwrap();

    sqlx::query(
        "UPDATE search_impressions
            SET clicked_result_id = $2,
                clicked_at = clock_timestamp()
          WHERE id = $1",
    )
    .bind(impression.id)
    .bind(fixture.first.to_uuid())
    .execute(&fixture.pool)
    .await
    .unwrap();

    for (participant, query, result, rank) in [
        (fixture.actor, "canonical query", fixture.second, 1_i32),
        (fixture.actor, "canonical query", fixture.first, 1_i32),
        (fixture.actor, "forged query", fixture.first, 0_i32),
        (fixture.outsider, "canonical query", fixture.first, 0_i32),
    ] {
        let error = sqlx::query(
            r"INSERT INTO search_click_events
                 (participant_id, workspace_id, query_text, result_id,
                  result_rank, clicked_at, impression_id)
               SELECT $2, workspace_id, $3, $4, $5, clicked_at, id
                 FROM search_impressions
                WHERE id = $1",
        )
        .bind(impression.id)
        .bind(participant.to_uuid())
        .bind(query)
        .bind(result.to_uuid())
        .bind(rank)
        .execute(&fixture.pool)
        .await
        .unwrap_err();
        assert_eq!(
            constraint(&error),
            Some("search_click_events_impression_proof_chk")
        );
    }

    sqlx::query(
        r"INSERT INTO search_click_events
             (participant_id, workspace_id, query_text, result_id,
              result_rank, clicked_at, impression_id)
           SELECT participant_id, workspace_id, query_text, $2, 0, clicked_at, id
             FROM search_impressions
            WHERE id = $1",
    )
    .bind(impression.id)
    .bind(fixture.first.to_uuid())
    .execute(&fixture.pool)
    .await
    .unwrap();

    let retry = repo
        .record_impression_click(fixture.actor, impression.id, fixture.first)
        .await
        .unwrap();
    assert_eq!(retry.result_rank, 0);
    let update_error = sqlx::query(
        "UPDATE search_click_events
            SET query_text = 'mutated'
          WHERE impression_id = $1",
    )
    .bind(impression.id)
    .execute(&fixture.pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&update_error),
        Some("search_click_events_proof_immutable_chk")
    );

    let rank_error = sqlx::query(
        r"INSERT INTO search_click_events
             (participant_id, workspace_id, query_text, result_id, result_rank)
           VALUES ($1, $2, 'legacy', $3, 100)",
    )
    .bind(fixture.actor.to_uuid())
    .bind(fixture.workspace.to_uuid())
    .bind(MessageId::new().to_uuid())
    .execute(&fixture.pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&rank_error),
        Some("search_click_events_rank_bounds_chk")
    );
    let query_error = sqlx::query(
        r"INSERT INTO search_click_events
             (participant_id, workspace_id, query_text, result_id, result_rank)
           VALUES ($1, $2, 'not  normalized', $3, 0)",
    )
    .bind(fixture.actor.to_uuid())
    .bind(fixture.workspace.to_uuid())
    .bind(MessageId::new().to_uuid())
    .execute(&fixture.pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&query_error),
        Some("search_click_events_query_normalized_chk")
    );
    let legacy_id: uuid::Uuid = sqlx::query_scalar(
        r"INSERT INTO search_click_events
             (participant_id, workspace_id, query_text, result_id, result_rank)
           VALUES ($1, $2, '', $3, 0)
           RETURNING id",
    )
    .bind(fixture.actor.to_uuid())
    .bind(fixture.workspace.to_uuid())
    .bind(MessageId::new().to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("operators-only rolling node may still insert empty parsed terms");
    let legacy_update_error =
        sqlx::query("UPDATE search_click_events SET query_text = 'mutated' WHERE id = $1")
            .bind(legacy_id)
            .execute(&fixture.pool)
            .await
            .unwrap_err();
    assert_eq!(
        constraint(&legacy_update_error),
        Some("search_click_events_legacy_immutable_chk"),
        "the rolling compatibility ingress remains append-only"
    );

    let impression_query_error = sqlx::query(
        r"INSERT INTO search_impressions
             (participant_id, workspace_id, query_text, result_ids)
           VALUES ($1, $2, 'not  normalized', $3)",
    )
    .bind(fixture.actor.to_uuid())
    .bind(fixture.workspace.to_uuid())
    .bind(vec![fixture.first.to_uuid()])
    .execute(&fixture.pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&impression_query_error),
        Some("search_impressions_query_normalized_chk")
    );
    let empty_impression_error = sqlx::query(
        r"INSERT INTO search_impressions
             (participant_id, workspace_id, query_text, result_ids)
           VALUES ($1, $2, '', $3)",
    )
    .bind(fixture.actor.to_uuid())
    .bind(fixture.workspace.to_uuid())
    .bind(vec![fixture.first.to_uuid()])
    .execute(&fixture.pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&empty_impression_error),
        Some("search_impressions_query_normalized_chk")
    );
    let future_impression_error = sqlx::query(
        r"WITH instant AS (SELECT clock_timestamp() AS now)
          INSERT INTO search_impressions
              (participant_id, workspace_id, query_text, result_ids,
               created_at, expires_at)
          SELECT $1, $2, 'future-issued proof', $3,
                 instant.now + INTERVAL '1 hour',
                 instant.now + INTERVAL '1 hour 15 minutes'
            FROM instant",
    )
    .bind(fixture.actor.to_uuid())
    .bind(fixture.workspace.to_uuid())
    .bind(vec![fixture.first.to_uuid()])
    .execute(&fixture.pool)
    .await
    .expect_err("raw SQL cannot mint a future-dated proof");
    assert_eq!(
        constraint(&future_impression_error),
        Some("search_impressions_issue_time_chk")
    );

    let foreign = Fixture::new("search-proof-foreign").await;
    let scope_error = sqlx::query(
        r"INSERT INTO search_impressions
             (participant_id, workspace_id, query_text, result_ids)
           VALUES ($1, $2, 'cross workspace', $3)",
    )
    .bind(fixture.actor.to_uuid())
    .bind(fixture.workspace.to_uuid())
    .bind(vec![foreign.first.to_uuid()])
    .execute(&fixture.pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&scope_error),
        Some("search_impressions_result_scope_chk")
    );

    foreign.cleanup().await;
    fixture.cleanup().await;
}

#[path = "security_tests/ctr_tests.rs"]
mod ctr_tests;
