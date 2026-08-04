use aero_common::{ParticipantId, RoomId, WebhookId, WorkspaceId};
use sqlx::postgres::PgConnectOptions;
use sqlx::PgPool;

use super::{WebhookRepo, WebhookWriteError};
use crate::webhook::{generate_secret, hash_token};

struct Fixture {
    workspace: WorkspaceId,
    room: RoomId,
    owner: ParticipantId,
}

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
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
        .max_connections(1)
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

async fn fixture(pool: &PgPool) -> Fixture {
    let owner = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(owner.to_uuid())
        .bind(format!("webhook-management-owner-{owner}"))
        .execute(pool)
        .await
        .unwrap();
    let workspace = WorkspaceId::new();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO workspaces (id, name, slug, created_by)
         VALUES ($1, 'Webhook Management Test', $2, $3)",
    )
    .bind(workspace.to_uuid())
    .bind(format!("webhook-management-{workspace}"))
    .bind(owner.to_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, 'owner')",
    )
    .bind(workspace.to_uuid())
    .bind(owner.to_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    let room = RoomId::new();
    sqlx::query(
        "INSERT INTO rooms (id, kind, name, created_by, workspace_id)
         VALUES ($1, 'group', $2, $3, $4)",
    )
    .bind(room.to_uuid())
    .bind(format!("Webhook Management {room}"))
    .bind(owner.to_uuid())
    .bind(workspace.to_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO room_members (room_id, participant_id, role)
         VALUES ($1, $2, 'owner')",
    )
    .bind(room.to_uuid())
    .bind(owner.to_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    Fixture {
        workspace,
        room,
        owner,
    }
}

async fn add_member(pool: &PgPool, fixture: &Fixture) -> ParticipantId {
    let actor = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(actor.to_uuid())
        .bind(format!("webhook-management-member-{actor}"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, 'member')",
    )
    .bind(fixture.workspace.to_uuid())
    .bind(actor.to_uuid())
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO room_members (room_id, participant_id, role)
         VALUES ($1, $2, 'member')",
    )
    .bind(fixture.room.to_uuid())
    .bind(actor.to_uuid())
    .execute(pool)
    .await
    .unwrap();
    actor
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn concurrent_incoming_creates_serialize_without_workspace_lock_upgrade_deadlock() {
    let pool = pool();
    let fixture = fixture(&pool).await;
    let first_repo = WebhookRepo::new(pool.clone());
    let second_repo = WebhookRepo::new(pool.clone());
    let first_hash = hash_token(&format!("concurrent-incoming-first-{}", fixture.owner));
    let second_hash = hash_token(&format!("concurrent-incoming-second-{}", fixture.owner));
    let room = fixture.room;
    let owner = fixture.owner;

    let first = tokio::spawn(async move {
        first_repo
            .create_incoming(room, "Concurrent incoming first", &first_hash, None, owner)
            .await
    });
    let second = tokio::spawn(async move {
        second_repo
            .create_incoming(
                room,
                "Concurrent incoming second",
                &second_hash,
                None,
                owner,
            )
            .await
    });

    let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(first, second)
    })
    .await
    .expect("concurrent webhook creates must not deadlock");
    first.expect("first task").expect("first create");
    second.expect("second task").expect("second create");
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn concurrent_access_removal_blocks_incoming_revoke_and_outgoing_writes() {
    let pool = pool();
    let fixture = fixture(&pool).await;
    let actor = add_member(&pool, &fixture).await;
    let setup = WebhookRepo::new(pool.clone());
    let incoming = setup
        .create_incoming(
            fixture.room,
            "Revocation race incoming",
            &hash_token(&format!("revocation-race-incoming-{actor}")),
            None,
            actor,
        )
        .await
        .unwrap();
    let outgoing = setup
        .create_outgoing(
            fixture.room,
            "https://revocation-race.example.test/existing",
            &generate_secret(),
            &[],
            None,
            actor,
        )
        .await
        .unwrap();

    let preflight_access: bool =
        sqlx::query_scalar("SELECT aero_effective_room_access($1, $2, $3)")
            .bind(fixture.room.to_uuid())
            .bind(actor.to_uuid())
            .bind(fixture.workspace.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(preflight_access, "the simulated route preflight passed");

    let mut governance = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(fixture.workspace.to_uuid())
        .execute(&mut *governance)
        .await
        .unwrap();
    sqlx::query("DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
        .bind(fixture.room.to_uuid())
        .bind(actor.to_uuid())
        .execute(&mut *governance)
        .await
        .unwrap();

    let incoming_app = format!("incoming-revoke-auth-race-{actor}");
    let incoming_repo = WebhookRepo::new(tagged_pool(&incoming_app).await);
    let incoming_revoke = tokio::spawn(async move {
        incoming_repo
            .revoke_incoming_and_cleanup(incoming.webhook_id, actor)
            .await
    });

    let outgoing_app = format!("outgoing-revoke-auth-race-{actor}");
    let outgoing_repo = WebhookRepo::new(tagged_pool(&outgoing_app).await);
    let outgoing_revoke =
        tokio::spawn(async move { outgoing_repo.revoke_outgoing(outgoing, actor).await });

    let create_app = format!("outgoing-create-auth-race-{actor}");
    let create_repo = WebhookRepo::new(tagged_pool(&create_app).await);
    let room = fixture.room;
    let outgoing_create = tokio::spawn(async move {
        create_repo
            .create_outgoing(
                room,
                "https://revocation-race.example.test/new",
                &generate_secret(),
                &[],
                None,
                actor,
            )
            .await
    });

    wait_until_tagged_query_waits_on_lock(&pool, &incoming_app).await;
    wait_until_tagged_query_waits_on_lock(&pool, &outgoing_app).await;
    wait_until_tagged_query_waits_on_lock(&pool, &create_app).await;
    governance.commit().await.unwrap();

    for result in [
        incoming_revoke.await.unwrap().map(|_| ()),
        outgoing_revoke.await.unwrap().map(|_| ()),
        outgoing_create.await.unwrap().map(|_| ()),
    ] {
        assert!(matches!(result, Err(WebhookWriteError::Forbidden)));
    }

    let incoming_state = sqlx::query_as::<_, (bool, i64)>(
        "SELECT hook.revoked_at IS NULL,
                (SELECT count(*) FROM workspace_members WHERE participant_id = hook.bot_id)
              + (SELECT count(*) FROM room_members WHERE participant_id = hook.bot_id)
           FROM incoming_webhooks hook
          WHERE hook.id = $1",
    )
    .bind(incoming.webhook_id.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(incoming_state.0, "failed revoke must leave the hook active");
    assert_eq!(
        incoming_state.1, 2,
        "failed revoke must leave both bot memberships"
    );
    assert!(
        setup.outgoing_target(outgoing).await.unwrap().is_some(),
        "failed outgoing revoke must leave its target active"
    );
    let raced_create_count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM outgoing_webhooks
          WHERE url = 'https://revocation-race.example.test/new'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(raced_create_count, 0);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn global_webhook_ids_are_tenant_contained_for_create_and_revoke() {
    let pool = pool();
    let left = fixture(&pool).await;
    let right = fixture(&pool).await;
    let repo = WebhookRepo::new(pool.clone());

    let cross_create = repo
        .create_outgoing(
            left.room,
            "https://cross-tenant.example.test/create",
            &generate_secret(),
            &[],
            None,
            right.owner,
        )
        .await;
    assert!(matches!(cross_create, Err(WebhookWriteError::Forbidden)));

    let outgoing = repo
        .create_outgoing(
            left.room,
            "https://cross-tenant.example.test/revoke",
            &generate_secret(),
            &[],
            None,
            left.owner,
        )
        .await
        .unwrap();
    let incoming = repo
        .create_incoming(
            left.room,
            "Cross tenant incoming",
            &hash_token(&format!("cross-tenant-incoming-{}", left.owner)),
            None,
            left.owner,
        )
        .await
        .unwrap();

    assert!(matches!(
        repo.revoke_outgoing(outgoing, right.owner).await,
        Err(WebhookWriteError::Forbidden)
    ));
    assert!(matches!(
        repo.revoke_incoming_and_cleanup(incoming.webhook_id, right.owner)
            .await,
        Err(WebhookWriteError::Forbidden)
    ));
    assert!(repo.outgoing_target(outgoing).await.unwrap().is_some());
    assert!(
        !repo
            .find_incoming_by_token_hash(&hash_token(&format!(
                "cross-tenant-incoming-{}",
                left.owner
            )))
            .await
            .unwrap()
            .unwrap()
            .revoked
    );

    assert!(matches!(
        repo.revoke_outgoing(WebhookId::new(), left.owner).await,
        Err(WebhookWriteError::NotFound)
    ));
    assert!(matches!(
        repo.revoke_incoming_and_cleanup(WebhookId::new(), left.owner)
            .await,
        Err(WebhookWriteError::NotFound)
    ));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn outgoing_revoke_locks_workspace_before_global_hook_id() {
    let pool = pool();
    let fixture = fixture(&pool).await;
    let hook = WebhookRepo::new(pool.clone())
        .create_outgoing(
            fixture.room,
            "https://lock-order.example.test/hook",
            &generate_secret(),
            &[],
            None,
            fixture.owner,
        )
        .await
        .unwrap();
    let application_name = format!("outgoing-revoke-lock-order-{}", fixture.owner);
    let repo = WebhookRepo::new(tagged_pool(&application_name).await);

    let mut governance = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(fixture.workspace.to_uuid())
        .execute(&mut *governance)
        .await
        .unwrap();

    let owner = fixture.owner;
    let revoke = tokio::spawn(async move { repo.revoke_outgoing(hook, owner).await });
    wait_until_tagged_query_waits_on_lock(&pool, &application_name).await;

    sqlx::query("SET LOCAL lock_timeout = '500ms'")
        .execute(&mut *governance)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM outgoing_webhooks WHERE id = $1 FOR UPDATE")
        .bind(hook.to_uuid())
        .execute(&mut *governance)
        .await
        .expect("waiting revoke must not hold the global hook id first");
    governance.commit().await.unwrap();

    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(3), revoke)
            .await
            .expect("revoke completed after workspace lock released")
            .expect("revoke task")
            .expect("revoke succeeds"),
        fixture.room
    );
}
