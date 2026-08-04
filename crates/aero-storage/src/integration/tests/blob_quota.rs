use super::*;

use std::time::Duration;

use aero_common::{Blob, Error, FileKind};
use sha2::{Digest as _, Sha256};

use crate::{BlobGcRepo, BlobRepo, NewBlob};

use super::machine_safety::blob_probe;

async fn sized_blob(fixture: &Fixture, label: &str, size: u64, finalize: bool) -> Blob {
    let repo = BlobRepo::new(fixture.pool.clone());
    let digest = hex::encode(Sha256::digest(
        format!("{label}-{}-{}", fixture.bot, fixture.workspace).as_bytes(),
    ));
    let reserved = repo
        .reserve_in_scope(
            NewBlob {
                owner_id: fixture.bot,
                kind: FileKind::Document,
                name: format!("{label}.bin"),
                mime: "application/octet-stream".into(),
                size,
                sha256: Some(digest),
                storage_key: format!("pending:{label}"),
            },
            Some(fixture.workspace),
            Some("us-west-2"),
        )
        .await
        .expect("reserve sized integration blob");
    if finalize {
        repo.finalize(reserved.id, &format!("us-west-2:{label}"))
            .await
            .expect("finalize sized integration blob")
            .expect("sized reservation remains available")
    } else {
        reserved
    }
}

async fn claimed_quota_reservation(
    fixture: &Fixture,
    installation: &IntegrationInstallation,
    blob: &Blob,
    marker: u8,
) -> (IntegrationBlobProbe, IntegrationBlobQuotaReservation) {
    let probe = blob_probe(installation, Uuid::new_v4(), [marker; 32]);
    let (lease_token, target) = match fixture.repo.claim_blob(&probe).await.unwrap() {
        IntegrationBlobClaim::Acquired {
            lease_token,
            target,
        } => (lease_token, target),
        IntegrationBlobClaim::Pending => panic!("fresh quota key cannot be pending"),
        IntegrationBlobClaim::Replay(_) => panic!("fresh quota key cannot replay"),
    };
    let room_id = target
        .room_id
        .expect("room target is materialized during claim");
    let reservation = IntegrationBlobQuotaReservation {
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
    };
    (probe, reservation)
}

async fn wait_until_blocked_by(fixture: &Fixture, blocker_pid: i32) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                r"SELECT EXISTS (
                      SELECT 1 FROM pg_stat_activity activity
                       WHERE $1 = ANY(pg_blocking_pids(activity.pid))
                         AND activity.wait_event_type = 'Lock'
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
    .expect("operation must wait on the blob lifecycle lock");
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn parallel_prewrite_quota_reservations_never_exceed_installation_cap() {
    let fixture = fixture().await;
    let installation = install(&fixture).await;
    let candidate_bytes = 16 * 1024 * 1024_u64;
    let base = sized_blob(
        &fixture,
        "quota-parallel-base",
        u64::try_from(MAX_INTEGRATION_BLOB_BYTES).unwrap() - candidate_bytes,
        // Keep the baseline unfinished: pending reservations are deliberately
        // chargeable and the global ordinary ledger sweep must not release a
        // different test fixture while these tests execute in parallel.
        false,
    )
    .await;
    sqlx::query("INSERT INTO integration_blob_ledger (installation_id, blob_id) VALUES ($1, $2)")
        .bind(installation.id)
        .bind(base.id.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();

    let first = sized_blob(&fixture, "quota-parallel-first", candidate_bytes, false).await;
    let second = sized_blob(&fixture, "quota-parallel-second", candidate_bytes, false).await;
    let (_, first_reservation) =
        claimed_quota_reservation(&fixture, &installation, &first, 81).await;
    let (_, second_reservation) =
        claimed_quota_reservation(&fixture, &installation, &second, 82).await;
    let first_repo = fixture.repo.clone();
    let second_repo = fixture.repo.clone();
    let first_task = tokio::spawn(async move {
        first_repo
            .reserve_blob_upload_quota(first_reservation)
            .await
    });
    let second_task = tokio::spawn(async move {
        second_repo
            .reserve_blob_upload_quota(second_reservation)
            .await
    });
    let (first_result, second_result) = tokio::join!(first_task, second_task);
    let results = [first_result.unwrap(), second_result.unwrap()];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(Error::Conflict(_))))
            .count(),
        1
    );

    let (count, bytes): (i64, i64) = sqlx::query_as(
        r"SELECT count(*), COALESCE(sum(blob.size), 0)::bigint
            FROM integration_blob_ledger ledger
            JOIN blobs blob ON blob.id = ledger.blob_id
           WHERE ledger.installation_id = $1",
    )
    .bind(installation.id)
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(count, 2, "only the base and one contender are charged");
    assert_eq!(bytes, MAX_INTEGRATION_BLOB_BYTES);

    let over_cap = sized_blob(&fixture, "quota-full-rejection", 1, false).await;
    let (_, over_cap_reservation) =
        claimed_quota_reservation(&fixture, &installation, &over_cap, 83).await;
    assert!(matches!(
        fixture
            .repo
            .reserve_blob_upload_quota(over_cap_reservation)
            .await,
        Err(Error::Conflict(_))
    ));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn commit_reuses_prewrite_quota_reservation_idempotently() {
    let fixture = fixture().await;
    let installation = install(&fixture).await;
    let blob = sized_blob(&fixture, "quota-prewrite-commit", 23, false).await;
    let (probe, reservation) = claimed_quota_reservation(&fixture, &installation, &blob, 84).await;
    fixture
        .repo
        .reserve_blob_upload_quota(reservation.clone())
        .await
        .unwrap();
    let outcome = fixture
        .repo
        .commit_blob(IntegrationBlobCommit {
            installation_id: reservation.installation_id,
            issuer: reservation.issuer,
            client_id: reservation.client_id,
            idempotency_key: reservation.idempotency_key,
            request_hash: reservation.request_hash,
            target: reservation.target,
            room_id: reservation.room_id,
            recipient: reservation.recipient,
            lease_token: reservation.lease_token,
            blob_id: reservation.blob_id,
            content_sha256: reservation.content_sha256,
            storage_key: Some("us-west-2:quota-prewrite-commit".into()),
        })
        .await
        .unwrap();
    assert_eq!(outcome.blob.id, blob.id);
    assert!(outcome.blob.finalized_at.is_some());

    let (ledger, receipt): (i64, i64) = sqlx::query_as(
        r"SELECT
             (SELECT count(*) FROM integration_blob_ledger
               WHERE installation_id = $1 AND blob_id = $2),
             (SELECT count(*) FROM integration_blob_receipts
               WHERE installation_id = $1 AND idempotency_key = $3)",
    )
    .bind(installation.id)
    .bind(blob.id.to_uuid())
    .bind(probe.idempotency_key)
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!((ledger, receipt), (1, 1));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn unfinished_quota_charge_survives_receipt_sweep_until_forced_stale_gc() {
    let fixture = fixture().await;
    let installation = install(&fixture).await;
    let blob = sized_blob(&fixture, "quota-stale-reservation", 29, false).await;
    let (probe, reservation) = claimed_quota_reservation(&fixture, &installation, &blob, 85).await;
    fixture
        .repo
        .reserve_blob_upload_quota(reservation.clone())
        .await
        .unwrap();
    fixture
        .repo
        .release_blob_claim(
            installation.id,
            probe.idempotency_key,
            reservation.lease_token,
        )
        .await
        .unwrap();
    sqlx::query(
        "UPDATE integration_machine_requests SET expires_at = now() - interval '1 second'
          WHERE installation_id = $1 AND operation = 'blob' AND idempotency_key = $2",
    )
    .bind(installation.id)
    .bind(probe.idempotency_key)
    .execute(&fixture.pool)
    .await
    .unwrap();

    fixture.repo.sweep_expired_machine_state(50).await.unwrap();
    let charged: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM integration_blob_ledger
          WHERE installation_id = $1 AND blob_id = $2)",
    )
    .bind(installation.id)
    .bind(blob.id.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert!(charged, "unfinished reservations remain fail-closed");

    sqlx::query("UPDATE blobs SET created_at = now() - interval '2 hours' WHERE id = $1")
        .bind(blob.id.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();
    let gc = BlobGcRepo::new(fixture.pool.clone());
    gc.enqueue_stale_reservations(time::OffsetDateTime::now_utc() - time::Duration::hours(1))
        .await
        .unwrap();
    let forced: bool =
        sqlx::query_scalar("SELECT force_delete FROM blob_gc_queue WHERE blob_id = $1")
            .bind(blob.id.to_uuid())
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert!(forced);
    gc.ack(blob.id).await.unwrap();
    let (blob_exists, ledger_exists): (bool, bool) = sqlx::query_as(
        r"SELECT
             EXISTS (SELECT 1 FROM blobs WHERE id = $1),
             EXISTS (SELECT 1 FROM integration_blob_ledger WHERE blob_id = $1)",
    )
    .bind(blob.id.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!((blob_exists, ledger_exists), (false, false));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn waiting_attachment_rechecks_ledger_and_gc_queue_after_blob_lock() {
    let fixture = fixture().await;
    let installation = install(&fixture).await;
    let blob = sized_blob(&fixture, "quota-attachment-lock", 31, true).await;
    sqlx::query("INSERT INTO integration_blob_ledger (installation_id, blob_id) VALUES ($1, $2)")
        .bind(installation.id)
        .bind(blob.id.to_uuid())
        .execute(&fixture.pool)
        .await
        .unwrap();

    let mut blocker = fixture.pool.begin().await.unwrap();
    let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM blobs WHERE id = $1 FOR UPDATE")
        .bind(blob.id.to_uuid())
        .fetch_one(&mut *blocker)
        .await
        .unwrap();

    let task_pool = fixture.pool.clone();
    let blocks = vec![file_block(&blob)];
    let bot = fixture.bot;
    let room = fixture.allowed_room;
    let installation_id = installation.id;
    let task = tokio::spawn(async move {
        let mut tx = task_pool.begin().await.unwrap();
        let result = BlobRepo::lock_integration_attachments_in_tx(
            &mut tx,
            &blocks,
            bot,
            room,
            installation_id,
        )
        .await;
        tx.rollback().await.unwrap();
        result
    });
    wait_until_blocked_by(&fixture, blocker_pid).await;
    sqlx::query("DELETE FROM integration_blob_ledger WHERE installation_id = $1 AND blob_id = $2")
        .bind(installation.id)
        .bind(blob.id.to_uuid())
        .execute(&mut *blocker)
        .await
        .unwrap();
    sqlx::query("INSERT INTO blob_gc_queue (blob_id, force_delete) VALUES ($1, FALSE)")
        .bind(blob.id.to_uuid())
        .execute(&mut *blocker)
        .await
        .unwrap();
    blocker.commit().await.unwrap();

    let authorized = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("attachment check must resume")
        .unwrap()
        .unwrap();
    assert!(!authorized, "post-wait lifecycle state must be rechecked");
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn waiting_prewrite_reservation_rechecks_gc_queue_after_blob_lock() {
    let fixture = fixture().await;
    let installation = install(&fixture).await;
    let blob = sized_blob(&fixture, "quota-reservation-lock", 37, false).await;
    let (_, reservation) = claimed_quota_reservation(&fixture, &installation, &blob, 86).await;

    let mut blocker = fixture.pool.begin().await.unwrap();
    let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM blobs WHERE id = $1 FOR UPDATE")
        .bind(blob.id.to_uuid())
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    let repo = fixture.repo.clone();
    let task = tokio::spawn(async move { repo.reserve_blob_upload_quota(reservation).await });
    wait_until_blocked_by(&fixture, blocker_pid).await;
    sqlx::query("INSERT INTO blob_gc_queue (blob_id, force_delete) VALUES ($1, TRUE)")
        .bind(blob.id.to_uuid())
        .execute(&mut *blocker)
        .await
        .unwrap();
    blocker.commit().await.unwrap();

    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("quota reservation must resume")
            .unwrap(),
        Err(Error::Conflict(_))
    ));
    let charged: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM integration_blob_ledger
          WHERE installation_id = $1 AND blob_id = $2)",
    )
    .bind(installation.id)
    .bind(blob.id.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert!(!charged);
}
