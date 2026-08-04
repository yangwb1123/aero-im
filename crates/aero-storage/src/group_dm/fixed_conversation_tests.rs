use std::time::Duration;

use aero_common::{ParticipantId, RoomId, WorkspaceId};
use sqlx::PgPool;

use super::*;
use crate::WorkspaceRepo;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect_lazy(&url)
        .expect("valid DATABASE_URL")
}

fn constraint(error: &sqlx::Error) -> Option<&str> {
    error
        .as_database_error()
        .and_then(|database_error| database_error.constraint())
}

struct Fixture {
    pool: PgPool,
    workspace: WorkspaceId,
    members: Vec<ParticipantId>,
}

impl Fixture {
    async fn new(member_count: usize) -> Self {
        let pool = pool();
        let mut members = Vec::with_capacity(member_count);
        for ordinal in 0..member_count {
            let participant = ParticipantId::new();
            sqlx::query(
                "INSERT INTO participants (id, kind, display_name)
                 VALUES ($1, 'human', $2)",
            )
            .bind(participant.to_uuid())
            .bind(format!("fixed-conversation-{ordinal}-{participant}"))
            .execute(&pool)
            .await
            .unwrap();
            members.push(participant);
        }

        let workspace = WorkspaceId::new();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(
            "INSERT INTO workspaces (id, name, slug, created_by)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(workspace.to_uuid())
        .bind(format!("Fixed conversation {workspace}"))
        .bind(format!("fixed-conversation-{workspace}"))
        .bind(members[0].to_uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
        for (ordinal, participant) in members.iter().enumerate() {
            sqlx::query(
                "INSERT INTO workspace_members
                     (workspace_id, participant_id, role)
                 VALUES ($1, $2, $3)",
            )
            .bind(workspace.to_uuid())
            .bind(participant.to_uuid())
            .bind(if ordinal == 0 { "owner" } else { "member" })
            .execute(&mut *tx)
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();

        Self {
            pool,
            workspace,
            members,
        }
    }

    async fn direct(&self, a: ParticipantId, b: ParticipantId) -> RoomId {
        let room = RoomId::new();
        let mut tx = self.pool.begin().await.unwrap();
        sqlx::query(
            "INSERT INTO rooms
                 (id, kind, name, created_by, workspace_id, is_group_dm)
             VALUES ($1, 'direct', NULL, $2, $3, false)",
        )
        .bind(room.to_uuid())
        .bind(a.to_uuid())
        .bind(self.workspace.to_uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'owner'), ($1, $3, 'member')",
        )
        .bind(room.to_uuid())
        .bind(a.to_uuid())
        .bind(b.to_uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        room
    }

    async fn group_dm(&self, members: &[ParticipantId]) -> RoomId {
        let room = RoomId::new();
        let mut tx = self.pool.begin().await.unwrap();
        sqlx::query(
            "INSERT INTO rooms
                 (id, kind, name, created_by, workspace_id, is_group_dm)
             VALUES ($1, 'group', NULL, $2, $3, false)",
        )
        .bind(room.to_uuid())
        .bind(members[0].to_uuid())
        .bind(self.workspace.to_uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
        for (ordinal, participant) in members.iter().enumerate() {
            sqlx::query(
                "INSERT INTO room_members (room_id, participant_id, role)
                 VALUES ($1, $2, $3)",
            )
            .bind(room.to_uuid())
            .bind(participant.to_uuid())
            .bind(if ordinal == 0 { "owner" } else { "member" })
            .execute(&mut *tx)
            .await
            .unwrap();
        }
        sqlx::query("UPDATE rooms SET is_group_dm = true WHERE id = $1")
            .bind(room.to_uuid())
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        room
    }
}

#[tokio::test]
async fn repo_rejects_illegal_fixed_set_before_database_io() {
    let repo = GroupDmRepo::new(pool());
    let creator = ParticipantId::new();
    let second = ParticipantId::new();
    let third = ParticipantId::new();
    let mut too_many = vec![creator];
    too_many.extend((0..MAX_GROUP_DM_MEMBERS).map(|_| ParticipantId::new()));

    for result in [
        repo.find_or_create_in_workspace(WorkspaceId::new(), &[creator, second], creator)
            .await,
        repo.find_or_create_in_workspace(WorkspaceId::new(), &[creator, second, second], creator)
            .await,
        repo.find_or_create_in_workspace(
            WorkspaceId::new(),
            &[creator, second, third],
            ParticipantId::new(),
        )
        .await,
        repo.find_or_create_in_workspace(WorkspaceId::new(), &too_many, creator)
            .await,
    ] {
        assert!(matches!(result, Err(GroupDmWriteError::InvalidInput(_))));
    }
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn direct_sql_birth_and_group_dm_seal_require_legal_sets() {
    let fixture = Fixture::new(10).await;
    let [alice, bob, carol, dan, eve, frank, grace, heidi, ivan, judy] = fixture.members.as_slice()
    else {
        panic!("fixture must contain ten participants");
    };

    let incomplete_direct = RoomId::new();
    let error = sqlx::query(
        "INSERT INTO rooms
             (id, kind, name, created_by, workspace_id, is_group_dm)
         VALUES ($1, 'direct', NULL, $2, $3, false)",
    )
    .bind(incomplete_direct.to_uuid())
    .bind(alice.to_uuid())
    .bind(fixture.workspace.to_uuid())
    .execute(&fixture.pool)
    .await
    .expect_err("an incomplete direct autocommit must fail at commit");
    assert_eq!(
        constraint(&error),
        Some("fixed_conversation_legal_membership")
    );
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT true FROM rooms WHERE id = $1")
            .bind(incomplete_direct.to_uuid())
            .fetch_optional(&fixture.pool)
            .await
            .unwrap()
            .is_none()
    );

    let direct = fixture.direct(*alice, *bob).await;
    let direct_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM room_members WHERE room_id = $1")
            .bind(direct.to_uuid())
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert_eq!(direct_count, 2);

    let marked_at_insert = RoomId::new();
    let error = sqlx::query(
        "INSERT INTO rooms
             (id, kind, name, created_by, workspace_id, is_group_dm)
         VALUES ($1, 'group', NULL, $2, $3, true)",
    )
    .bind(marked_at_insert.to_uuid())
    .bind(alice.to_uuid())
    .bind(fixture.workspace.to_uuid())
    .execute(&fixture.pool)
    .await
    .expect_err("marked group-DM rows must use assemble-then-seal birth");
    assert_eq!(constraint(&error), Some("rooms_group_dm_birth_protocol"));

    let too_small = RoomId::new();
    sqlx::query(
        "INSERT INTO rooms
             (id, kind, name, created_by, workspace_id, is_group_dm)
         VALUES ($1, 'group', NULL, $2, $3, false)",
    )
    .bind(too_small.to_uuid())
    .bind(alice.to_uuid())
    .bind(fixture.workspace.to_uuid())
    .execute(&fixture.pool)
    .await
    .unwrap();
    for participant in [alice, bob] {
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'member')",
        )
        .bind(too_small.to_uuid())
        .bind(participant.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();
    }
    let error = sqlx::query("UPDATE rooms SET is_group_dm = true WHERE id = $1")
        .bind(too_small.to_uuid())
        .execute(&fixture.pool)
        .await
        .expect_err("two members is not a group DM");
    assert_eq!(
        constraint(&error),
        Some("fixed_conversation_legal_membership")
    );
    sqlx::query(
        "INSERT INTO room_members (room_id, participant_id, role)
         VALUES ($1, $2, 'member')",
    )
    .bind(too_small.to_uuid())
    .bind(carol.to_uuid())
    .execute(&fixture.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE rooms SET is_group_dm = true WHERE id = $1")
        .bind(too_small.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();

    let creator_missing = RoomId::new();
    sqlx::query(
        "INSERT INTO rooms
             (id, kind, name, created_by, workspace_id, is_group_dm)
         VALUES ($1, 'group', NULL, $2, $3, false)",
    )
    .bind(creator_missing.to_uuid())
    .bind(judy.to_uuid())
    .bind(fixture.workspace.to_uuid())
    .execute(&fixture.pool)
    .await
    .unwrap();
    for participant in [alice, bob, carol] {
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'member')",
        )
        .bind(creator_missing.to_uuid())
        .bind(participant.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();
    }
    let error = sqlx::query("UPDATE rooms SET is_group_dm = true WHERE id = $1")
        .bind(creator_missing.to_uuid())
        .execute(&fixture.pool)
        .await
        .expect_err("the group-DM creator must be a member");
    assert_eq!(
        constraint(&error),
        Some("fixed_conversation_legal_membership")
    );

    let too_large = RoomId::new();
    sqlx::query(
        "INSERT INTO rooms
             (id, kind, name, created_by, workspace_id, is_group_dm)
         VALUES ($1, 'group', NULL, $2, $3, false)",
    )
    .bind(too_large.to_uuid())
    .bind(alice.to_uuid())
    .bind(fixture.workspace.to_uuid())
    .execute(&fixture.pool)
    .await
    .unwrap();
    for participant in [alice, bob, carol, dan, eve, frank, grace, heidi, ivan] {
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'member')",
        )
        .bind(too_large.to_uuid())
        .bind(participant.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();
    }
    let error = sqlx::query("UPDATE rooms SET is_group_dm = true WHERE id = $1")
        .bind(too_large.to_uuid())
        .execute(&fixture.pool)
        .await
        .expect_err("nine members is not a group DM");
    assert_eq!(
        constraint(&error),
        Some("fixed_conversation_legal_membership")
    );

    let error = sqlx::query("UPDATE rooms SET is_group_dm = false WHERE id = $1")
        .bind(too_small.to_uuid())
        .execute(&fixture.pool)
        .await
        .expect_err("a sealed group DM cannot be unmarked");
    assert_eq!(constraint(&error), Some("rooms_group_dm_marker_immutable"));
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn fixed_membership_rejects_direct_sql_and_concurrent_appends() {
    let fixture = Fixture::new(7).await;
    let direct = fixture.direct(fixture.members[0], fixture.members[1]).await;
    let group_dm = fixture.group_dm(&fixture.members[..3]).await;

    let append = |room: RoomId, participant: ParticipantId, pool: PgPool| async move {
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'member')",
        )
        .bind(room.to_uuid())
        .bind(participant.to_uuid())
        .execute(&pool)
        .await
    };
    let (first, second) = tokio::join!(
        append(direct, fixture.members[2], fixture.pool.clone()),
        append(direct, fixture.members[3], fixture.pool.clone())
    );
    for result in [first, second] {
        let error = result.expect_err("both concurrent direct appends must fail");
        assert_eq!(
            constraint(&error),
            Some("room_members_fixed_membership_insert")
        );
    }

    for (room, participant) in [(group_dm, fixture.members[3]), (direct, fixture.members[4])] {
        let error = sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'member')",
        )
        .bind(room.to_uuid())
        .bind(participant.to_uuid())
        .execute(&fixture.pool)
        .await
        .expect_err("a fixed aggregate cannot gain a member");
        assert_eq!(
            constraint(&error),
            Some("room_members_fixed_membership_insert")
        );

        let error = sqlx::query(
            "DELETE FROM room_members
              WHERE room_id = $1 AND participant_id = $2",
        )
        .bind(room.to_uuid())
        .bind(fixture.members[0].to_uuid())
        .execute(&fixture.pool)
        .await
        .expect_err("a fixed aggregate cannot lose a member");
        assert_eq!(
            constraint(&error),
            Some("room_members_fixed_membership_delete")
        );
    }

    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM room_members WHERE room_id = $1")
            .bind(direct.to_uuid())
            .fetch_one(&fixture.pool)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM room_members WHERE room_id = $1")
            .bind(group_dm.to_uuid())
            .fetch_one(&fixture.pool)
            .await
            .unwrap(),
        3
    );
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn whole_room_and_workspace_deletion_bypass_only_the_parent_cascade() {
    let fixture = Fixture::new(6).await;
    let direct = fixture.direct(fixture.members[0], fixture.members[1]).await;
    let group_dm = fixture.group_dm(&fixture.members[..3]).await;

    sqlx::query("DELETE FROM rooms WHERE id = ANY($1)")
        .bind([direct.to_uuid(), group_dm.to_uuid()])
        .execute(&fixture.pool)
        .await
        .expect("parent-first deletion must cascade through fixed member edges");
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM rooms WHERE id = ANY($1)")
        .bind([direct.to_uuid(), group_dm.to_uuid()])
        .fetch_one(&fixture.pool)
        .await
        .unwrap();
    assert_eq!(remaining, 0);

    let direct = fixture.direct(fixture.members[0], fixture.members[1]).await;
    let group_dm = fixture.group_dm(&fixture.members[..3]).await;
    assert!(
        WorkspaceRepo::new(fixture.pool.clone())
            .delete(fixture.workspace)
            .await
            .unwrap(),
        "workspace aggregate deletion must remain legal"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM rooms WHERE id = ANY($1)")
            .bind([direct.to_uuid(), group_dm.to_uuid()])
            .fetch_one(&fixture.pool)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn ordinary_group_and_channel_membership_remain_mutable() {
    let fixture = Fixture::new(4).await;
    let ordinary_group = RoomId::new();
    sqlx::query(
        "INSERT INTO rooms
             (id, kind, name, created_by, workspace_id, is_group_dm)
         VALUES ($1, 'group', 'ordinary', $2, $3, false)",
    )
    .bind(ordinary_group.to_uuid())
    .bind(fixture.members[0].to_uuid())
    .bind(fixture.workspace.to_uuid())
    .execute(&fixture.pool)
    .await
    .unwrap();

    let channel = RoomId::new();
    let mut channel_birth = fixture.pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO rooms
             (id, kind, name, created_by, workspace_id, is_group_dm)
         VALUES ($1, 'channel', 'ordinary-channel', $2, $3, false)",
    )
    .bind(channel.to_uuid())
    .bind(fixture.members[0].to_uuid())
    .bind(fixture.workspace.to_uuid())
    .execute(&mut *channel_birth)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO room_members (room_id, participant_id, role)
         VALUES ($1, $2, 'owner')",
    )
    .bind(channel.to_uuid())
    .bind(fixture.members[0].to_uuid())
    .execute(&mut *channel_birth)
    .await
    .unwrap();
    channel_birth.commit().await.unwrap();

    for room in [ordinary_group, channel] {
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'member')",
        )
        .bind(room.to_uuid())
        .bind(fixture.members[1].to_uuid())
        .execute(&fixture.pool)
        .await
        .expect("ordinary membership append remains supported");
        sqlx::query(
            "DELETE FROM room_members
              WHERE room_id = $1 AND participant_id = $2",
        )
        .bind(room.to_uuid())
        .bind(fixture.members[1].to_uuid())
        .execute(&fixture.pool)
        .await
        .expect("ordinary membership removal remains supported");
    }
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn room_birth_holds_legacy_advisory_before_first_member_write() {
    let fixture = Fixture::new(4).await;
    let expected = fixture.members[..3].to_vec();
    let legacy_room = RoomId::new();
    let mut old_pod = fixture.pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO rooms
             (id, kind, name, created_by, workspace_id, is_group_dm)
         VALUES ($1, 'group', NULL, $2, $3, false)",
    )
    .bind(legacy_room.to_uuid())
    .bind(expected[0].to_uuid())
    .bind(fixture.workspace.to_uuid())
    .execute(&mut *old_pod)
    .await
    .unwrap();

    let compatibility_key = format!("aero:group-dm-legacy:{}", fixture.workspace.to_uuid());
    let mut observer = fixture.pool.begin().await.unwrap();
    let acquired: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(&compatibility_key)
            .fetch_one(&mut *observer)
            .await
            .unwrap();
    assert!(
        !acquired,
        "the old-pod room INSERT itself must own the compatibility advisory"
    );
    observer.rollback().await.unwrap();

    let repo = GroupDmRepo::new(fixture.pool.clone());
    let workspace = fixture.workspace;
    let creator = expected[0];
    let requested = expected.clone();
    let mut new_pod = tokio::spawn(async move {
        repo.find_or_create_in_workspace(workspace, &requested, creator)
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut new_pod)
            .await
            .is_err(),
        "new-pod lookup must wait until the old birth transaction completes"
    );

    for (ordinal, participant) in expected.iter().enumerate() {
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, $3)",
        )
        .bind(legacy_room.to_uuid())
        .bind(participant.to_uuid())
        .bind(if ordinal == 0 { "owner" } else { "member" })
        .execute(&mut *old_pod)
        .await
        .unwrap();
    }
    old_pod.commit().await.unwrap();

    let opened = tokio::time::timeout(Duration::from_secs(5), new_pod)
        .await
        .expect("new pod must resume after legacy commit")
        .unwrap()
        .unwrap();
    assert_eq!(opened.id, legacy_room);
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT is_group_dm FROM rooms WHERE id = $1")
            .bind(legacy_room.to_uuid())
            .fetch_one(&fixture.pool)
            .await
            .unwrap()
    );
    let exact_rooms: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM rooms
          WHERE workspace_id = $1
            AND kind = 'group'
            AND is_group_dm",
    )
    .bind(fixture.workspace.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(exact_rooms, 1, "rolling old/new birth must not duplicate");
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations"]
async fn concurrent_parent_delete_does_not_deadlock_with_member_delete() {
    let fixture = Fixture::new(3).await;
    let direct = fixture.direct(fixture.members[0], fixture.members[1]).await;

    let mut parent_delete = fixture.pool.begin().await.unwrap();
    sqlx::query("DELETE FROM rooms WHERE id = $1")
        .bind(direct.to_uuid())
        .execute(&mut *parent_delete)
        .await
        .expect("whole-room delete must pass the cascade exemption");

    let pool = fixture.pool.clone();
    let participant = fixture.members[0];
    let mut member_delete = tokio::spawn(async move {
        sqlx::query(
            "DELETE FROM room_members
              WHERE room_id = $1 AND participant_id = $2",
        )
        .bind(direct.to_uuid())
        .bind(participant.to_uuid())
        .execute(&pool)
        .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut member_delete)
            .await
            .is_err(),
        "the concurrent child statement should wait for the parent outcome"
    );
    parent_delete.commit().await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), member_delete)
        .await
        .expect("the child statement must finish without a deadlock")
        .unwrap()
        .unwrap();
    assert_eq!(result.rows_affected(), 0);
}
