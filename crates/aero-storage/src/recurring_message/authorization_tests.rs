use std::time::Duration;

use aero_common::{
    Block, Error, ParticipantId, RecurringMessageId, RoomId, RoomKind, WorkspaceId, WorkspaceRole,
};
use sqlx::PgPool;

use super::{RecurringFailureDisposition, RecurringMessageRepo};
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
    let id = ParticipantId::new();
    sqlx::query(
        "INSERT INTO participants (id, kind, display_name)
         VALUES ($1, 'human', $2)",
    )
    .bind(id.to_uuid())
    .bind(format!("{label}-{id}"))
    .execute(pool)
    .await
    .unwrap();
    id
}

struct Fixture {
    pool: PgPool,
    workspace: WorkspaceId,
    owner: ParticipantId,
    actor: ParticipantId,
    peer: ParticipantId,
    outsider: ParticipantId,
    room_a: RoomId,
    room_b: RoomId,
}

impl Fixture {
    async fn create() -> Self {
        let pool = pool();
        let owner = participant(&pool, "recurring-owner").await;
        let actor = participant(&pool, "recurring-actor").await;
        let peer = participant(&pool, "recurring-peer").await;
        let outsider = participant(&pool, "recurring-outsider").await;
        let workspaces = WorkspaceRepo::new(pool.clone());
        let workspace = workspaces
            .create(
                format!("Recurring auth {owner}"),
                format!("recurring-auth-{owner}"),
                owner,
            )
            .await
            .unwrap()
            .id;
        for member in [actor, peer, outsider] {
            workspaces
                .add_member(workspace, member, WorkspaceRole::Member)
                .await
                .unwrap();
        }
        let rooms = RoomRepo::new(pool.clone());
        let room_a = rooms
            .create_in_workspace(
                workspace,
                RoomKind::Channel,
                Some(format!("recurring-a-{actor}")),
                owner,
            )
            .await
            .unwrap()
            .id;
        let room_b = rooms
            .create_in_workspace(
                workspace,
                RoomKind::Channel,
                Some(format!("recurring-b-{actor}")),
                owner,
            )
            .await
            .unwrap()
            .id;
        for room in [room_a, room_b] {
            rooms.add_member(room, actor).await.unwrap();
            rooms.add_member(room, peer).await.unwrap();
        }
        Self {
            pool,
            workspace,
            owner,
            actor,
            peer,
            outsider,
            room_a,
            room_b,
        }
    }

    async fn cleanup(&self) {
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(self.workspace.to_uuid())
            .execute(&self.pool)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
            .bind(vec![
                self.owner.to_uuid(),
                self.actor.to_uuid(),
                self.peer.to_uuid(),
                self.outsider.to_uuid(),
            ])
            .execute(&self.pool)
            .await
            .ok();
    }
}

fn blocks(text: &str) -> serde_json::Value {
    serde_json::to_value(vec![Block::text(text)]).unwrap()
}

fn constraint(error: &sqlx::Error) -> Option<&str> {
    match error {
        sqlx::Error::Database(database) => database.constraint(),
        _ => None,
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn recurring_path_room_and_non_owner_ids_are_opaque() {
    let fixture = Fixture::create().await;
    let repo = RecurringMessageRepo::new(fixture.pool.clone());
    let id = repo
        .create_authorized(
            fixture.room_a,
            fixture.actor,
            &blocks("repeat"),
            "daily",
            time::OffsetDateTime::now_utc() + time::Duration::hours(1),
        )
        .await
        .unwrap();
    assert!(matches!(
        repo.cancel_authorized(Some(fixture.room_b), id, fixture.actor)
            .await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        repo.cancel_authorized(Some(fixture.room_a), id, fixture.peer)
            .await,
        Err(Error::NotFound(_))
    ));
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT active FROM recurring_messages WHERE id = $1")
            .bind(id.to_uuid())
            .fetch_one(&fixture.pool)
            .await
            .unwrap()
    );
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn recurring_cancel_waits_for_revocation_and_owner_inventory_hides_room() {
    let fixture = Fixture::create().await;
    let repo = RecurringMessageRepo::new(fixture.pool.clone());
    let first = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let hidden = repo
        .create_authorized(
            fixture.room_a,
            fixture.actor,
            &blocks("hidden"),
            "daily",
            first,
        )
        .await
        .unwrap();
    let visible = repo
        .create_authorized(
            fixture.room_b,
            fixture.actor,
            &blocks("visible"),
            "daily",
            first,
        )
        .await
        .unwrap();

    let mut revoke = fixture.pool.begin().await.unwrap();
    sqlx::query("DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
        .bind(fixture.room_a.to_uuid())
        .bind(fixture.actor.to_uuid())
        .execute(&mut *revoke)
        .await
        .unwrap();
    let cancel_repo = repo.clone();
    let room = fixture.room_a;
    let actor = fixture.actor;
    let cancel = tokio::spawn(async move {
        cancel_repo
            .cancel_authorized(Some(room), hidden, actor)
            .await
    });
    tokio::time::sleep(Duration::from_millis(75)).await;
    assert!(
        !cancel.is_finished(),
        "recurring cancel must wait behind membership revocation"
    );
    revoke.commit().await.unwrap();
    assert!(matches!(cancel.await.unwrap(), Err(Error::Forbidden(_))));
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT active FROM recurring_messages WHERE id = $1")
            .bind(hidden.to_uuid())
            .fetch_one(&fixture.pool)
            .await
            .unwrap()
    );

    let global = repo
        .list_for_sender_authorized(fixture.actor)
        .await
        .unwrap();
    assert!(global.iter().any(|row| row.id == visible));
    assert!(global.iter().all(|row| row.id != hidden));
    assert!(matches!(
        repo.list_for_room_authorized(fixture.room_a, fixture.actor)
            .await,
        Err(Error::Forbidden(_))
    ));
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn recurring_raw_guard_and_claim_fence_reject_identity_and_stale_tokens() {
    let fixture = Fixture::create().await;
    let repo = RecurringMessageRepo::new(fixture.pool.clone());
    let now = time::OffsetDateTime::now_utc();
    let error = sqlx::query(
        r"INSERT INTO recurring_messages
              (id, room_id, sender_id, blocks, cadence, next_run)
           VALUES ($1, $2, $3, $4, 'daily', $5)",
    )
    .bind(RecurringMessageId::new().to_uuid())
    .bind(fixture.room_a.to_uuid())
    .bind(fixture.outsider.to_uuid())
    .bind(blocks("raw"))
    .bind(now)
    .execute(&fixture.pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&error),
        Some("recurring_messages_sender_scope_chk")
    );

    let id = repo
        .create_authorized(
            fixture.room_a,
            fixture.actor,
            &blocks("claim"),
            "hourly",
            now - time::Duration::minutes(1),
        )
        .await
        .unwrap();
    let error = sqlx::query("UPDATE recurring_messages SET room_id = $2 WHERE id = $1")
        .bind(id.to_uuid())
        .bind(fixture.room_b.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap_err();
    assert_eq!(
        constraint(&error),
        Some("recurring_messages_identity_immutable_chk")
    );
    let error = sqlx::query(
        "UPDATE recurring_messages
            SET next_run = next_run + interval '1 hour'
          WHERE id = $1",
    )
    .bind(id.to_uuid())
    .execute(&fixture.pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&error),
        Some("recurring_messages_next_run_transition_chk")
    );

    let claim = repo
        .claim_due(now, time::Duration::seconds(30), 10)
        .await
        .unwrap()
        .into_iter()
        .find(|claim| claim.series.id == id)
        .unwrap();
    let error =
        sqlx::query("UPDATE recurring_messages SET claim_token = gen_random_uuid() WHERE id = $1")
            .bind(id.to_uuid())
            .execute(&fixture.pool)
            .await
            .unwrap_err();
    assert_eq!(
        constraint(&error),
        Some("recurring_messages_claim_transition_chk")
    );

    let retry_at = now + time::Duration::minutes(1);
    assert_eq!(
        repo.record_failure(
            id,
            claim.claim_token,
            claim.delivery_key,
            claim.attempt,
            retry_at,
            "temporary",
            true,
            8,
        )
        .await
        .unwrap(),
        RecurringFailureDisposition::RetryScheduled
    );
    let reclaimed = repo
        .claim_due(retry_at, time::Duration::seconds(30), 10)
        .await
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.series.id == id)
        .unwrap();
    assert_ne!(reclaimed.claim_token, claim.claim_token);
    assert_eq!(reclaimed.delivery_key, claim.delivery_key);
    assert!(!repo
        .confirm_sent(
            id,
            claim.claim_token,
            claim.delivery_key,
            claim.attempt,
            retry_at + time::Duration::hours(1),
            retry_at,
        )
        .await
        .unwrap());
    fixture.cleanup().await;
}
