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
        .bind(format!("scim-group-actor-{actor}"))
        .execute(pool)
        .await
        .expect("insert participant");
    actor
}

async fn workspace(pool: &PgPool, owner: ParticipantId) -> WorkspaceId {
    WorkspaceRepo::new(pool.clone())
        .create(
            format!("SCIM group test {owner}"),
            format!("scim-group-{owner}"),
            owner,
        )
        .await
        .expect("create workspace")
        .id
}

async fn enroll(pool: &PgPool, workspace: WorkspaceId, participant: ParticipantId) {
    WorkspaceRepo::new(pool.clone())
        .add_member(workspace, participant, WorkspaceRole::Member)
        .await
        .expect("add workspace member");
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn scim_group_create_is_atomic_and_rejects_non_workspace_members() {
    let pool = pool();
    let repo = UserGroupRepo::new(pool.clone());
    let owner = actor(&pool).await;
    let workspace = workspace(&pool, owner).await;
    let member = actor(&pool).await;
    let outsider = actor(&pool).await;
    enroll(&pool, workspace, member).await;

    let error = repo
        .create_scim_group(workspace, "atomic-team", "Atomic Team", &[member, outsider])
        .await
        .expect_err("cross-workspace member must reject the whole create");
    assert!(matches!(
        error,
        UserGroupWriteError::MemberNotInWorkspace(id) if id == outsider
    ));
    assert!(
        repo.list_for_workspace(workspace).await.unwrap().is_empty(),
        "failed create must not leave an orphan group"
    );

    let group = repo
        .create_scim_group(workspace, "atomic-team", "Atomic Team", &[member, member])
        .await
        .unwrap();
    assert_eq!(
        group.group.created_by, owner,
        "SCIM attribution must select a real workspace owner"
    );
    assert_eq!(
        repo.members(group.group.id).await.unwrap(),
        vec![member],
        "initial membership is exact and deduplicated"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn scim_group_replace_and_patch_roll_back_as_one_unit() {
    let pool = pool();
    let repo = UserGroupRepo::new(pool.clone());
    let owner = actor(&pool).await;
    let workspace = workspace(&pool, owner).await;
    let alice = actor(&pool).await;
    let bob = actor(&pool).await;
    let outsider = actor(&pool).await;
    enroll(&pool, workspace, alice).await;
    enroll(&pool, workspace, bob).await;

    let group = repo
        .create_scim_group(workspace, "ops", "Operations", &[alice])
        .await
        .unwrap();

    let replace_error = repo
        .replace_scim_group(workspace, group.group.id, "Changed", &[bob, outsider])
        .await
        .expect_err("invalid desired member must roll back rename and membership");
    assert!(matches!(
        replace_error,
        UserGroupWriteError::MemberNotInWorkspace(id) if id == outsider
    ));
    assert_eq!(
        repo.get(group.group.id).await.unwrap().unwrap().name,
        "Operations"
    );
    assert_eq!(repo.members(group.group.id).await.unwrap(), vec![alice]);

    let patch_error = repo
        .patch_scim_group(
            workspace,
            group.group.id,
            &[
                ScimGroupMutation::SetName("Partially changed".into()),
                ScimGroupMutation::RemoveMember(alice),
                ScimGroupMutation::AddMember(outsider),
            ],
        )
        .await
        .expect_err("a later invalid add must reject the complete patch");
    assert!(matches!(
        patch_error,
        UserGroupWriteError::MemberNotInWorkspace(id) if id == outsider
    ));
    assert_eq!(
        repo.get(group.group.id).await.unwrap().unwrap().name,
        "Operations"
    );
    assert_eq!(repo.members(group.group.id).await.unwrap(), vec![alice]);

    let replaced = repo
        .replace_scim_group(workspace, group.group.id, "New Operations", &[bob])
        .await
        .unwrap();
    assert_eq!(replaced.group.name, "New Operations");
    assert_eq!(replaced.members, vec![bob]);
    assert_eq!(repo.members(group.group.id).await.unwrap(), vec![bob]);

    let patched = repo
        .patch_scim_group(
            workspace,
            group.group.id,
            &[
                ScimGroupMutation::SetName("Final Operations".into()),
                ScimGroupMutation::RemoveMember(bob),
                ScimGroupMutation::AddMember(alice),
            ],
        )
        .await
        .unwrap();
    assert_eq!(patched.group.name, "Final Operations");
    assert_eq!(patched.members, vec![alice]);
    assert_eq!(repo.members(group.group.id).await.unwrap(), vec![alice]);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn workspace_scoped_add_rejects_cross_tenant_and_phantom_members() {
    let pool = pool();
    let repo = UserGroupRepo::new(pool.clone());
    let owner = actor(&pool).await;
    let workspace = workspace(&pool, owner).await;
    let member = actor(&pool).await;
    let outsider = actor(&pool).await;
    let phantom = ParticipantId::new();
    let group = repo
        .create(workspace, "validated-add", "Validated Add", owner)
        .await
        .unwrap();

    for invalid in [outsider, phantom] {
        let error = repo
            .add_member_in_workspace(workspace, group, invalid)
            .await
            .expect_err("non-workspace member must be rejected");
        assert!(matches!(
            error,
            UserGroupWriteError::MemberNotInWorkspace(id) if id == invalid
        ));
    }
    assert!(
        repo.members(group).await.unwrap().is_empty(),
        "rejected additions must not pollute group membership"
    );

    let wrong_workspace = WorkspaceId::new();
    assert!(matches!(
        repo.add_member_in_workspace(wrong_workspace, group, owner)
            .await
            .expect_err("group ids are scoped to the supplied workspace"),
        UserGroupWriteError::NotFound
    ));

    enroll(&pool, workspace, member).await;
    assert!(repo
        .add_member_in_workspace(workspace, group, member)
        .await
        .unwrap());
    assert!(
        !repo
            .add_member_in_workspace(workspace, group, member)
            .await
            .unwrap(),
        "duplicate addition remains idempotent"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn scim_patch_can_remove_a_member_after_workspace_deprovisioning() {
    let pool = pool();
    let repo = UserGroupRepo::new(pool.clone());
    let owner = actor(&pool).await;
    let workspace = workspace(&pool, owner).await;
    let deprovisioned = actor(&pool).await;
    enroll(&pool, workspace, deprovisioned).await;

    let group = repo
        .create_scim_group(
            workspace,
            "deprovision-cleanup",
            "Deprovision cleanup",
            &[deprovisioned],
        )
        .await
        .unwrap();
    WorkspaceRepo::new(pool.clone())
        .remove_member(workspace, deprovisioned)
        .await
        .unwrap();

    let updated = repo
        .patch_scim_group(
            workspace,
            group.group.id,
            &[ScimGroupMutation::RemoveMember(deprovisioned)],
        )
        .await
        .expect("removing a stale edge must not require active workspace membership");
    assert!(updated.members.is_empty());
    assert!(repo.members(group.group.id).await.unwrap().is_empty());

    let error = repo
        .patch_scim_group(
            workspace,
            group.group.id,
            &[ScimGroupMutation::AddMember(deprovisioned)],
        )
        .await
        .expect_err("re-adding a deprovisioned participant must remain forbidden");
    assert!(matches!(
        error,
        UserGroupWriteError::MemberNotInWorkspace(id) if id == deprovisioned
    ));
}
