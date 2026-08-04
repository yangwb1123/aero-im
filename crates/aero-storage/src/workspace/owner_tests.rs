use std::time::Duration;

use aero_common::{Error, ParticipantId, WorkspaceId, WorkspaceRole};
use sqlx::PgPool;

use super::WorkspaceRepo;

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

async fn raw_workspace(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace: WorkspaceId,
    owner: ParticipantId,
    label: &str,
) {
    sqlx::query(
        "INSERT INTO workspaces (id, name, slug, created_by)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(workspace.to_uuid())
    .bind(label)
    .bind(format!("{label}-{workspace}"))
    .bind(owner.to_uuid())
    .execute(&mut **tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, 'owner')",
    )
    .bind(workspace.to_uuid())
    .bind(owner.to_uuid())
    .execute(&mut **tx)
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn workspace_birth_requires_owner_at_commit_but_atomic_birth_succeeds() {
    let pool = pool();
    let owner = participant(&pool, "workspace-owner-birth").await;
    let ownerless = WorkspaceId::new();
    let error = sqlx::query(
        "INSERT INTO workspaces (id, name, slug, created_by)
         VALUES ($1, 'Ownerless', $2, $3)",
    )
    .bind(ownerless.to_uuid())
    .bind(format!("ownerless-{ownerless}"))
    .bind(owner.to_uuid())
    .execute(&pool)
    .await
    .expect_err("autocommit ownerless workspace birth must fail");
    assert!(crate::is_workspace_owner_violation(&error));

    let workspace = WorkspaceId::new();
    let mut tx = pool.begin().await.unwrap();
    raw_workspace(&mut tx, workspace, owner, "atomic-owner-birth").await;
    tx.commit()
        .await
        .expect("workspace and owner edge commit atomically");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM workspace_members
              WHERE workspace_id = $1 AND role = 'owner' AND NOT is_guest",
        )
        .bind(workspace.to_uuid())
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(workspace.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn direct_final_owner_demotion_and_delete_are_rejected() {
    let pool = pool();
    let repo = WorkspaceRepo::new(pool.clone());
    let owner = participant(&pool, "workspace-final-owner").await;
    let workspace = repo
        .create(
            format!("Final owner {owner}"),
            format!("final-owner-{owner}"),
            owner,
        )
        .await
        .unwrap()
        .id;

    let demotion = sqlx::query(
        "UPDATE workspace_members
            SET role = 'member'
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(workspace.to_uuid())
    .bind(owner.to_uuid())
    .execute(&pool)
    .await
    .expect_err("direct final-owner demotion must fail");
    assert!(crate::is_workspace_owner_violation(&demotion));

    let deletion = sqlx::query(
        "DELETE FROM workspace_members
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(workspace.to_uuid())
    .bind(owner.to_uuid())
    .execute(&pool)
    .await
    .expect_err("direct final-owner deletion must fail");
    assert!(crate::is_workspace_owner_violation(&deletion));

    assert!(repo.delete(workspace).await.unwrap());
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn concurrent_owner_demotions_leave_one_owner() {
    let pool = pool();
    let repo = WorkspaceRepo::new(pool.clone());
    let first = participant(&pool, "workspace-concurrent-owner-a").await;
    let second = participant(&pool, "workspace-concurrent-owner-b").await;
    let workspace = repo
        .create(
            format!("Concurrent owner {first}"),
            format!("concurrent-owner-{first}"),
            first,
        )
        .await
        .unwrap()
        .id;
    repo.add_member(workspace, second, WorkspaceRole::Owner)
        .await
        .unwrap();

    let mut first_demotion = pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE workspace_members
            SET role = 'member'
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(workspace.to_uuid())
    .bind(first.to_uuid())
    .execute(&mut *first_demotion)
    .await
    .unwrap();

    let raced_pool = pool.clone();
    let mut raced = tokio::spawn(async move {
        let mut tx = raced_pool.begin().await?;
        sqlx::query(
            "UPDATE workspace_members
                SET role = 'member'
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(second.to_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut raced)
            .await
            .is_err(),
        "second demotion must wait for the workspace governance fence"
    );
    first_demotion.commit().await.unwrap();
    let error = raced
        .await
        .unwrap()
        .expect_err("second owner demotion cannot commit");
    assert!(crate::is_workspace_owner_violation(&error));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM workspace_members
              WHERE workspace_id = $1 AND role = 'owner' AND NOT is_guest",
        )
        .bind(workspace.to_uuid())
        .fetch_one(&pool)
        .await
        .unwrap(),
        1
    );
    assert!(repo.delete(workspace).await.unwrap());
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn workspace_cascade_delete_remains_valid() {
    let pool = pool();
    let owner = participant(&pool, "workspace-cascade-owner").await;
    let workspace = WorkspaceId::new();
    let mut tx = pool.begin().await.unwrap();
    raw_workspace(&mut tx, workspace, owner, "workspace-cascade").await;
    tx.commit().await.unwrap();

    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(workspace.to_uuid())
        .execute(&pool)
        .await
        .expect("workspace cascade skips the surviving-owner check");
    assert!(!sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM workspaces WHERE id = $1)"
    )
    .bind(workspace.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap());
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn revocation_linearizes_with_settings_and_workspace_delete() {
    let pool = pool();
    let repo = WorkspaceRepo::new(pool.clone());
    let first_owner = participant(&pool, "workspace-race-owner-a").await;
    let second_owner = participant(&pool, "workspace-race-owner-b").await;
    let admin = participant(&pool, "workspace-race-admin").await;
    let workspace = repo
        .create(
            format!("Governance race {first_owner}"),
            format!("governance-race-{first_owner}"),
            first_owner,
        )
        .await
        .unwrap()
        .id;
    repo.add_member(workspace, second_owner, WorkspaceRole::Owner)
        .await
        .unwrap();
    repo.add_member(workspace, admin, WorkspaceRole::Admin)
        .await
        .unwrap();

    let mut demote_admin = pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE workspace_members
            SET role = 'member'
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(workspace.to_uuid())
    .bind(admin.to_uuid())
    .execute(&mut *demote_admin)
    .await
    .unwrap();
    let settings_repo = repo.clone();
    let mut settings = tokio::spawn(async move {
        settings_repo
            .update_name_authorized(workspace, "stale admin write", admin)
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut settings)
            .await
            .is_err()
    );
    demote_admin.commit().await.unwrap();
    assert!(matches!(settings.await.unwrap(), Err(Error::Forbidden(_))));

    let mut demote_owner = pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE workspace_members
            SET role = 'member'
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(workspace.to_uuid())
    .bind(first_owner.to_uuid())
    .execute(&mut *demote_owner)
    .await
    .unwrap();
    let delete_repo = repo.clone();
    let mut deletion =
        tokio::spawn(async move { delete_repo.delete_authorized(workspace, first_owner).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut deletion)
            .await
            .is_err()
    );
    demote_owner.commit().await.unwrap();
    assert!(matches!(deletion.await.unwrap(), Err(Error::Forbidden(_))));
    assert!(repo
        .delete_authorized(workspace, second_owner)
        .await
        .unwrap());
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn rate_tier_write_is_owner_only_and_linearizes_with_demotion_and_delete() {
    let pool = pool();
    let repo = WorkspaceRepo::new(pool.clone());
    let owner = participant(&pool, "rate-tier-race-owner").await;
    let other_owner = participant(&pool, "rate-tier-race-other-owner").await;
    let admin = participant(&pool, "rate-tier-race-admin").await;
    let workspace = repo
        .create(
            format!("Rate tier race {owner}"),
            format!("rate-tier-race-{owner}"),
            owner,
        )
        .await
        .unwrap()
        .id;
    repo.add_member(workspace, other_owner, WorkspaceRole::Owner)
        .await
        .unwrap();
    repo.add_member(workspace, admin, WorkspaceRole::Admin)
        .await
        .unwrap();

    assert!(matches!(
        repo.set_rate_tier_authorized(workspace, "premium", admin)
            .await,
        Err(Error::Forbidden(_))
    ));

    let mut demotion = pool.begin().await.unwrap();
    crate::ownership::lock_membership_governance(&mut demotion)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE workspace_members
            SET role = 'member'
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(workspace.to_uuid())
    .bind(owner.to_uuid())
    .execute(&mut *demotion)
    .await
    .unwrap();
    let raced_repo = repo.clone();
    let mut raced = tokio::spawn(async move {
        raced_repo
            .set_rate_tier_authorized(workspace, "unlimited", owner)
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut raced)
            .await
            .is_err(),
        "tier write must wait behind an in-flight owner demotion"
    );
    demotion.commit().await.unwrap();
    assert!(matches!(raced.await.unwrap(), Err(Error::Forbidden(_))));
    assert_eq!(
        repo.rate_tier(workspace).await.unwrap().as_deref(),
        Some("standard"),
        "a stale owner decision must not change the tier"
    );

    let mut deletion = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE workspaces IN ROW EXCLUSIVE MODE")
        .execute(&mut *deletion)
        .await
        .unwrap();
    crate::ownership::lock_membership_governance(&mut deletion)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace.to_uuid())
        .fetch_one(&mut *deletion)
        .await
        .unwrap();
    let raced_repo = repo.clone();
    let mut raced = tokio::spawn(async move {
        raced_repo
            .set_rate_tier_authorized(workspace, "premium", other_owner)
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut raced)
            .await
            .is_err(),
        "tier write must wait behind workspace deletion"
    );
    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(workspace.to_uuid())
        .execute(&mut *deletion)
        .await
        .unwrap();
    deletion.commit().await.unwrap();
    assert!(matches!(raced.await.unwrap(), Err(Error::NotFound(_))));
    assert_eq!(repo.rate_tier(workspace).await.unwrap(), None);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn live_raw_owner_requires_explicit_demotion_before_account_erasure() {
    let pool = pool();
    let repo = WorkspaceRepo::new(pool.clone());
    let owner = participant(&pool, "workspace-erasure-owner").await;
    let successor = participant(&pool, "workspace-erasure-successor").await;
    let tombstone = participant(&pool, "workspace-erasure-tombstone").await;
    let workspace = repo
        .create(
            format!("Owner erasure {owner}"),
            format!("owner-erasure-{owner}"),
            owner,
        )
        .await
        .unwrap()
        .id;
    repo.add_member(workspace, successor, WorkspaceRole::Owner)
        .await
        .unwrap();

    let direct_tombstone =
        sqlx::query("UPDATE participants SET deleted_at = clock_timestamp() WHERE id = $1")
            .bind(owner.to_uuid())
            .execute(&pool)
            .await
            .expect_err("a raw owner cannot be tombstoned even when a successor exists");
    assert!(crate::is_workspace_owner_violation(&direct_tombstone));

    let mut transfer = pool.begin().await.unwrap();
    crate::ownership::lock_membership_governance(&mut transfer)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE workspace_members
            SET role = 'member'
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(workspace.to_uuid())
    .bind(owner.to_uuid())
    .execute(&mut *transfer)
    .await
    .unwrap();
    sqlx::query("UPDATE participants SET deleted_at = clock_timestamp() WHERE id = $1")
        .bind(owner.to_uuid())
        .execute(&mut *transfer)
        .await
        .expect("a demoted predecessor may be tombstoned");
    transfer.commit().await.unwrap();

    sqlx::query(
        "UPDATE participants
            SET deleted_at = clock_timestamp(),
                display_name = '[deleted]'
          WHERE id = $1",
    )
    .bind(tombstone.to_uuid())
    .execute(&pool)
    .await
    .unwrap();
    let promote_tombstone = sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, 'owner')",
    )
    .bind(workspace.to_uuid())
    .bind(tombstone.to_uuid())
    .execute(&pool)
    .await
    .expect_err("a tombstoned participant cannot be promoted into ownership");
    assert!(crate::is_workspace_owner_violation(&promote_tombstone));

    assert!(repo.delete(workspace).await.unwrap());
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn mandatory_2fa_owner_loss_is_checked_after_a_multirow_totp_change() {
    let pool = pool();
    let repo = WorkspaceRepo::new(pool.clone());
    let first = participant(&pool, "workspace-totp-owner-a").await;
    let second = participant(&pool, "workspace-totp-owner-b").await;
    let unready = participant(&pool, "workspace-totp-unready").await;
    let workspace = repo
        .create(
            format!("TOTP owners {first}"),
            format!("totp-owners-{first}"),
            first,
        )
        .await
        .unwrap()
        .id;
    repo.add_member(workspace, second, WorkspaceRole::Owner)
        .await
        .unwrap();
    for owner in [first, second] {
        sqlx::query(
            "INSERT INTO totp_secrets (participant_id, secret, activated)
             VALUES ($1, $2, true)",
        )
        .bind(owner.to_uuid())
        .bind(format!("owner-secret-{owner}"))
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query("UPDATE workspaces SET require_2fa = true WHERE id = $1")
        .bind(workspace.to_uuid())
        .execute(&pool)
        .await
        .expect("two enrolled owners satisfy mandatory 2FA");

    let both = vec![first.to_uuid(), second.to_uuid()];
    let remove_both = sqlx::query(
        "UPDATE totp_secrets
            SET activated = false
          WHERE participant_id = ANY($1)",
    )
    .bind(&both)
    .execute(&pool)
    .await
    .expect_err("one statement cannot make every owner ineffective");
    assert!(crate::is_workspace_owner_violation(&remove_both));
    let active_totp: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM totp_secrets
          WHERE participant_id = ANY($1) AND activated",
    )
    .bind(&both)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        active_totp, 2,
        "the rejected multi-row statement is fully rolled back"
    );

    sqlx::query("UPDATE totp_secrets SET activated = false WHERE participant_id = $1")
        .bind(first.to_uuid())
        .execute(&pool)
        .await
        .expect("one enrolled successor remains effective");

    let unready_workspace = repo
        .create(
            format!("Unready owner {unready}"),
            format!("unready-owner-{unready}"),
            unready,
        )
        .await
        .unwrap()
        .id;
    let require_without_ready_owner =
        sqlx::query("UPDATE workspaces SET require_2fa = true WHERE id = $1")
            .bind(unready_workspace.to_uuid())
            .execute(&pool)
            .await
            .expect_err("mandatory 2FA needs at least one enrolled owner");
    assert!(crate::is_workspace_owner_violation(
        &require_without_ready_owner
    ));

    assert!(repo.delete(unready_workspace).await.unwrap());
    assert!(repo.delete(workspace).await.unwrap());
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn service_owner_kind_changes_cannot_collectively_remove_2fa_bypass() {
    let pool = pool();
    let repo = WorkspaceRepo::new(pool.clone());
    let first = ParticipantId::new();
    let second = ParticipantId::new();
    for owner in [first, second] {
        sqlx::query(
            "INSERT INTO participants (id, kind, display_name)
             VALUES ($1, 'bot', $2)",
        )
        .bind(owner.to_uuid())
        .bind(format!("workspace-service-owner-{owner}"))
        .execute(&pool)
        .await
        .unwrap();
    }
    let workspace = repo
        .create(
            format!("Service owners {first}"),
            format!("service-owners-{first}"),
            first,
        )
        .await
        .unwrap()
        .id;
    repo.add_member(workspace, second, WorkspaceRole::Owner)
        .await
        .unwrap();
    sqlx::query("UPDATE workspaces SET require_2fa = true WHERE id = $1")
        .bind(workspace.to_uuid())
        .execute(&pool)
        .await
        .expect("service identities are exempt from mandatory TOTP");

    let owners = vec![first.to_uuid(), second.to_uuid()];
    let both_human = sqlx::query(
        "UPDATE participants
            SET kind = 'human'
          WHERE id = ANY($1)",
    )
    .bind(&owners)
    .execute(&pool)
    .await
    .expect_err("one statement cannot remove every service-identity bypass");
    assert!(crate::is_workspace_owner_violation(&both_human));
    let retained_bots: i64 =
        sqlx::query_scalar("SELECT count(*) FROM participants WHERE id = ANY($1) AND kind = 'bot'")
            .bind(&owners)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(retained_bots, 2);

    sqlx::query("UPDATE participants SET kind = 'human' WHERE id = $1")
        .bind(first.to_uuid())
        .execute(&pool)
        .await
        .expect("the second service owner remains effective");
    assert!(repo.delete(workspace).await.unwrap());
}
