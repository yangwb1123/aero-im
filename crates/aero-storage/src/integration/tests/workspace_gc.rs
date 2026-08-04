use super::*;

use super::machine_safety::{another_installation, blob_probe, commit_blob};
use std::time::Duration;

use crate::{BlobGcRepo, BlobRepo, WorkspaceRepo};
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn workspace_hard_delete_enqueues_shared_installation_blob_once() {
    let fixture = fixture().await;
    let first = install(&fixture).await;
    let second = another_installation(&fixture, "workspace-gc-second").await;
    let blobs = BlobRepo::new(fixture.pool.clone());
    let blob = scoped_blob(
        &blobs,
        fixture.bot,
        fixture.workspace,
        "us-west-2",
        "workspace-hard-delete-shared-blob",
        false,
    )
    .await;
    for (installation, marker) in [(&first, 75_u8), (&second, 76_u8)] {
        let probe = blob_probe(installation, Uuid::new_v4(), [marker; 32]);
        commit_blob(
            &fixture,
            installation,
            &probe,
            &blob,
            if installation.id == first.id {
                Some("us-west-2:workspace-hard-delete-shared-blob".into())
            } else {
                None
            },
        )
        .await;
    }

    let deleted = WorkspaceRepo::new(fixture.pool.clone())
        .delete_authorized(fixture.workspace, fixture.owner)
        .await
        .expect("workspace hard delete");
    assert!(deleted);

    let queued: (i64, bool) = sqlx::query_as(
        r"SELECT count(*), COALESCE(bool_or(force_delete), FALSE)
            FROM blob_gc_queue
           WHERE blob_id = $1",
    )
    .bind(blob.id.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(queued, (1, false));
    assert!(!blobs.has_live_references(blob.id).await.unwrap());

    // The ordinary GC item is now drainable: in production the worker first
    // deletes Vault bytes, then acks the metadata row exactly as below.
    BlobGcRepo::new(fixture.pool.clone())
        .ack(blob.id)
        .await
        .unwrap();
    assert!(blobs.get(blob.id).await.unwrap().is_none());
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn workspace_delete_and_blob_sweep_share_ledger_then_queue_lock_order() {
    let fixture = fixture().await;
    let first = install(&fixture).await;
    let second = another_installation(&fixture, "workspace-lock-order-second").await;
    let blobs = BlobRepo::new(fixture.pool.clone());
    let blob = scoped_blob(
        &blobs,
        fixture.bot,
        fixture.workspace,
        "us-west-2",
        "workspace-delete-sweep-lock-order",
        false,
    )
    .await;
    for (installation, marker) in [(&first, 77_u8), (&second, 78_u8)] {
        let probe = blob_probe(installation, Uuid::new_v4(), [marker; 32]);
        commit_blob(
            &fixture,
            installation,
            &probe,
            &blob,
            if installation.id == first.id {
                Some("us-west-2:workspace-delete-sweep-lock-order".into())
            } else {
                None
            },
        )
        .await;
        sqlx::query(
            "UPDATE integration_machine_requests SET expires_at = now() - interval '1 second'
              WHERE installation_id = $1 AND operation = 'blob' AND idempotency_key = $2",
        )
        .bind(installation.id)
        .bind(probe.idempotency_key)
        .execute(&fixture.pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE integration_blob_receipts SET expires_at = now() - interval '1 second'
              WHERE installation_id = $1 AND idempotency_key = $2",
        )
        .bind(installation.id)
        .bind(probe.idempotency_key)
        .execute(&fixture.pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO blob_gc_queue (blob_id, force_delete) VALUES ($1, FALSE)
         ON CONFLICT (blob_id) DO NOTHING",
    )
    .bind(blob.id.to_uuid())
    .execute(&fixture.pool)
    .await
    .unwrap();

    let mut queue_lock = fixture.pool.begin().await.unwrap();
    let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *queue_lock)
        .await
        .unwrap();
    sqlx::query("SELECT true FROM blob_gc_queue WHERE blob_id = $1 FOR UPDATE")
        .bind(blob.id.to_uuid())
        .fetch_one(&mut *queue_lock)
        .await
        .unwrap();

    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    let delete_pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|connection, _| {
            Box::pin(async move {
                sqlx::query("SET lock_timeout = '5s'")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&database_url)
        .await
        .unwrap();
    let delete_repo = WorkspaceRepo::new(delete_pool);
    let workspace = fixture.workspace;
    let owner = fixture.owner;
    let delete_task =
        tokio::spawn(async move { delete_repo.delete_authorized(workspace, owner).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                r"SELECT EXISTS (
                      SELECT 1 FROM pg_stat_activity activity
                       WHERE $1 = ANY(pg_blocking_pids(activity.pid))
                         AND activity.wait_event_type = 'Lock'
                         AND activity.query ILIKE '%INSERT INTO blob_gc_queue%'
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
    .expect("workspace delete must reach the queue after locking ledger rows");

    let sweep = tokio::time::timeout(
        Duration::from_secs(2),
        fixture.repo.sweep_expired_machine_state(50),
    )
    .await
    .expect("sweep must skip the delete-owned ledger instead of deadlocking")
    .unwrap();
    assert_eq!(sweep.blob_ledger_releases, 0);
    queue_lock.commit().await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(2), delete_task)
        .await
        .expect("workspace delete must finish after queue lock releases")
        .unwrap()
        .unwrap());
}
