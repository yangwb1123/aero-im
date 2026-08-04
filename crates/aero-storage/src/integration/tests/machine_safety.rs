use super::*;

use std::time::Duration;

use aero_common::{Blob, Error, WorkspaceRole};

use crate::{BlobRepo, DeactivationRepo, SsoRepo, WorkspaceRepo};

fn notification_probe(
    installation: &IntegrationInstallation,
    key: Uuid,
    hash: [u8; 32],
) -> IntegrationReplayProbe {
    IntegrationReplayProbe {
        installation_id: installation.id,
        issuer: installation.issuer.clone(),
        client_id: installation.client_id.clone(),
        idempotency_key: key,
        request_hash: hash,
        target: IntegrationTarget::Room(installation.room_ids[0]),
    }
}

pub(super) fn blob_probe(
    installation: &IntegrationInstallation,
    key: Uuid,
    hash: [u8; 32],
) -> IntegrationBlobProbe {
    IntegrationBlobProbe {
        installation_id: installation.id,
        issuer: installation.issuer.clone(),
        client_id: installation.client_id.clone(),
        idempotency_key: key,
        request_hash: hash,
        target: IntegrationTarget::Room(installation.room_ids[0]),
    }
}

async fn acquire_blob(
    fixture: &Fixture,
    probe: &IntegrationBlobProbe,
) -> (Uuid, PreparedIntegrationTarget) {
    match fixture.repo.claim_blob(probe).await.expect("claim blob") {
        IntegrationBlobClaim::Acquired {
            lease_token,
            target,
        } => (lease_token, target),
        IntegrationBlobClaim::Pending => panic!("fresh blob key cannot be pending"),
        IntegrationBlobClaim::Replay(_) => panic!("fresh blob key cannot replay"),
    }
}

pub(super) async fn commit_blob(
    fixture: &Fixture,
    installation: &IntegrationInstallation,
    probe: &IntegrationBlobProbe,
    blob: &Blob,
    storage_key: Option<String>,
) -> (Uuid, IntegrationBlobOutcome) {
    let (lease_token, prepared) = acquire_blob(fixture, probe).await;
    let room_id = prepared
        .room_id
        .expect("room target is already materialized");
    let outcome = fixture
        .repo
        .commit_blob(IntegrationBlobCommit {
            installation_id: installation.id,
            issuer: installation.issuer.clone(),
            client_id: installation.client_id.clone(),
            idempotency_key: probe.idempotency_key,
            request_hash: probe.request_hash,
            target: probe.target.clone(),
            room_id,
            recipient: None,
            lease_token,
            blob_id: blob.id,
            content_sha256: blob.sha256.clone().expect("fixture digest"),
            storage_key,
        })
        .await
        .expect("commit integration blob");
    (lease_token, outcome)
}

pub(super) async fn another_installation(
    fixture: &Fixture,
    label: &str,
) -> IntegrationInstallation {
    let client_id = format!("{label}-{}", Uuid::new_v4());
    fixture
        .repo
        .create(installation_input(
            fixture,
            fixture.owner,
            fixture.bot,
            &client_id,
            vec![fixture.allowed_room],
        ))
        .await
        .expect("create additional installation")
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn notification_claim_lock_namespace_coalesces_and_replays() {
    let fixture = fixture().await;
    let installation = install(&fixture).await;
    let key = Uuid::new_v4();
    let hash = [61; 32];
    let probe = notification_probe(&installation, key, hash);

    let mut blocker = fixture.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!(
            "aero:integration:notification:{}:{key}",
            installation.id
        ))
        .execute(&mut *blocker)
        .await
        .unwrap();
    let repo = fixture.repo.clone();
    let task_probe = probe.clone();
    let mut task = tokio::spawn(async move { repo.claim_notification(&task_probe).await });
    assert!(tokio::time::timeout(Duration::from_millis(100), &mut task)
        .await
        .is_err());
    blocker.commit().await.unwrap();

    let IntegrationNotificationClaim::Acquired {
        lease_token,
        target: prepared,
    } = task.await.unwrap().unwrap()
    else {
        panic!("first owner must acquire after advisory lock releases");
    };
    assert_eq!(prepared.room_id, Some(fixture.allowed_room));
    assert!(matches!(
        fixture.repo.claim_notification(&probe).await.unwrap(),
        IntegrationNotificationClaim::Pending
    ));

    let mut request = notification(
        &installation,
        key,
        hash,
        fixture.allowed_room,
        "coalesced notification",
    );
    request.lease_token = Some(lease_token);
    let published = fixture.repo.publish(request).await.unwrap();
    let replay = fixture.repo.claim_notification(&probe).await.unwrap();
    match replay {
        IntegrationNotificationClaim::Replay(replay) => {
            assert!(replay.deduplicated);
            assert_eq!(replay.message.id, published.message.id);
        }
        _ => panic!("completed claim must replay canonical projection"),
    }
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM integration_machine_requests
          WHERE installation_id = $1 AND operation = 'notification' AND idempotency_key = $2",
    )
    .bind(installation.id)
    .bind(key)
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(rows, 1);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn machine_sweep_preserves_live_lease_then_removes_expired_state() {
    let fixture = fixture().await;
    let installation = install(&fixture).await;
    let key = Uuid::new_v4();
    let probe = notification_probe(&installation, key, [62; 32]);
    let IntegrationNotificationClaim::Acquired {
        lease_token: lease, ..
    } = fixture.repo.claim_notification(&probe).await.unwrap()
    else {
        panic!("fresh claim must be acquired");
    };
    sqlx::query(
        "UPDATE integration_machine_requests SET expires_at = now() - interval '1 second'
          WHERE installation_id = $1 AND operation = 'notification' AND idempotency_key = $2",
    )
    .bind(installation.id)
    .bind(key)
    .execute(&fixture.pool)
    .await
    .unwrap();
    let live = fixture.repo.sweep_expired_machine_state(50).await.unwrap();
    assert_eq!(live.requests, 0);
    fixture
        .repo
        .release_notification_claim(installation.id, key, lease)
        .await
        .unwrap();
    let expired = fixture.repo.sweep_expired_machine_state(50).await.unwrap();
    assert_eq!(expired.requests, 1);
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM integration_machine_requests
          WHERE installation_id = $1 AND operation = 'notification' AND idempotency_key = $2",
    )
    .bind(installation.id)
    .bind(key)
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(remaining, 0);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn machine_sweep_full_request_batch_does_not_starve_orphans_or_blob_ledger() {
    let fixture = fixture().await;
    let installation = install(&fixture).await;

    // Fill the one-row request phase with a retryable request that has no
    // canonical receipt. The later cleanup phases must still receive a batch.
    let request_key = Uuid::new_v4();
    let request_probe = notification_probe(&installation, request_key, [73; 32]);
    let IntegrationNotificationClaim::Acquired {
        lease_token: request_lease,
        ..
    } = fixture
        .repo
        .claim_notification(&request_probe)
        .await
        .unwrap()
    else {
        panic!("fresh request must acquire");
    };
    fixture
        .repo
        .release_notification_claim(installation.id, request_key, request_lease)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE integration_machine_requests SET expires_at = now() - interval '100 years'
          WHERE installation_id = $1 AND operation = 'notification' AND idempotency_key = $2",
    )
    .bind(installation.id)
    .bind(request_key)
    .execute(&fixture.pool)
    .await
    .unwrap();

    // The storage-level publisher deliberately supports trusted callers that
    // do not use a machine lease, which gives us a real orphan receipt.
    let notification_key = Uuid::new_v4();
    fixture
        .repo
        .publish(notification(
            &installation,
            notification_key,
            [74; 32],
            fixture.allowed_room,
            "orphan notification receipt",
        ))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE integration_notification_receipts SET expires_at = now() - interval '100 years'
          WHERE installation_id = $1 AND idempotency_key = $2",
    )
    .bind(installation.id)
    .bind(notification_key)
    .execute(&fixture.pool)
    .await
    .unwrap();

    let blobs = BlobRepo::new(fixture.pool.clone());
    let label = format!("independent-sweep-phases-{}", Uuid::new_v4());
    let blob = scoped_blob(
        &blobs,
        fixture.bot,
        fixture.workspace,
        "us-west-2",
        &label,
        false,
    )
    .await;
    let blob_key = Uuid::new_v4();
    let blob_probe = blob_probe(&installation, blob_key, [75; 32]);
    commit_blob(
        &fixture,
        &installation,
        &blob_probe,
        &blob,
        Some(format!("us-west-2:{label}")),
    )
    .await;
    sqlx::query(
        "DELETE FROM integration_machine_requests
          WHERE installation_id = $1 AND operation = 'blob' AND idempotency_key = $2",
    )
    .bind(installation.id)
    .bind(blob_key)
    .execute(&fixture.pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE integration_blob_receipts SET expires_at = now() - interval '100 years'
          WHERE installation_id = $1 AND idempotency_key = $2",
    )
    .bind(installation.id)
    .bind(blob_key)
    .execute(&fixture.pool)
    .await
    .unwrap();

    let swept = fixture.repo.sweep_expired_machine_state(1).await.unwrap();
    assert_eq!(swept.requests, 1, "request phase must consume its batch");
    assert_eq!(
        swept.notification_receipts, 1,
        "notification orphan phase must still run"
    );
    assert_eq!(swept.blob_receipts, 1, "blob orphan phase must still run");
    assert_eq!(
        swept.blob_ledger_releases, 1,
        "ledger release phase must still run"
    );

    let request_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM integration_machine_requests
          WHERE installation_id = $1 AND operation = 'notification' AND idempotency_key = $2)",
    )
    .bind(installation.id)
    .bind(request_key)
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    let notification_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM integration_notification_receipts
          WHERE installation_id = $1 AND idempotency_key = $2)",
    )
    .bind(installation.id)
    .bind(notification_key)
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    let blob_receipt_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM integration_blob_receipts
          WHERE installation_id = $1 AND idempotency_key = $2)",
    )
    .bind(installation.id)
    .bind(blob_key)
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    let ledger_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM integration_blob_ledger
          WHERE installation_id = $1 AND blob_id = $2)",
    )
    .bind(installation.id)
    .bind(blob.id.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    let queued: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM blob_gc_queue WHERE blob_id = $1)")
            .bind(blob.id.to_uuid())
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert!(!request_exists);
    assert!(!notification_exists);
    assert!(!blob_receipt_exists);
    assert!(!ledger_exists);
    assert!(
        queued,
        "last unreferenced ledger release must enqueue blob GC"
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn expired_claim_and_sweep_share_machine_then_receipt_lock_order() {
    let fixture = fixture().await;
    let installation = install(&fixture).await;
    let key = Uuid::new_v4();
    let hash = [72; 32];
    let probe = notification_probe(&installation, key, hash);
    let IntegrationNotificationClaim::Acquired {
        lease_token: lease, ..
    } = fixture.repo.claim_notification(&probe).await.unwrap()
    else {
        panic!("fresh claim must acquire");
    };
    let mut request = notification(
        &installation,
        key,
        hash,
        fixture.allowed_room,
        "expiry lock-order fixture",
    );
    request.lease_token = Some(lease);
    fixture.repo.publish(request).await.unwrap();
    sqlx::query(
        "UPDATE integration_machine_requests SET expires_at = now() - interval '1 second'
          WHERE installation_id = $1 AND operation = 'notification' AND idempotency_key = $2",
    )
    .bind(installation.id)
    .bind(key)
    .execute(&fixture.pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE integration_notification_receipts SET expires_at = now() - interval '1 second'
          WHERE installation_id = $1 AND idempotency_key = $2",
    )
    .bind(installation.id)
    .bind(key)
    .execute(&fixture.pool)
    .await
    .unwrap();

    // Emulate the sweep's first step on a dedicated connection: it locks the
    // machine row before touching the receipt. A correctly ordered claim must
    // wait here without having locked the receipt yet.
    let mut machine_lock = fixture.pool.begin().await.unwrap();
    let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *machine_lock)
        .await
        .unwrap();
    sqlx::query(
        "SELECT true FROM integration_machine_requests
          WHERE installation_id = $1 AND operation = 'notification' AND idempotency_key = $2
          FOR UPDATE",
    )
    .bind(installation.id)
    .bind(key)
    .fetch_one(&mut *machine_lock)
    .await
    .unwrap();

    let claim_repo = fixture.repo.clone();
    let task_probe = probe.clone();
    let claim_task = tokio::spawn(async move { claim_repo.claim_notification(&task_probe).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                r"SELECT EXISTS (
                      SELECT 1 FROM pg_stat_activity activity
                       WHERE $1 = ANY(pg_blocking_pids(activity.pid))
                         AND activity.wait_event_type = 'Lock'
                         AND activity.query ILIKE '%DELETE FROM integration_machine_requests%'
                  )",
            )
            .bind(blocker_pid)
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
            if waiting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("claim must be observed waiting on the machine row");

    let mut receipt_lock = fixture.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT true FROM integration_notification_receipts
          WHERE installation_id = $1 AND idempotency_key = $2 FOR UPDATE NOWAIT",
    )
    .bind(installation.id)
    .bind(key)
    .fetch_one(&mut *receipt_lock)
    .await
    .expect("waiting claim must not lock receipt before machine request");
    receipt_lock.commit().await.unwrap();
    machine_lock.commit().await.unwrap();

    let claim = tokio::time::timeout(Duration::from_secs(2), claim_task)
        .await
        .expect("claim must finish after machine lock releases")
        .unwrap()
        .unwrap();
    let IntegrationNotificationClaim::Acquired {
        lease_token: new_lease,
        ..
    } = claim
    else {
        panic!("expired canonical state must yield one fresh owner");
    };
    let (status, rows): (String, i64) = sqlx::query_as(
        r"SELECT min(status), count(*)
            FROM integration_machine_requests
           WHERE installation_id = $1 AND operation = 'notification' AND idempotency_key = $2",
    )
    .bind(installation.id)
    .bind(key)
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!((status.as_str(), rows), ("processing", 1));
    fixture
        .repo
        .release_notification_claim(installation.id, key, new_lease)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn revocation_survives_deactivated_bot_and_sql_default_denies_user_dm() {
    let fixture = fixture().await;
    let raw_id = Uuid::new_v4();
    sqlx::query(
        r"INSERT INTO integration_installations
              (id, workspace_id, bot_id, issuer, user_identity_issuer,
               client_id, name, created_by)
           VALUES ($1, $2, $3, $4, $5, $6, 'raw default policy', $7)",
    )
    .bind(raw_id)
    .bind(fixture.workspace.to_uuid())
    .bind(fixture.bot.to_uuid())
    .bind(&fixture.issuer)
    .bind(&fixture.user_identity_issuer)
    .bind(format!("raw-default-{}", Uuid::new_v4()))
    .bind(fixture.owner.to_uuid())
    .execute(&fixture.pool)
    .await
    .unwrap();
    let allow_user_dm: bool =
        sqlx::query_scalar("SELECT allow_user_dm FROM integration_installations WHERE id = $1")
            .bind(raw_id)
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert!(!allow_user_dm);

    let installation = install(&fixture).await;
    DeactivationRepo::new(fixture.pool.clone())
        .deactivate(fixture.workspace, fixture.bot, fixture.owner)
        .await
        .unwrap();
    let revoked = fixture
        .repo
        .update(
            fixture.workspace,
            installation.id,
            fixture.owner,
            UpdateIntegrationInstallation {
                active: Some(false),
                ..UpdateIntegrationInstallation::default()
            },
        )
        .await
        .expect("invalid current bot must not prevent emergency revoke");
    assert!(!revoked.active);
    let ordinary = fixture
        .repo
        .update(
            fixture.workspace,
            installation.id,
            fixture.owner,
            UpdateIntegrationInstallation {
                name: Some("must fail while bot is deactivated".into()),
                ..UpdateIntegrationInstallation::default()
            },
        )
        .await;
    assert!(matches!(ordinary, Err(Error::Invalid(_))));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn blocked_snaplink_target_is_rejected_before_request_or_dm_creation() {
    let fixture = fixture().await;
    let target = participant(&fixture.pool, "blocked-integration-target").await;
    WorkspaceRepo::new(fixture.pool.clone())
        .add_member(fixture.workspace, target, WorkspaceRole::Member)
        .await
        .unwrap();
    let subject = format!("blocked-subject-{}", Uuid::new_v4());
    SsoRepo::new(fixture.pool.clone())
        .link(&fixture.user_identity_issuer, &subject, target, None)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_blocks (blocker_id, blocked_id) VALUES ($1, $2)")
        .bind(target.to_uuid())
        .bind(fixture.bot.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();
    let mut input = installation_input(
        &fixture,
        fixture.owner,
        fixture.bot,
        &fixture.client_id,
        Vec::new(),
    );
    input.allow_user_dm = true;
    let installation = fixture.repo.create(input).await.unwrap();
    let key = Uuid::new_v4();
    let probe = IntegrationReplayProbe {
        installation_id: installation.id,
        issuer: fixture.issuer.clone(),
        client_id: fixture.client_id.clone(),
        idempotency_key: key,
        request_hash: [63; 32],
        target: IntegrationTarget::SnaplinkUser(subject),
    };
    assert!(matches!(
        fixture
            .repo
            .preauthorize_target(
                installation.id,
                &fixture.issuer,
                &fixture.client_id,
                &probe.target,
            )
            .await,
        Err(Error::Forbidden(_))
    ));
    assert!(matches!(
        fixture.repo.claim_notification(&probe).await,
        Err(Error::Forbidden(_))
    ));
    let (rooms, requests): (i64, i64) = sqlx::query_as(
        r"SELECT
             (SELECT count(*) FROM rooms room
               WHERE room.workspace_id = $1 AND room.kind = 'direct'
                 AND EXISTS (SELECT 1 FROM room_members member
                              WHERE member.room_id = room.id AND member.participant_id = $2)
                 AND EXISTS (SELECT 1 FROM room_members member
                              WHERE member.room_id = room.id AND member.participant_id = $3)),
             (SELECT count(*) FROM integration_machine_requests
               WHERE installation_id = $4 AND idempotency_key = $5)",
    )
    .bind(fixture.workspace.to_uuid())
    .bind(fixture.bot.to_uuid())
    .bind(target.to_uuid())
    .bind(installation.id)
    .bind(key)
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!((rooms, requests), (0, 0));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn blob_commit_resolution_distinguishes_canonical_and_owned_release() {
    let fixture = fixture().await;
    let installation = install(&fixture).await;
    let blobs = BlobRepo::new(fixture.pool.clone());
    let blob = scoped_blob(
        &blobs,
        fixture.bot,
        fixture.workspace,
        "us-west-2",
        "ambiguous-blob-commit",
        false,
    )
    .await;
    let probe = blob_probe(&installation, Uuid::new_v4(), [64; 32]);
    let (lease, outcome) = commit_blob(
        &fixture,
        &installation,
        &probe,
        &blob,
        Some("us-west-2:ambiguous-blob-commit".into()),
    )
    .await;
    match fixture
        .repo
        .resolve_blob_commit(&probe, lease)
        .await
        .unwrap()
    {
        IntegrationBlobCommitResolution::Completed(replay) => {
            assert!(replay.deduplicated);
            assert_eq!(replay.blob.id, outcome.blob.id);
        }
        _ => panic!("canonical receipt must win ambiguous resolution"),
    }

    let pending_probe = blob_probe(&installation, Uuid::new_v4(), [65; 32]);
    let (pending_lease, _) = acquire_blob(&fixture, &pending_probe).await;
    assert!(matches!(
        fixture
            .repo
            .resolve_blob_commit(&pending_probe, pending_lease)
            .await
            .unwrap(),
        IntegrationBlobCommitResolution::Released
    ));
    let status: String = sqlx::query_scalar(
        "SELECT status FROM integration_machine_requests
          WHERE installation_id = $1 AND operation = 'blob' AND idempotency_key = $2",
    )
    .bind(installation.id)
    .bind(pending_probe.idempotency_key)
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(status, "retryable");
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn shared_blob_release_and_installation_delete_never_enqueue_while_still_owned() {
    let fixture = fixture().await;
    let first = install(&fixture).await;
    let second = another_installation(&fixture, "shared-second").await;
    let third = another_installation(&fixture, "shared-third").await;
    let blobs = BlobRepo::new(fixture.pool.clone());
    let blob = scoped_blob(
        &blobs,
        fixture.bot,
        fixture.workspace,
        "us-west-2",
        "shared-integration-blob",
        false,
    )
    .await;
    let first_probe = blob_probe(&first, Uuid::new_v4(), [66; 32]);
    commit_blob(
        &fixture,
        &first,
        &first_probe,
        &blob,
        Some("us-west-2:shared-integration-blob".into()),
    )
    .await;
    for (installation, byte) in [(&second, 67_u8), (&third, 68_u8)] {
        let probe = blob_probe(installation, Uuid::new_v4(), [byte; 32]);
        commit_blob(&fixture, installation, &probe, &blob, None).await;
    }

    sqlx::query(
        "UPDATE integration_blob_receipts SET expires_at = now() - interval '1 second'
          WHERE installation_id = $1 AND idempotency_key = $2",
    )
    .bind(first.id)
    .bind(first_probe.idempotency_key)
    .execute(&fixture.pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE integration_machine_requests SET expires_at = now() - interval '1 second'
          WHERE installation_id = $1 AND operation = 'blob' AND idempotency_key = $2",
    )
    .bind(first.id)
    .bind(first_probe.idempotency_key)
    .execute(&fixture.pool)
    .await
    .unwrap();
    let swept = fixture.repo.sweep_expired_machine_state(100).await.unwrap();
    assert_eq!(swept.blob_ledger_releases, 1);
    let queued: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM blob_gc_queue WHERE blob_id = $1)")
            .bind(blob.id.to_uuid())
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert!(!queued);
    assert!(blobs.get(blob.id).await.unwrap().is_some());
    assert!(blobs.has_live_references(blob.id).await.unwrap());

    sqlx::query("DELETE FROM integration_installations WHERE id = $1")
        .bind(second.id)
        .execute(&fixture.pool)
        .await
        .unwrap();
    let queued_after_delete: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM blob_gc_queue WHERE blob_id = $1)")
            .bind(blob.id.to_uuid())
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert!(!queued_after_delete);
    assert!(blobs.get(blob.id).await.unwrap().is_some());

    let key = Uuid::new_v4();
    let mut request = notification(
        &third,
        key,
        [69; 32],
        fixture.allowed_room,
        "shared blob remains immediately attachable",
    );
    request.blocks = vec![file_block(&blob)];
    fixture.repo.publish(request).await.unwrap();
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn installation_blob_ledger_survives_bot_rotation_for_future_attachment() {
    let fixture = fixture().await;
    let installation = install(&fixture).await;
    let blobs = BlobRepo::new(fixture.pool.clone());
    let blob = scoped_blob(
        &blobs,
        fixture.bot,
        fixture.workspace,
        "us-west-2",
        "rotation-ledger-blob",
        false,
    )
    .await;
    let probe = blob_probe(&installation, Uuid::new_v4(), [70; 32]);
    commit_blob(
        &fixture,
        &installation,
        &probe,
        &blob,
        Some("us-west-2:rotation-ledger-blob".into()),
    )
    .await;
    let references: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM messages
          WHERE deleted_at IS NULL
            AND blocks @> jsonb_build_array(jsonb_build_object('blob_id', $1::text))",
    )
    .bind(blob.id.to_string())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(references, 0);

    let rotated = fixture
        .repo
        .update(
            fixture.workspace,
            installation.id,
            fixture.owner,
            UpdateIntegrationInstallation {
                bot_id: Some(fixture.replacement_bot),
                room_ids: Some(vec![fixture.allowed_room]),
                ..UpdateIntegrationInstallation::default()
            },
        )
        .await
        .unwrap();
    let key = Uuid::new_v4();
    let hash = [71; 32];
    let probe = notification_probe(&rotated, key, hash);
    let IntegrationNotificationClaim::Acquired {
        lease_token: lease, ..
    } = fixture.repo.claim_notification(&probe).await.unwrap()
    else {
        panic!("rotated bot notification claim must acquire");
    };
    let mut request = notification(
        &rotated,
        key,
        hash,
        fixture.allowed_room,
        "attach pre-rotation upload",
    );
    request.blocks = vec![file_block(&blob)];
    request.lease_token = Some(lease);
    let published = fixture.repo.publish(request).await.unwrap();
    assert_eq!(published.message.sender_id, fixture.replacement_bot);
}
