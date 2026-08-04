use super::*;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

async fn new_owner(p: &PgPool) -> ParticipantId {
    let id = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(id.to_uuid())
        .bind(format!("bot-owner-{id}"))
        .execute(p)
        .await
        .expect("insert owner");
    id
}

async fn new_workspace(p: &PgPool, owner: ParticipantId, label: &str) -> WorkspaceId {
    crate::WorkspaceRepo::new(p.clone())
        .create(
            format!("{label}-{owner}"),
            format!("{label}-{owner}").to_ascii_lowercase(),
            owner,
        )
        .await
        .unwrap()
        .id
}

async fn new_workspace_room(
    p: &PgPool,
    owner: ParticipantId,
    label: &str,
) -> (WorkspaceId, aero_common::RoomId) {
    let workspace = new_workspace(p, owner, label).await;
    let room = crate::RoomRepo::new(p.clone())
        .create_in_workspace(
            workspace,
            aero_common::RoomKind::Channel,
            Some(format!("{label}-room")),
            owner,
        )
        .await
        .unwrap()
        .id;
    (workspace, room)
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn bot_token_verifies_then_rotation_invalidates_old() {
    let p = pool();
    let repo = BotRepo::new(p.clone());
    let owner = new_owner(&p).await;
    let bot = repo.create(owner, "ci-bot", None, None).await.unwrap();

    let token = repo.rotate_token(bot).await.unwrap().expect("token minted");
    assert!(
        token.starts_with(BotRepo::TOKEN_PREFIX),
        "minted token carries prefix"
    );

    // The active token resolves to the bot's participant id …
    assert_eq!(repo.verify_token(&token).await.unwrap(), Some(bot));
    // … an unknown token does not.
    assert_eq!(repo.verify_token("bot_unknown").await.unwrap(), None);

    // Rotating mints a fresh token and invalidates the old one (only one hash
    // is stored per bot).
    let token2 = repo.rotate_token(bot).await.unwrap().expect("re-minted");
    assert_ne!(token, token2);
    assert_eq!(
        repo.verify_token(&token).await.unwrap(),
        None,
        "old token dead"
    );
    assert_eq!(repo.verify_token(&token2).await.unwrap(), Some(bot));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn deleted_bot_token_does_not_verify() {
    // The security invariant: once the bot's participant is soft-deleted
    // (`deleted_at` set), its token must stop authenticating — even though the
    // `bots` row and its `token_hash` survive.
    let p = pool();
    let repo = BotRepo::new(p.clone());
    let owner = new_owner(&p).await;
    let bot = repo.create(owner, "doomed-bot", None, None).await.unwrap();

    let token = repo.rotate_token(bot).await.unwrap().expect("token minted");
    assert_eq!(
        repo.verify_token(&token).await.unwrap(),
        Some(bot),
        "active before delete"
    );

    // Soft-delete the bot's participant row (what `delete_participant` does).
    sqlx::query("UPDATE participants SET deleted_at = now() WHERE id = $1")
        .bind(bot.to_uuid())
        .execute(&p)
        .await
        .expect("soft-delete bot participant");

    // The bot row (and its token_hash) is still present …
    let still_there: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM bots WHERE id = $1 AND token_hash IS NOT NULL")
            .bind(bot.to_uuid())
            .fetch_optional(&p)
            .await
            .unwrap();
    assert!(still_there.is_some(), "bots row survives the soft-delete");

    // … but the token must no longer authenticate.
    assert_eq!(
        repo.verify_token(&token).await.unwrap(),
        None,
        "a deleted bot's token must not authenticate"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn matched_subscription_carries_owner_for_send_edge_authorization() {
    let p = pool();
    let repo = BotRepo::new(p.clone());
    let owner = new_owner(&p).await;
    let bot = repo.create(owner, "scoped-bot", None, None).await.unwrap();
    let (workspace, room) = new_workspace_room(&p, owner, "matched-sub").await;
    let filters = serde_json::json!({
        "room_id": room.to_string(),
        "workspace_id": workspace.to_string(),
    });
    let sub = repo
        .subscribe(
            bot,
            "message",
            Some(&filters),
            Some("https://example.test/hook"),
            Some("test-webhook-secret-0123456789abcdef"),
        )
        .await
        .unwrap();

    let (matched, truncated) = repo
        .subscriptions_for_event("message", room, Some(workspace))
        .await
        .unwrap();
    assert!(!truncated);
    let matched = matched
        .into_iter()
        .find(|candidate| candidate.id == sub)
        .expect("new subscription is matched");
    assert_eq!(matched.bot_id, bot);
    assert_eq!(
        matched.owner_id, owner,
        "dispatcher must receive the identity it re-authorizes"
    );
    assert_eq!(
        matched.webhook_secret, "test-webhook-secret-0123456789abcdef",
        "delivery path receives the non-empty signing secret"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn workspace_external_subscription_quota_is_atomic_under_race() {
    let p = pool();
    let repo = BotRepo::new(p.clone());
    let owner = new_owner(&p).await;
    let seed_bot = repo.create(owner, "quota-seed", None, None).await.unwrap();
    let contender_a = repo
        .create(owner, "quota-contender-a", None, None)
        .await
        .unwrap();
    let contender_b = repo
        .create(owner, "quota-contender-b", None, None)
        .await
        .unwrap();
    let workspace = new_workspace(&p, owner, "quota-sub").await;
    let filters = serde_json::json!({ "workspace_id": workspace.to_string() });

    // Model an existing workspace one slot below the cap without spending 1,023
    // application round-trips. The two public writes below must serialize on the
    // workspace advisory lock so exactly one can reserve the final slot.
    sqlx::query(
        r"INSERT INTO bot_event_subscriptions
              (id, bot_id, event_type, filters, webhook_url, webhook_secret)
           SELECT gen_random_uuid(), $1, 'message',
                  jsonb_build_object('workspace_id', $2::TEXT),
                  'https://example.test/hook', repeat('s', 64)
             FROM generate_series(1, $3)",
    )
    .bind(seed_bot.to_uuid())
    .bind(workspace.to_string())
    .bind(MAX_BOT_EXTERNAL_SUBSCRIPTIONS_PER_WORKSPACE - 1)
    .execute(&p)
    .await
    .unwrap();

    let first = repo.subscribe(
        contender_a,
        "message",
        Some(&filters),
        Some("https://example.test/hook-a"),
        Some("quota-test-secret-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
    );
    let second = repo.subscribe(
        contender_b,
        "message",
        Some(&filters),
        Some("https://example.test/hook-b"),
        Some("quota-test-secret-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
    );
    let (first, second) = tokio::join!(first, second);
    let mut succeeded = 0;
    let mut quota_rejected = 0;
    for result in [first, second] {
        match result {
            Ok(_) => succeeded += 1,
            Err(BotWriteError::WorkspaceSubscriptionQuotaExceeded) => quota_rejected += 1,
            Err(error) => panic!("unexpected subscription result: {error}"),
        }
    }

    let count = sqlx::query_scalar::<_, i64>(
        r"SELECT COUNT(*)
            FROM bot_event_subscriptions
           WHERE webhook_url IS NOT NULL
             AND scope_workspace_id = $1",
    )
    .bind(workspace.to_string())
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(succeeded, 1);
    assert_eq!(quota_rejected, 1);
    assert_eq!(count, MAX_BOT_EXTERNAL_SUBSCRIPTIONS_PER_WORKSPACE);

    sqlx::query("DELETE FROM bots WHERE owner_id = $1")
        .bind(owner.to_uuid())
        .execute(&p)
        .await
        .unwrap();
    sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
        .bind(vec![
            seed_bot.to_uuid(),
            contender_a.to_uuid(),
            contender_b.to_uuid(),
        ])
        .execute(&p)
        .await
        .unwrap();
    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(workspace.to_uuid())
        .execute(&p)
        .await
        .unwrap();
    sqlx::query("DELETE FROM participants WHERE id = $1")
        .bind(owner.to_uuid())
        .execute(&p)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn bot_webhook_secret_is_hidden_and_owner_scoped_rotation_replaces_it() {
    let p = pool();
    let repo = BotRepo::new(p.clone());
    let owner = new_owner(&p).await;
    let bot = repo.create(owner, "secret-bot", None, None).await.unwrap();
    let (workspace, room) = new_workspace_room(&p, owner, "secret-sub").await;
    let filters = serde_json::json!({
        "room_id": room.to_string(),
        "workspace_id": workspace.to_string(),
    });
    let sub = repo
        .subscribe(
            bot,
            "message",
            Some(&filters),
            Some("https://example.test/hook"),
            Some("initial-webhook-secret-0123456789abcdef"),
        )
        .await
        .unwrap();

    let listed = repo.list_subscriptions(bot).await.unwrap();
    let encoded = serde_json::to_value(
        listed
            .iter()
            .find(|candidate| candidate.id == sub)
            .expect("subscription is listed"),
    )
    .unwrap();
    assert!(
        encoded.get("webhook_secret").is_none(),
        "list projection must never expose the signing secret"
    );

    let other_owner = new_owner(&p).await;
    let other_bot = repo
        .create(other_owner, "other-secret-bot", None, None)
        .await
        .unwrap();
    assert!(
        !repo
            .rotate_subscription_secret(sub, other_bot, "attacker-webhook-secret-0123456789abcdef",)
            .await
            .unwrap(),
        "a different bot cannot rotate this subscription"
    );
    assert!(repo
        .rotate_subscription_secret(sub, bot, "rotated-webhook-secret-0123456789abcdef",)
        .await
        .unwrap());
    let (matched, truncated) = repo
        .subscriptions_for_event("message", room, Some(workspace))
        .await
        .unwrap();
    assert!(!truncated);
    let matched = matched
        .into_iter()
        .find(|candidate| candidate.id == sub)
        .unwrap();
    assert_eq!(
        matched.webhook_secret,
        "rotated-webhook-secret-0123456789abcdef"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn delivery_log_records_and_lists_by_subscription_and_bot() {
    // record_delivery persists a row; the two list paths surface it, newest
    // first, scoped correctly. Exercises both the success and failure shapes.
    let p = pool();
    let repo = BotRepo::new(p.clone());
    let owner = new_owner(&p).await;
    let bot = repo
        .create(owner, "delivery-bot", None, None)
        .await
        .unwrap();
    let (workspace, room) = new_workspace_room(&p, owner, "delivery-log").await;
    let filters = serde_json::json!({
        "room_id": room.to_string(),
        "workspace_id": workspace.to_string(),
    });
    let sub = repo
        .subscribe(
            bot,
            "message",
            Some(&filters),
            Some("https://example.test/hook"),
            Some("test-webhook-secret-0123456789abcdef"),
        )
        .await
        .unwrap();

    // A delivered (2xx) attempt …
    repo.record_delivery(
        sub,
        bot,
        "message",
        DeliveryStatus::Delivered,
        Some(200),
        None,
    )
    .await
    .unwrap();
    // … then a failed (5xx) attempt with an error excerpt.
    repo.record_delivery(
        sub,
        bot,
        "message",
        DeliveryStatus::Failed,
        Some(503),
        Some("upstream unavailable"),
    )
    .await
    .unwrap();

    let by_sub = repo
        .list_deliveries_for_subscription(sub, 100)
        .await
        .unwrap();
    assert_eq!(
        by_sub.len(),
        2,
        "both attempts recorded for the subscription"
    );
    // Newest first: the failed 503 is the most recent.
    assert_eq!(by_sub[0].status, "failed");
    assert_eq!(by_sub[0].http_status, Some(503));
    assert_eq!(by_sub[0].error.as_deref(), Some("upstream unavailable"));
    assert_eq!(by_sub[1].status, "delivered");
    assert_eq!(by_sub[1].http_status, Some(200));
    assert!(by_sub[1].error.is_none());

    let by_bot = repo.list_deliveries_for_bot(bot, 100).await.unwrap();
    assert_eq!(
        by_bot.len(),
        2,
        "owner-scoped listing sees both via denormalized bot_id"
    );

    // A different bot sees none of these.
    let other_owner = new_owner(&p).await;
    let other_bot = repo
        .create(other_owner, "other-bot", None, None)
        .await
        .unwrap();
    assert!(
        repo.list_deliveries_for_bot(other_bot, 100)
            .await
            .unwrap()
            .is_empty(),
        "delivery log is bot-scoped"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn delivery_log_cascades_on_subscription_delete() {
    // The FK is ON DELETE CASCADE: removing a subscription removes its
    // delivery rows, so a deleted subscription leaves no orphan log.
    let p = pool();
    let repo = BotRepo::new(p.clone());
    let owner = new_owner(&p).await;
    let bot = repo.create(owner, "cascade-bot", None, None).await.unwrap();
    let (workspace, room) = new_workspace_room(&p, owner, "delivery-cascade").await;
    let filters = serde_json::json!({
        "room_id": room.to_string(),
        "workspace_id": workspace.to_string(),
    });
    let sub = repo
        .subscribe(
            bot,
            "reaction",
            Some(&filters),
            Some("https://example.test/hook"),
            Some("test-webhook-secret-0123456789abcdef"),
        )
        .await
        .unwrap();
    repo.record_delivery(
        sub,
        bot,
        "reaction",
        DeliveryStatus::Failed,
        None,
        Some("dns"),
    )
    .await
    .unwrap();
    // Transport error ⇒ http_status NULL.
    let before = repo
        .list_deliveries_for_subscription(sub, 100)
        .await
        .unwrap();
    assert_eq!(before.len(), 1);
    assert!(
        before[0].http_status.is_none(),
        "transport error has no http status"
    );

    assert!(repo.delete_subscription(sub, bot).await.unwrap());
    let after = repo
        .list_deliveries_for_subscription(sub, 100)
        .await
        .unwrap();
    assert!(
        after.is_empty(),
        "delivery rows cascade-deleted with the subscription"
    );
}
