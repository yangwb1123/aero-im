use aero_common::{Error, RoomKind};

use super::{new_participant, pool, service};
use aero_storage::{ParticipantRepo, RoomRepo};

#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn channel_operations_and_generic_direct_creation_are_kind_contained() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let owner = new_participant(&participants, "kind-owner").await;
    let member = new_participant(&participants, "kind-member").await;
    let svc = service(pool.clone());

    assert!(matches!(
        svc.create_room(owner.id, RoomKind::Direct, None)
            .await
            .expect_err("generic direct creation is forbidden"),
        Error::Invalid(_)
    ));

    let group = svc
        .create_room(owner.id, RoomKind::Group, Some("ordinary group".into()))
        .await
        .unwrap();
    svc.add_member(owner.id, group.id, member.id).await.unwrap();
    for error in [
        svc.join_channel(owner.id, group.id)
            .await
            .expect_err("group cannot use channel join"),
        svc.leave_channel(owner.id, group.id)
            .await
            .expect_err("group cannot use channel leave"),
        svc.archive_channel(owner.id, group.id, true)
            .await
            .expect_err("group cannot use channel archive"),
        svc.set_channel_meta(
            owner.id,
            group.id,
            Some(Some("not a channel".into())),
            None,
            Some(false),
        )
        .await
        .expect_err("group cannot use channel metadata"),
        svc.set_room_post_policy(owner.id, group.id, "admins")
            .await
            .expect_err("group cannot use channel post policy"),
    ] {
        assert!(matches!(error, Error::Invalid(_)));
    }
    assert_eq!(
        RoomRepo::new(pool).room_kind(group.id).await.unwrap(),
        Some(RoomKind::Group)
    );
}
