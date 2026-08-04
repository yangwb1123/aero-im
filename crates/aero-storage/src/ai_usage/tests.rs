use super::*;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_lazy(&url)
        .expect("well-formed DATABASE_URL")
}

fn charge(workspace_id: Option<Uuid>, kind: &str, micros: i64) -> UsageCharge {
    UsageCharge {
        usage_id: Uuid::new_v4(),
        workspace_id,
        kind: kind.into(),
        cost_micros: micros,
        outcome_kind: None,
    }
}

#[test]
fn backoff_is_bounded_and_exponential() {
    assert_eq!(ai_usage_backoff(1), Duration::seconds(1));
    assert_eq!(ai_usage_backoff(2), Duration::seconds(2));
    assert_eq!(ai_usage_backoff(3), Duration::seconds(4));
    assert_eq!(
        ai_usage_backoff(i32::MAX),
        Duration::seconds(MAX_BACKOFF_SECONDS)
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn stable_reservation_and_settlement_are_exactly_once() {
    let repo = AiUsageRepo::new(pool());
    let ws = Uuid::new_v4();
    let event = charge(Some(ws), "answer", 4_500);
    let now = OffsetDateTime::now_utc();
    let UsageReserveOutcome::Reserved(reservation) = repo
        .reserve(&event, now, Duration::minutes(5))
        .await
        .unwrap()
    else {
        panic!("first caller owns the reservation");
    };
    assert_eq!(
        repo.reserve(&event, now, Duration::minutes(5))
            .await
            .unwrap(),
        UsageReserveOutcome::InFlight
    );
    assert_eq!(
        repo.finalize(reservation, 5_000, None, now).await.unwrap(),
        UsageFinalizeOutcome::Finalized
    );
    assert_eq!(
        repo.finalize(reservation, 5_000, None, now).await.unwrap(),
        UsageFinalizeOutcome::Duplicate
    );
    let claimed = repo
        .claim_due(now, Duration::minutes(1), 100)
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.usage_id == event.usage_id)
        .expect("event claimed");
    assert!(repo
        .settle(claimed.usage_id, claimed.claim_token)
        .await
        .unwrap());
    let pending_for_event: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint
           FROM ai_usage_outbox
          WHERE usage_id = $1 AND status IN ('reserved', 'ready')",
    )
    .bind(event.usage_id)
    .fetch_one(&repo.pool)
    .await
    .unwrap();
    assert_eq!(pending_for_event, 0);

    // An ambiguous caller retry sees the completed outbox record and cannot
    // make a second provider call or ledger row.
    assert_eq!(
        repo.reserve(&event, now, Duration::minutes(5))
            .await
            .unwrap(),
        UsageReserveOutcome::Finalized(None)
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ai_usage_ledger WHERE usage_id = $1")
        .bind(event.usage_id)
        .fetch_one(&repo.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn finalized_outcome_replays_after_business_commit_failure() {
    let repo = AiUsageRepo::new(pool());
    let mut event = charge(Some(Uuid::new_v4()), "summarize", 5_000);
    event.outcome_kind = Some("anthropic_completion_v1".into());
    let now = OffsetDateTime::now_utc();
    let UsageReserveOutcome::Reserved(reservation) = repo
        .reserve(&event, now, Duration::minutes(5))
        .await
        .unwrap()
    else {
        panic!("first caller owns the provider reservation");
    };
    let saved = UsageOutcome {
        kind: "anthropic_completion_v1".into(),
        payload: serde_json::json!({
            "text": "durable digest",
            "usage": {"input_tokens": 100, "output_tokens": 10}
        }),
    };
    assert_eq!(
        repo.finalize(reservation, 5_100, Some(&saved), now)
            .await
            .unwrap(),
        UsageFinalizeOutcome::Finalized
    );

    // Model save_prepared_summary failing after the provider result and
    // charge committed. The stable retry receives the result and is not
    // granted a second provider fence.
    assert_eq!(
        repo.reserve(&event, now, Duration::minutes(5))
            .await
            .unwrap(),
        UsageReserveOutcome::Finalized(Some(saved))
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn stale_claim_cannot_ack_after_reclaim() {
    let repo = AiUsageRepo::new(pool());
    let event = charge(None, "moderate", 100);
    let now = OffsetDateTime::now_utc();
    let UsageReserveOutcome::Reserved(reservation) = repo
        .reserve(&event, now, Duration::minutes(5))
        .await
        .unwrap()
    else {
        panic!("reservation");
    };
    repo.finalize(reservation, event.cost_micros, None, now)
        .await
        .unwrap();
    let first = repo
        .claim_due(now, Duration::seconds(1), 100)
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.usage_id == event.usage_id)
        .unwrap();
    let second = repo
        .claim_due(now + Duration::seconds(2), Duration::minutes(1), 100)
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.usage_id == event.usage_id)
        .unwrap();
    assert_ne!(first.claim_token, second.claim_token);
    assert!(!repo
        .settle(first.usage_id, first.claim_token)
        .await
        .unwrap());
    assert!(repo
        .settle(second.usage_id, second.claim_token)
        .await
        .unwrap());
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn ledger_insert_before_ack_is_deduplicated_on_retry() {
    let repo = AiUsageRepo::new(pool());
    let event = charge(Some(Uuid::new_v4()), "embed", 20);
    let now = OffsetDateTime::now_utc();
    let UsageReserveOutcome::Reserved(reservation) = repo
        .reserve(&event, now, Duration::minutes(5))
        .await
        .unwrap()
    else {
        panic!("reservation");
    };
    repo.finalize(reservation, event.cost_micros, None, now)
        .await
        .unwrap();
    let claim = repo
        .claim_due(now, Duration::minutes(1), 100)
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.usage_id == event.usage_id)
        .unwrap();

    // Simulate the classic crash window: the ledger write was visible but
    // the relay did not observe/record its outbox acknowledgement.
    sqlx::query(
        r"INSERT INTO ai_usage_ledger
                    (usage_id, workspace_id, kind, cost_micros, created_at)
              VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(event.usage_id)
    .bind(event.workspace_id)
    .bind(&event.kind)
    .bind(event.cost_micros)
    .bind(claim.created_at)
    .execute(&repo.pool)
    .await
    .unwrap();

    assert!(repo
        .settle(claim.usage_id, claim.claim_token)
        .await
        .unwrap());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ai_usage_ledger WHERE usage_id = $1")
        .bind(event.usage_id)
        .fetch_one(&repo.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn reused_id_with_different_payload_is_rejected() {
    let repo = AiUsageRepo::new(pool());
    let event = charge(None, "answer", 123);
    let now = OffsetDateTime::now_utc();
    repo.reserve(&event, now, Duration::minutes(5))
        .await
        .unwrap();
    let mut conflicting = event.clone();
    conflicting.kind = "summarize".into();
    assert!(matches!(
        repo.reserve(&conflicting, now, Duration::minutes(5)).await,
        Err(sqlx::Error::Protocol(_))
    ));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn cancelled_provider_failure_can_reserve_a_fresh_retry() {
    let repo = AiUsageRepo::new(pool());
    let event = charge(None, "transcribe_estimate", 6_000);
    let now = OffsetDateTime::now_utc();
    let UsageReserveOutcome::Reserved(first) = repo
        .reserve(&event, now, Duration::minutes(5))
        .await
        .unwrap()
    else {
        panic!("first reservation");
    };
    assert!(repo.cancel(first, now).await.unwrap());
    assert!(repo.cancel(first, now).await.unwrap());
    let UsageReserveOutcome::Reserved(second) = repo
        .reserve(&event, now, Duration::minutes(5))
        .await
        .unwrap()
    else {
        panic!("retry reservation");
    };
    assert_ne!(first.reservation_token, second.reservation_token);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn expired_ambiguous_reservation_is_conservatively_finalized() {
    let repo = AiUsageRepo::new(pool());
    let event = charge(None, "answer", 5_000);
    let now = OffsetDateTime::now_utc();
    repo.reserve(&event, now, Duration::seconds(1))
        .await
        .unwrap();
    let recovered = repo
        .recover_expired_reservations(now + Duration::seconds(2), 100)
        .await
        .unwrap();
    assert!(recovered.iter().any(|row| row.usage_id == event.usage_id));
    assert_eq!(
        repo.reserve(&event, now + Duration::seconds(2), Duration::minutes(5))
            .await
            .unwrap(),
        UsageReserveOutcome::Finalized(None)
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn batch_insert_then_summary_rolls_up_per_kind() {
    let repo = AiUsageRepo::new(pool());
    let ws = Uuid::new_v4();
    let other = Uuid::new_v4();
    repo.insert_batch(&[
        UsageRow {
            workspace_id: Some(ws),
            kind: "answer".into(),
            cost_micros: 4_500,
        },
        UsageRow {
            workspace_id: Some(ws),
            kind: "answer".into(),
            cost_micros: 1_500,
        },
        UsageRow {
            workspace_id: Some(ws),
            kind: "embed".into(),
            cost_micros: 100,
        },
        UsageRow {
            workspace_id: Some(other),
            kind: "answer".into(),
            cost_micros: 9_000,
        },
        UsageRow {
            workspace_id: None,
            kind: "moderate".into(),
            cost_micros: 200,
        },
    ])
    .await
    .unwrap();
    let summary = repo
        .summary_since(ws, OffsetDateTime::UNIX_EPOCH)
        .await
        .unwrap();
    assert_eq!(summary.len(), 2);
    assert_eq!(summary[0].kind, "answer");
    assert_eq!(summary[0].calls, 2);
    assert_eq!(summary[0].cost_micros, 6_000);
    assert_eq!(summary[1].kind, "embed");
    assert_eq!(repo.insert_batch(&[]).await.unwrap(), 0);
}
