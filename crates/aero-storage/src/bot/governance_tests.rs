//! PostgreSQL-backed bot governance tests.

use std::time::Duration as StdDuration;

use aero_common::{Error, ParticipantId, RoomId, RoomKind, WorkspaceId, WorkspaceRole};
use sqlx::PgPool;
use time::Duration;
use uuid::Uuid;

use super::*;
use crate::{BotDeliveryOutboxRepo, RoomRepo, WorkspaceRepo};

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect_lazy(&url)
        .expect("valid DATABASE_URL")
}

async fn participant(pool: &PgPool, label: &str) -> ParticipantId {
    let participant = ParticipantId::new();
    sqlx::query(
        "INSERT INTO participants (id, kind, display_name)
         VALUES ($1, 'human', $2)",
    )
    .bind(participant.to_uuid())
    .bind(format!("{label}-{participant}"))
    .execute(pool)
    .await
    .unwrap();
    participant
}

async fn workspace_room(pool: &PgPool, owner: ParticipantId, label: &str) -> (WorkspaceId, RoomId) {
    let workspace = WorkspaceRepo::new(pool.clone())
        .create(
            format!("{label}-{owner}"),
            format!("{label}-{owner}").to_ascii_lowercase(),
            owner,
        )
        .await
        .unwrap()
        .id;
    let room = RoomRepo::new(pool.clone())
        .create_in_workspace(
            workspace,
            RoomKind::Channel,
            Some(format!("{label}-room")),
            owner,
        )
        .await
        .unwrap()
        .id;
    (workspace, room)
}

async fn workspace_bot(
    pool: &PgPool,
    owner: ParticipantId,
    workspace: WorkspaceId,
    label: &str,
) -> (ParticipantId, String) {
    BotRepo::new(pool.clone())
        .create_authorized_with_token(owner, label, None, Some(workspace))
        .await
        .unwrap()
}

async fn room_subscription(
    repo: &BotRepo,
    bot: ParticipantId,
    owner: ParticipantId,
    room: RoomId,
) -> Uuid {
    repo.subscribe_authorized(
        bot,
        owner,
        "message",
        &serde_json::json!({ "room_id": room.to_string() }),
        Some("https://example.test/bot"),
    )
    .await
    .unwrap()
    .0
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn bot_create_and_initial_token_are_atomic_and_workspace_authorized() {
    let pool = pool();
    let repo = BotRepo::new(pool.clone());
    let owner = participant(&pool, "bot-create-owner").await;
    let outsider = participant(&pool, "bot-create-outsider").await;
    let (workspace, _) = workspace_room(&pool, owner, "bot-create").await;

    let denied = repo
        .create_authorized_with_token(outsider, "denied-bot", None, Some(workspace))
        .await;
    assert!(matches!(denied, Err(Error::NotFound(_))));
    let denied_rows = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM participants
          WHERE created_by = $1 AND display_name = 'denied-bot'",
    )
    .bind(outsider.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(denied_rows, 0);

    let (bot, token) = repo
        .create_authorized_with_token(owner, "atomic-bot", None, Some(workspace))
        .await
        .unwrap();
    assert_eq!(repo.verify_token(&token).await.unwrap(), Some(bot));
    let stored = sqlx::query_as::<_, (Uuid, Option<Uuid>, bool)>(
        "SELECT owner_id, workspace_id, token_hash IS NOT NULL
           FROM bots WHERE id = $1",
    )
    .bind(bot.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored.0, owner.to_uuid());
    assert_eq!(stored.1, Some(workspace.to_uuid()));
    assert!(stored.2, "the first committed bot already has a token hash");
    let membership = sqlx::query_as::<_, (String, bool)>(
        "SELECT role, is_guest
           FROM workspace_members
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(workspace.to_uuid())
    .bind(bot.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(membership, ("member".into(), false));
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT aero_effective_workspace_access($1, $2)")
            .bind(workspace.to_uuid())
            .bind(bot.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap(),
        "workspace-scoped bot must be installable into a room"
    );

    let failure_name = format!("partial-bot-{owner}");
    let function = format!("aero_fail_bot_create_{}", Uuid::new_v4().simple());
    let trigger = format!("fail_bot_create_{}", Uuid::new_v4().simple());
    sqlx::query(&format!(
        "CREATE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             IF NEW.name = '{failure_name}' THEN
                 RAISE EXCEPTION 'injected bot insert failure';
             END IF;
             RETURN NEW;
         END $$"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER {trigger}
         BEFORE INSERT ON bots
         FOR EACH ROW EXECUTE FUNCTION {function}()"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let failed = repo
        .create_authorized_with_token(owner, &failure_name, None, Some(workspace))
        .await;
    assert!(matches!(failed, Err(Error::Database(_))));
    let partial = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM participants
          WHERE created_by = $1 AND display_name = $2",
    )
    .bind(owner.to_uuid())
    .bind(&failure_name)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(partial, 0, "participant insert rolled back with bot/token");
    let partial_membership = sqlx::query_scalar::<_, i64>(
        r"SELECT count(*)
            FROM workspace_members membership
            JOIN participants participant
              ON participant.id = membership.participant_id
           WHERE membership.workspace_id = $1
             AND participant.created_by = $2
             AND participant.display_name = $3",
    )
    .bind(workspace.to_uuid())
    .bind(owner.to_uuid())
    .bind(&failure_name)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        partial_membership, 0,
        "workspace membership rolled back with bot/token"
    );
    sqlx::query(&format!("DROP TRIGGER {trigger} ON bots"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("DROP FUNCTION {function}()"))
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn bot_governance_waits_for_workspace_revocation_and_writes_nothing() {
    let pool = pool();
    let repo = BotRepo::new(pool.clone());
    let workspace_owner = participant(&pool, "bot-revoke-workspace-owner").await;
    let bot_owner = participant(&pool, "bot-revoke-owner").await;
    let (workspace, _) = workspace_room(&pool, workspace_owner, "bot-revoke").await;
    WorkspaceRepo::new(pool.clone())
        .add_member(workspace, bot_owner, WorkspaceRole::Member)
        .await
        .unwrap();
    let (bot, old_token) = workspace_bot(&pool, bot_owner, workspace, "revoked-bot").await;
    let old_hash = crate::revoked_token::hash_token(&old_token);

    let mut revocation = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace.to_uuid())
        .execute(&mut *revocation)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspace_deactivations
             (workspace_id, participant_id, deactivated_by)
         VALUES ($1, $2, $3)",
    )
    .bind(workspace.to_uuid())
    .bind(bot_owner.to_uuid())
    .bind(workspace_owner.to_uuid())
    .execute(&mut *revocation)
    .await
    .unwrap();

    let raced_repo = repo.clone();
    let mut raced =
        tokio::spawn(async move { raced_repo.rotate_token_authorized(bot, bot_owner).await });
    assert!(
        tokio::time::timeout(StdDuration::from_millis(150), &mut raced)
            .await
            .is_err(),
        "token rotation waits behind the workspace revocation fence"
    );
    revocation.commit().await.unwrap();
    assert!(matches!(raced.await.unwrap(), Err(Error::Forbidden(_))));

    let current_hash =
        sqlx::query_scalar::<_, Option<String>>("SELECT token_hash FROM bots WHERE id = $1")
            .bind(bot.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(current_hash.as_deref(), Some(old_hash.as_str()));
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn subscription_crud_is_owner_scoped_and_canonicalizes_tenant_filters() {
    let pool = pool();
    let repo = BotRepo::new(pool.clone());
    let owner = participant(&pool, "bot-sub-owner").await;
    let attacker = participant(&pool, "bot-sub-attacker").await;
    let (workspace, room) = workspace_room(&pool, owner, "bot-sub-left").await;
    let (other_workspace, other_room) = workspace_room(&pool, attacker, "bot-sub-right").await;
    let (bot, _) = workspace_bot(&pool, owner, workspace, "scoped-bot").await;

    let (subscription, secret) = repo
        .subscribe_authorized(
            bot,
            owner,
            "message",
            &serde_json::json!({ "room_id": room.to_string() }),
            Some("https://example.test/canonical"),
        )
        .await
        .unwrap();
    assert!(secret.is_some());
    let scopes = sqlx::query_as::<_, (Option<String>, Option<String>)>(
        "SELECT scope_room_id, scope_workspace_id
           FROM bot_event_subscriptions WHERE id = $1",
    )
    .bind(subscription)
    .fetch_one(&pool)
    .await
    .unwrap();
    let room_text = room.to_string();
    let workspace_text = workspace.to_string();
    assert_eq!(scopes.0.as_deref(), Some(room_text.as_str()));
    assert_eq!(
        scopes.1.as_deref(),
        Some(workspace_text.as_str()),
        "storage projected the room's canonical workspace"
    );

    let mismatched = repo
        .subscribe_authorized(
            bot,
            owner,
            "message",
            &serde_json::json!({
                "room_id": other_room.to_string(),
                "workspace_id": other_workspace.to_string(),
            }),
            Some("https://example.test/escape"),
        )
        .await;
    assert!(matches!(mismatched, Err(Error::NotFound(_))));

    assert!(matches!(
        repo.list_subscriptions_authorized(bot, attacker).await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        repo.rotate_subscription_secret_authorized(bot, subscription, attacker)
            .await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        repo.delete_subscription_authorized(bot, subscription, attacker)
            .await,
        Err(Error::NotFound(_))
    ));

    let rotated = repo
        .rotate_subscription_secret_authorized(bot, subscription, owner)
        .await
        .unwrap();
    assert_ne!(secret.as_deref(), Some(rotated.as_str()));
    let listed = repo
        .list_subscriptions_authorized(bot, owner)
        .await
        .unwrap();
    assert!(listed.iter().any(|row| row.id == subscription));
    repo.delete_subscription_authorized(bot, subscription, owner)
        .await
        .unwrap();
    assert!(!sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
                 SELECT 1 FROM bot_event_subscriptions WHERE id = $1
             )",
    )
    .bind(subscription)
    .fetch_one(&pool)
    .await
    .unwrap());
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn bot_delivery_requeue_is_owner_dead_audit_and_claim_generation_fenced() {
    let pool = pool();
    let bots = BotRepo::new(pool.clone());
    let queue = BotDeliveryOutboxRepo::new(pool.clone());
    let owner = participant(&pool, "bot-dlq-owner").await;
    let attacker = participant(&pool, "bot-dlq-attacker").await;
    let (workspace, room) = workspace_room(&pool, owner, "bot-dlq").await;
    let (bot, _) = workspace_bot(&pool, owner, workspace, "dlq-bot").await;
    let subscription = room_subscription(&bots, bot, owner, room).await;
    let event = Uuid::new_v4();
    let body = serde_json::to_vec(&serde_json::json!({
        "kind": "message",
        "event_id": event,
    }))
    .unwrap();
    let delivery = queue
        .enqueue(subscription, bot, event, "message", room, &body)
        .await
        .unwrap();
    let now = time::OffsetDateTime::now_utc();
    let claimed = queue
        .claim_due(now, Duration::minutes(2), 32)
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.id == delivery)
        .unwrap();
    let old_claim = claimed.claim_token.unwrap();
    assert!(queue
        .mark_dead(delivery, old_claim, now, "terminal")
        .await
        .unwrap());

    assert!(matches!(
        bots.list_deliveries_authorized(bot, attacker, 100).await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        bots.requeue_delivery_authorized(bot, delivery, attacker)
            .await,
        Err(Error::NotFound(_))
    ));
    assert_eq!(queue.get(delivery).await.unwrap().unwrap().status, "dead");

    bots.requeue_delivery_authorized(bot, delivery, owner)
        .await
        .unwrap();
    let requeued = queue.get(delivery).await.unwrap().unwrap();
    assert_eq!(requeued.status, "pending");
    assert_eq!(requeued.attempts, 0);
    assert_eq!(requeued.request_body, body);
    let audits = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM audit_events
          WHERE workspace_id = $1
            AND actor_id = $2
            AND action = 'bot.delivery.requeued'
            AND target = $3",
    )
    .bind(workspace.to_uuid())
    .bind(owner.to_uuid())
    .bind(delivery.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audits, 1);

    let claimed_again = queue
        .claim_due(now + Duration::seconds(1), Duration::minutes(2), 32)
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.id == delivery)
        .unwrap();
    let new_claim = claimed_again.claim_token.unwrap();
    assert_ne!(old_claim, new_claim);
    assert!(
        !queue
            .mark_delivered(delivery, old_claim, now, 204)
            .await
            .unwrap(),
        "a worker from before requeue cannot settle the successor claim"
    );
    assert!(matches!(
        bots.requeue_delivery_authorized(bot, delivery, owner).await,
        Err(Error::NotFound(_))
    ));
    let audits_after = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM audit_events
          WHERE action = 'bot.delivery.requeued' AND target = $1",
    )
    .bind(delivery.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audits_after, 1);
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn bot_delivery_requeue_rolls_back_when_audit_append_fails() {
    let pool = pool();
    let bots = BotRepo::new(pool.clone());
    let queue = BotDeliveryOutboxRepo::new(pool.clone());
    let owner = participant(&pool, "bot-audit-owner").await;
    let (workspace, room) = workspace_room(&pool, owner, "bot-audit").await;
    let (bot, _) = workspace_bot(&pool, owner, workspace, "audit-bot").await;
    let subscription = room_subscription(&bots, bot, owner, room).await;
    let delivery = queue
        .enqueue(
            subscription,
            bot,
            Uuid::new_v4(),
            "message",
            room,
            br#"{"kind":"message"}"#,
        )
        .await
        .unwrap();
    let now = time::OffsetDateTime::now_utc();
    let claimed = queue
        .claim_due(now, Duration::minutes(2), 32)
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.id == delivery)
        .unwrap();
    queue
        .mark_dead(delivery, claimed.claim_token.unwrap(), now, "terminal")
        .await
        .unwrap();

    let function = format!("aero_fail_bot_audit_{}", Uuid::new_v4().simple());
    let trigger = format!("fail_bot_audit_{}", Uuid::new_v4().simple());
    sqlx::query(&format!(
        "CREATE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             IF NEW.action = 'bot.delivery.requeued'
                AND NEW.target = '{}' THEN
                 RAISE EXCEPTION 'injected bot audit failure';
             END IF;
             RETURN NEW;
         END $$",
        delivery
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER {trigger}
         BEFORE INSERT ON audit_events
         FOR EACH ROW EXECUTE FUNCTION {function}()"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let result = bots.requeue_delivery_authorized(bot, delivery, owner).await;
    assert!(matches!(result, Err(Error::Database(_))));
    assert_eq!(queue.get(delivery).await.unwrap().unwrap().status, "dead");
    let audits = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM audit_events
          WHERE action = 'bot.delivery.requeued' AND target = $1",
    )
    .bind(delivery.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audits, 0);
    sqlx::query(&format!("DROP TRIGGER {trigger} ON audit_events"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("DROP FUNCTION {function}()"))
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn raw_sql_cannot_drift_bot_subscription_or_delivery_tenant_identity() {
    let pool = pool();
    let bots = BotRepo::new(pool.clone());
    let owner = participant(&pool, "bot-raw-owner").await;
    let other = participant(&pool, "bot-raw-other").await;
    let (workspace, room) = workspace_room(&pool, owner, "bot-raw-left").await;
    let (other_workspace, other_room) = workspace_room(&pool, other, "bot-raw-right").await;
    let (bot, _) = workspace_bot(&pool, owner, workspace, "raw-bot").await;
    let (other_bot, _) = workspace_bot(&pool, other, other_workspace, "raw-other-bot").await;
    let subscription = room_subscription(&bots, bot, owner, room).await;

    let owner_drift = sqlx::query("UPDATE bots SET owner_id = $2 WHERE id = $1")
        .bind(bot.to_uuid())
        .bind(other.to_uuid())
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(
        owner_drift
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("bots_owner_workspace_identity_immutable")
    );
    let workspace_drift = sqlx::query("UPDATE bots SET workspace_id = $2 WHERE id = $1")
        .bind(bot.to_uuid())
        .bind(other_workspace.to_uuid())
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(
        workspace_drift
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("bots_owner_workspace_identity_immutable")
    );

    let scope_drift = sqlx::query(
        "UPDATE bot_event_subscriptions
            SET filters = jsonb_set(filters, '{workspace_id}', to_jsonb($2::text))
          WHERE id = $1",
    )
    .bind(subscription)
    .bind(other_workspace.to_string())
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        scope_drift
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("bot_subscription_identity_immutable")
    );
    let bot_drift = sqlx::query(
        "UPDATE bot_event_subscriptions
            SET bot_id = $2
          WHERE id = $1",
    )
    .bind(subscription)
    .bind(other_bot.to_uuid())
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        bot_drift
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("bot_subscription_identity_immutable")
    );

    let invalid_subscription = sqlx::query(
        r"INSERT INTO bot_event_subscriptions
              (id, bot_id, event_type, filters)
          VALUES ($1, $2, 'message',
                  jsonb_build_object(
                      'room_id', $3::text,
                      'workspace_id', $4::text
                  ))",
    )
    .bind(Uuid::new_v4())
    .bind(bot.to_uuid())
    .bind(other_room.to_string())
    .bind(other_workspace.to_string())
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        invalid_subscription
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("bot_subscription_bot_workspace_containment")
    );

    let forged_delivery = sqlx::query(
        r"INSERT INTO bot_subscription_delivery_outbox
              (id, subscription_id, bot_id, event_id, event_type, room_id,
               request_body)
          VALUES ($1, $2, $3, $4, 'message', $5, $6)",
    )
    .bind(Uuid::new_v4())
    .bind(subscription)
    .bind(other_bot.to_uuid())
    .bind(Uuid::new_v4())
    .bind(room.to_uuid())
    .bind(br#"{"kind":"message"}"#.as_slice())
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        forged_delivery
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("bot_delivery_outbox_subscription_containment")
    );

    let queue = BotDeliveryOutboxRepo::new(pool.clone());
    let valid_delivery = queue
        .enqueue(
            subscription,
            bot,
            Uuid::new_v4(),
            "message",
            room,
            br#"{"kind":"message"}"#,
        )
        .await
        .unwrap();
    let other_subscription = room_subscription(&bots, other_bot, other, other_room).await;
    let outbox_drift = sqlx::query(
        "UPDATE bot_subscription_delivery_outbox
            SET subscription_id = $2, bot_id = $3
          WHERE id = $1",
    )
    .bind(valid_delivery)
    .bind(other_subscription)
    .bind(other_bot.to_uuid())
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        outbox_drift
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("bot_delivery_outbox_identity_immutable")
    );
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn authorized_workspace_external_quota_is_atomic_under_race() {
    let pool = pool();
    let bots = BotRepo::new(pool.clone());
    let owner = participant(&pool, "bot-quota-owner").await;
    let (workspace, _) = workspace_room(&pool, owner, "bot-quota").await;
    let (seed, _) = workspace_bot(&pool, owner, workspace, "quota-seed").await;
    let (first_bot, _) = workspace_bot(&pool, owner, workspace, "quota-first").await;
    let (second_bot, _) = workspace_bot(&pool, owner, workspace, "quota-second").await;
    let filters = serde_json::json!({ "workspace_id": workspace.to_string() });

    sqlx::query(
        r"INSERT INTO bot_event_subscriptions
              (id, bot_id, event_type, filters, webhook_url, webhook_secret)
          SELECT gen_random_uuid(), $1, 'message',
                 jsonb_build_object('workspace_id', $2::text),
                 'https://example.test/seed', repeat('s', 64)
            FROM generate_series(1, $3)",
    )
    .bind(seed.to_uuid())
    .bind(workspace.to_string())
    .bind(MAX_BOT_EXTERNAL_SUBSCRIPTIONS_PER_WORKSPACE - 1)
    .execute(&pool)
    .await
    .unwrap();

    let first = bots.subscribe_authorized(
        first_bot,
        owner,
        "message",
        &filters,
        Some("https://example.test/first"),
    );
    let second = bots.subscribe_authorized(
        second_bot,
        owner,
        "message",
        &filters,
        Some("https://example.test/second"),
    );
    let (first, second) = tokio::join!(first, second);
    let results = [first, second];
    let successes = results.iter().filter(|result| result.is_ok()).count();
    let conflicts = results
        .iter()
        .filter(|result| matches!(result, Err(Error::Conflict(_))))
        .count();
    assert_eq!(successes, 1);
    assert_eq!(conflicts, 1);
}
