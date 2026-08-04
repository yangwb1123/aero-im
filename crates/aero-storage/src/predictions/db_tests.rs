use super::*;
use crate::ChannelPointsRepo;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

async fn participant(p: &PgPool, label: &str) -> ParticipantId {
    let id = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
        .bind(id.to_uuid())
        .bind(format!("pred-{label}-{id}"))
        .execute(p)
        .await
        .expect("insert participant");
    id
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn predictions_stake_debits_and_rejects_dupes() {
    let p = pool();
    let cp = ChannelPointsRepo::new(p.clone());
    let repo = PredictionRepo::new(p.clone());
    let stream = Ulid::new();
    let creator = participant(&p, "creator").await;
    let viewer = participant(&p, "viewer").await;
    let poor = participant(&p, "poor").await;

    // Fund the viewer with the creator.
    cp.earn(viewer, creator, 100, "watch").await.unwrap();

    let pred = repo
        .create_prediction(
            stream,
            creator,
            "Will they win?",
            &["yes".into(), "no".into()],
            None,
        )
        .await
        .unwrap();
    assert_eq!(pred.outcomes.len(), 2);
    assert_eq!(pred.status, "open");

    // Stake 40 on outcome 0 -> balance drops to 60.
    repo.stake(pred.id, viewer, 0, 40).await.unwrap();
    assert_eq!(cp.balance(viewer, creator).await.unwrap(), 60);
    let stake_uuid: Uuid = sqlx::query_scalar(
        r"SELECT id
                FROM prediction_stakes
               WHERE prediction_id = $1
                 AND viewer_id = $2",
    )
    .bind(pred.id.to_uuid())
    .bind(viewer.to_uuid())
    .fetch_one(&p)
    .await
    .unwrap();
    let stake_reason = format!("prediction_stake:{stake_uuid}");
    let debit_history: Vec<i64> = sqlx::query_scalar(
        r"SELECT delta
                FROM points_earn_history
               WHERE viewer_id = $1
                 AND creator_id = $2
                 AND reason = $3",
    )
    .bind(viewer.to_uuid())
    .bind(creator.to_uuid())
    .bind(&stake_reason)
    .fetch_all(&p)
    .await
    .unwrap();
    assert_eq!(
        debit_history,
        vec![-40],
        "one stable negative history row reconstructs the stake debit"
    );

    // A second stake (any outcome) is rejected — one stake per viewer.
    let err = repo.stake(pred.id, viewer, 1, 10).await.unwrap_err();
    assert!(matches!(err, StakeError::AlreadyStaked));
    assert_eq!(
        cp.balance(viewer, creator).await.unwrap(),
        60,
        "no extra debit"
    );
    let debit_history_count: i64 = sqlx::query_scalar(
        r"SELECT COUNT(*)
                FROM points_earn_history
               WHERE viewer_id = $1
                 AND creator_id = $2
                 AND reason = $3",
    )
    .bind(viewer.to_uuid())
    .bind(creator.to_uuid())
    .bind(stake_reason)
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(
        debit_history_count, 1,
        "a duplicate stake cannot duplicate debit history"
    );

    // A staker with no points cannot stake.
    let err = repo.stake(pred.id, poor, 0, 5).await.unwrap_err();
    assert!(matches!(err, StakeError::InsufficientPoints));

    // A bad outcome index is rejected.
    let other = participant(&p, "other").await;
    cp.earn(other, creator, 10, "watch").await.unwrap();
    let err = repo.stake(pred.id, other, 9, 5).await.unwrap_err();
    assert!(matches!(err, StakeError::BadOutcome));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn predictions_resolve_pays_winners_proportionally() {
    let p = pool();
    let cp = ChannelPointsRepo::new(p.clone());
    let repo = PredictionRepo::new(p.clone());
    let stream = Ulid::new();
    let creator = participant(&p, "creator").await;
    let alice = participant(&p, "alice").await; // bets on the WINNER (0)
    let bob = participant(&p, "bob").await; // bets on the LOSER (1)

    cp.earn(alice, creator, 100, "watch").await.unwrap();
    cp.earn(bob, creator, 200, "watch").await.unwrap();

    let pred = repo
        .create_prediction(stream, creator, "Pick one", &["A".into(), "B".into()], None)
        .await
        .unwrap();

    // Alice stakes 100 on A (idx 0); Bob stakes 200 on B (idx 1).
    repo.stake(pred.id, alice, 0, 100).await.unwrap();
    repo.stake(pred.id, bob, 1, 200).await.unwrap();
    assert_eq!(cp.balance(alice, creator).await.unwrap(), 0);
    assert_eq!(cp.balance(bob, creator).await.unwrap(), 0);

    // Resolve to A: total_pool = 300, winning_pool = 100 (just Alice).
    // Alice's payout = floor(100 * 300 / 100) = 300; Bob gets 0.
    let payouts = repo.resolve(pred.id, 0).await.unwrap();
    assert_eq!(payouts.len(), 1);
    assert_eq!(payouts[0], (alice, 300));
    assert_eq!(
        cp.balance(alice, creator).await.unwrap(),
        300,
        "winner paid the full pool"
    );
    assert_eq!(
        cp.balance(bob, creator).await.unwrap(),
        0,
        "loser paid nothing"
    );

    let got = repo.get(pred.id).await.unwrap().unwrap();
    assert_eq!(got.status, "resolved");
    assert_eq!(got.winning_outcome_idx, Some(0));
    assert!(got.resolved_at.is_some());

    // Re-resolving is rejected (bad state).
    let err = repo.resolve(pred.id, 0).await.unwrap_err();
    assert!(matches!(err, ResolveError::BadState));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn predictions_resolve_no_winner_refunds_all() {
    let p = pool();
    let cp = ChannelPointsRepo::new(p.clone());
    let repo = PredictionRepo::new(p.clone());
    let stream = Ulid::new();
    let creator = participant(&p, "creator").await;
    let alice = participant(&p, "alice").await;
    let bob = participant(&p, "bob").await;

    cp.earn(alice, creator, 50, "watch").await.unwrap();
    cp.earn(bob, creator, 70, "watch").await.unwrap();

    let pred = repo
        .create_prediction(
            stream,
            creator,
            "Three-way",
            &["A".into(), "B".into(), "C".into()],
            None,
        )
        .await
        .unwrap();

    // Both stake on A (0) and B (1); resolve to C (2) — nobody picked the winner.
    repo.stake(pred.id, alice, 0, 50).await.unwrap();
    repo.stake(pred.id, bob, 1, 70).await.unwrap();

    let payouts = repo.resolve(pred.id, 2).await.unwrap();
    // Everyone refunded their own stake.
    assert_eq!(payouts.len(), 2);
    assert_eq!(cp.balance(alice, creator).await.unwrap(), 50);
    assert_eq!(cp.balance(bob, creator).await.unwrap(), 70);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn predictions_cancel_refunds_all() {
    let p = pool();
    let cp = ChannelPointsRepo::new(p.clone());
    let repo = PredictionRepo::new(p.clone());
    let stream = Ulid::new();
    let creator = participant(&p, "creator").await;
    let alice = participant(&p, "alice").await;

    cp.earn(alice, creator, 80, "watch").await.unwrap();
    let pred = repo
        .create_prediction(
            stream,
            creator,
            "Cancelable",
            &["A".into(), "B".into()],
            None,
        )
        .await
        .unwrap();
    repo.stake(pred.id, alice, 0, 80).await.unwrap();
    assert_eq!(cp.balance(alice, creator).await.unwrap(), 0);

    // Cancel refunds the stake; a second cancel is a no-op.
    assert!(repo.cancel(pred.id).await.unwrap());
    assert!(!repo.cancel(pred.id).await.unwrap());
    assert_eq!(
        cp.balance(alice, creator).await.unwrap(),
        80,
        "stake refunded"
    );
    assert_eq!(
        repo.get(pred.id).await.unwrap().unwrap().status,
        "cancelled"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn predictions_cannot_stake_after_lock_or_resolve() {
    let p = pool();
    let cp = ChannelPointsRepo::new(p.clone());
    let repo = PredictionRepo::new(p.clone());
    let stream = Ulid::new();
    let creator = participant(&p, "creator").await;
    let alice = participant(&p, "alice").await;
    let bob = participant(&p, "bob").await;

    cp.earn(alice, creator, 100, "watch").await.unwrap();
    cp.earn(bob, creator, 100, "watch").await.unwrap();

    let pred = repo
        .create_prediction(
            stream,
            creator,
            "Locked soon",
            &["A".into(), "B".into()],
            None,
        )
        .await
        .unwrap();
    repo.stake(pred.id, alice, 0, 10).await.unwrap();

    // Lock -> the prediction is no longer open; staking is rejected.
    assert!(repo.lock(pred.id).await.unwrap());
    assert!(!repo.lock(pred.id).await.unwrap(), "second lock is a no-op");
    let err = repo.stake(pred.id, bob, 0, 10).await.unwrap_err();
    assert!(matches!(err, StakeError::BadState));

    // Resolve a LOCKED prediction (open/locked are both resolvable).
    repo.resolve(pred.id, 0).await.unwrap();

    // Staking on a resolved prediction is likewise rejected.
    let err = repo.stake(pred.id, bob, 0, 10).await.unwrap_err();
    assert!(matches!(err, StakeError::BadState));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn predictions_expiry_hides_and_rejects_without_debit() {
    let p = pool();
    let cp = ChannelPointsRepo::new(p.clone());
    let repo = PredictionRepo::new(p.clone());
    let stream = Ulid::new();
    let creator = participant(&p, "expiry-creator").await;
    let viewer = participant(&p, "expiry-viewer").await;
    cp.earn(viewer, creator, 100, "watch").await.unwrap();

    let pred = repo
        .create_prediction(
            stream,
            creator,
            "Expires",
            &["A".into(), "B".into()],
            Some(OffsetDateTime::now_utc() + time::Duration::minutes(5)),
        )
        .await
        .unwrap();
    // Keep expires_at after created_at (the schema invariant), but move both
    // far enough into the past to exercise runtime expiry without sleeping.
    sqlx::query(
        r"UPDATE predictions
                  SET created_at = created_at - interval '1 day',
                      expires_at = clock_timestamp() - interval '1 second'
                WHERE id = $1",
    )
    .bind(pred.id.to_uuid())
    .execute(&p)
    .await
    .unwrap();

    assert!(
        repo.list_open(stream)
            .await
            .unwrap()
            .iter()
            .all(|candidate| candidate.id != pred.id),
        "expired predictions are not advertised as open"
    );
    let err = repo.stake(pred.id, viewer, 0, 40).await.unwrap_err();
    assert!(matches!(err, StakeError::Expired));
    assert_eq!(
        cp.balance(viewer, creator).await.unwrap(),
        100,
        "expiry rejection retains the full balance"
    );
    let raw_error = sqlx::query(
        r"INSERT INTO prediction_stakes
                  (id, prediction_id, outcome_idx, viewer_id, points)
               VALUES ($1, $2, 0, $3, 10)",
    )
    .bind(aero_common::PredictionStakeId::new().to_uuid())
    .bind(pred.id.to_uuid())
    .bind(viewer.to_uuid())
    .execute(&p)
    .await
    .unwrap_err();
    assert_eq!(
        database_constraint(&raw_error),
        Some("prediction_stake_not_expired_chk"),
        "the SQL backstop applies the same expiry boundary"
    );
    let stakes: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM prediction_stakes WHERE prediction_id = $1")
            .bind(pred.id.to_uuid())
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(stakes, 0);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn predictions_stake_racing_lock_is_rejected_or_refundable() {
    let p = pool();
    let cp = ChannelPointsRepo::new(p.clone());
    let repo = PredictionRepo::new(p.clone());
    let creator = participant(&p, "lock-race-creator").await;
    let viewer = participant(&p, "lock-race-viewer").await;
    cp.earn(viewer, creator, 100, "watch").await.unwrap();
    let pred = repo
        .create_prediction(
            Ulid::new(),
            creator,
            "Lock race",
            &["A".into(), "B".into()],
            None,
        )
        .await
        .unwrap();

    let (stake_result, lock_result) =
        tokio::join!(repo.stake(pred.id, viewer, 0, 40), repo.lock(pred.id));
    assert!(lock_result.unwrap(), "the open prediction is locked once");
    assert!(
        stake_result.is_ok() || matches!(stake_result, Err(StakeError::BadState)),
        "the racing stake either linearizes before lock or is rejected"
    );

    assert!(repo.cancel(pred.id).await.unwrap());
    assert_eq!(
        cp.balance(viewer, creator).await.unwrap(),
        100,
        "an admitted pre-lock stake is included in the later refund"
    );
    let unsettled: i64 = sqlx::query_scalar(
        r"SELECT COUNT(*)
                FROM prediction_stakes
               WHERE prediction_id = $1
                 AND payout IS NULL",
    )
    .bind(pred.id.to_uuid())
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(unsettled, 0);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn predictions_stake_racing_resolve_is_settled_or_not_debited() {
    let p = pool();
    let cp = ChannelPointsRepo::new(p.clone());
    let repo = PredictionRepo::new(p.clone());
    let creator = participant(&p, "resolve-race-creator").await;
    let viewer = participant(&p, "resolve-race-viewer").await;
    cp.earn(viewer, creator, 100, "watch").await.unwrap();
    let pred = repo
        .create_prediction(
            Ulid::new(),
            creator,
            "Resolve race",
            &["A".into(), "B".into()],
            None,
        )
        .await
        .unwrap();

    let (stake_result, resolve_result) =
        tokio::join!(repo.stake(pred.id, viewer, 0, 40), repo.resolve(pred.id, 0));
    resolve_result.unwrap();
    assert!(
        stake_result.is_ok() || matches!(stake_result, Err(StakeError::BadState)),
        "the racing stake either joins the resolved pool or is rejected"
    );
    assert_eq!(
        cp.balance(viewer, creator).await.unwrap(),
        100,
        "a winning admitted stake is paid; a rejected stake is never debited"
    );
    let unsettled: i64 = sqlx::query_scalar(
        r"SELECT COUNT(*)
                FROM prediction_stakes
               WHERE prediction_id = $1
                 AND payout IS NULL",
    )
    .bind(pred.id.to_uuid())
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(
        unsettled, 0,
        "a terminal prediction cannot retain a late unsettled stake"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn predictions_stake_racing_cancel_is_refunded_or_not_debited() {
    let p = pool();
    let cp = ChannelPointsRepo::new(p.clone());
    let repo = PredictionRepo::new(p.clone());
    let creator = participant(&p, "cancel-race-creator").await;
    let viewer = participant(&p, "cancel-race-viewer").await;
    cp.earn(viewer, creator, 100, "watch").await.unwrap();
    let pred = repo
        .create_prediction(
            Ulid::new(),
            creator,
            "Cancel race",
            &["A".into(), "B".into()],
            None,
        )
        .await
        .unwrap();

    let (stake_result, cancel_result) =
        tokio::join!(repo.stake(pred.id, viewer, 0, 40), repo.cancel(pred.id));
    assert!(cancel_result.unwrap());
    assert!(
        stake_result.is_ok() || matches!(stake_result, Err(StakeError::BadState)),
        "the racing stake either joins the refund set or is rejected"
    );
    assert_eq!(
        cp.balance(viewer, creator).await.unwrap(),
        100,
        "an admitted stake is refunded; a rejected stake is never debited"
    );
    let unsettled: i64 = sqlx::query_scalar(
        r"SELECT COUNT(*)
                FROM prediction_stakes
               WHERE prediction_id = $1
                 AND payout IS NULL",
    )
    .bind(pred.id.to_uuid())
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(
        unsettled, 0,
        "cancel cannot miss a concurrently admitted stake"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn predictions_sql_backstop_rejects_terminal_stake() {
    let p = pool();
    let repo = PredictionRepo::new(p.clone());
    let creator = participant(&p, "trigger-creator").await;
    let viewer = participant(&p, "trigger-viewer").await;
    let pred = repo
        .create_prediction(
            Ulid::new(),
            creator,
            "Trigger fence",
            &["A".into(), "B".into()],
            None,
        )
        .await
        .unwrap();
    repo.resolve(pred.id, 0).await.unwrap();

    let error = sqlx::query(
        r"INSERT INTO prediction_stakes
                  (id, prediction_id, outcome_idx, viewer_id, points)
               VALUES ($1, $2, 0, $3, 10)",
    )
    .bind(aero_common::PredictionStakeId::new().to_uuid())
    .bind(pred.id.to_uuid())
    .bind(viewer.to_uuid())
    .execute(&p)
    .await
    .unwrap_err();
    assert_eq!(
        database_constraint(&error),
        Some("prediction_stake_open_state_chk")
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn predictions_sql_backstop_journals_rolling_stake_and_allows_settlement() {
    let p = pool();
    let cp = ChannelPointsRepo::new(p.clone());
    let repo = PredictionRepo::new(p.clone());
    let creator = participant(&p, "rolling-trigger-creator").await;
    let viewer = participant(&p, "rolling-trigger-viewer").await;
    cp.earn(viewer, creator, 100, "watch").await.unwrap();
    let pred = repo
        .create_prediction(
            Ulid::new(),
            creator,
            "Rolling binary",
            &["A".into(), "B".into()],
            None,
        )
        .await
        .unwrap();

    // Simulate the pre-0217 binary: it debited first and then inserted the
    // stake, but did not append a negative points-history entry itself.
    let stake = aero_common::PredictionStakeId::new();
    let mut tx = p.begin().await.unwrap();
    sqlx::query(
        r"UPDATE points_ledger
                  SET balance = balance - 30
                WHERE viewer_id = $1
                  AND creator_id = $2
                  AND balance >= 30",
    )
    .bind(viewer.to_uuid())
    .bind(creator.to_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        r"INSERT INTO prediction_stakes
                  (id, prediction_id, outcome_idx, viewer_id, points)
               VALUES ($1, $2, 0, $3, 30)",
    )
    .bind(stake.to_uuid())
    .bind(pred.id.to_uuid())
    .bind(viewer.to_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let reason = format!("prediction_stake:{}", stake.to_uuid());
    let deltas: Vec<i64> = sqlx::query_scalar(
        r"SELECT delta
                FROM points_earn_history
               WHERE viewer_id = $1
                 AND creator_id = $2
                 AND reason = $3",
    )
    .bind(viewer.to_uuid())
    .bind(creator.to_uuid())
    .bind(reason)
    .fetch_all(&p)
    .await
    .unwrap();
    assert_eq!(deltas, vec![-30]);

    // The identity trigger deliberately leaves payout mutable: the canonical
    // lock/cancel path can still settle a rolling-upgrade stake normally.
    assert!(repo.lock(pred.id).await.unwrap());
    assert!(repo.cancel(pred.id).await.unwrap());
    assert_eq!(cp.balance(viewer, creator).await.unwrap(), 100);
    let payout: Option<i64> =
        sqlx::query_scalar("SELECT payout FROM prediction_stakes WHERE id = $1")
            .bind(stake.to_uuid())
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(payout, Some(30));

    let illegal = sqlx::query("UPDATE predictions SET status = 'open' WHERE id = $1")
        .bind(pred.id.to_uuid())
        .execute(&p)
        .await
        .unwrap_err();
    assert_eq!(
        database_constraint(&illegal),
        Some("prediction_state_transition_chk")
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn predictions_create_requires_two_outcomes() {
    let p = pool();
    let repo = PredictionRepo::new(p.clone());
    let creator = participant(&p, "creator").await;
    let err = repo
        .create_prediction(Ulid::new(), creator, "Only one", &["solo".into()], None)
        .await;
    assert!(err.is_err(), "fewer than 2 outcomes is rejected");

    let err = repo
        .create_prediction(
            Ulid::new(),
            creator,
            "Already expired",
            &["A".into(), "B".into()],
            Some(OffsetDateTime::now_utc() - time::Duration::seconds(1)),
        )
        .await;
    assert!(err.is_err(), "an already-expired prediction is rejected");
}
