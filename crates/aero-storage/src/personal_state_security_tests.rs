use std::time::Duration;

use aero_common::{
    Block, Error, MessageId, ParticipantId, RoomId, RoomKind, WorkspaceId, WorkspaceRole,
};
use sqlx::PgPool;

use crate::{
    ChannelFavoriteRepo, DraftRepo, NotificationPrefsRepo, RoomRepo, ThreadMuteRepo,
    ThreadNotificationPrefsRepo, ThreadReadStateRepo, ThreadSubscriptionRepo, WorkspaceRepo,
};

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(12)
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

async fn message(
    pool: &PgPool,
    room: RoomId,
    sender: ParticipantId,
    reply_to: Option<MessageId>,
    deleted: bool,
) -> MessageId {
    let id = MessageId::new();
    sqlx::query(
        r"INSERT INTO messages
              (id, room_id, sender_id, blocks, reply_to, searchable_text,
               deleted_at)
           VALUES ($1, $2, $3, '[]'::jsonb, $4, 'personal-state-test',
                   CASE WHEN $5 THEN now() ELSE NULL END)",
    )
    .bind(id.to_uuid())
    .bind(room.to_uuid())
    .bind(sender.to_uuid())
    .bind(reply_to.map(|root| root.to_uuid()))
    .bind(deleted)
    .execute(pool)
    .await
    .unwrap();
    id
}

struct Fixture {
    pool: PgPool,
    workspace: WorkspaceId,
    other_workspace: WorkspaceId,
    room: RoomId,
    other_room: RoomId,
    root: MessageId,
    reply: MessageId,
    deleted_root: MessageId,
    other_root: MessageId,
    owner: ParticipantId,
    actor: ParticipantId,
    outsider: ParticipantId,
}

impl Fixture {
    async fn create() -> Self {
        let pool = pool();
        let workspaces = WorkspaceRepo::new(pool.clone());
        let rooms = RoomRepo::new(pool.clone());
        let owner = participant(&pool, "personal-state-owner").await;
        let actor = participant(&pool, "personal-state-actor").await;
        let outsider = participant(&pool, "personal-state-outsider").await;
        let workspace = workspaces
            .create(
                format!("Personal state {owner}"),
                format!("personal-state-{owner}"),
                owner,
            )
            .await
            .unwrap()
            .id;
        workspaces
            .add_member(workspace, actor, WorkspaceRole::Member)
            .await
            .unwrap();
        let room = rooms
            .create_in_workspace(
                workspace,
                RoomKind::Channel,
                Some(format!("personal-state-{owner}")),
                owner,
            )
            .await
            .unwrap()
            .id;
        rooms.add_member(room, actor).await.unwrap();

        let other_workspace = workspaces
            .create(
                format!("Personal state other {outsider}"),
                format!("personal-state-other-{outsider}"),
                outsider,
            )
            .await
            .unwrap()
            .id;
        let other_room = rooms
            .create_in_workspace(
                other_workspace,
                RoomKind::Channel,
                Some(format!("personal-state-other-{outsider}")),
                outsider,
            )
            .await
            .unwrap()
            .id;

        let root_message = message(&pool, room, owner, None, false).await;
        let reply = message(&pool, room, owner, Some(root_message), false).await;
        let deleted_root = message(&pool, room, owner, None, true).await;
        let other_root_message = message(&pool, other_room, outsider, None, false).await;
        Self {
            pool,
            workspace,
            other_workspace,
            room,
            other_room,
            root: root_message,
            reply,
            deleted_root,
            other_root: other_root_message,
            owner,
            actor,
            outsider,
        }
    }

    async fn cleanup(&self) {
        sqlx::query("DELETE FROM workspaces WHERE id = ANY($1)")
            .bind(vec![
                self.workspace.to_uuid(),
                self.other_workspace.to_uuid(),
            ])
            .execute(&self.pool)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
            .bind(vec![
                self.owner.to_uuid(),
                self.actor.to_uuid(),
                self.outsider.to_uuid(),
            ])
            .execute(&self.pool)
            .await
            .ok();
    }
}

fn constraint(error: &sqlx::Error) -> Option<&str> {
    error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::constraint)
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn personal_state_authorized_roundtrip_and_lists_filter_deactivation() {
    let fixture = Fixture::create().await;
    let drafts = DraftRepo::new(fixture.pool.clone());
    let favorites = ChannelFavoriteRepo::new(fixture.pool.clone());
    let notifications = NotificationPrefsRepo::new(fixture.pool.clone());
    let subscriptions = ThreadSubscriptionRepo::new(fixture.pool.clone());
    let thread_notifications = ThreadNotificationPrefsRepo::new(fixture.pool.clone());
    let thread_mutes = ThreadMuteRepo::new(fixture.pool.clone());
    let reads = ThreadReadStateRepo::new(fixture.pool.clone());

    drafts
        .upsert_authorized(
            fixture.actor,
            fixture.room,
            &[Block::text("private draft")],
            Some(fixture.root),
        )
        .await
        .unwrap();
    favorites
        .add_authorized(fixture.actor, fixture.room)
        .await
        .unwrap();
    notifications
        .mute_authorized(fixture.actor, fixture.room)
        .await
        .unwrap();
    notifications
        .set_level_authorized(fixture.actor, fixture.room, "mentions")
        .await
        .unwrap();
    notifications
        .set_dnd(fixture.actor, Some(60), Some(120))
        .await
        .unwrap();
    subscriptions
        .subscribe_authorized(fixture.actor, fixture.root)
        .await
        .unwrap();
    thread_notifications
        .set_level_authorized(fixture.actor, fixture.root, "none")
        .await
        .unwrap();
    thread_mutes
        .mute_authorized(fixture.actor, fixture.root)
        .await
        .unwrap();

    assert!(drafts
        .get_authorized(fixture.actor, fixture.room)
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        notifications
            .room_preference_authorized(fixture.actor, fixture.room)
            .await
            .unwrap(),
        (Some("mentions".to_owned()), true)
    );
    assert_eq!(
        thread_notifications
            .get_level_authorized(fixture.actor, fixture.root)
            .await
            .unwrap(),
        "none"
    );
    assert!(subscriptions
        .followed_by_accessible(fixture.actor)
        .await
        .unwrap()
        .contains(&fixture.root));
    assert_eq!(
        reads
            .unread_count_authorized(fixture.actor, fixture.root)
            .await
            .unwrap(),
        1
    );
    reads
        .mark_read_authorized(fixture.actor, fixture.root)
        .await
        .unwrap();
    assert_eq!(
        reads
            .unread_count_authorized(fixture.actor, fixture.root)
            .await
            .unwrap(),
        0
    );

    sqlx::query(
        r"INSERT INTO workspace_deactivations
              (workspace_id, participant_id, deactivated_by)
           VALUES ($1, $2, $3)",
    )
    .bind(fixture.workspace.to_uuid())
    .bind(fixture.actor.to_uuid())
    .bind(fixture.owner.to_uuid())
    .execute(&fixture.pool)
    .await
    .unwrap();

    assert!(drafts
        .list_accessible(fixture.actor)
        .await
        .unwrap()
        .is_empty());
    assert!(favorites
        .list_accessible(fixture.actor)
        .await
        .unwrap()
        .is_empty());
    assert!(notifications
        .muted_rooms_accessible(fixture.actor)
        .await
        .unwrap()
        .is_empty());
    assert!(subscriptions
        .followed_by_accessible(fixture.actor)
        .await
        .unwrap()
        .is_empty());
    assert!(thread_notifications
        .level_map_accessible(fixture.actor, &[fixture.root])
        .await
        .unwrap()
        .is_empty());
    assert!(reads
        .unread_counts_accessible(fixture.actor, &[fixture.root])
        .await
        .unwrap()
        .is_empty());

    let retained: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM message_drafts
          WHERE participant_id = $1 AND room_id = $2",
    )
    .bind(fixture.actor.to_uuid())
    .bind(fixture.room.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(
        retained, 1,
        "temporary deactivation filters without data loss"
    );
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn personal_state_cross_room_reply_and_deleted_root_writes_fail_closed() {
    let fixture = Fixture::create().await;
    let drafts = DraftRepo::new(fixture.pool.clone());
    let favorites = ChannelFavoriteRepo::new(fixture.pool.clone());
    let notifications = NotificationPrefsRepo::new(fixture.pool.clone());
    let subscriptions = ThreadSubscriptionRepo::new(fixture.pool.clone());
    let thread_notifications = ThreadNotificationPrefsRepo::new(fixture.pool.clone());
    let thread_mutes = ThreadMuteRepo::new(fixture.pool.clone());
    let reads = ThreadReadStateRepo::new(fixture.pool.clone());

    assert!(matches!(
        drafts
            .upsert_authorized(
                fixture.actor,
                fixture.other_room,
                &[Block::text("forged")],
                None,
            )
            .await,
        Err(Error::Forbidden(_))
    ));
    assert!(matches!(
        drafts
            .get_authorized(fixture.actor, fixture.other_room)
            .await,
        Err(Error::Forbidden(_))
    ));
    assert!(matches!(
        drafts
            .delete_authorized(fixture.actor, fixture.other_room)
            .await,
        Err(Error::Forbidden(_))
    ));
    assert!(matches!(
        favorites
            .add_authorized(fixture.actor, fixture.other_room)
            .await,
        Err(Error::Forbidden(_))
    ));
    assert!(matches!(
        favorites
            .remove_authorized(fixture.actor, fixture.other_room)
            .await,
        Err(Error::Forbidden(_))
    ));
    assert!(matches!(
        notifications
            .mute_authorized(fixture.actor, fixture.other_room)
            .await,
        Err(Error::Forbidden(_))
    ));
    assert!(matches!(
        notifications
            .set_level_authorized(fixture.actor, fixture.other_room, "none")
            .await,
        Err(Error::Forbidden(_))
    ));

    assert!(matches!(
        subscriptions
            .subscribe_authorized(fixture.actor, fixture.other_root)
            .await,
        Err(Error::Forbidden(_))
    ));
    assert!(matches!(
        subscriptions
            .unsubscribe_authorized(fixture.actor, fixture.other_root)
            .await,
        Err(Error::Forbidden(_))
    ));
    for invalid_root in [fixture.reply, fixture.deleted_root] {
        assert!(matches!(
            subscriptions
                .subscribe_authorized(fixture.actor, invalid_root)
                .await,
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            thread_notifications
                .set_level_authorized(fixture.actor, invalid_root, "none")
                .await,
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            thread_mutes
                .mute_authorized(fixture.actor, invalid_root)
                .await,
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            reads
                .mark_read_authorized(fixture.actor, invalid_root)
                .await,
            Err(Error::NotFound(_))
        ));
    }
    assert!(matches!(
        drafts
            .upsert_authorized(
                fixture.actor,
                fixture.room,
                &[Block::text("stale reply")],
                Some(fixture.deleted_root),
            )
            .await,
        Err(Error::Invalid(_))
    ));

    let writes: i64 = sqlx::query_scalar(
        r"SELECT
              (SELECT COUNT(*) FROM message_drafts WHERE participant_id = $1)
            + (SELECT COUNT(*) FROM channel_favorites WHERE participant_id = $1)
            + (SELECT COUNT(*) FROM channel_mutes WHERE participant_id = $1)
            + (SELECT COUNT(*) FROM channel_notification_prefs WHERE participant_id = $1)
            + (SELECT COUNT(*) FROM thread_subscriptions WHERE participant_id = $1)
            + (SELECT COUNT(*) FROM thread_mutes WHERE participant_id = $1)
            + (SELECT COUNT(*) FROM thread_notification_prefs WHERE participant_id = $1)
            + (SELECT COUNT(*) FROM thread_read_state WHERE participant_id = $1)",
    )
    .bind(fixture.actor.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(writes, 0);
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn personal_state_revocation_wins_waiting_room_and_thread_writes() {
    let fixture = Fixture::create().await;
    let mut revocation = fixture.pool.begin().await.unwrap();
    crate::ownership::lock_membership_governance(&mut revocation)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(fixture.workspace.to_uuid())
        .execute(&mut *revocation)
        .await
        .unwrap();
    sqlx::query(
        "DELETE FROM workspace_members
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(fixture.workspace.to_uuid())
    .bind(fixture.actor.to_uuid())
    .execute(&mut *revocation)
    .await
    .unwrap();

    let favorite_repo = ChannelFavoriteRepo::new(fixture.pool.clone());
    let favorite_room = fixture.room;
    let actor = fixture.actor;
    let mut favorite =
        tokio::spawn(async move { favorite_repo.add_authorized(actor, favorite_room).await });
    let subscription_repo = ThreadSubscriptionRepo::new(fixture.pool.clone());
    let thread_root = fixture.root;
    let mut follow = tokio::spawn(async move {
        subscription_repo
            .subscribe_authorized(actor, thread_root)
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut favorite)
            .await
            .is_err(),
        "room-state write waits behind workspace revocation"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut follow)
            .await
            .is_err(),
        "thread-state write waits behind workspace revocation"
    );

    revocation.commit().await.unwrap();
    assert!(matches!(favorite.await.unwrap(), Err(Error::Forbidden(_))));
    assert!(matches!(follow.await.unwrap(), Err(Error::Forbidden(_))));
    let rows: i64 = sqlx::query_scalar(
        r"SELECT
              (SELECT COUNT(*) FROM channel_favorites
                WHERE participant_id = $1 AND room_id = $2)
            + (SELECT COUNT(*) FROM thread_subscriptions
                WHERE participant_id = $1 AND root_message_id = $3)",
    )
    .bind(fixture.actor.to_uuid())
    .bind(fixture.room.to_uuid())
    .bind(fixture.root.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(rows, 0);
    fixture.cleanup().await;
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn personal_state_raw_guards_and_leave_tombstone_cleanup_hold() {
    let fixture = Fixture::create().await;
    let forged_room = sqlx::query(
        "INSERT INTO channel_favorites (participant_id, room_id)
         VALUES ($1, $2)",
    )
    .bind(fixture.outsider.to_uuid())
    .bind(fixture.room.to_uuid())
    .execute(&fixture.pool)
    .await
    .expect_err("nonmember room state must fail");
    assert_eq!(
        constraint(&forged_room),
        Some("channel_favorites_participant_scope_chk")
    );

    let forged_thread = sqlx::query(
        "INSERT INTO thread_subscriptions (participant_id, root_message_id)
         VALUES ($1, $2)",
    )
    .bind(fixture.outsider.to_uuid())
    .bind(fixture.root.to_uuid())
    .execute(&fixture.pool)
    .await
    .expect_err("nonmember thread state must fail");
    assert_eq!(
        constraint(&forged_thread),
        Some("thread_subscriptions_participant_scope_chk")
    );

    for invalid_root in [fixture.reply, fixture.deleted_root] {
        let invalid = sqlx::query(
            "INSERT INTO thread_mutes (participant_id, root_message_id)
             VALUES ($1, $2)",
        )
        .bind(fixture.actor.to_uuid())
        .bind(invalid_root.to_uuid())
        .execute(&fixture.pool)
        .await
        .expect_err("reply/deleted root must fail");
        assert_eq!(constraint(&invalid), Some("thread_mutes_root_scope_chk"));
    }

    sqlx::query(
        "INSERT INTO dnd_settings (participant_id, start_minute, end_minute)
         VALUES ($1, 60, 120)",
    )
    .bind(fixture.actor.to_uuid())
    .execute(&fixture.pool)
    .await
    .unwrap();
    let moved_dnd = sqlx::query(
        "UPDATE dnd_settings SET participant_id = $2
          WHERE participant_id = $1",
    )
    .bind(fixture.actor.to_uuid())
    .bind(fixture.owner.to_uuid())
    .execute(&fixture.pool)
    .await
    .expect_err("DND owner identity must not move");
    assert_eq!(
        constraint(&moved_dnd),
        Some("dnd_settings_identity_immutable_chk")
    );

    let drafts = DraftRepo::new(fixture.pool.clone());
    let favorites = ChannelFavoriteRepo::new(fixture.pool.clone());
    let notifications = NotificationPrefsRepo::new(fixture.pool.clone());
    let subscriptions = ThreadSubscriptionRepo::new(fixture.pool.clone());
    let thread_notifications = ThreadNotificationPrefsRepo::new(fixture.pool.clone());
    let thread_mutes = ThreadMuteRepo::new(fixture.pool.clone());
    let reads = ThreadReadStateRepo::new(fixture.pool.clone());
    drafts
        .upsert_authorized(
            fixture.actor,
            fixture.room,
            &[Block::text("survives parent tombstone")],
            Some(fixture.root),
        )
        .await
        .unwrap();
    favorites
        .add_authorized(fixture.actor, fixture.room)
        .await
        .unwrap();
    notifications
        .mute_authorized(fixture.actor, fixture.room)
        .await
        .unwrap();
    notifications
        .set_level_authorized(fixture.actor, fixture.room, "mentions")
        .await
        .unwrap();
    subscriptions
        .subscribe_authorized(fixture.actor, fixture.root)
        .await
        .unwrap();
    thread_notifications
        .set_level_authorized(fixture.actor, fixture.root, "none")
        .await
        .unwrap();
    thread_mutes
        .mute_authorized(fixture.actor, fixture.root)
        .await
        .unwrap();
    reads
        .mark_read_authorized(fixture.actor, fixture.root)
        .await
        .unwrap();

    let moved_favorite = sqlx::query(
        "UPDATE channel_favorites SET participant_id = $3
          WHERE participant_id = $1 AND room_id = $2",
    )
    .bind(fixture.actor.to_uuid())
    .bind(fixture.room.to_uuid())
    .bind(fixture.owner.to_uuid())
    .execute(&fixture.pool)
    .await
    .expect_err("favorite identity must not move");
    assert_eq!(
        constraint(&moved_favorite),
        Some("channel_favorites_identity_immutable_chk")
    );

    sqlx::query("UPDATE messages SET deleted_at = now() WHERE id = $1")
        .bind(fixture.root.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();
    let routed_rows: i64 = sqlx::query_scalar(
        r"SELECT
              (SELECT COUNT(*) FROM thread_subscriptions
                WHERE root_message_id = $1)
            + (SELECT COUNT(*) FROM thread_mutes
                WHERE root_message_id = $1)
            + (SELECT COUNT(*) FROM thread_notification_prefs
                WHERE root_message_id = $1)",
    )
    .bind(fixture.root.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(routed_rows, 0);
    let reply_to: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT reply_to FROM message_drafts
          WHERE participant_id = $1 AND room_id = $2",
    )
    .bind(fixture.actor.to_uuid())
    .bind(fixture.room.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(reply_to, None);
    let historical_cursor: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM thread_read_state
          WHERE participant_id = $1 AND root_message_id = $2",
    )
    .bind(fixture.actor.to_uuid())
    .bind(fixture.root.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(historical_cursor, 1);

    sqlx::query(
        "DELETE FROM room_members
          WHERE room_id = $1 AND participant_id = $2",
    )
    .bind(fixture.room.to_uuid())
    .bind(fixture.actor.to_uuid())
    .execute(&fixture.pool)
    .await
    .unwrap();
    let remaining: i64 = sqlx::query_scalar(
        r"SELECT
              (SELECT COUNT(*) FROM message_drafts WHERE participant_id = $1)
            + (SELECT COUNT(*) FROM channel_favorites WHERE participant_id = $1)
            + (SELECT COUNT(*) FROM channel_mutes WHERE participant_id = $1)
            + (SELECT COUNT(*) FROM channel_notification_prefs WHERE participant_id = $1)
            + (SELECT COUNT(*) FROM thread_subscriptions WHERE participant_id = $1)
            + (SELECT COUNT(*) FROM thread_mutes WHERE participant_id = $1)
            + (SELECT COUNT(*) FROM thread_notification_prefs WHERE participant_id = $1)
            + (SELECT COUNT(*) FROM thread_read_state WHERE participant_id = $1)",
    )
    .bind(fixture.actor.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(remaining, 0);
    fixture.cleanup().await;
}
