use std::time::Duration;

use aero_common::{
    BarrierId, Block, Error, ParticipantId, RoomId, UserGroupId, WorkspaceId, WorkspaceRole,
};
use sqlx::PgPool;

use super::{MessageRepo, NewMessage};
use crate::{BarrierRepo, DmRepo, GroupDmRepo, UserGroupRepo, WorkspaceRepo};

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
    alice: ParticipantId,
    bob: ParticipantId,
    carol: ParticipantId,
    group_a: UserGroupId,
    group_b: UserGroupId,
}

impl Fixture {
    async fn create() -> Self {
        let pool = pool();
        let owner = participant(&pool, "barrier-send-owner").await;
        let alice = participant(&pool, "barrier-send-alice").await;
        let bob = participant(&pool, "barrier-send-bob").await;
        let carol = participant(&pool, "barrier-send-carol").await;
        let workspaces = WorkspaceRepo::new(pool.clone());
        let workspace = workspaces
            .create(
                format!("Barrier send {owner}"),
                format!("barrier-send-{owner}"),
                owner,
            )
            .await
            .unwrap()
            .id;
        for member in [alice, bob, carol] {
            workspaces
                .add_member(workspace, member, WorkspaceRole::Member)
                .await
                .unwrap();
        }

        let groups = UserGroupRepo::new(pool.clone());
        let group_a = groups
            .create_authorized(workspace, "barrier-a", "Barrier A", owner)
            .await
            .unwrap()
            .id;
        let group_b = groups
            .create_authorized(workspace, "barrier-b", "Barrier B", owner)
            .await
            .unwrap()
            .id;

        Self {
            pool,
            workspace,
            owner,
            alice,
            bob,
            carol,
            group_a,
            group_b,
        }
    }

    async fn add_group_member(&self, group: UserGroupId, participant: ParticipantId) {
        UserGroupRepo::new(self.pool.clone())
            .add_member_authorized(self.workspace, group, participant, self.owner)
            .await
            .unwrap();
    }

    async fn create_barrier(&self) -> BarrierId {
        BarrierRepo::new(self.pool.clone())
            .create_authorized(self.workspace, self.group_a, self.group_b, self.owner)
            .await
            .unwrap()
    }

    async fn direct_room(&self) -> RoomId {
        DmRepo::new(self.pool.clone())
            .find_or_create_in_workspace(self.workspace, self.alice, self.bob)
            .await
            .unwrap()
            .id
    }

    async fn group_dm(&self) -> RoomId {
        GroupDmRepo::new(self.pool.clone())
            .find_or_create_in_workspace(
                self.workspace,
                &[self.alice, self.bob, self.carol],
                self.alice,
            )
            .await
            .unwrap()
            .id
    }
}

fn new_message(room: RoomId, sender: ParticipantId, text: &str) -> NewMessage {
    NewMessage {
        room_id: room,
        sender_id: sender,
        blocks: vec![Block::text(text)],
        reply_to: None,
        metadata: serde_json::Value::Null,
        expires_at: None,
    }
}

async fn send(
    pool: &PgPool,
    room: RoomId,
    sender: ParticipantId,
    text: &str,
) -> aero_common::Result<aero_common::Message> {
    Ok(MessageRepo::new(pool.clone())
        .insert_outboxed(new_message(room, sender, text), None, Vec::new(), None)
        .await?
        .into_message())
}

async fn lock_workspace(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, workspace: WorkspaceId) {
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace.to_uuid())
        .fetch_one(&mut **tx)
        .await
        .unwrap();
}

async fn assert_waiting<T>(task: &tokio::task::JoinHandle<T>) {
    tokio::time::sleep(Duration::from_millis(75)).await;
    assert!(
        !task.is_finished(),
        "message send must wait behind the workspace policy fence"
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn barrier_added_after_direct_and_group_dm_blocks_send_and_edit() {
    let fixture = Fixture::create().await;
    let direct = fixture.direct_room().await;
    let group_dm = fixture.group_dm().await;
    let before = send(
        &fixture.pool,
        direct,
        fixture.alice,
        "committed before barrier",
    )
    .await
    .unwrap();

    fixture
        .add_group_member(fixture.group_a, fixture.alice)
        .await;
    fixture.add_group_member(fixture.group_b, fixture.bob).await;
    fixture.create_barrier().await;

    for room in [direct, group_dm] {
        for sender in [fixture.alice, fixture.bob] {
            let error = send(&fixture.pool, room, sender, "must not cross barrier")
                .await
                .expect_err("existing conversations must honor both barrier directions");
            assert!(matches!(error, Error::Forbidden(_)));
        }
    }

    let edit_error = MessageRepo::new(fixture.pool.clone())
        .edit_outboxed_authorized(
            before.id,
            fixture.alice,
            vec![Block::text("must not become new cross-barrier content")],
            before.version,
            true,
            None,
        )
        .await
        .expect_err("editing visible content is communication too");
    assert!(matches!(edit_error, Error::Forbidden(_)));

    let allowed = send(
        &fixture.pool,
        group_dm,
        fixture.carol,
        "an unsegmented sender remains allowed",
    )
    .await
    .unwrap();
    assert_eq!(allowed.sender_id, fixture.carol);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn committed_barrier_creation_wins_race_with_existing_dm_send() {
    let fixture = Fixture::create().await;
    let direct = fixture.direct_room().await;
    fixture
        .add_group_member(fixture.group_a, fixture.alice)
        .await;
    fixture.add_group_member(fixture.group_b, fixture.bob).await;

    let mut policy = fixture.pool.begin().await.unwrap();
    lock_workspace(&mut policy, fixture.workspace).await;
    sqlx::query(
        "INSERT INTO info_barriers (id, workspace_id, group_a, group_b, created_by)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(BarrierId::new().to_uuid())
    .bind(fixture.workspace.to_uuid())
    .bind(fixture.group_a.to_uuid())
    .bind(fixture.group_b.to_uuid())
    .bind(fixture.owner.to_uuid())
    .execute(&mut *policy)
    .await
    .unwrap();

    let pool = fixture.pool.clone();
    let alice = fixture.alice;
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let raced_send = tokio::spawn(async move {
        let _ = started_tx.send(());
        send(&pool, direct, alice, "must wait for barrier").await
    });
    started_rx.await.unwrap();
    assert_waiting(&raced_send).await;
    policy.commit().await.unwrap();

    let error = tokio::time::timeout(Duration::from_secs(3), raced_send)
        .await
        .expect("send unblocked after barrier commit")
        .unwrap()
        .expect_err("committed barrier must be re-read");
    assert!(matches!(error, Error::Forbidden(_)));
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn committed_group_membership_change_wins_race_with_existing_dm_send() {
    let fixture = Fixture::create().await;
    let direct = fixture.direct_room().await;
    fixture.add_group_member(fixture.group_b, fixture.bob).await;
    fixture.create_barrier().await;

    let mut group_change = fixture.pool.begin().await.unwrap();
    lock_workspace(&mut group_change, fixture.workspace).await;
    sqlx::query(
        "INSERT INTO user_group_members (group_id, participant_id)
         VALUES ($1, $2)",
    )
    .bind(fixture.group_a.to_uuid())
    .bind(fixture.alice.to_uuid())
    .execute(&mut *group_change)
    .await
    .unwrap();

    let pool = fixture.pool.clone();
    let alice = fixture.alice;
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let raced_send = tokio::spawn(async move {
        let _ = started_tx.send(());
        send(&pool, direct, alice, "must wait for group change").await
    });
    started_rx.await.unwrap();
    assert_waiting(&raced_send).await;
    group_change.commit().await.unwrap();

    let error = tokio::time::timeout(Duration::from_secs(3), raced_send)
        .await
        .expect("send unblocked after group membership commit")
        .unwrap()
        .expect_err("committed group membership must be re-read");
    assert!(matches!(error, Error::Forbidden(_)));
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn committed_barrier_delete_allows_waiting_existing_dm_send() {
    let fixture = Fixture::create().await;
    let direct = fixture.direct_room().await;
    fixture
        .add_group_member(fixture.group_a, fixture.alice)
        .await;
    fixture.add_group_member(fixture.group_b, fixture.bob).await;
    let barrier = fixture.create_barrier().await;

    let mut policy = fixture.pool.begin().await.unwrap();
    lock_workspace(&mut policy, fixture.workspace).await;
    sqlx::query("DELETE FROM info_barriers WHERE id = $1")
        .bind(barrier.to_uuid())
        .execute(&mut *policy)
        .await
        .unwrap();

    let pool = fixture.pool.clone();
    let alice = fixture.alice;
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let raced_send = tokio::spawn(async move {
        let _ = started_tx.send(());
        send(&pool, direct, alice, "allowed after barrier delete").await
    });
    started_rx.await.unwrap();
    assert_waiting(&raced_send).await;
    policy.commit().await.unwrap();

    let message = tokio::time::timeout(Duration::from_secs(3), raced_send)
        .await
        .expect("send unblocked after barrier delete")
        .unwrap()
        .expect("deleted barrier must no longer deny");
    assert_eq!(message.sender_id, fixture.alice);
}
