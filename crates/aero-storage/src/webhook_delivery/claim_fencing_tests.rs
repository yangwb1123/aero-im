use super::db_tests::{actor_for_hook, begin_new_claim, begin_retry, fixture, pool, record_claim};
use super::*;

/// Claim A may sit in a limiter queue beyond the stale window. Claim B then
/// recovers the row; A's late `begin_attempt` is fenced before HTTP, while B
/// can begin and is the only call charged.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn webhook_delivery_reclaims_stale_pending() {
    let pool = pool();
    let repo = WebhookDeliveryRepo::new(pool.clone());
    let hook = fixture(&pool).await;
    let first = record_claim(&repo, hook, "evt-stale-pending").await;
    let id = first.id;

    let before_cutoff = OffsetDateTime::now_utc() + Duration::seconds(PENDING_STALE_AFTER_SECS - 1);
    assert!(
        repo.claim_due(before_cutoff, 10)
            .await
            .unwrap()
            .iter()
            .all(|delivery| delivery.id != id),
        "a live in-flight row must not be reclaimed"
    );

    let after_cutoff = OffsetDateTime::now_utc() + Duration::seconds(PENDING_STALE_AFTER_SECS + 1);
    let reclaimed = repo
        .claim_due(after_cutoff, 10)
        .await
        .unwrap()
        .into_iter()
        .find(|delivery| delivery.id == id)
        .expect("stale pending row is reclaimed");
    assert_eq!(reclaimed.status, "pending");
    assert_eq!(reclaimed.attempts, 0);
    assert_ne!(reclaimed.claim_token, first.claim_token);
    assert_eq!(
        repo.begin_attempt(id, first.claim_token).await.unwrap(),
        None,
        "queued stale worker A cannot begin HTTP"
    );
    assert_eq!(
        repo.begin_attempt(id, reclaimed.claim_token).await.unwrap(),
        Some(1),
        "successor B begins the first real HTTP call"
    );
}

/// A dead row can be admin-requeued with `attempts` reset to zero. The token
/// still prevents an old worker from exploiting that numeric ABA and
/// settling/releasing/deferring the fresh generation.
#[tokio::test]
#[ignore = "requires live Postgres"]
async fn webhook_delivery_claim_token_fences_dead_requeue_aba() {
    let pool = pool();
    let repo = WebhookDeliveryRepo::new(pool.clone());
    let hook = fixture(&pool).await;
    let stale = record_claim(&repo, hook, "evt-generation-fence").await;
    let id = stale.id;
    assert_eq!(begin_new_claim(&repo, stale).await, 1);
    assert!(repo
        .mark_dead(id, stale.claim_token, Some(400), "non-retryable")
        .await
        .unwrap());
    let actor = actor_for_hook(&pool, hook).await;
    repo.requeue_authorized(id, actor).await.unwrap();
    let after_requeue = repo.get(id).await.unwrap().unwrap();
    assert_eq!(after_requeue.attempts, 0);
    assert_ne!(after_requeue.claim_token, stale.claim_token);

    let successor = repo
        .claim_due(OffsetDateTime::now_utc() + Duration::days(365), MAX_PAGE)
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.id == id)
        .expect("requeued delivery receives a fresh claim");
    assert_eq!(successor.attempts, 0);
    assert_ne!(successor.claim_token, stale.claim_token);

    assert!(
        !repo
            .mark_delivered(id, stale.claim_token, 200)
            .await
            .unwrap(),
        "pre-DLQ success cannot settle the requeued generation"
    );
    assert!(
        !repo
            .mark_failed_with_backoff(id, stale.claim_token, 1, Some(500), "late failure")
            .await
            .unwrap(),
        "pre-DLQ failure cannot settle the requeued generation"
    );
    assert!(
        !repo.release_claim(id, stale.claim_token).await.unwrap(),
        "pre-DLQ release cannot touch the successor"
    );
    assert!(
        !repo
            .defer_claim(
                id,
                stale.claim_token,
                OffsetDateTime::now_utc() + Duration::hours(1),
                "late breaker",
            )
            .await
            .unwrap(),
        "pre-DLQ defer cannot postpone the successor"
    );

    assert_eq!(begin_retry(&repo, &successor).await, 1);
    assert!(repo
        .mark_delivered(id, successor.claim_token, 204)
        .await
        .unwrap());
    let row = repo.get(id).await.unwrap().expect("row remains");
    assert_eq!(row.status, "delivered");
    assert_eq!(row.attempts, 1);
    assert_eq!(row.last_status_code, Some(204));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn webhook_delivery_enqueue_materializes_due_without_attempt() {
    let pool = pool();
    let repo = WebhookDeliveryRepo::new(pool.clone());
    let hook = fixture(&pool).await;

    assert!(repo
        .enqueue(
            hook,
            Some("evt-enqueue"),
            b"{\"kind\":\"stream_live\"}",
            &[("Content-Type".to_owned(), "application/json".to_owned())],
        )
        .await
        .unwrap());
    assert!(
        !repo
            .enqueue(hook, Some("evt-enqueue"), b"changed", &[])
            .await
            .unwrap(),
        "materialization is idempotent"
    );
    let due = repo
        .claim_due(OffsetDateTime::now_utc() + Duration::seconds(1), 10)
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.event_id.as_deref() == Some("evt-enqueue"))
        .expect("enqueued row is immediately due");
    assert_eq!(due.status, "pending");
    assert_eq!(due.attempts, 0);
    assert_eq!(
        due.request_body.as_deref(),
        Some(b"{\"kind\":\"stream_live\"}".as_slice())
    );
}
