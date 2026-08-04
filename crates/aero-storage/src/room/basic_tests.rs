use super::*;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

async fn fixture(p: &PgPool) -> (WorkspaceId, ParticipantId) {
    let actor = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(actor.to_uuid())
        .bind(format!("chan-actor-{actor}"))
        .execute(p)
        .await
        .expect("insert participant");
    let ws = crate::WorkspaceRepo::new(p.clone())
        .create("Channel Test WS".into(), format!("chan-{actor}"), actor)
        .await
        .expect("insert workspace")
        .id;
    (ws, actor)
}

async fn insert_channel(
    p: &PgPool,
    workspace: WorkspaceId,
    creator: ParticipantId,
    is_private: bool,
) -> RoomId {
    insert_room_of_kind(p, workspace, creator, "channel", is_private).await
}

async fn insert_room_of_kind(
    p: &PgPool,
    workspace: WorkspaceId,
    creator: ParticipantId,
    kind: &str,
    is_private: bool,
) -> RoomId {
    if kind == "channel" {
        let room = RoomRepo::new(p.clone())
            .create_in_workspace(
                workspace,
                RoomKind::Channel,
                Some(format!("room-{kind}-{}", RoomId::new())),
                creator,
            )
            .await
            .expect("insert channel");
        RoomRepo::new(p.clone())
            .set_visibility(room.id, is_private)
            .await
            .expect("set channel visibility");
        return room.id;
    }

    let id = RoomId::new();
    sqlx::query(
        r"INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id, is_private)
           VALUES ($1, $2, $3, $4, now(), $5, $6)",
    )
    .bind(id.to_uuid())
    .bind(kind)
    .bind(format!("room-{kind}-{id}"))
    .bind(creator.to_uuid())
    .bind(workspace.to_uuid())
    .bind(is_private)
    .execute(p)
    .await
    .expect("insert channel");
    id
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn channel_visibility_and_archive_roundtrip() {
    let p = pool();
    let repo = RoomRepo::new(p.clone());
    let (ws, actor) = fixture(&p).await;
    let room = insert_channel(&p, ws, actor, true).await;

    assert_eq!(repo.is_private(room).await.unwrap(), Some(true));
    assert_eq!(repo.is_archived(room).await.unwrap(), Some(false));

    repo.set_visibility(room, false).await.unwrap();
    assert_eq!(repo.is_private(room).await.unwrap(), Some(false));

    repo.set_archived(room, true).await.unwrap();
    assert_eq!(repo.is_archived(room).await.unwrap(), Some(true));

    assert_eq!(repo.is_private(RoomId::new()).await.unwrap(), None);
    assert_eq!(repo.is_archived(RoomId::new()).await.unwrap(), None);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn channel_topic_and_description_roundtrip() {
    let p = pool();
    let repo = RoomRepo::new(p.clone());
    let (ws, actor) = fixture(&p).await;
    let room = insert_channel(&p, ws, actor, false).await;

    repo.set_topic(room, Some("daily standup")).await.unwrap();
    repo.set_description(room, Some("the team channel"))
        .await
        .unwrap();
    let (topic, desc) = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        r"SELECT topic, description FROM rooms WHERE id = $1",
    )
    .bind(room.to_uuid())
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(topic.as_deref(), Some("daily standup"));
    assert_eq!(desc.as_deref(), Some("the team channel"));

    // Clearing sets them back to NULL.
    repo.set_topic(room, None).await.unwrap();
    let (topic, _) = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        r"SELECT topic, description FROM rooms WHERE id = $1",
    )
    .bind(room.to_uuid())
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(topic, None);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn channel_list_public_excludes_private_and_archived() {
    let p = pool();
    let repo = RoomRepo::new(p.clone());
    let (ws, actor) = fixture(&p).await;

    let public = insert_channel(&p, ws, actor, false).await;
    let private = insert_channel(&p, ws, actor, true).await;
    let archived = insert_channel(&p, ws, actor, false).await;
    let public_group = insert_room_of_kind(&p, ws, actor, "group", false).await;
    repo.set_archived(archived, true).await.unwrap();

    let listed = repo.list_public_channels(ws, None).await.unwrap();
    let ids: Vec<RoomId> = listed.iter().map(|r| r.id).collect();
    assert!(ids.contains(&public), "public channel is discoverable");
    assert!(!ids.contains(&private), "private channel is hidden");
    assert!(!ids.contains(&archived), "archived channel is hidden");
    assert!(
        !ids.contains(&public_group),
        "a public-looking group is never a channel discovery result"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn channel_join_then_leave_roundtrip() {
    let p = pool();
    let repo = RoomRepo::new(p.clone());
    let (ws, actor) = fixture(&p).await;
    let room = insert_channel(&p, ws, actor, false).await;

    let joiner = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(joiner.to_uuid())
        .bind(format!("joiner-{joiner}"))
        .execute(&p)
        .await
        .unwrap();

    assert!(!repo.is_member(room, joiner).await.unwrap());
    repo.add_member(room, joiner).await.unwrap();
    assert!(repo.is_member(room, joiner).await.unwrap());
    // Leave is idempotent: a second remove is a harmless no-op.
    repo.remove_member(room, joiner).await.unwrap();
    assert!(!repo.is_member(room, joiner).await.unwrap());
    repo.remove_member(room, joiner).await.unwrap();
    assert!(!repo.is_member(room, joiner).await.unwrap());
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn channels_not_member_excludes_joined_private_and_archived() {
    let p = pool();
    let repo = RoomRepo::new(p.clone());
    let (ws, actor) = fixture(&p).await;

    // A public channel the caller is NOT in (the expected candidate),
    // a public channel the caller HAS joined (excluded by the anti-join),
    // a private channel (excluded by discovery boundary),
    // an archived channel (excluded by discovery boundary).
    let candidate = insert_channel(&p, ws, actor, false).await;
    let joined = insert_channel(&p, ws, actor, false).await;
    let private = insert_channel(&p, ws, actor, true).await;
    let archived = insert_channel(&p, ws, actor, false).await;
    let public_group = insert_room_of_kind(&p, ws, actor, "group", false).await;
    repo.set_archived(archived, true).await.unwrap();

    let caller = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(caller.to_uuid())
        .bind(format!("caller-{caller}"))
        .execute(&p)
        .await
        .unwrap();
    repo.add_member(joined, caller).await.unwrap();

    let listed = repo
        .list_workspace_channels_not_member(ws, caller, 30)
        .await
        .unwrap();
    let ids: Vec<RoomId> = listed.iter().map(|(r, _)| r.id).collect();
    assert!(
        ids.contains(&candidate),
        "non-member public channel is a candidate"
    );
    assert!(
        !ids.contains(&joined),
        "channel the caller is in is excluded"
    );
    assert!(!ids.contains(&private), "private channel is excluded");
    assert!(!ids.contains(&archived), "archived channel is excluded");
    assert!(
        !ids.contains(&public_group),
        "a public-looking group is excluded"
    );
    // Each candidate carries a non-negative activity count.
    for (_, activity) in &listed {
        assert!(*activity >= 0);
    }
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn shared_room_counts_excludes_caller_and_counts_overlap() {
    let p = pool();
    let repo = RoomRepo::new(p.clone());
    let (ws, actor) = fixture(&p).await;

    let caller = ParticipantId::new();
    let buddy = ParticipantId::new();
    for (id, name) in [(caller, "caller"), (buddy, "buddy")] {
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("{name}-{id}"))
            .execute(&p)
            .await
            .unwrap();
    }

    // Two channels both the caller and buddy belong to → shared count 2.
    let c1 = insert_channel(&p, ws, actor, false).await;
    let c2 = insert_channel(&p, ws, actor, false).await;
    for room in [c1, c2] {
        repo.add_member(room, caller).await.unwrap();
        repo.add_member(room, buddy).await.unwrap();
    }

    let counts = repo
        .shared_room_counts_in_workspace(ws, caller)
        .await
        .unwrap();
    // The caller never appears as a candidate for following themselves.
    assert!(!counts.iter().any(|(p, _)| *p == caller), "caller excluded");
    let buddy_count = counts.iter().find(|(p, _)| *p == buddy).map(|(_, n)| *n);
    assert_eq!(buddy_count, Some(2), "buddy shares both channels");
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn post_policy_roundtrip_defaults_to_everyone() {
    let p = pool();
    let repo = RoomRepo::new(p.clone());
    let (ws, actor) = fixture(&p).await;
    let room = insert_channel(&p, ws, actor, false).await;

    // A fresh row carries the NOT NULL DEFAULT 'everyone' (migration 0030).
    assert_eq!(repo.post_policy(room).await.unwrap(), "everyone");
    // The creator is recoverable for the post-policy guard.
    assert_eq!(repo.created_by(room).await.unwrap(), Some(actor));

    repo.set_post_policy(room, "admins").await.unwrap();
    assert_eq!(repo.post_policy(room).await.unwrap(), "admins");

    repo.set_post_policy(room, "everyone").await.unwrap();
    assert_eq!(repo.post_policy(room).await.unwrap(), "everyone");

    // Unknown room ⇒ open default, never an error (fail-open on read).
    assert_eq!(repo.post_policy(RoomId::new()).await.unwrap(), "everyone");
    assert_eq!(repo.created_by(RoomId::new()).await.unwrap(), None);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn delivery_members_enforces_workspace_deactivation_and_mandatory_2fa() {
    let p = pool();
    let repo = RoomRepo::new(p.clone());
    let (ws, actor) = fixture(&p).await;
    let room = insert_room_of_kind(&p, ws, actor, "group", false).await;
    let activated = ParticipantId::new();
    let unenrolled = ParticipantId::new();
    let deactivated = ParticipantId::new();
    let removed = ParticipantId::new();
    let deleted = ParticipantId::new();
    for participant in [activated, unenrolled, deactivated, removed, deleted] {
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(participant.to_uuid())
            .bind(format!("delivery-member-{participant}"))
            .execute(&p)
            .await
            .unwrap();
        repo.add_member(room, participant).await.unwrap();
    }
    for participant in [activated, unenrolled, deactivated, deleted] {
        sqlx::query(
            r"INSERT INTO workspace_members
                  (workspace_id, participant_id, role, joined_at)
               VALUES ($1, $2, 'member', now())",
        )
        .bind(ws.to_uuid())
        .bind(participant.to_uuid())
        .execute(&p)
        .await
        .unwrap();
    }
    sqlx::query(
        r"INSERT INTO workspace_deactivations
              (workspace_id, participant_id, deactivated_by)
           VALUES ($1, $2, $3)",
    )
    .bind(ws.to_uuid())
    .bind(deactivated.to_uuid())
    .bind(actor.to_uuid())
    .execute(&p)
    .await
    .unwrap();
    sqlx::query("UPDATE participants SET deleted_at = now() WHERE id = $1")
        .bind(deleted.to_uuid())
        .execute(&p)
        .await
        .unwrap();
    sqlx::query(
        r"INSERT INTO totp_secrets
              (participant_id, secret, activated, activated_at)
           VALUES ($1, 'test-secret', true, now())",
    )
    .bind(activated.to_uuid())
    .execute(&p)
    .await
    .unwrap();

    let unrestricted = repo.delivery_members(room).await.unwrap();
    assert!(unrestricted.contains(&activated));
    assert!(unrestricted.contains(&unenrolled));
    assert!(!unrestricted.contains(&deactivated));
    assert!(!unrestricted.contains(&removed));
    assert!(!unrestricted.contains(&deleted));

    sqlx::query(
        r"INSERT INTO totp_secrets
              (participant_id, secret, activated, activated_at)
           VALUES ($1, 'delivery-workspace-owner', true, now())",
    )
    .bind(actor.to_uuid())
    .execute(&p)
    .await
    .unwrap();
    sqlx::query("UPDATE workspaces SET require_2fa = true WHERE id = $1")
        .bind(ws.to_uuid())
        .execute(&p)
        .await
        .unwrap();
    assert_eq!(repo.delivery_members(room).await.unwrap(), vec![activated]);
}
