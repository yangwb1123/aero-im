use super::*;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

/// Create a throwaway participant so the test is self-contained.
async fn participant(p: &PgPool) -> ParticipantId {
    let id = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(id.to_uuid())
        .bind(format!("digest-actor-{id}"))
        .execute(p)
        .await
        .expect("insert participant");
    id
}

async fn workspace(p: &PgPool, member: ParticipantId) -> (WorkspaceId, ParticipantId) {
    let id = WorkspaceId::new();
    let owner = participant(p).await;
    let mut tx = p.begin().await.expect("begin workspace fixture");
    sqlx::query(
        "INSERT INTO workspaces (id, name, slug, created_by)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(id.to_uuid())
    .bind(format!("digest-workspace-{id}"))
    .bind(format!("digest-{id}"))
    .bind(owner.to_uuid())
    .execute(&mut *tx)
    .await
    .expect("insert workspace");
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, 'owner')",
    )
    .bind(id.to_uuid())
    .bind(owner.to_uuid())
    .execute(&mut *tx)
    .await
    .expect("insert workspace membership");
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, 'member')",
    )
    .bind(id.to_uuid())
    .bind(member.to_uuid())
    .execute(&mut *tx)
    .await
    .expect("insert digest member");
    tx.commit().await.expect("commit workspace fixture");
    (id, owner)
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn digest_failure_retry_prepared_payload_and_fenced_success() {
    let p = pool();
    let repo = DigestSubscriptionRepo::new(p.clone());
    let owner = participant(&p).await;
    let room = RoomId::new();

    // Create a room digest whose first run is already in the past → due now.
    let past = time::OffsetDateTime::now_utc() - time::Duration::minutes(5);
    let id = repo
        .create(owner, DigestTarget::Room(room), "daily", past)
        .await
        .unwrap();

    // list_for shows it (owner-scoped); a stranger's list does not.
    let listed = repo.list_for(owner).await.unwrap();
    assert!(listed.iter().any(|s| s.id == id), "owner list shows it");
    assert_eq!(
        listed.iter().find(|s| s.id == id).unwrap().target,
        DigestTarget::Room(room)
    );
    let stranger = participant(&p).await;
    assert!(
        !repo
            .list_for(stranger)
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == id),
        "another user's list does not show it"
    );

    let now = time::OffsetDateTime::now_utc();
    let first = repo
        .claim_due(now, time::Duration::seconds(1), 10)
        .await
        .unwrap()
        .into_iter()
        .find(|claim| claim.subscription.id == id)
        .unwrap();
    assert_eq!(first.attempt, 1);
    assert!(
        !repo.delete(id, owner).await.unwrap(),
        "active occurrence fences deletion"
    );
    assert!(repo
        .save_prepared_summary(
            id,
            first.claim_token,
            first.delivery_key,
            first.attempt,
            "stable summary",
        )
        .await
        .unwrap());

    let retry_at = now + time::Duration::minutes(2);
    assert_eq!(
        repo.record_failure(
            id,
            first.claim_token,
            first.delivery_key,
            first.attempt,
            retry_at,
            "temporary",
            true,
            8,
        )
        .await
        .unwrap(),
        DigestFailureDisposition::RetryScheduled
    );
    assert!(repo
        .claim_due(
            now + time::Duration::minutes(1),
            time::Duration::minutes(5),
            10,
        )
        .await
        .unwrap()
        .iter()
        .all(|claim| claim.subscription.id != id));
    let second = repo
        .claim_due(retry_at, time::Duration::minutes(5), 10)
        .await
        .unwrap()
        .into_iter()
        .find(|claim| claim.subscription.id == id)
        .unwrap();
    assert_eq!(second.delivery_key, first.delivery_key);
    assert_eq!(second.prepared_summary.as_deref(), Some("stable summary"));
    assert_eq!(second.attempt, 2);

    let next = next_run_at("daily", now).expect("known cadence");
    assert!(!repo
        .confirm_sent(
            id,
            first.claim_token,
            first.delivery_key,
            first.attempt,
            next,
            now,
        )
        .await
        .unwrap());
    assert!(repo
        .confirm_sent(
            id,
            second.claim_token,
            second.delivery_key,
            second.attempt,
            next,
            now,
        )
        .await
        .unwrap());
    let state: (
        time::OffsetDateTime,
        Option<time::OffsetDateTime>,
        i32,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT next_run_at, last_sent_at, delivery_attempts, prepared_summary
           FROM digest_subscriptions WHERE id = $1",
    )
    .bind(id.to_uuid())
    .fetch_one(&p)
    .await
    .unwrap();
    // PostgreSQL timestamps round-trip at microsecond precision, while
    // `OffsetDateTime` may carry nanoseconds.
    assert!((state.0.unix_timestamp_nanos() - next.unix_timestamp_nanos()).abs() < 1_000);
    assert!(state.1.is_some());
    assert_eq!(state.2, 0);
    assert!(state.3.is_none());

    // delete: owner-scoped; stranger cannot, owner can, second is a no-op.
    assert!(
        !repo.delete(id, stranger).await.unwrap(),
        "stranger cannot delete"
    );
    assert!(repo.delete(id, owner).await.unwrap(), "owner deletes");
    assert!(
        !repo.delete(id, owner).await.unwrap(),
        "second delete is a no-op"
    );
    assert!(
        !repo
            .list_for(owner)
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == id),
        "deleted subscription leaves the list"
    );

    // A live lease fences deletion. Once a failed attempt is re-parked, the
    // owner can delete the subscription instead of being locked into
    // retries forever.
    let retry_id = repo
        .create(
            owner,
            DigestTarget::Room(room),
            "daily",
            time::OffsetDateTime::now_utc() - time::Duration::minutes(1),
        )
        .await
        .unwrap();
    let retry_claim = repo
        .claim_due(
            time::OffsetDateTime::now_utc(),
            time::Duration::minutes(5),
            10,
        )
        .await
        .unwrap()
        .into_iter()
        .find(|claim| claim.subscription.id == retry_id)
        .unwrap();
    assert!(!repo.delete(retry_id, owner).await.unwrap());
    assert_eq!(
        repo.record_failure(
            retry_id,
            retry_claim.claim_token,
            retry_claim.delivery_key,
            retry_claim.attempt,
            time::OffsetDateTime::now_utc() + time::Duration::hours(1),
            "temporary",
            true,
            8,
        )
        .await
        .unwrap(),
        DigestFailureDisposition::RetryScheduled
    );
    assert!(repo.delete(retry_id, owner).await.unwrap());

    // Cleanup participants.
    for who in [owner, stranger] {
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(who.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn digest_concurrent_workers_and_expired_claims_are_fenced() {
    let p = pool();
    let repo = DigestSubscriptionRepo::new(p.clone());
    let owner = participant(&p).await;
    let now = time::OffsetDateTime::now_utc();
    for _ in 0..2 {
        repo.create(
            owner,
            DigestTarget::Room(RoomId::new()),
            "daily",
            now - time::Duration::minutes(1),
        )
        .await
        .unwrap();
    }

    let (left, right) = tokio::join!(
        repo.claim_due_with_policy(now, time::Duration::seconds(1), 1, 1),
        repo.claim_due_with_policy(now, time::Duration::seconds(1), 1, 1),
    );
    let left = left.unwrap().pop().unwrap();
    let right = right.unwrap().pop().unwrap();
    assert_ne!(left.subscription.id, right.subscription.id);

    let after_expiry = repo
        .claim_due_with_policy(
            now + time::Duration::seconds(2),
            time::Duration::minutes(5),
            1,
            10,
        )
        .await
        .unwrap();
    assert!(after_expiry.is_empty());
    assert_eq!(
        repo.record_failure(
            left.subscription.id,
            left.claim_token,
            left.delivery_key,
            left.attempt,
            now,
            "stale worker",
            true,
            8,
        )
        .await
        .unwrap(),
        DigestFailureDisposition::FenceLost
    );

    let listed = repo.list_for(owner).await.unwrap();
    for claim in [&left, &right] {
        let dead = listed
            .iter()
            .find(|subscription| subscription.id == claim.subscription.id)
            .expect("expired max-attempt claim stays visible");
        assert!(dead.dead_at.is_some());
        assert_eq!(dead.delivery_attempts, 1);
        assert!(repo.delete(dead.id, owner).await.unwrap());
    }
    sqlx::query("DELETE FROM participants WHERE id = $1")
        .bind(owner.to_uuid())
        .execute(&p)
        .await
        .ok();
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn digest_workspace_target_persists() {
    let p = pool();
    let repo = DigestSubscriptionRepo::new(p.clone());
    let owner = participant(&p).await;
    let (ws, workspace_owner) = workspace(&p, owner).await;

    let first = time::OffsetDateTime::now_utc() - time::Duration::minutes(1);
    let id = repo
        .create(owner, DigestTarget::Workspace(ws), "weekly", first)
        .await
        .unwrap();
    let listed = repo.list_for(owner).await.unwrap();
    let found = listed.iter().find(|s| s.id == id).expect("present");
    assert_eq!(found.target, DigestTarget::Workspace(ws));
    assert_eq!(found.frequency, "weekly");

    let now = time::OffsetDateTime::now_utc();
    let claim = repo
        .claim_due(now, time::Duration::minutes(5), 10)
        .await
        .unwrap()
        .into_iter()
        .find(|claim| claim.subscription.id == id)
        .unwrap();
    assert_eq!(
        repo.insert_workspace_delivery(owner, ws, claim.delivery_key, "workspace summary")
            .await
            .unwrap(),
        WorkspaceDigestDelivery::Inserted
    );
    assert_eq!(
        repo.insert_workspace_delivery(owner, ws, claim.delivery_key, "workspace summary")
            .await
            .unwrap(),
        WorkspaceDigestDelivery::AlreadyDelivered,
        "same occurrence is idempotent after a confirm crash"
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM activity_feed
          WHERE participant_id = $1 AND kind = 'digest' AND subject_id = $2",
    )
    .bind(owner.to_uuid())
    .bind(claim.delivery_key)
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(count, 1);
    let next = next_run_at("weekly", now).unwrap();
    assert!(repo
        .confirm_sent(
            id,
            claim.claim_token,
            claim.delivery_key,
            claim.attempt,
            next,
            now,
        )
        .await
        .unwrap());
    assert!(repo.delete(id, owner).await.unwrap());

    // Prepared-summary retries must still pass the final effective-access
    // gate. Removing membership after preparation cannot leak the old
    // workspace summary into the personal feed.
    let retry_id = repo
        .create(
            owner,
            DigestTarget::Workspace(ws),
            "daily",
            now - time::Duration::minutes(1),
        )
        .await
        .unwrap();
    let prepared = repo
        .claim_due(now, time::Duration::minutes(5), 10)
        .await
        .unwrap()
        .into_iter()
        .find(|claim| claim.subscription.id == retry_id)
        .unwrap();
    assert!(repo
        .save_prepared_summary(
            retry_id,
            prepared.claim_token,
            prepared.delivery_key,
            prepared.attempt,
            "prepared before revocation",
        )
        .await
        .unwrap());
    assert_eq!(
        repo.record_failure(
            retry_id,
            prepared.claim_token,
            prepared.delivery_key,
            prepared.attempt,
            now,
            "simulate crash before final insert",
            true,
            8,
        )
        .await
        .unwrap(),
        DigestFailureDisposition::RetryScheduled
    );
    let prepared_retry = repo
        .claim_due(now, time::Duration::minutes(5), 10)
        .await
        .unwrap()
        .into_iter()
        .find(|claim| claim.subscription.id == retry_id)
        .unwrap();
    assert_eq!(
        prepared_retry.prepared_summary.as_deref(),
        Some("prepared before revocation")
    );
    let revoked_key = prepared_retry.delivery_key;
    sqlx::query(
        "DELETE FROM workspace_members
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(ws.to_uuid())
    .bind(owner.to_uuid())
    .execute(&p)
    .await
    .unwrap();
    assert_eq!(
        repo.insert_workspace_delivery(owner, ws, revoked_key, "must not leak")
            .await
            .unwrap(),
        WorkspaceDigestDelivery::AccessRevoked
    );
    let leaked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM activity_feed
          WHERE participant_id = $1 AND subject_id = $2",
    )
    .bind(owner.to_uuid())
    .bind(revoked_key)
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(leaked, 0);
    assert_eq!(
        repo.record_failure(
            retry_id,
            prepared_retry.claim_token,
            prepared_retry.delivery_key,
            prepared_retry.attempt,
            now,
            "workspace access revoked before final delivery",
            false,
            8,
        )
        .await
        .unwrap(),
        DigestFailureDisposition::Dead
    );
    let dead = repo
        .list_for(owner)
        .await
        .unwrap()
        .into_iter()
        .find(|subscription| subscription.id == retry_id)
        .expect("terminal workspace digest remains owner-visible");
    assert!(dead.dead_at.is_some());
    assert_eq!(
        dead.last_error.as_deref(),
        Some("workspace access revoked before final delivery")
    );
    assert!(repo.delete(retry_id, owner).await.unwrap());

    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, 'member')",
    )
    .bind(ws.to_uuid())
    .bind(owner.to_uuid())
    .execute(&p)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO workspace_deactivations
             (workspace_id, participant_id, deactivated_by)
         VALUES ($1, $2, $3)",
    )
    .bind(ws.to_uuid())
    .bind(owner.to_uuid())
    .bind(workspace_owner.to_uuid())
    .execute(&p)
    .await
    .unwrap();
    assert_eq!(
        repo.insert_workspace_delivery(
            owner,
            ws,
            uuid::Uuid::new_v4(),
            "deactivated must not leak",
        )
        .await
        .unwrap(),
        WorkspaceDigestDelivery::AccessRevoked
    );
    sqlx::query(
        "DELETE FROM workspace_deactivations
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(ws.to_uuid())
    .bind(owner.to_uuid())
    .execute(&p)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO totp_secrets (participant_id, secret, activated, activated_at)
         VALUES ($1, 'digest-workspace-owner', true, now())",
    )
    .bind(workspace_owner.to_uuid())
    .execute(&p)
    .await
    .unwrap();
    sqlx::query("UPDATE workspaces SET require_2fa = true WHERE id = $1")
        .bind(ws.to_uuid())
        .execute(&p)
        .await
        .unwrap();
    let twofa_key = uuid::Uuid::new_v4();
    assert_eq!(
        repo.insert_workspace_delivery(owner, ws, twofa_key, "2fa required")
            .await
            .unwrap(),
        WorkspaceDigestDelivery::AccessRevoked
    );
    sqlx::query(
        "INSERT INTO totp_secrets (participant_id, secret, activated, activated_at)
         VALUES ($1, 'test-secret', true, now())",
    )
    .bind(owner.to_uuid())
    .execute(&p)
    .await
    .unwrap();
    assert_eq!(
        repo.insert_workspace_delivery(owner, ws, twofa_key, "2fa satisfied")
            .await
            .unwrap(),
        WorkspaceDigestDelivery::Inserted
    );
    sqlx::query("UPDATE participants SET deleted_at = now() WHERE id = $1")
        .bind(owner.to_uuid())
        .execute(&p)
        .await
        .unwrap();
    assert_eq!(
        repo.insert_workspace_delivery(
            owner,
            ws,
            uuid::Uuid::new_v4(),
            "deleted account must not leak",
        )
        .await
        .unwrap(),
        WorkspaceDigestDelivery::AccessRevoked
    );
    sqlx::query("UPDATE participants SET deleted_at = NULL WHERE id = $1")
        .bind(owner.to_uuid())
        .execute(&p)
        .await
        .unwrap();

    sqlx::query("DELETE FROM activity_feed WHERE participant_id = $1")
        .bind(owner.to_uuid())
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM totp_secrets WHERE participant_id = $1")
        .bind(owner.to_uuid())
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(ws.to_uuid())
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM participants WHERE id = $1")
        .bind(owner.to_uuid())
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM participants WHERE id = $1")
        .bind(workspace_owner.to_uuid())
        .execute(&p)
        .await
        .ok();
}
