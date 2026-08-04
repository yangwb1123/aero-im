use std::time::Duration;

use aero_common::{Error, ParticipantId, WorkspaceRole};
use sqlx::PgPool;

use super::ParticipantRepo;
use crate::WorkspaceRepo;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
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
    .bind(format!("verified-{label}-{participant}"))
    .execute(pool)
    .await
    .unwrap();
    participant
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn verified_badge_rechecks_platform_admin_and_audits_with_update() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let workspaces = WorkspaceRepo::new(pool.clone());
    let owner = participant(&pool, "owner").await;
    let admin = participant(&pool, "admin").await;
    let outsider = participant(&pool, "outsider").await;
    let target = participant(&pool, "target").await;
    let workspace = workspaces
        .create(
            format!("Verified admin {owner}"),
            format!("verified-admin-{owner}"),
            owner,
        )
        .await
        .unwrap()
        .id;
    workspaces
        .add_member(workspace, admin, WorkspaceRole::Admin)
        .await
        .unwrap();

    let verified_at = participants
        .set_verified_authorized(workspace, admin, target, true)
        .await
        .unwrap();
    assert!(verified_at.is_some());
    let stored: (bool, Option<time::OffsetDateTime>) =
        sqlx::query_as("SELECT is_verified, verified_at FROM participants WHERE id = $1")
            .bind(target.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(stored.0 && stored.1.is_some());
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
           FROM audit_events
          WHERE workspace_id = $1
            AND actor_id = $2
            AND action = 'participant.verified'
            AND target = $3",
    )
    .bind(workspace.to_uuid())
    .bind(admin.to_uuid())
    .bind(target.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audit_count, 1);
    assert!(matches!(
        participants
            .set_verified_authorized(workspace, outsider, target, false)
            .await,
        Err(Error::Forbidden(_))
    ));

    let mut demotion = pool.begin().await.unwrap();
    crate::ownership::lock_membership_governance(&mut demotion)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace.to_uuid())
        .execute(&mut *demotion)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE workspace_members
            SET role = 'member'
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(workspace.to_uuid())
    .bind(admin.to_uuid())
    .execute(&mut *demotion)
    .await
    .unwrap();
    let raced_repo = participants.clone();
    let mut raced = tokio::spawn(async move {
        raced_repo
            .set_verified_authorized(workspace, admin, target, false)
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut raced)
            .await
            .is_err(),
        "badge mutation waits behind platform-admin demotion"
    );
    demotion.commit().await.unwrap();
    assert!(matches!(raced.await.unwrap(), Err(Error::Forbidden(_))));
    let still_verified: bool =
        sqlx::query_scalar("SELECT is_verified FROM participants WHERE id = $1")
            .bind(target.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(still_verified);
}
