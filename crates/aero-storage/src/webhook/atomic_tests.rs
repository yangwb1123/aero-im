use aero_common::{ParticipantId, RoomId, WebhookId, WorkspaceId};
use sqlx::postgres::PgConnectOptions;
use sqlx::PgPool;

use super::{WebhookRepo, WebhookWriteError};
use crate::webhook::hash_token;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

async fn tagged_pool(application_name: &str) -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    let options = url
        .parse::<PgConnectOptions>()
        .expect("valid DATABASE_URL")
        .application_name(application_name);
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .expect("connect tagged webhook test pool")
}

async fn wait_until_tagged_query_waits_on_lock(pool: &PgPool, application_name: &str) {
    for _ in 0..100 {
        let waiting = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
                 SELECT 1
                   FROM pg_stat_activity
                  WHERE datname = current_database()
                    AND application_name = $1
                    AND wait_event_type = 'Lock'
             )",
        )
        .bind(application_name)
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("tagged webhook transaction never reached its expected lock wait");
}

async fn fixture(pool: &PgPool) -> (RoomId, ParticipantId) {
    let actor = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(actor.to_uuid())
        .bind(format!("atomic-webhook-actor-{actor}"))
        .execute(pool)
        .await
        .expect("insert actor");
    let workspace = WorkspaceId::new();
    let mut tx = pool.begin().await.expect("begin fixture transaction");
    sqlx::query(
        "INSERT INTO workspaces (id, name, slug, created_by)
         VALUES ($1, 'Webhook Atomic Test', $2, $3)",
    )
    .bind(workspace.to_uuid())
    .bind(format!("webhook-atomic-{workspace}"))
    .bind(actor.to_uuid())
    .execute(&mut *tx)
    .await
    .expect("insert workspace");
    let room = RoomId::new();
    sqlx::query(
        "INSERT INTO rooms (id, kind, created_by, workspace_id)
         VALUES ($1, 'group', $2, $3)",
    )
    .bind(room.to_uuid())
    .bind(actor.to_uuid())
    .bind(workspace.to_uuid())
    .execute(&mut *tx)
    .await
    .expect("insert room");
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, 'owner')",
    )
    .bind(workspace.to_uuid())
    .bind(actor.to_uuid())
    .execute(&mut *tx)
    .await
    .expect("join workspace");
    sqlx::query(
        "INSERT INTO room_members (room_id, participant_id, role)
         VALUES ($1, $2, 'owner')",
    )
    .bind(room.to_uuid())
    .bind(actor.to_uuid())
    .execute(&mut *tx)
    .await
    .expect("join room");
    tx.commit().await.expect("commit fixture transaction");
    (room, actor)
}

async fn assert_registration_absent(pool: &PgPool, bot_name: &str, token_hash: &str) {
    let bot_count =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM participants WHERE display_name = $1")
            .bind(bot_name)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(bot_count, 0, "rejected registration must not leave a bot");
    let hook_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM incoming_webhooks WHERE token_hash = $1",
    )
    .bind(token_hash)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(hook_count, 0, "rejected registration must not leave a hook");
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn webhook_incoming_atomic_create_commits_bot_membership_and_hook() {
    let pool = pool();
    let repo = WebhookRepo::new(pool.clone());
    let (room, actor) = fixture(&pool).await;
    let token_hash = hash_token(&format!("atomic-success-{actor}"));

    let created = repo
        .create_incoming(
            room,
            "Atomic webhook bot",
            &token_hash,
            Some("Atomic hook"),
            actor,
        )
        .await
        .unwrap();

    let bot = sqlx::query_as::<_, (String, String, Option<uuid::Uuid>)>(
        "SELECT kind, display_name, created_by FROM participants WHERE id = $1",
    )
    .bind(created.bot_id.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(bot.0, "bot");
    assert_eq!(bot.1, "Atomic webhook bot");
    assert_eq!(bot.2, Some(actor.to_uuid()));

    let workspace_role = sqlx::query_scalar::<_, String>(
        "SELECT membership.role
           FROM workspace_members membership
           JOIN rooms room ON room.workspace_id = membership.workspace_id
          WHERE room.id = $1 AND membership.participant_id = $2",
    )
    .bind(room.to_uuid())
    .bind(created.bot_id.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(workspace_role, "member");
    let effective_access = sqlx::query_scalar::<_, bool>(
        "SELECT aero_effective_workspace_access(
             (SELECT workspace_id FROM rooms WHERE id = $1),
             $2
         )",
    )
    .bind(room.to_uuid())
    .bind(created.bot_id.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        effective_access,
        "incoming bot must pass the workspace half of send_message authorization"
    );

    let role = sqlx::query_scalar::<_, String>(
        "SELECT role FROM room_members WHERE room_id = $1 AND participant_id = $2",
    )
    .bind(room.to_uuid())
    .bind(created.bot_id.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(role, "member");

    let hook = repo
        .find_incoming_by_token_hash(&token_hash)
        .await
        .unwrap()
        .expect("hook committed");
    assert_eq!(hook.id, created.webhook_id);
    assert_eq!(hook.bot_id, created.bot_id);
    assert_eq!(hook.room_id, room);

    repo.revoke_incoming_and_cleanup(created.webhook_id, actor)
        .await
        .unwrap();
    let membership_count = sqlx::query_scalar::<_, i64>(
        "SELECT
             (SELECT count(*) FROM workspace_members WHERE participant_id = $1)
           + (SELECT count(*) FROM room_members WHERE participant_id = $1)",
    )
    .bind(created.bot_id.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        membership_count, 0,
        "revoke atomically removes both bot memberships"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn webhook_incoming_atomic_duplicate_hash_rolls_back_bot_and_membership() {
    let pool = pool();
    let repo = WebhookRepo::new(pool.clone());
    let (room, actor) = fixture(&pool).await;
    let token_hash = hash_token(&format!("atomic-duplicate-{actor}"));
    sqlx::query(
        "INSERT INTO incoming_webhooks
             (id, room_id, bot_id, token_hash, created_by)
         VALUES ($1, $2, $3, $4, $3)",
    )
    .bind(WebhookId::new().to_uuid())
    .bind(room.to_uuid())
    .bind(actor.to_uuid())
    .bind(&token_hash)
    .execute(&pool)
    .await
    .unwrap();
    let bot_name = format!("rollback-webhook-bot-{room}");

    let error = repo
        .create_incoming(room, &bot_name, &token_hash, None, actor)
        .await
        .unwrap_err();
    assert!(matches!(error, WebhookWriteError::Storage(_)));

    let bot_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM participants
          WHERE display_name = $1 AND kind = 'bot' AND created_by = $2",
    )
    .bind(&bot_name)
    .bind(actor.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(bot_count, 0, "failed hook insert must roll back its bot");

    let membership_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)
           FROM room_members membership
           JOIN participants bot ON bot.id = membership.participant_id
          WHERE membership.room_id = $1 AND bot.display_name = $2",
    )
    .bind(room.to_uuid())
    .bind(&bot_name)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        membership_count, 0,
        "failed hook insert must roll back bot membership"
    );
    let workspace_membership_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)
           FROM workspace_members membership
           JOIN participants bot ON bot.id = membership.participant_id
          WHERE bot.display_name = $1",
    )
    .bind(&bot_name)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        workspace_membership_count, 0,
        "failed hook insert must roll back bot workspace membership"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn webhook_incoming_rejects_marker_group_dm_without_writes() {
    let pool = pool();
    let repo = WebhookRepo::new(pool.clone());
    let (room, actor) = fixture(&pool).await;
    let workspace: uuid::Uuid = sqlx::query_scalar("SELECT workspace_id FROM rooms WHERE id = $1")
        .bind(room.to_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    for ordinal in 0..2 {
        let peer = ParticipantId::new();
        sqlx::query(
            "INSERT INTO participants (id, kind, display_name)
             VALUES ($1, 'human', $2)",
        )
        .bind(peer.to_uuid())
        .bind(format!("group-dm-webhook-peer-{ordinal}-{peer}"))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1, $2, 'member')",
        )
        .bind(workspace)
        .bind(peer.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'member')",
        )
        .bind(room.to_uuid())
        .bind(peer.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query("UPDATE rooms SET is_group_dm = true WHERE id = $1")
        .bind(room.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    let bot_name = format!("group-dm-webhook-bot-{room}");
    let token_hash = hash_token(&format!("group-dm-webhook-{actor}"));

    let error = repo
        .create_incoming(room, &bot_name, &token_hash, None, actor)
        .await
        .unwrap_err();
    assert!(matches!(error, WebhookWriteError::FixedMembership));
    assert_registration_absent(&pool, &bot_name, &token_hash).await;
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn webhook_incoming_rechecks_creator_room_and_workspace_access() {
    let pool = pool();
    let repo = WebhookRepo::new(pool.clone());
    let (room, actor) = fixture(&pool).await;

    sqlx::query("DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    let no_room_bot = format!("no-room-webhook-bot-{room}");
    let no_room_hash = hash_token(&format!("no-room-webhook-{actor}"));
    let error = repo
        .create_incoming(room, &no_room_bot, &no_room_hash, None, actor)
        .await
        .unwrap_err();
    assert!(matches!(error, WebhookWriteError::Forbidden));
    assert_registration_absent(&pool, &no_room_bot, &no_room_hash).await;

    sqlx::query(
        "INSERT INTO room_members (room_id, participant_id, role)
         VALUES ($1, $2, 'owner')",
    )
    .bind(room.to_uuid())
    .bind(actor.to_uuid())
    .execute(&pool)
    .await
    .unwrap();
    let workspace_id =
        sqlx::query_scalar::<_, uuid::Uuid>("SELECT workspace_id FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    let guardian = ParticipantId::new();
    sqlx::query(
        "INSERT INTO participants (id, kind, display_name)
         VALUES ($1, 'human', $2)",
    )
    .bind(guardian.to_uuid())
    .bind(format!("webhook-deactivation-guardian-{guardian}"))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, 'owner')",
    )
    .bind(workspace_id)
    .bind(guardian.to_uuid())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE workspace_members
            SET role = 'member'
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(workspace_id)
    .bind(actor.to_uuid())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspace_deactivations
             (workspace_id, participant_id, deactivated_by)
         VALUES ($1, $2, $3)",
    )
    .bind(workspace_id)
    .bind(actor.to_uuid())
    .bind(guardian.to_uuid())
    .execute(&pool)
    .await
    .unwrap();
    let deactivated_bot = format!("deactivated-webhook-bot-{room}");
    let deactivated_hash = hash_token(&format!("deactivated-webhook-{actor}"));
    let error = repo
        .create_incoming(room, &deactivated_bot, &deactivated_hash, None, actor)
        .await
        .unwrap_err();
    assert!(matches!(error, WebhookWriteError::Forbidden));
    assert_registration_absent(&pool, &deactivated_bot, &deactivated_hash).await;
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn webhook_incoming_create_locks_workspace_before_room() {
    let pool = pool();
    let (room, actor) = fixture(&pool).await;
    let workspace_id =
        sqlx::query_scalar::<_, uuid::Uuid>("SELECT workspace_id FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    let application_name = format!("webhook-create-lock-{actor}");
    let repo = WebhookRepo::new(tagged_pool(&application_name).await);
    let token_hash = hash_token(&format!("create-lock-order-{actor}"));

    let mut governance = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace_id)
        .execute(&mut *governance)
        .await
        .unwrap();

    let create = tokio::spawn(async move {
        repo.create_incoming(
            room,
            "workspace-first webhook bot",
            &token_hash,
            None,
            actor,
        )
        .await
    });
    wait_until_tagged_query_waits_on_lock(&pool, &application_name).await;

    sqlx::query("SET LOCAL lock_timeout = '500ms'")
        .execute(&mut *governance)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM rooms WHERE id = $1 FOR UPDATE")
        .bind(room.to_uuid())
        .execute(&mut *governance)
        .await
        .expect("waiting create must not already hold a room lock");
    governance.commit().await.unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(3), create)
        .await
        .expect("create completed after workspace lock released")
        .expect("create task")
        .expect("create succeeds");
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn webhook_incoming_revoke_locks_workspace_before_hook() {
    let pool = pool();
    let (room, actor) = fixture(&pool).await;
    let workspace_id =
        sqlx::query_scalar::<_, uuid::Uuid>("SELECT workspace_id FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    let created = WebhookRepo::new(pool.clone())
        .create_incoming(
            room,
            "revoke lock-order webhook bot",
            &hash_token(&format!("revoke-lock-order-{actor}")),
            None,
            actor,
        )
        .await
        .unwrap();
    let application_name = format!("webhook-revoke-lock-{actor}");
    let repo = WebhookRepo::new(tagged_pool(&application_name).await);

    let mut governance = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace_id)
        .execute(&mut *governance)
        .await
        .unwrap();

    let webhook_id = created.webhook_id;
    let revoke =
        tokio::spawn(async move { repo.revoke_incoming_and_cleanup(webhook_id, actor).await });
    wait_until_tagged_query_waits_on_lock(&pool, &application_name).await;

    sqlx::query("SET LOCAL lock_timeout = '500ms'")
        .execute(&mut *governance)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM incoming_webhooks WHERE id = $1 FOR UPDATE")
        .bind(created.webhook_id.to_uuid())
        .execute(&mut *governance)
        .await
        .expect("waiting revoke must not already hold the hook lock");
    governance.commit().await.unwrap();

    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(3), revoke)
            .await
            .expect("revoke completed after workspace lock released")
            .expect("revoke task")
            .expect("revoke succeeds"),
        (room, created.bot_id)
    );
}
