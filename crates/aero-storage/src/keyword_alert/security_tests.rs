//! PostgreSQL-backed keyword-alert transaction and tenant-boundary tests.

use std::time::Duration;

use aero_common::WorkspaceRole;
use sqlx::PgPool;

use super::*;
use crate::WorkspaceRepo;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(6)
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

async fn workspace(pool: &PgPool, owner: ParticipantId, label: &str) -> WorkspaceId {
    WorkspaceRepo::new(pool.clone())
        .create(
            format!("{label}-{owner}"),
            format!("{label}-{owner}").to_ascii_lowercase(),
            owner,
        )
        .await
        .unwrap()
        .id
}

async fn add_member(pool: &PgPool, workspace: WorkspaceId, participant: ParticipantId) {
    WorkspaceRepo::new(pool.clone())
        .add_member(workspace, participant, WorkspaceRole::Member)
        .await
        .unwrap();
}

fn constraint(error: &sqlx::Error) -> Option<&str> {
    error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::constraint)
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn authorized_crud_waits_for_revocation_and_matching_reenters_after_restore() {
    let pool = pool();
    let repo = KeywordAlertRepo::new(pool.clone());
    let owner = participant(&pool, "keyword-revoke-owner").await;
    let subscriber = participant(&pool, "keyword-revoke-subscriber").await;
    let other_member = participant(&pool, "keyword-revoke-other").await;
    let workspace = workspace(&pool, owner, "keyword-revoke").await;
    add_member(&pool, workspace, subscriber).await;
    add_member(&pool, workspace, other_member).await;

    let alert = repo
        .add_authorized(subscriber, workspace, "  Incident  ")
        .await
        .unwrap();
    assert_eq!(alert.keyword, "incident");
    let same = repo
        .add_authorized(subscriber, workspace, "INCIDENT")
        .await
        .unwrap();
    assert_eq!(same.id, alert.id);
    assert_eq!(
        repo.list_authorized(subscriber, workspace)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(matches!(
        repo.delete_authorized(alert.id, other_member).await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        repo.delete_authorized(KeywordAlertId::new(), other_member)
            .await,
        Err(Error::NotFound(_))
    ));
    assert!(repo
        .matching_subscribers(workspace, "production INCIDENT")
        .await
        .unwrap()
        .contains(&subscriber));

    let mut revocation = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace.to_uuid())
        .execute(&mut *revocation)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspace_deactivations
             (workspace_id, participant_id, deactivated_by)
         VALUES ($1, $2, $3)",
    )
    .bind(workspace.to_uuid())
    .bind(subscriber.to_uuid())
    .bind(owner.to_uuid())
    .execute(&mut *revocation)
    .await
    .unwrap();

    let raced_repo = repo.clone();
    let mut raced = tokio::spawn(async move {
        raced_repo
            .add_authorized(subscriber, workspace, "database")
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut raced)
            .await
            .is_err(),
        "keyword insert waits behind the workspace revocation fence"
    );
    revocation.commit().await.unwrap();
    assert!(matches!(raced.await.unwrap(), Err(Error::Forbidden(_))));
    assert!(matches!(
        repo.list_authorized(subscriber, workspace).await,
        Err(Error::Forbidden(_))
    ));
    assert!(
        !repo
            .matching_subscribers(workspace, "production incident")
            .await
            .unwrap()
            .contains(&subscriber),
        "revoked subscribers do not re-enter notification dispatch"
    );

    sqlx::query(
        "DELETE FROM workspace_deactivations
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(workspace.to_uuid())
    .bind(subscriber.to_uuid())
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        repo.matching_subscribers(workspace, "production incident")
            .await
            .unwrap()
            .contains(&subscriber),
        "retained subscription re-enters only after effective access is restored"
    );
    repo.delete_authorized(alert.id, subscriber).await.unwrap();
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn matching_subscribers_excludes_deleted_accounts() {
    let pool = pool();
    let repo = KeywordAlertRepo::new(pool.clone());
    let owner = participant(&pool, "keyword-account-owner").await;
    let subscriber = participant(&pool, "keyword-account-subscriber").await;
    let workspace = workspace(&pool, owner, "keyword-account").await;
    add_member(&pool, workspace, subscriber).await;
    repo.add_authorized(subscriber, workspace, "outage")
        .await
        .unwrap();
    assert!(repo
        .matching_subscribers(workspace, "OUTAGE")
        .await
        .unwrap()
        .contains(&subscriber));

    sqlx::query(
        "INSERT INTO totp_secrets
             (participant_id, secret, activated, activated_at)
         VALUES ($1, $2, true, CURRENT_TIMESTAMP)",
    )
    .bind(owner.to_uuid())
    .bind(format!("keyword-owner-secret-{owner}"))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE workspaces SET require_2fa = true WHERE id = $1")
        .bind(workspace.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        !repo
            .matching_subscribers(workspace, "outage")
            .await
            .unwrap()
            .contains(&subscriber),
        "mandatory-2FA-ineligible subscribers are filtered"
    );
    sqlx::query(
        "INSERT INTO totp_secrets
             (participant_id, secret, activated, activated_at)
         VALUES ($1, $2, true, CURRENT_TIMESTAMP)",
    )
    .bind(subscriber.to_uuid())
    .bind(format!("keyword-secret-{subscriber}"))
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        repo.matching_subscribers(workspace, "outage")
            .await
            .unwrap()
            .contains(&subscriber),
        "activated mandatory 2FA restores eligibility"
    );

    sqlx::query("UPDATE participants SET deleted_at = CURRENT_TIMESTAMP WHERE id = $1")
        .bind(subscriber.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(!repo
        .matching_subscribers(workspace, "outage")
        .await
        .unwrap()
        .contains(&subscriber));
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn raw_sql_enforces_current_owner_canonical_identity_and_cascade() {
    let pool = pool();
    let repo = KeywordAlertRepo::new(pool.clone());
    let owner = participant(&pool, "keyword-raw-owner").await;
    let member = participant(&pool, "keyword-raw-member").await;
    let outsider = participant(&pool, "keyword-raw-outsider").await;
    let workspace = workspace(&pool, owner, "keyword-raw").await;
    add_member(&pool, workspace, member).await;

    let no_access = sqlx::query(
        "INSERT INTO keyword_alerts
             (id, participant_id, workspace_id, keyword)
         VALUES ($1, $2, $3, 'incident')",
    )
    .bind(KeywordAlertId::new().to_uuid())
    .bind(outsider.to_uuid())
    .bind(workspace.to_uuid())
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&no_access),
        Some("keyword_alerts_owner_scope_chk")
    );

    let noncanonical = sqlx::query(
        "INSERT INTO keyword_alerts
             (id, participant_id, workspace_id, keyword)
         VALUES ($1, $2, $3, ' Incident ')",
    )
    .bind(KeywordAlertId::new().to_uuid())
    .bind(member.to_uuid())
    .bind(workspace.to_uuid())
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&noncanonical),
        Some("keyword_alerts_keyword_canonical_chk")
    );

    let alert = repo
        .add_authorized(member, workspace, "incident")
        .await
        .unwrap();
    let rewritten = sqlx::query(
        "UPDATE keyword_alerts
            SET participant_id = $2
          WHERE id = $1",
    )
    .bind(alert.id.to_uuid())
    .bind(owner.to_uuid())
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&rewritten),
        Some("keyword_alerts_identity_immutable_chk")
    );

    sqlx::query("DELETE FROM participants WHERE id = $1")
        .bind(member.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    let remaining =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM keyword_alerts WHERE id = $1")
            .bind(alert.id.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(remaining, 0, "participant deletion cascades alert metadata");
}
