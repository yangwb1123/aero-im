use super::*;
use aero_common::{RoomKind, WorkspaceRole};

struct Fixture {
    owner: ParticipantId,
    member: ParticipantId,
    outsider: ParticipantId,
    room: RoomId,
    other_room: RoomId,
    group: RoomId,
}

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect_lazy(&url)
        .expect("connect_lazy accepts the configured URL")
}

async fn participant(pool: &PgPool, label: &str) -> ParticipantId {
    let id = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(id.to_uuid())
        .bind(format!("channel-bookmark-{label}-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn fixture(pool: &PgPool) -> Fixture {
    let owner = participant(pool, "owner").await;
    let member = participant(pool, "member").await;
    let outsider = participant(pool, "outsider").await;
    let workspaces = crate::WorkspaceRepo::new(pool.clone());
    let workspace = workspaces
        .create(
            format!("Channel bookmark {owner}"),
            format!("channel-bookmark-{owner}"),
            owner,
        )
        .await
        .unwrap()
        .id;
    for participant in [member, outsider] {
        workspaces
            .add_member(workspace, participant, WorkspaceRole::Member)
            .await
            .unwrap();
    }
    let rooms = crate::RoomRepo::new(pool.clone());
    let room = rooms
        .create_in_workspace(
            workspace,
            RoomKind::Channel,
            Some("bookmark-primary".into()),
            owner,
        )
        .await
        .unwrap()
        .id;
    rooms.add_member(room, member).await.unwrap();
    let other_room = rooms
        .create_in_workspace(
            workspace,
            RoomKind::Channel,
            Some("bookmark-other".into()),
            owner,
        )
        .await
        .unwrap()
        .id;
    let group = rooms
        .create_in_workspace(
            workspace,
            RoomKind::Group,
            Some("bookmark-group".into()),
            owner,
        )
        .await
        .unwrap()
        .id;
    Fixture {
        owner,
        member,
        outsider,
        room,
        other_room,
        group,
    }
}

fn constraint(error: &sqlx::Error) -> Option<&str> {
    error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::constraint)
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn authorized_bookmark_crud_is_validated_and_path_scoped() {
    let pool = pool();
    let repo = ChannelBookmarkRepo::new(pool.clone());
    let fixture = fixture(&pool).await;
    let first = repo
        .add_channel_bookmark_authorized(
            fixture.room,
            fixture.member,
            "  Docs  ",
            "  https://docs.example  ",
            Some(" 📚 "),
            1,
        )
        .await
        .unwrap();
    let second = repo
        .add_channel_bookmark_authorized(
            fixture.room,
            fixture.owner,
            "Wiki",
            "https://wiki.example",
            None,
            0,
        )
        .await
        .unwrap();
    assert_eq!(first.title, "Docs");
    assert_eq!(first.url, "https://docs.example");
    assert_eq!(first.emoji.as_deref(), Some("📚"));
    let listed = repo
        .list_channel_bookmarks_authorized(fixture.room, fixture.member)
        .await
        .unwrap();
    assert_eq!(
        listed.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![second.id, first.id]
    );
    assert!(matches!(
        repo.get_channel_bookmark_authorized(fixture.other_room, first.id, fixture.owner)
            .await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        repo.update_channel_bookmark_authorized(
            fixture.other_room,
            first.id,
            fixture.owner,
            ChannelBookmarkPatch {
                title: Some("wrong room"),
                url: None,
                emoji: None,
                position: None,
            },
        )
        .await,
        Err(Error::NotFound(_))
    ));
    let updated = repo
        .update_channel_bookmark_authorized(
            fixture.room,
            first.id,
            fixture.member,
            ChannelBookmarkPatch {
                title: Some("Handbook"),
                url: None,
                emoji: Some(None),
                position: Some(7),
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.title, "Handbook");
    assert_eq!(updated.emoji, None);
    assert_eq!(updated.position, 7);
    assert!(matches!(
        repo.add_channel_bookmark_authorized(
            fixture.group,
            fixture.owner,
            "No",
            "https://no.example",
            None,
            0,
        )
        .await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        repo.add_channel_bookmark_authorized(
            fixture.room,
            fixture.outsider,
            "No",
            "https://no.example",
            None,
            0,
        )
        .await,
        Err(Error::Forbidden(_))
    ));
    assert!(matches!(
        repo.delete_channel_bookmark_authorized(fixture.other_room, first.id, fixture.owner,)
            .await,
        Err(Error::NotFound(_))
    ));
    repo.delete_channel_bookmark_authorized(fixture.room, first.id, fixture.member)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn raw_guards_freeze_identity_preserve_departed_history_and_room_cascade() {
    let pool = pool();
    let repo = ChannelBookmarkRepo::new(pool.clone());
    let fixture = fixture(&pool).await;
    let error = sqlx::query(
        "INSERT INTO channel_bookmarks
             (id, room_id, title, url, created_by)
         VALUES ($1, $2, 'forged', 'https://forged.example', $3)",
    )
    .bind(ChannelBookmarkId::new().to_uuid())
    .bind(fixture.room.to_uuid())
    .bind(fixture.outsider.to_uuid())
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        constraint(&error),
        Some("channel_bookmarks_creator_scope_chk")
    );
    let error = sqlx::query(
        "INSERT INTO channel_bookmarks
             (id, room_id, title, url, created_by)
         VALUES ($1, $2, 'wrong kind', 'https://wrong.example', $3)",
    )
    .bind(ChannelBookmarkId::new().to_uuid())
    .bind(fixture.group.to_uuid())
    .bind(fixture.owner.to_uuid())
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(constraint(&error), Some("channel_bookmarks_room_scope_chk"));

    let bookmark = repo
        .add_channel_bookmark_authorized(
            fixture.room,
            fixture.member,
            "History",
            "https://history.example",
            None,
            0,
        )
        .await
        .unwrap();
    let error = sqlx::query("UPDATE channel_bookmarks SET room_id = $2 WHERE id = $1")
        .bind(bookmark.id.to_uuid())
        .bind(fixture.other_room.to_uuid())
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(
        constraint(&error),
        Some("channel_bookmarks_identity_immutable_chk")
    );

    sqlx::query("DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
        .bind(fixture.room.to_uuid())
        .bind(fixture.member.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE channel_bookmarks SET title = 'retained' WHERE id = $1")
        .bind(bookmark.id.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        repo.get_channel_bookmark_authorized(fixture.room, bookmark.id, fixture.member)
            .await,
        Err(Error::Forbidden(_))
    ));
    assert_eq!(
        repo.get_channel_bookmark_authorized(fixture.room, bookmark.id, fixture.owner)
            .await
            .unwrap()
            .title,
        "retained"
    );

    let cascade = repo
        .add_channel_bookmark_authorized(
            fixture.other_room,
            fixture.owner,
            "Cascade",
            "https://cascade.example",
            None,
            0,
        )
        .await
        .unwrap();
    sqlx::query("DELETE FROM rooms WHERE id = $1")
        .bind(fixture.other_room.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM channel_bookmarks WHERE id = $1")
            .bind(cascade.id.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn concurrent_revocation_causes_zero_bookmark_write_or_delete() {
    let pool = pool();
    let repo = ChannelBookmarkRepo::new(pool.clone());
    let fixture = fixture(&pool).await;
    let bookmark = repo
        .add_channel_bookmark_authorized(
            fixture.room,
            fixture.member,
            "Before",
            "https://before.example",
            None,
            0,
        )
        .await
        .unwrap();

    let mut revoke = pool.begin().await.unwrap();
    sqlx::query("DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
        .bind(fixture.room.to_uuid())
        .bind(fixture.member.to_uuid())
        .execute(&mut *revoke)
        .await
        .unwrap();
    let room = fixture.room;
    let member = fixture.member;
    let bookmark_id = bookmark.id;
    let update_repo = repo.clone();
    let update = tokio::spawn(async move {
        update_repo
            .update_channel_bookmark_authorized(
                room,
                bookmark_id,
                member,
                ChannelBookmarkPatch {
                    title: Some("After"),
                    url: None,
                    emoji: None,
                    position: None,
                },
            )
            .await
    });
    let delete_repo = repo.clone();
    let delete = tokio::spawn(async move {
        delete_repo
            .delete_channel_bookmark_authorized(room, bookmark_id, member)
            .await
    });
    tokio::task::yield_now().await;
    revoke.commit().await.unwrap();

    assert!(matches!(update.await.unwrap(), Err(Error::Forbidden(_))));
    assert!(matches!(delete.await.unwrap(), Err(Error::Forbidden(_))));
    assert_eq!(
        repo.get_channel_bookmark_authorized(fixture.room, bookmark.id, fixture.owner)
            .await
            .unwrap()
            .title,
        "Before"
    );
}
