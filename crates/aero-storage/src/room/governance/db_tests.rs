use super::*;

use crate::{DmRepo, TotpRepo, WorkspaceRepo};
use aero_common::{RoomKind, WorkspaceRole};
use sqlx::PgPool;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect_lazy(&url)
        .expect("valid DATABASE_URL")
}

async fn participant(pool: &PgPool, label: &str) -> ParticipantId {
    participant_with_kind(pool, label, "human").await
}

async fn participant_with_kind(pool: &PgPool, label: &str, kind: &str) -> ParticipantId {
    let participant = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, $2, $3)")
        .bind(participant.to_uuid())
        .bind(kind)
        .bind(format!("channel-governance-{label}-{participant}"))
        .execute(pool)
        .await
        .unwrap();
    participant
}

async fn fixture(
    pool: &PgPool,
    label: &str,
) -> (
    WorkspaceId,
    ParticipantId,
    ParticipantId,
    ParticipantId,
    RoomId,
) {
    let workspace_owner = participant(pool, &format!("{label}-workspace-owner")).await;
    let channel_owner = participant(pool, &format!("{label}-channel-owner")).await;
    let member = participant(pool, &format!("{label}-member")).await;
    let workspaces = WorkspaceRepo::new(pool.clone());
    let workspace = workspaces
        .create(
            format!("Channel governance {label}"),
            format!("channel-governance-{label}-{workspace_owner}"),
            workspace_owner,
        )
        .await
        .unwrap()
        .id;
    for participant in [channel_owner, member] {
        workspaces
            .add_member(workspace, participant, WorkspaceRole::Member)
            .await
            .unwrap();
    }
    let rooms = RoomRepo::new(pool.clone());
    let room = rooms
        .create_in_workspace(
            workspace,
            RoomKind::Channel,
            Some(format!("governance-{label}")),
            channel_owner,
        )
        .await
        .unwrap()
        .id;
    rooms.add_member(room, member).await.unwrap();
    (workspace, workspace_owner, channel_owner, member, room)
}

async fn role(pool: &PgPool, room: RoomId, participant: ParticipantId) -> Option<String> {
    sqlx::query_scalar("SELECT role FROM room_members WHERE room_id = $1 AND participant_id = $2")
        .bind(room.to_uuid())
        .bind(participant.to_uuid())
        .fetch_optional(pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn leave_and_role_writes_preserve_owner_and_channel_kind() {
    let pool = pool();
    let (workspace, _, owner, member, room) = fixture(&pool, "basic").await;
    let rooms = RoomRepo::new(pool.clone());

    assert!(matches!(
        rooms
            .leave_channel_authorized(room, owner)
            .await
            .expect_err("sole owner cannot leave"),
        RoomMembershipWriteError::LastOwner
    ));
    assert_eq!(role(&pool, room, owner).await.as_deref(), Some("owner"));
    assert!(rooms.leave_channel_authorized(room, member).await.unwrap());
    assert!(!rooms.leave_channel_authorized(room, member).await.unwrap());

    let group = rooms
        .create_in_workspace(workspace, RoomKind::Group, Some("group".into()), owner)
        .await
        .unwrap()
        .id;
    for error in [
        rooms
            .leave_channel_authorized(group, owner)
            .await
            .expect_err("group leave is not a channel operation"),
        rooms
            .set_channel_archived_authorized(group, owner, true)
            .await
            .expect_err("group archive is not a channel operation"),
        rooms
            .change_channel_member_role_authorized(group, owner, owner, RoomMemberRole::Member)
            .await
            .expect_err("group roles are not channel roles"),
    ] {
        assert!(matches!(error, RoomMembershipWriteError::NotChannel));
    }

    let peer = participant(&pool, "direct-peer").await;
    let third = participant(&pool, "direct-third").await;
    let workspaces = WorkspaceRepo::new(pool.clone());
    for participant in [peer, third] {
        workspaces
            .add_member(workspace, participant, WorkspaceRole::Member)
            .await
            .unwrap();
    }
    let direct = DmRepo::new(pool.clone())
        .find_or_create_in_workspace(workspace, owner, peer)
        .await
        .unwrap();
    assert!(
        rooms.add_member(direct.id, third).await.is_err(),
        "storage must reject generic direct membership"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM room_members WHERE room_id = $1")
        .bind(direct.id.to_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn concurrent_owner_demotions_leave_exactly_one_owner() {
    let pool = pool();
    let (_, _, first, second, room) = fixture(&pool, "demotion-race").await;
    let rooms = RoomRepo::new(pool.clone());
    rooms
        .change_channel_member_role_authorized(room, first, second, RoomMemberRole::Owner)
        .await
        .unwrap();

    let first_repo = rooms.clone();
    let second_repo = rooms.clone();
    let first_demotion = tokio::spawn(async move {
        first_repo
            .change_channel_member_role_authorized(room, first, first, RoomMemberRole::Member)
            .await
    });
    let second_demotion = tokio::spawn(async move {
        second_repo
            .change_channel_member_role_authorized(room, second, second, RoomMemberRole::Member)
            .await
    });
    let outcomes = [
        first_demotion.await.unwrap(),
        second_demotion.await.unwrap(),
    ];
    assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| matches!(result, Err(RoomMembershipWriteError::LastOwner)))
            .count(),
        1
    );
    let owners: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM room_members WHERE room_id = $1 AND role = 'owner'",
    )
    .bind(room.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(owners, 1);
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn ineffective_second_owner_does_not_allow_last_effective_owner_to_leave() {
    let pool = pool();
    let (workspace, workspace_owner, first, second, room) = fixture(&pool, "effective-owner").await;
    let rooms = RoomRepo::new(pool.clone());
    rooms
        .change_channel_member_role_authorized(room, first, second, RoomMemberRole::Owner)
        .await
        .unwrap();

    // This is legal while `first` remains effective. Once `second` is
    // deactivated, its role='owner' row must not count as remaining governance.
    sqlx::query(
        r"INSERT INTO workspace_deactivations
              (workspace_id, participant_id, deactivated_by)
           VALUES ($1, $2, $3)",
    )
    .bind(workspace.to_uuid())
    .bind(second.to_uuid())
    .bind(workspace_owner.to_uuid())
    .execute(&pool)
    .await
    .unwrap();

    assert!(matches!(
        rooms
            .leave_channel_authorized(room, first)
            .await
            .expect_err("inactive owner cannot satisfy the invariant"),
        RoomMembershipWriteError::LastOwner
    ));
    assert!(matches!(
        rooms
            .change_channel_member_role_authorized(room, first, first, RoomMemberRole::Member,)
            .await
            .expect_err("last effective owner cannot self-demote"),
        RoomMembershipWriteError::LastOwner
    ));
    assert_eq!(role(&pool, room, first).await.as_deref(), Some("owner"));
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn mandatory_2fa_applies_to_human_channel_owners_not_service_identities() {
    let pool = pool();
    let (workspace, workspace_owner, human_owner, _, room) =
        fixture(&pool, "service-owner-2fa").await;
    let workspaces = WorkspaceRepo::new(pool.clone());
    let rooms = RoomRepo::new(pool.clone());
    let bot_owner = participant_with_kind(&pool, "bot-owner", "bot").await;
    let bot_member = participant_with_kind(&pool, "bot-member", "bot").await;
    let unenrolled_human = participant(&pool, "unenrolled-owner-target").await;
    for participant in [bot_owner, bot_member, unenrolled_human] {
        workspaces
            .add_member(workspace, participant, WorkspaceRole::Member)
            .await
            .unwrap();
    }
    rooms.add_member(room, bot_owner).await.unwrap();
    rooms
        .change_channel_member_role_authorized(room, human_owner, bot_owner, RoomMemberRole::Owner)
        .await
        .unwrap();

    let totp = TotpRepo::new(pool.clone());
    totp.upsert_secret(workspace_owner, "JBSWY3DPEHPK3PXP")
        .await
        .unwrap();
    assert!(totp.activate(workspace_owner).await.unwrap());
    workspaces
        .set_require_2fa_authorized(workspace, true, workspace_owner)
        .await
        .expect("a bot owner remains effective without TOTP");

    assert!(sqlx::query_scalar::<_, bool>(
        "SELECT aero_participant_has_effective_workspace_access($1, $2)",
    )
    .bind(workspace.to_uuid())
    .bind(bot_owner.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap());
    assert!(!sqlx::query_scalar::<_, bool>(
        "SELECT aero_participant_has_effective_workspace_access($1, $2)",
    )
    .bind(workspace.to_uuid())
    .bind(unenrolled_human.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap());

    // Ordinary service-identity membership must not be mistaken for an owner
    // promotion by the INSERT trigger.
    rooms.add_member(room, bot_member).await.unwrap();
    rooms
        .change_channel_member_role_authorized(room, bot_owner, bot_member, RoomMemberRole::Owner)
        .await
        .expect("service identities bypass human-only mandatory TOTP");

    rooms.add_member(room, unenrolled_human).await.unwrap();
    assert!(matches!(
        rooms
            .change_channel_member_role_authorized(
                room,
                bot_owner,
                unenrolled_human,
                RoomMemberRole::Owner,
            )
            .await
            .expect_err("an unenrolled human cannot become channel owner"),
        RoomMembershipWriteError::TargetNotEligible
    ));
    let raw_error = sqlx::query(
        "UPDATE room_members
            SET role = 'owner'
          WHERE room_id = $1 AND participant_id = $2",
    )
    .bind(room.to_uuid())
    .bind(unenrolled_human.to_uuid())
    .execute(&pool)
    .await
    .expect_err("the database guard must reject a raw human owner promotion");
    assert!(crate::is_channel_effective_owner_violation(&raw_error));
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn direct_and_group_membership_inserts_do_not_upgrade_workspace_share_locks() {
    let pool = pool();
    let (workspace, _, creator, member, _) = fixture(&pool, "fixed-room-lock-order").await;
    let direct = RoomId::new();
    let group = RoomId::new();
    sqlx::query(
        r"INSERT INTO rooms
              (id, kind, name, created_by, workspace_id, is_group_dm)
           VALUES ($1, 'group', NULL, $2, $3, false)",
    )
    .bind(group.to_uuid())
    .bind(creator.to_uuid())
    .bind(workspace.to_uuid())
    .execute(&pool)
    .await
    .unwrap();

    // Both canonical DM paths can hold this SHARE lock before inserting their
    // first member.  A channel-only trigger must not try to upgrade either
    // transaction to FOR UPDATE, otherwise this pair forms an upgrade deadlock.
    let mut direct_tx = pool.begin().await.unwrap();
    let mut group_tx = pool.begin().await.unwrap();
    for tx in [&mut direct_tx, &mut group_tx] {
        sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR SHARE")
            .bind(workspace.to_uuid())
            .fetch_one(&mut **tx)
            .await
            .unwrap();
    }

    let direct_write = tokio::spawn(async move {
        // Migration 0199 validates a direct aggregate at commit, so the room
        // and both fixed member edges must be born in this same transaction.
        sqlx::query(
            r"INSERT INTO rooms
                  (id, kind, name, created_by, workspace_id, is_group_dm)
               VALUES ($1, 'direct', NULL, $2, $3, false)",
        )
        .bind(direct.to_uuid())
        .bind(creator.to_uuid())
        .bind(workspace.to_uuid())
        .execute(&mut *direct_tx)
        .await?;
        sqlx::query(
            r"INSERT INTO room_members
                  (room_id, participant_id, role, joined_at)
               VALUES ($1, $2, 'owner', NOW()),
                      ($1, $3, 'member', NOW())",
        )
        .bind(direct.to_uuid())
        .bind(creator.to_uuid())
        .bind(member.to_uuid())
        .execute(&mut *direct_tx)
        .await?;
        direct_tx.commit().await
    });
    let group_write = tokio::spawn(async move {
        sqlx::query(
            r"INSERT INTO room_members
                  (room_id, participant_id, role, joined_at)
               VALUES ($1, $2, 'owner', NOW())",
        )
        .bind(group.to_uuid())
        .bind(member.to_uuid())
        .execute(&mut *group_tx)
        .await?;
        group_tx.commit().await
    });
    let (direct_result, group_result) =
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(direct_write, group_write)
        })
        .await
        .expect("non-channel inserts must not wait on a workspace lock upgrade");
    direct_result
        .expect("direct writer task")
        .expect("direct membership insert");
    group_result
        .expect("group writer task")
        .expect("group membership insert");
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn concurrent_channel_creation_serializes_before_room_fk_and_owner_edge() {
    let pool = pool();
    let (workspace, _, first_creator, second_creator, _) =
        fixture(&pool, "channel-create-lock-order").await;

    // The room INSERT itself must already own the workspace governance fence,
    // before its FK and later owner edge can introduce weaker row locks.
    let pending_room = RoomId::new();
    let mut fenced_create = pool.begin().await.unwrap();
    sqlx::query(
        r"INSERT INTO rooms (id, kind, name, created_by, workspace_id)
           VALUES ($1, 'channel', $2, $3, $4)",
    )
    .bind(pending_room.to_uuid())
    .bind(format!("pending-channel-{pending_room}"))
    .bind(first_creator.to_uuid())
    .bind(workspace.to_uuid())
    .execute(&mut *fenced_create)
    .await
    .unwrap();

    let mut observer = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL lock_timeout = '150ms'")
        .execute(&mut *observer)
        .await
        .unwrap();
    let blocked = sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR SHARE")
        .bind(workspace.to_uuid())
        .execute(&mut *observer)
        .await
        .expect_err("channel room INSERT must hold the conflicting workspace fence");
    assert_eq!(
        blocked
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code),
        Some(std::borrow::Cow::Borrowed("55P03"))
    );
    observer.rollback().await.unwrap();
    fenced_create.rollback().await.unwrap();

    let start = std::sync::Arc::new(tokio::sync::Barrier::new(3));
    let first_repo = RoomRepo::new(pool.clone());
    let first_start = start.clone();
    let first = tokio::spawn(async move {
        first_start.wait().await;
        first_repo
            .create_in_workspace(
                workspace,
                RoomKind::Channel,
                Some(format!("parallel-channel-{first_creator}")),
                first_creator,
            )
            .await
    });
    let second_repo = RoomRepo::new(pool.clone());
    let second_start = start.clone();
    let second = tokio::spawn(async move {
        second_start.wait().await;
        second_repo
            .create_in_workspace(
                workspace,
                RoomKind::Channel,
                Some(format!("parallel-channel-{second_creator}")),
                second_creator,
            )
            .await
    });
    start.wait().await;
    let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(first, second)
    })
    .await
    .expect("concurrent channel creation must serialize rather than deadlock");
    first
        .expect("first channel writer task")
        .expect("first channel create");
    second
        .expect("second channel writer task")
        .expect("second channel create");
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn channel_birth_requires_an_effective_owner_at_transaction_commit() {
    let pool = pool();
    let (workspace, _, owner, _, _) = fixture(&pool, "channel-birth-commit").await;

    let ownerless = RoomId::new();
    let error = sqlx::query(
        r"INSERT INTO rooms (id, kind, name, created_by, workspace_id)
           VALUES ($1, 'channel', $2, $3, $4)",
    )
    .bind(ownerless.to_uuid())
    .bind(format!("ownerless-{ownerless}"))
    .bind(owner.to_uuid())
    .bind(workspace.to_uuid())
    .execute(&pool)
    .await
    .expect_err("an autocommit channel INSERT cannot publish an ownerless aggregate");
    assert!(crate::is_channel_effective_owner_violation(&error));
    assert!(
        !sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM rooms WHERE id = $1)")
            .bind(ownerless.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap()
    );

    let complete = RoomId::new();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        r"INSERT INTO rooms (id, kind, name, created_by, workspace_id)
           VALUES ($1, 'channel', $2, $3, $4)",
    )
    .bind(complete.to_uuid())
    .bind(format!("complete-{complete}"))
    .bind(owner.to_uuid())
    .bind(workspace.to_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        r"INSERT INTO room_members (room_id, participant_id, role, joined_at)
           VALUES ($1, $2, 'owner', NOW())",
    )
    .bind(complete.to_uuid())
    .bind(owner.to_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit()
        .await
        .expect("the supported room+owner transaction satisfies the deferred constraint");

    assert_eq!(role(&pool, complete, owner).await.as_deref(), Some("owner"));
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn direct_participant_and_totp_downgrades_preserve_effective_channel_owner() {
    let pool = pool();
    let (workspace, workspace_owner, owner, successor, room) =
        fixture(&pool, "direct-effective-state").await;

    let deleted = sqlx::query("UPDATE participants SET deleted_at = NOW() WHERE id = $1")
        .bind(owner.to_uuid())
        .execute(&pool)
        .await
        .expect_err("direct SQL cannot tombstone a sole effective channel owner");
    assert!(crate::is_channel_effective_owner_violation(&deleted));

    let totp = TotpRepo::new(pool.clone());
    for participant in [workspace_owner, owner, successor] {
        totp.upsert_secret(participant, "JBSWY3DPEHPK3PXP")
            .await
            .unwrap();
        assert!(totp.activate(participant).await.unwrap());
    }
    WorkspaceRepo::new(pool.clone())
        .set_require_2fa_authorized(workspace, true, workspace_owner)
        .await
        .unwrap();

    let deactivated =
        sqlx::query("UPDATE totp_secrets SET activated = false WHERE participant_id = $1")
            .bind(owner.to_uuid())
            .execute(&pool)
            .await
            .expect_err("direct SQL cannot deactivate the sole owner's mandatory TOTP");
    assert!(crate::is_channel_effective_owner_violation(&deactivated));
    let removed = sqlx::query("DELETE FROM totp_secrets WHERE participant_id = $1")
        .bind(owner.to_uuid())
        .execute(&pool)
        .await
        .expect_err("direct SQL cannot remove the sole owner's mandatory TOTP");
    assert!(crate::is_channel_effective_owner_violation(&removed));

    RoomRepo::new(pool.clone())
        .change_channel_member_role_authorized(room, owner, successor, RoomMemberRole::Owner)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query("DELETE FROM totp_secrets WHERE participant_id = $1")
            .bind(owner.to_uuid())
            .execute(&pool)
            .await
            .expect("a successor makes the direct TOTP downgrade safe")
            .rows_affected(),
        1
    );

    let bot = participant_with_kind(&pool, "direct-kind-owner", "bot").await;
    WorkspaceRepo::new(pool.clone())
        .add_member(workspace, bot, WorkspaceRole::Member)
        .await
        .unwrap();
    let bot_room = RoomRepo::new(pool.clone())
        .create_in_workspace(
            workspace,
            RoomKind::Channel,
            Some(format!("bot-kind-{bot}")),
            bot,
        )
        .await
        .unwrap();
    let kind = sqlx::query("UPDATE participants SET kind = 'human' WHERE id = $1")
        .bind(bot.to_uuid())
        .execute(&pool)
        .await
        .expect_err("turning an unenrolled service owner human cannot strand governance");
    assert!(crate::is_channel_effective_owner_violation(&kind));
    assert_eq!(
        role(&pool, bot_room.id, bot).await.as_deref(),
        Some("owner")
    );
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn multirow_identity_downgrades_cannot_remove_every_effective_owner() {
    let pool = pool();
    let (workspace, workspace_owner, first, second, room) =
        fixture(&pool, "multirow-effective-state").await;
    let rooms = RoomRepo::new(pool.clone());
    rooms
        .change_channel_member_role_authorized(room, first, second, RoomMemberRole::Owner)
        .await
        .unwrap();

    let participants = [first.to_uuid(), second.to_uuid()];
    let tombstone = sqlx::query("UPDATE participants SET deleted_at = NOW() WHERE id = ANY($1)")
        .bind(participants)
        .execute(&pool)
        .await
        .expect_err("one multi-row statement cannot tombstone every channel owner");
    assert!(crate::is_channel_effective_owner_violation(&tombstone));
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM participants WHERE id = ANY($1) AND deleted_at IS NULL",
    )
    .bind(participants)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        active, 2,
        "the failed multi-row UPDATE rolls back every row"
    );

    let totp = TotpRepo::new(pool.clone());
    for participant in [workspace_owner, first, second] {
        totp.upsert_secret(participant, "JBSWY3DPEHPK3PXP")
            .await
            .unwrap();
        assert!(totp.activate(participant).await.unwrap());
    }
    WorkspaceRepo::new(pool.clone())
        .set_require_2fa_authorized(workspace, true, workspace_owner)
        .await
        .unwrap();

    let remove_totp = sqlx::query("DELETE FROM totp_secrets WHERE participant_id = ANY($1)")
        .bind(participants)
        .execute(&pool)
        .await
        .expect_err("one multi-row DELETE cannot remove every owner's mandatory TOTP");
    assert!(crate::is_channel_effective_owner_violation(&remove_totp));
    let enrolled: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM totp_secrets
          WHERE participant_id = ANY($1) AND activated",
    )
    .bind(participants)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        enrolled, 2,
        "the failed multi-row DELETE rolls back every row"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn direct_identity_downgrade_and_owner_promotion_do_not_deadlock() {
    let pool = pool();
    let (_, _, owner, successor, room) = fixture(&pool, "identity-owner-race").await;
    let start = std::sync::Arc::new(tokio::sync::Barrier::new(3));

    let downgrade_pool = pool.clone();
    let downgrade_start = start.clone();
    let downgrade = tokio::spawn(async move {
        downgrade_start.wait().await;
        sqlx::query("UPDATE participants SET deleted_at = NOW() WHERE id = $1")
            .bind(owner.to_uuid())
            .execute(&downgrade_pool)
            .await
    });
    let promote_pool = pool.clone();
    let promote_start = start.clone();
    let promote = tokio::spawn(async move {
        promote_start.wait().await;
        RoomRepo::new(promote_pool)
            .change_channel_member_role_authorized(room, owner, successor, RoomMemberRole::Owner)
            .await
    });

    start.wait().await;
    let (downgrade, promote) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(downgrade, promote)
    })
    .await
    .expect("identity downgrade and owner promotion must serialize, not deadlock");
    let downgrade = downgrade.expect("downgrade task");
    if let Err(error) = downgrade {
        assert!(crate::is_channel_effective_owner_violation(&error));
    }
    promote
        .expect("promotion task")
        .expect("the successor promotion must eventually commit");
    assert_eq!(role(&pool, room, successor).await.as_deref(), Some("owner"));
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn workspace_delete_and_identity_fence_do_not_deadlock() {
    let pool = pool();
    let (workspace, _, owner, _, _) = fixture(&pool, "workspace-delete-fence").await;
    let start = std::sync::Arc::new(tokio::sync::Barrier::new(3));

    let delete_pool = pool.clone();
    let delete_start = start.clone();
    let delete = tokio::spawn(async move {
        delete_start.wait().await;
        WorkspaceRepo::new(delete_pool).delete(workspace).await
    });
    let downgrade_pool = pool.clone();
    let downgrade_start = start.clone();
    let downgrade = tokio::spawn(async move {
        downgrade_start.wait().await;
        sqlx::query("UPDATE participants SET deleted_at = NOW() WHERE id = $1")
            .bind(owner.to_uuid())
            .execute(&downgrade_pool)
            .await
    });

    start.wait().await;
    let (delete, downgrade) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(delete, downgrade)
    })
    .await
    .expect("workspace delete and identity fence must serialize, not deadlock");
    assert!(delete
        .expect("workspace delete task")
        .expect("workspace delete result"));
    if let Err(error) = downgrade.expect("downgrade task") {
        assert!(crate::is_channel_effective_owner_violation(&error));
    }
    assert!(WorkspaceRepo::new(pool)
        .get(workspace)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn ownership_transfer_rolls_back_both_role_updates_on_failure() {
    let pool = pool();
    let (_, _, owner, target, room) = fixture(&pool, "transfer-rollback").await;
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let function = format!("channel_transfer_fail_fn_{suffix}");
    let trigger = format!("channel_transfer_fail_trigger_{suffix}");
    sqlx::query(&format!(
        "CREATE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RAISE EXCEPTION 'forced transfer failure'; END $$"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER {trigger} BEFORE UPDATE ON room_members \
         FOR EACH ROW WHEN (OLD.room_id = '{}'::uuid \
           AND OLD.participant_id = '{}'::uuid AND NEW.role = 'member') \
         EXECUTE FUNCTION {function}()",
        room.to_uuid(),
        owner.to_uuid()
    ))
    .execute(&pool)
    .await
    .unwrap();

    let result = RoomRepo::new(pool.clone())
        .transfer_channel_ownership_authorized(room, owner, target)
        .await;
    sqlx::query(&format!("DROP TRIGGER {trigger} ON room_members"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("DROP FUNCTION {function}()"))
        .execute(&pool)
        .await
        .unwrap();

    assert!(matches!(
        result.expect_err("the second update must fail"),
        RoomMembershipWriteError::Storage(_)
    ));
    assert_eq!(role(&pool, room, owner).await.as_deref(), Some("owner"));
    assert_eq!(role(&pool, room, target).await.as_deref(), Some("member"));
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn channel_management_requires_current_effective_manager_and_atomic_patch() {
    let pool = pool();
    let (workspace, workspace_owner, owner, member, room) =
        fixture(&pool, "manager-boundary").await;
    let rooms = RoomRepo::new(pool.clone());

    assert!(matches!(
        rooms
            .patch_channel_authorized(
                room,
                member,
                ChannelMetaPatch {
                    topic: Some(Some("forbidden".into())),
                    description: None,
                    is_private: None,
                },
            )
            .await
            .expect_err("ordinary member cannot patch channel"),
        RoomMembershipWriteError::NotAuthorized
    ));
    assert!(matches!(
        rooms
            .patch_channel_authorized(
                room,
                owner,
                ChannelMetaPatch {
                    topic: Some(Some("x".repeat(MAX_CHANNEL_TOPIC_CHARS + 1))),
                    description: None,
                    is_private: None,
                },
            )
            .await
            .expect_err("oversize topic is rejected"),
        RoomMembershipWriteError::InvalidInput(_)
    ));

    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let function = format!("channel_patch_fail_fn_{suffix}");
    let trigger = format!("channel_patch_fail_trigger_{suffix}");
    let before_patch = sqlx::query_as::<_, (Option<String>, Option<String>, bool)>(
        "SELECT topic, description, is_private FROM rooms WHERE id = $1",
    )
    .bind(room.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF NEW.id = '{}'::uuid THEN RAISE EXCEPTION 'forced patch failure'; \
         END IF; RETURN NEW; END $$",
        room.to_uuid()
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER {trigger} BEFORE UPDATE ON rooms \
         FOR EACH ROW EXECUTE FUNCTION {function}()"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let failed = rooms
        .patch_channel_authorized(
            room,
            owner,
            ChannelMetaPatch {
                topic: Some(Some("new topic".into())),
                description: Some(Some("new description".into())),
                is_private: Some(true),
            },
        )
        .await;
    sqlx::query(&format!("DROP TRIGGER {trigger} ON rooms"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("DROP FUNCTION {function}()"))
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        failed.expect_err("forced patch failure"),
        RoomMembershipWriteError::Storage(_)
    ));
    let unchanged = sqlx::query_as::<_, (Option<String>, Option<String>, bool)>(
        "SELECT topic, description, is_private FROM rooms WHERE id = $1",
    )
    .bind(room.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(unchanged, before_patch);

    // The topic-history append is transaction-coupled to the room UPDATE. If
    // history persistence fails, neither half may commit.
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let function = format!("channel_history_fail_fn_{suffix}");
    let trigger = format!("channel_history_fail_trigger_{suffix}");
    sqlx::query(&format!(
        "CREATE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF NEW.room_id = '{}'::uuid THEN RAISE EXCEPTION 'forced history failure'; \
         END IF; RETURN NEW; END $$",
        room.to_uuid()
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER {trigger} BEFORE INSERT ON channel_topic_history \
         FOR EACH ROW EXECUTE FUNCTION {function}()"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let failed = rooms
        .patch_channel_authorized(
            room,
            owner,
            ChannelMetaPatch {
                topic: Some(Some("must roll back".into())),
                description: None,
                is_private: None,
            },
        )
        .await;
    sqlx::query(&format!("DROP TRIGGER {trigger} ON channel_topic_history"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("DROP FUNCTION {function}()"))
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        failed.expect_err("history failure must abort metadata patch"),
        RoomMembershipWriteError::Storage(_)
    ));
    let rolled_back_topic =
        sqlx::query_scalar::<_, Option<String>>("SELECT topic FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rolled_back_topic, before_patch.0);
    let history_after_failure: i64 =
        sqlx::query_scalar("SELECT count(*) FROM channel_topic_history WHERE room_id = $1")
            .bind(room.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(history_after_failure, 0);

    rooms
        .patch_channel_authorized(
            room,
            owner,
            ChannelMetaPatch {
                topic: Some(Some("transactional topic".into())),
                description: None,
                is_private: None,
            },
        )
        .await
        .unwrap();
    let history = sqlx::query_as::<_, (Option<String>, Option<String>, uuid::Uuid)>(
        r"SELECT old_topic, new_topic, changed_by
             FROM channel_topic_history
            WHERE room_id = $1",
    )
    .bind(room.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(history.0, before_patch.0);
    assert_eq!(history.1.as_deref(), Some("transactional topic"));
    assert_eq!(history.2, owner.to_uuid());

    rooms
        .change_channel_member_role_authorized(room, owner, member, RoomMemberRole::Owner)
        .await
        .unwrap();
    let totp = TotpRepo::new(pool.clone());
    totp.upsert_secret(member, "JBSWY3DPEHPK3PXP")
        .await
        .unwrap();
    assert!(totp.activate(member).await.unwrap());

    sqlx::query(
        r"INSERT INTO workspace_deactivations
              (workspace_id, participant_id, deactivated_by)
           VALUES ($1, $2, $3)",
    )
    .bind(workspace.to_uuid())
    .bind(owner.to_uuid())
    .bind(workspace_owner.to_uuid())
    .execute(&pool)
    .await
    .unwrap();
    assert!(matches!(
        rooms
            .set_channel_archived_authorized(room, owner, true)
            .await
            .expect_err("deactivated room owner cannot archive"),
        RoomMembershipWriteError::NotAuthorized
    ));
    sqlx::query(
        "DELETE FROM workspace_deactivations WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(workspace.to_uuid())
    .bind(owner.to_uuid())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r"INSERT INTO totp_secrets
              (participant_id, secret, activated, activated_at)
           VALUES ($1, 'channel-governance-workspace-owner', true, now())",
    )
    .bind(workspace_owner.to_uuid())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE workspaces SET require_2fa = true WHERE id = $1")
        .bind(workspace.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        rooms
            .set_channel_slowmode_authorized(room, owner, 30)
            .await
            .expect_err("owner missing mandatory 2FA cannot manage"),
        RoomMembershipWriteError::NotAuthorized
    ));
    sqlx::query("DELETE FROM workspace_members WHERE workspace_id = $1 AND participant_id = $2")
        .bind(workspace.to_uuid())
        .bind(owner.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        rooms
            .set_channel_reaction_limit_authorized(room, owner, Some(3))
            .await
            .expect_err("removed creator cannot manage channel"),
        RoomMembershipWriteError::NotAuthorized
    ));
}
