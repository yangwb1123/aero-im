use super::*;

use crate::{DmRepo, RoomRepo};
use sqlx::PgPool;

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
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(participant.to_uuid())
        .bind(format!("guest-aggregate-{label}-{participant}"))
        .execute(pool)
        .await
        .unwrap();
    participant
}

async fn test_workspace(repo: &WorkspaceRepo, owner: ParticipantId, label: &str) -> WorkspaceId {
    repo.create(
        format!("guest-aggregate-{label}-{owner}"),
        format!("guest-aggregate-{label}-{owner}"),
        owner,
    )
    .await
    .unwrap()
    .id
}

async fn room(pool: &PgPool, workspace: WorkspaceId, owner: ParticipantId, label: &str) -> RoomId {
    room_of_kind(
        pool,
        workspace,
        owner,
        label,
        aero_common::RoomKind::Channel,
    )
    .await
}

async fn room_of_kind(
    pool: &PgPool,
    workspace: WorkspaceId,
    owner: ParticipantId,
    label: &str,
    kind: aero_common::RoomKind,
) -> RoomId {
    RoomRepo::new(pool.clone())
        .create_in_workspace(
            workspace,
            kind,
            Some(format!("guest-aggregate-{label}")),
            owner,
        )
        .await
        .unwrap()
        .id
}

async fn room_memberships(
    pool: &PgPool,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Vec<RoomId> {
    sqlx::query_scalar::<_, uuid::Uuid>(
        r"SELECT rm.room_id
            FROM room_members rm
            JOIN rooms r ON r.id = rm.room_id
           WHERE r.workspace_id = $1 AND rm.participant_id = $2
           ORDER BY rm.room_id",
    )
    .bind(workspace.to_uuid())
    .bind(participant.to_uuid())
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(RoomId::from_uuid)
    .collect()
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn guest_aggregate_is_atomic_tenant_scoped_and_rejects_existing_members() {
    let pool = pool();
    let repo = WorkspaceRepo::new(pool.clone());
    let owner = participant(&pool, "owner").await;
    let other_owner = participant(&pool, "other-owner").await;
    let admin = participant(&pool, "admin").await;
    let member = participant(&pool, "member").await;
    let direct_peer = participant(&pool, "direct-peer").await;
    let guest = participant(&pool, "guest").await;
    let cross_tenant_target = participant(&pool, "cross-tenant").await;
    let workspace = test_workspace(&repo, owner, "primary").await;
    let other_workspace = test_workspace(&repo, other_owner, "other").await;
    let first_room = room(&pool, workspace, owner, "first").await;
    let second_room = room(&pool, workspace, owner, "second").await;
    let group_room = room_of_kind(
        &pool,
        workspace,
        owner,
        "group",
        aero_common::RoomKind::Group,
    )
    .await;
    let other_room = room(&pool, other_workspace, other_owner, "other").await;

    repo.add_member(workspace, admin, WorkspaceRole::Admin)
        .await
        .unwrap();
    repo.add_member(workspace, member, WorkspaceRole::Member)
        .await
        .unwrap();
    repo.add_member(workspace, direct_peer, WorkspaceRole::Member)
        .await
        .unwrap();
    let direct_room = DmRepo::new(pool.clone())
        .find_or_create_in_workspace(workspace, owner, direct_peer)
        .await
        .unwrap()
        .id;

    assert!(matches!(
        repo.add_guest_authorized(workspace, owner, member, first_room)
            .await
            .expect_err("an existing member cannot be converted"),
        GuestMembershipWriteError::ExistingMember
    ));
    assert!(!repo.is_guest(workspace, member).await.unwrap());
    assert!(room_memberships(&pool, workspace, member).await.is_empty());

    for privileged in [owner, admin] {
        assert!(matches!(
            repo.add_guest_authorized(workspace, owner, privileged, first_room)
                .await
                .expect_err("privileged member cannot be converted"),
            GuestMembershipWriteError::ExistingMember
        ));
        assert!(!repo.is_guest(workspace, privileged).await.unwrap());
    }

    assert!(matches!(
        repo.add_guest_authorized(workspace, admin, cross_tenant_target, other_room)
            .await
            .expect_err("a room from another tenant is rejected"),
        GuestMembershipWriteError::RoomOutsideWorkspace
    ));
    assert!(
        !repo
            .is_member(workspace, cross_tenant_target)
            .await
            .unwrap(),
        "tenant rejection leaves no workspace edge"
    );
    assert!(
        !RoomRepo::new(pool.clone())
            .is_member(other_room, cross_tenant_target)
            .await
            .unwrap(),
        "tenant rejection leaves no room edge"
    );
    assert!(matches!(
        repo.add_guest_authorized(workspace, admin, cross_tenant_target, group_room)
            .await
            .expect_err("a group room cannot receive a single-channel guest"),
        GuestMembershipWriteError::NotChannel
    ));
    assert!(
        !repo
            .is_member(workspace, cross_tenant_target)
            .await
            .unwrap(),
        "kind rejection leaves no workspace edge"
    );
    assert!(!RoomRepo::new(pool.clone())
        .is_member(group_room, cross_tenant_target)
        .await
        .unwrap());
    assert!(matches!(
        repo.add_guest_authorized(workspace, admin, cross_tenant_target, direct_room)
            .await
            .expect_err("a direct room cannot receive a guest"),
        GuestMembershipWriteError::NotChannel
    ));
    assert!(!RoomRepo::new(pool.clone())
        .is_member(direct_room, cross_tenant_target)
        .await
        .unwrap());

    repo.add_guest_authorized(workspace, admin, guest, first_room)
        .await
        .unwrap();
    assert!(repo.is_guest(workspace, guest).await.unwrap());
    assert_eq!(
        room_memberships(&pool, workspace, guest).await,
        vec![first_room]
    );

    let affected = repo
        .add_guest_authorized(workspace, admin, guest, second_room)
        .await
        .unwrap();
    assert!(affected.contains(&first_room));
    assert!(affected.contains(&second_room));
    assert_eq!(
        room_memberships(&pool, workspace, guest).await,
        vec![second_room],
        "re-scoping preserves the single-channel invariant"
    );

    assert!(
        repo.remove_guest_authorized(workspace, admin, member)
            .await
            .unwrap()
            .is_empty(),
        "ordinary non-guest removal is an idempotent no-op"
    );
    assert!(repo.is_member(workspace, member).await.unwrap());
    assert!(matches!(
        repo.remove_guest_authorized(workspace, owner, admin)
            .await
            .expect_err("admin cannot be removed through guest endpoint"),
        GuestMembershipWriteError::ExistingMember
    ));

    assert_eq!(
        repo.remove_guest_authorized(workspace, admin, guest)
            .await
            .unwrap(),
        vec![second_room]
    );
    assert!(!repo.is_member(workspace, guest).await.unwrap());
    assert!(room_memberships(&pool, workspace, guest).await.is_empty());
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn concurrent_caller_demotion_cannot_reuse_stale_guest_authority() {
    let pool = pool();
    let repo = WorkspaceRepo::new(pool.clone());
    let owner = participant(&pool, "race-owner").await;
    let admin = participant(&pool, "race-admin").await;
    let guest = participant(&pool, "race-guest").await;
    let workspace = test_workspace(&repo, owner, "race").await;
    let room = room(&pool, workspace, owner, "race").await;
    repo.add_member(workspace, admin, WorkspaceRole::Admin)
        .await
        .unwrap();

    let mut demotion = pool.begin().await.unwrap();
    crate::ownership::lock_membership_governance(&mut demotion)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace.to_uuid())
        .fetch_one(&mut *demotion)
        .await
        .unwrap();
    sqlx::query(
        r"UPDATE workspace_members
              SET role = 'member'
            WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(workspace.to_uuid())
    .bind(admin.to_uuid())
    .execute(&mut *demotion)
    .await
    .unwrap();

    let contender_repo = repo.clone();
    let contender = tokio::spawn(async move {
        contender_repo
            .add_guest_authorized(workspace, admin, guest, room)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(75)).await;
    assert!(
        !contender.is_finished(),
        "guest write waits behind the canonical workspace lock"
    );
    demotion.commit().await.unwrap();

    assert!(matches!(
        contender
            .await
            .unwrap()
            .expect_err("the committed demotion must be observed"),
        GuestMembershipWriteError::NotAuthorized
    ));
    assert!(!repo.is_member(workspace, guest).await.unwrap());
    assert!(!RoomRepo::new(pool.clone())
        .is_member(room, guest)
        .await
        .unwrap());
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn guest_writes_require_effective_admin_access_until_commit() {
    let pool = pool();
    let repo = WorkspaceRepo::new(pool.clone());
    let owner = participant(&pool, "effective-owner").await;
    let admin = participant(&pool, "effective-admin").await;
    let first_guest = participant(&pool, "effective-first-guest").await;
    let workspace = test_workspace(&repo, owner, "effective").await;
    let room = room(&pool, workspace, owner, "effective").await;
    repo.add_member(workspace, admin, WorkspaceRole::Admin)
        .await
        .unwrap();

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
        repo.add_guest_authorized(workspace, admin, first_guest, room)
            .await
            .expect_err("a deactivated admin cannot enroll a guest"),
        GuestMembershipWriteError::NotAuthorized
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
           VALUES ($1, 'guest-effective-owner-test', true, NOW())",
    )
    .bind(owner.to_uuid())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE workspaces SET require_2fa = true WHERE id = $1")
        .bind(workspace.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        repo.add_guest_authorized(workspace, admin, first_guest, room)
            .await
            .expect_err("an admin missing mandatory 2FA cannot enroll a guest"),
        GuestMembershipWriteError::NotAuthorized
    ));
    assert!(!repo.is_member(workspace, first_guest).await.unwrap());

    sqlx::query(
        r"INSERT INTO totp_secrets
              (participant_id, secret, activated, activated_at)
           VALUES ($1, 'guest-effective-test', true, NOW())",
    )
    .bind(admin.to_uuid())
    .execute(&pool)
    .await
    .unwrap();
    repo.add_guest_authorized(workspace, admin, first_guest, room)
        .await
        .unwrap();

    sqlx::query("UPDATE participants SET deleted_at = NOW() WHERE id = $1")
        .bind(admin.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        repo.remove_guest_authorized(workspace, admin, first_guest)
            .await
            .expect_err("a deleted admin cannot remove a guest"),
        GuestMembershipWriteError::NotAuthorized
    ));
    assert!(repo.is_guest(workspace, first_guest).await.unwrap());
}

async fn install_room_member_failure(
    pool: &PgPool,
    participant: ParticipantId,
    operation: &str,
) -> (String, String) {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let function = format!("guest_atomic_fail_fn_{suffix}");
    let trigger = format!("guest_atomic_fail_trigger_{suffix}");
    let row_ref = if operation == "INSERT" { "NEW" } else { "OLD" };
    let create_function = format!(
        "CREATE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF {row_ref}.participant_id = '{}'::uuid THEN \
         RAISE EXCEPTION 'forced guest aggregate failure'; END IF; \
         RETURN {row_ref}; END $$",
        participant.to_uuid()
    );
    sqlx::query(&create_function).execute(pool).await.unwrap();
    let create_trigger = format!(
        "CREATE TRIGGER {trigger} BEFORE {operation} ON room_members \
         FOR EACH ROW EXECUTE FUNCTION {function}()"
    );
    sqlx::query(&create_trigger).execute(pool).await.unwrap();
    (trigger, function)
}

async fn remove_room_member_failure(pool: &PgPool, trigger: &str, function: &str) {
    sqlx::query(&format!("DROP TRIGGER {trigger} ON room_members"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(&format!("DROP FUNCTION {function}()"))
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn downstream_db_failures_roll_back_both_guest_aggregate_edges() {
    let pool = pool();
    let repo = WorkspaceRepo::new(pool.clone());
    let owner = participant(&pool, "rollback-owner").await;
    let guest = participant(&pool, "rollback-guest").await;
    let workspace = test_workspace(&repo, owner, "rollback").await;
    let room = room(&pool, workspace, owner, "rollback").await;

    let (insert_trigger, insert_function) =
        install_room_member_failure(&pool, guest, "INSERT").await;
    let add_result = repo
        .add_guest_authorized(workspace, owner, guest, room)
        .await;
    remove_room_member_failure(&pool, &insert_trigger, &insert_function).await;
    assert!(matches!(
        add_result.expect_err("forced room insert failure"),
        GuestMembershipWriteError::Storage(_)
    ));
    assert!(
        !repo.is_member(workspace, guest).await.unwrap(),
        "failed room insert rolls back the earlier workspace guest insert"
    );
    assert!(!RoomRepo::new(pool.clone())
        .is_member(room, guest)
        .await
        .unwrap());

    repo.add_guest_authorized(workspace, owner, guest, room)
        .await
        .unwrap();
    let (delete_trigger, delete_function) =
        install_room_member_failure(&pool, guest, "DELETE").await;
    let remove_result = repo.remove_guest_authorized(workspace, owner, guest).await;
    remove_room_member_failure(&pool, &delete_trigger, &delete_function).await;
    assert!(matches!(
        remove_result.expect_err("forced room delete failure"),
        GuestMembershipWriteError::Storage(_)
    ));
    assert!(
        repo.is_guest(workspace, guest).await.unwrap(),
        "failed room delete rolls back the earlier workspace guest deletion"
    );
    assert!(RoomRepo::new(pool.clone())
        .is_member(room, guest)
        .await
        .unwrap());
}
