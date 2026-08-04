use aero_common::{Error, ParticipantId, RoomId, WebhookDeliveryId, WebhookId, WorkspaceId};
use sqlx::PgPool;

use super::super::db_tests::{begin_new_claim, fixture, pool, record_claim};
use super::*;

#[derive(Clone, Copy)]
struct Scope {
    webhook: WebhookId,
    room: RoomId,
    workspace: WorkspaceId,
    owner: ParticipantId,
}

async fn scope(pool: &PgPool) -> Scope {
    let webhook = fixture(pool).await;
    let (room, workspace, owner) = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, uuid::Uuid)>(
        r"SELECT hook.room_id, room.workspace_id, hook.created_by
                FROM outgoing_webhooks hook
                JOIN rooms room ON room.id = hook.room_id
               WHERE hook.id = $1",
    )
    .bind(webhook.to_uuid())
    .fetch_one(pool)
    .await
    .unwrap();
    Scope {
        webhook,
        room: RoomId::from_uuid(room),
        workspace: WorkspaceId::from_uuid(workspace),
        owner: ParticipantId::from_uuid(owner),
    }
}

async fn dead_delivery(repo: &WebhookDeliveryRepo, scope: Scope, event: &str) -> WebhookDelivery {
    let claim = record_claim(repo, scope.webhook, event).await;
    assert_eq!(begin_new_claim(repo, claim).await, 1);
    assert!(repo
        .mark_dead(
            claim.id,
            claim.claim_token,
            Some(422),
            "terminal test failure"
        )
        .await
        .unwrap());
    repo.get(claim.id).await.unwrap().unwrap()
}

async fn audit_count(pool: &PgPool, workspace: WorkspaceId, id: WebhookDeliveryId) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*)
           FROM audit_events
          WHERE workspace_id = $1
            AND action = $2
            AND target = $3",
    )
    .bind(workspace.to_uuid())
    .bind(REQUEUE_AUDIT_ACTION)
    .bind(id.to_string())
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn add_member(pool: &PgPool, workspace: WorkspaceId) -> ParticipantId {
    let member = ParticipantId::new();
    sqlx::query(
        "INSERT INTO participants (id, kind, display_name)
         VALUES ($1, 'human', $2)",
    )
    .bind(member.to_uuid())
    .bind(format!("webhook-dlq-member-{member}"))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, 'member')",
    )
    .bind(workspace.to_uuid())
    .bind(member.to_uuid())
    .execute(pool)
    .await
    .unwrap();
    member
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn webhook_delivery_admin_reads_are_tenant_scoped_and_role_aware() {
    let pool = pool();
    let repo = WebhookDeliveryRepo::new(pool.clone());
    let left = scope(&pool).await;
    let right = scope(&pool).await;
    let delivery = dead_delivery(&repo, left, "admin-read-scope").await;

    let listed = repo
        .list_for_webhook_authorized(left.webhook, left.owner, Some(10))
        .await
        .unwrap();
    assert!(listed.iter().any(|row| row.id == delivery.id));
    let dead = repo
        .list_dead_for_webhook_authorized(left.webhook, left.owner, Some(10))
        .await
        .unwrap();
    assert!(dead.iter().any(|row| row.id == delivery.id));
    assert_eq!(
        repo.get_authorized(delivery.id, left.owner)
            .await
            .unwrap()
            .id,
        delivery.id
    );

    assert!(matches!(
        repo.list_for_webhook_authorized(left.webhook, right.owner, Some(10))
            .await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        repo.get_authorized(delivery.id, right.owner).await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        repo.get_authorized(WebhookDeliveryId::new(), left.owner)
            .await,
        Err(Error::NotFound(_))
    ));

    let member = add_member(&pool, left.workspace).await;
    assert!(matches!(
        repo.list_dead_for_webhook_authorized(left.webhook, member, Some(10))
            .await,
        Err(Error::Forbidden(_))
    ));
    assert!(matches!(
        repo.get_authorized(delivery.id, member).await,
        Err(Error::Forbidden(_))
    ));
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn webhook_delivery_requeue_fences_state_claim_generation_and_audit() {
    let pool = pool();
    let repo = WebhookDeliveryRepo::new(pool.clone());
    let scope = scope(&pool).await;

    let live = record_claim(&repo, scope.webhook, "admin-nondead").await;
    assert!(matches!(
        repo.requeue_authorized(live.id, scope.owner).await,
        Err(Error::NotFound(_))
    ));
    let live_after = repo.get(live.id).await.unwrap().unwrap();
    assert_eq!(live_after.status, "pending");
    assert_eq!(live_after.claim_token, live.claim_token);
    assert_eq!(audit_count(&pool, scope.workspace, live.id).await, 0);

    let dead = dead_delivery(&repo, scope, "admin-claim-fence").await;
    let stale_token = dead.claim_token;
    repo.requeue_authorized(dead.id, scope.owner).await.unwrap();
    let requeued = repo.get(dead.id).await.unwrap().unwrap();
    assert_eq!(requeued.status, "failed");
    assert_eq!(requeued.attempts, 0);
    assert!(requeued.next_attempt_at.is_some());
    assert_ne!(requeued.claim_token, stale_token);
    assert_eq!(audit_count(&pool, scope.workspace, dead.id).await, 1);

    let claimed = repo
        .claim_due(
            time::OffsetDateTime::now_utc() + time::Duration::seconds(1),
            10,
        )
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.id == dead.id)
        .expect("requeued row is immediately claimable");
    assert!(claimed.next_attempt_at.is_none());
    assert_ne!(claimed.claim_token, stale_token);
    assert!(
        !repo
            .mark_delivered(dead.id, stale_token, 204)
            .await
            .unwrap(),
        "pre-requeue worker cannot settle the new claim generation"
    );
    assert!(matches!(
        repo.requeue_authorized(dead.id, scope.owner).await,
        Err(Error::NotFound(_))
    ));
    assert_eq!(
        audit_count(&pool, scope.workspace, dead.id).await,
        1,
        "a rejected second requeue must not append an audit event"
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn webhook_delivery_requeue_waits_for_revoke_and_then_writes_nothing() {
    let pool = pool();
    let repo = WebhookDeliveryRepo::new(pool.clone());
    let scope = scope(&pool).await;
    let dead = dead_delivery(&repo, scope, "admin-revoke-race").await;

    let mut revoke = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(scope.workspace.to_uuid())
        .execute(&mut *revoke)
        .await
        .unwrap();
    sqlx::query("UPDATE outgoing_webhooks SET revoked_at = now() WHERE id = $1")
        .bind(scope.webhook.to_uuid())
        .execute(&mut *revoke)
        .await
        .unwrap();

    let raced_repo = repo.clone();
    let mut raced =
        tokio::spawn(async move { raced_repo.requeue_authorized(dead.id, scope.owner).await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut raced)
            .await
            .is_err(),
        "requeue must wait behind the workspace/revocation transaction"
    );
    revoke.commit().await.unwrap();

    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(3), raced)
            .await
            .expect("requeue resumed")
            .expect("requeue task"),
        Err(Error::NotFound(_))
    ));
    let after = repo.get(dead.id).await.unwrap().unwrap();
    assert_eq!(after.status, "dead");
    assert_eq!(after.attempts, dead.attempts);
    assert_eq!(after.claim_token, dead.claim_token);
    assert_eq!(audit_count(&pool, scope.workspace, dead.id).await, 0);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn webhook_delivery_requeue_rolls_back_when_audit_append_fails() {
    let pool = pool();
    let repo = WebhookDeliveryRepo::new(pool.clone());
    let scope = scope(&pool).await;
    let dead = dead_delivery(&repo, scope, "admin-audit-rollback").await;
    let suffix = dead.id.to_uuid().simple().to_string();
    let function_name = format!("aero_test_fail_webhook_requeue_audit_{suffix}");
    let trigger_name = format!("aero_test_fail_webhook_requeue_audit_trg_{suffix}");
    let create_function = format!(
        "CREATE FUNCTION {function_name}()
         RETURNS trigger
         LANGUAGE plpgsql
         AS $$
         BEGIN
             IF NEW.action = '{REQUEUE_AUDIT_ACTION}'
                AND NEW.target = '{}' THEN
                 RAISE EXCEPTION 'injected webhook requeue audit failure';
             END IF;
             RETURN NEW;
         END
         $$",
        dead.id
    );
    sqlx::query(&create_function).execute(&pool).await.unwrap();
    let create_trigger = format!(
        "CREATE TRIGGER {trigger_name}
         BEFORE INSERT ON audit_events
         FOR EACH ROW
         EXECUTE FUNCTION {function_name}()"
    );
    sqlx::query(&create_trigger).execute(&pool).await.unwrap();

    let result = repo.requeue_authorized(dead.id, scope.owner).await;

    let drop_trigger = format!("DROP TRIGGER {trigger_name} ON audit_events");
    sqlx::query(&drop_trigger).execute(&pool).await.unwrap();
    let drop_function = format!("DROP FUNCTION {function_name}()");
    sqlx::query(&drop_function).execute(&pool).await.unwrap();

    assert!(matches!(result, Err(Error::Database(_))));
    let after = repo.get(dead.id).await.unwrap().unwrap();
    assert_eq!(after.status, "dead");
    assert_eq!(after.attempts, dead.attempts);
    assert_eq!(after.claim_token, dead.claim_token);
    assert_eq!(audit_count(&pool, scope.workspace, dead.id).await, 0);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL with migrations applied"]
async fn webhook_delivery_identity_is_immutable_but_delete_cascade_is_preserved() {
    let pool = pool();
    let repo = WebhookDeliveryRepo::new(pool.clone());
    let left = scope(&pool).await;
    let right = scope(&pool).await;
    let dead = dead_delivery(&repo, left, "admin-identity-fence").await;

    let delivery_move = sqlx::query(
        "UPDATE webhook_delivery_log
            SET webhook_id = $2
          WHERE id = $1",
    )
    .bind(dead.id.to_uuid())
    .bind(right.webhook.to_uuid())
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        delivery_move
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("webhook_delivery_webhook_identity_immutable")
    );

    let hook_move = sqlx::query("UPDATE outgoing_webhooks SET room_id = $2 WHERE id = $1")
        .bind(left.webhook.to_uuid())
        .bind(right.room.to_uuid())
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(
        hook_move
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("outgoing_webhook_room_identity_immutable")
    );

    let zombie = sqlx::query(
        "UPDATE webhook_delivery_log
            SET status = 'failed', next_attempt_at = NULL
          WHERE id = $1",
    )
    .bind(dead.id.to_uuid())
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(
        zombie
            .as_database_error()
            .and_then(|error| error.constraint()),
        Some("webhook_delivery_retry_deadline_chk")
    );

    sqlx::query("DELETE FROM outgoing_webhooks WHERE id = $1")
        .bind(left.webhook.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    let retained =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM webhook_delivery_log WHERE id = $1")
            .bind(dead.id.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        retained, 0,
        "0210 UPDATE guards must preserve the historical delete cascade"
    );
}
