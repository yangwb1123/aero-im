//! PG-gated integration tests.
//!
//! Run with a live Postgres and applied migrations:
//! `DATABASE_URL=... cargo test -p aero-storage --lib -- --ignored user_group`.

use super::*;
use crate::WorkspaceRepo;
use aero_common::WorkspaceRole;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

async fn actor(pool: &PgPool) -> ParticipantId {
    let actor = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(actor.to_uuid())
        .bind(format!("user-group-actor-{actor}"))
        .execute(pool)
        .await
        .expect("insert participant");
    actor
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn user_group_create_get_list_resolve() {
    let pool = pool();
    let repo = UserGroupRepo::new(pool.clone());
    let workspace = WorkspaceId::new();
    let creator = actor(&pool).await;

    let id = repo
        .create(workspace, "  Designers ", "Design Team", creator)
        .await
        .unwrap();
    let group = repo.get(id).await.unwrap().expect("group present");
    assert_eq!(group.id, id);
    assert_eq!(group.handle, "designers");
    assert_eq!(group.name, "Design Team");
    assert_eq!(group.created_by, creator);
    assert_eq!(group.workspace_id, workspace);

    let listed = repo.list_for_workspace(workspace).await.unwrap();
    assert!(listed.iter().any(|group| group.id == id));
    let resolved = repo
        .resolve(workspace, "DESIGNERS")
        .await
        .unwrap()
        .expect("resolve by handle");
    assert_eq!(resolved.id, id);
    assert!(repo
        .resolve(WorkspaceId::new(), "designers")
        .await
        .unwrap()
        .is_none());

    repo.delete(id, workspace).await.ok();
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn user_group_membership_add_remove_and_cascade() {
    let pool = pool();
    let repo = UserGroupRepo::new(pool.clone());
    let workspace = WorkspaceId::new();
    let creator = actor(&pool).await;
    let alice = actor(&pool).await;
    let bob = actor(&pool).await;
    let id = repo
        .create(workspace, "oncall", "On-Call", creator)
        .await
        .unwrap();

    repo.add_member(id, alice).await.unwrap();
    repo.add_member(id, bob).await.unwrap();
    repo.add_member(id, alice).await.unwrap();
    assert_eq!(repo.members(id).await.unwrap(), vec![alice, bob]);

    assert!(repo.remove_member(id, alice).await.unwrap());
    assert!(!repo.remove_member(id, alice).await.unwrap());
    assert_eq!(repo.members(id).await.unwrap(), vec![bob]);

    assert!(repo.delete(id, workspace).await.unwrap());
    assert!(repo.get(id).await.unwrap().is_none());
    assert!(repo.members(id).await.unwrap().is_empty());
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn ordinary_group_writes_recheck_current_caller_authority() {
    let pool = pool();
    let repo = UserGroupRepo::new(pool.clone());
    let workspaces = WorkspaceRepo::new(pool.clone());
    let owner = actor(&pool).await;
    let creator = actor(&pool).await;
    let admin = actor(&pool).await;
    let target = actor(&pool).await;
    let workspace = workspaces
        .create(
            format!("group-auth-{owner}"),
            format!("group-auth-{owner}"),
            owner,
        )
        .await
        .unwrap()
        .id;
    for (participant, role) in [
        (creator, WorkspaceRole::Member),
        (admin, WorkspaceRole::Admin),
        (target, WorkspaceRole::Member),
    ] {
        workspaces
            .add_member(workspace, participant, role)
            .await
            .unwrap();
    }

    let group = repo
        .create_authorized(workspace, "authorized", "Authorized", creator)
        .await
        .unwrap();
    assert!(repo
        .add_member_authorized(workspace, group.id, target, admin)
        .await
        .unwrap());

    sqlx::query(
        r"INSERT INTO workspace_deactivations
              (workspace_id, participant_id, deactivated_by)
           VALUES ($1, $2, $3)",
    )
    .bind(workspace.to_uuid())
    .bind(admin.to_uuid())
    .bind(owner.to_uuid())
    .execute(&pool)
    .await
    .unwrap();
    assert!(matches!(
        repo.remove_member_authorized(workspace, group.id, target, admin)
            .await
            .expect_err("a deactivated admin cannot manage group membership"),
        UserGroupWriteError::NotAuthorized
    ));
    sqlx::query(
        "DELETE FROM workspace_deactivations
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(workspace.to_uuid())
    .bind(admin.to_uuid())
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        r"INSERT INTO totp_secrets
              (participant_id, secret, activated, activated_at)
           VALUES ($1, $2, true, NOW())",
    )
    .bind(owner.to_uuid())
    .bind(format!("user-group-effective-{owner}"))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE workspaces SET require_2fa = true WHERE id = $1")
        .bind(workspace.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        repo.remove_member_authorized(workspace, group.id, target, admin)
            .await
            .expect_err("an admin missing mandatory 2FA cannot manage a group"),
        UserGroupWriteError::NotAuthorized
    ));
    sqlx::query(
        r"INSERT INTO totp_secrets
              (participant_id, secret, activated, activated_at)
           VALUES ($1, $2, true, NOW())",
    )
    .bind(admin.to_uuid())
    .bind(format!("user-group-effective-{admin}"))
    .execute(&pool)
    .await
    .unwrap();
    assert!(repo
        .remove_member_authorized(workspace, group.id, target, admin)
        .await
        .unwrap());
    assert!(repo
        .add_member_authorized(workspace, group.id, target, admin)
        .await
        .unwrap());

    workspaces
        .update_member_role(workspace, admin, WorkspaceRole::Member)
        .await
        .unwrap();
    assert!(matches!(
        repo.remove_member_authorized(workspace, group.id, target, admin)
            .await
            .expect_err("a downgraded admin must not retain stale route authority"),
        UserGroupWriteError::NotAuthorized
    ));

    workspaces.remove_member(workspace, creator).await.unwrap();
    assert!(matches!(
        repo.delete_authorized(workspace, group.id, creator)
            .await
            .expect_err("a removed creator must not retain stale route authority"),
        UserGroupWriteError::NotAuthorized
    ));

    repo.delete_authorized(workspace, group.id, owner)
        .await
        .unwrap();
}
