use super::*;
use crate::{BlobRepo, BlobStorageScope, DeactivationRepo, SsoRepo, WorkspaceRepo};
use aero_common::{Error, WorkspaceRole};

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn integration_snaplink_user_resolves_canonical_dm_and_rejects_revoked_identities() {
    let fixture = fixture().await;
    let workspaces = WorkspaceRepo::new(fixture.pool.clone());
    let sso = SsoRepo::new(fixture.pool.clone());
    let deactivations = DeactivationRepo::new(fixture.pool.clone());
    let subject = format!("snaplink-user-{}", Uuid::new_v4());
    let target = participant(&fixture.pool, "integration-dm-target").await;
    let alternate_issuer_target = participant(&fixture.pool, "integration-dm-decoy").await;
    for member in [target, alternate_issuer_target] {
        workspaces
            .add_member(fixture.workspace, member, WorkspaceRole::Member)
            .await
            .expect("enroll Snaplink identity in integration workspace");
    }
    sso.link(&fixture.issuer, &subject, alternate_issuer_target, None)
        .await
        .expect("bind same subject under another issuer");
    sso.link(&fixture.user_identity_issuer, &subject, target, None)
        .await
        .expect("bind exact human identity issuer and subject");

    let mut input = installation_input(
        &fixture,
        fixture.owner,
        fixture.bot,
        &fixture.client_id,
        Vec::new(),
    );
    input.allow_user_dm = true;
    let installation = fixture
        .repo
        .create(input)
        .await
        .expect("create user-DM-enabled installation");
    let first = fixture
        .repo
        .resolve_target(
            installation.id,
            &fixture.issuer,
            &fixture.client_id,
            IntegrationTarget::SnaplinkUser(subject.clone()),
        )
        .await
        .expect("resolve exact Snaplink identity");
    let second = fixture
        .repo
        .resolve_target(
            installation.id,
            &fixture.issuer,
            &fixture.client_id,
            IntegrationTarget::SnaplinkUser(subject.clone()),
        )
        .await
        .expect("repeat resolution reuses canonical DM");
    assert_eq!(first.recipient, Some(target));
    assert_ne!(first.recipient, Some(alternate_issuer_target));
    assert_eq!(first.room_id, second.room_id);
    let canonical_dm_count: i64 = sqlx::query_scalar(
        r"SELECT count(*)
            FROM rooms room
           WHERE room.workspace_id = $1
             AND room.kind = 'direct'
             AND (SELECT count(*) FROM room_members member WHERE member.room_id = room.id) = 2
             AND EXISTS (
                 SELECT 1 FROM room_members member
                  WHERE member.room_id = room.id AND member.participant_id = $2
             )
             AND EXISTS (
                 SELECT 1 FROM room_members member
                  WHERE member.room_id = room.id AND member.participant_id = $3
             )",
    )
    .bind(fixture.workspace.to_uuid())
    .bind(fixture.bot.to_uuid())
    .bind(target.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(canonical_dm_count, 1);

    let dm_key = Uuid::new_v4();
    let dm_hash = [31; 32];
    let published = fixture
        .repo
        .publish(resolved_notification(
            &first,
            dm_key,
            dm_hash,
            vec![Block::text("ERP direct notification")],
        ))
        .await
        .expect("publish canonical direct notification");
    let stored_target_key: String = sqlx::query_scalar(
        "SELECT target_key FROM integration_notification_receipts
          WHERE installation_id = $1 AND idempotency_key = $2",
    )
    .bind(installation.id)
    .bind(dm_key)
    .fetch_one(&fixture.pool)
    .await
    .expect("load privacy-preserving receipt target");
    assert_eq!(
        stored_target_key,
        IntegrationTarget::SnaplinkUser(subject.clone()).key()
    );
    assert!(!stored_target_key.contains(&subject));
    let notification_jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM message_side_effect_jobs
          WHERE message_id = $1 AND mutation_version = $2 AND kind = 'notifications'",
    )
    .bind(published.message.id.to_uuid())
    .bind(published.message.version)
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(notification_jobs, 1);

    let replay = fixture
        .repo
        .probe_replay(&IntegrationReplayProbe {
            installation_id: installation.id,
            issuer: fixture.issuer.clone(),
            client_id: fixture.client_id.clone(),
            idempotency_key: dm_key,
            request_hash: dm_hash,
            target: IntegrationTarget::SnaplinkUser(subject.clone()),
        })
        .await
        .expect("probe a completed DM notification")
        .expect("completed DM has a canonical replay");
    assert_eq!(replay.message.id, published.message.id);

    // A completed conflicting retry must be rejected before target resolution;
    // in particular it must not materialize an empty DM for the wrong user.
    let conflicting_subject = format!("conflicting-user-{}", Uuid::new_v4());
    let conflicting_target = participant(&fixture.pool, "integration-conflicting-target").await;
    workspaces
        .add_member(fixture.workspace, conflicting_target, WorkspaceRole::Member)
        .await
        .unwrap();
    sso.link(
        &fixture.user_identity_issuer,
        &conflicting_subject,
        conflicting_target,
        None,
    )
    .await
    .unwrap();
    let conflict = fixture
        .repo
        .probe_replay(&IntegrationReplayProbe {
            installation_id: installation.id,
            issuer: fixture.issuer.clone(),
            client_id: fixture.client_id.clone(),
            idempotency_key: dm_key,
            request_hash: [32; 32],
            target: IntegrationTarget::SnaplinkUser(conflicting_subject),
        })
        .await
        .expect_err("a conflicting target is rejected before DM resolution");
    assert!(matches!(conflict, Error::Conflict(_)));
    let conflicting_dm_count: i64 = sqlx::query_scalar(
        r"SELECT count(*)
            FROM rooms room
           WHERE room.workspace_id = $1
             AND room.kind = 'direct'
             AND EXISTS (
                 SELECT 1 FROM room_members member
                  WHERE member.room_id = room.id AND member.participant_id = $2
             )
             AND EXISTS (
                 SELECT 1 FROM room_members member
                  WHERE member.room_id = room.id AND member.participant_id = $3
             )",
    )
    .bind(fixture.workspace.to_uuid())
    .bind(fixture.bot.to_uuid())
    .bind(conflicting_target.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(conflicting_dm_count, 0);

    let mut disabled_input = installation_input(
        &fixture,
        fixture.owner,
        fixture.bot,
        &format!("dm-disabled-{}", Uuid::new_v4()),
        Vec::new(),
    );
    disabled_input.allow_user_dm = false;
    let dm_disabled = fixture.repo.create(disabled_input).await.unwrap();
    assert!(matches!(
        fixture
            .repo
            .resolve_target(
                dm_disabled.id,
                &fixture.issuer,
                &dm_disabled.client_id,
                IntegrationTarget::SnaplinkUser(subject.clone()),
            )
            .await,
        Err(Error::Forbidden(_))
    ));

    let deactivated_subject = format!("deactivated-user-{}", Uuid::new_v4());
    let deactivated_target = participant(&fixture.pool, "integration-deactivated-target").await;
    workspaces
        .add_member(fixture.workspace, deactivated_target, WorkspaceRole::Member)
        .await
        .unwrap();
    sso.link(
        &fixture.user_identity_issuer,
        &deactivated_subject,
        deactivated_target,
        None,
    )
    .await
    .unwrap();
    deactivations
        .deactivate(fixture.workspace, deactivated_target, fixture.owner)
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .repo
            .resolve_target(
                installation.id,
                &fixture.issuer,
                &fixture.client_id,
                IntegrationTarget::SnaplinkUser(deactivated_subject),
            )
            .await,
        Err(Error::NotFound(_))
    ));

    sqlx::query("UPDATE participants SET deleted_at = now() WHERE id = $1")
        .bind(target.to_uuid())
        .execute(&fixture.pool)
        .await
        .expect("tombstone Snaplink target");
    assert!(matches!(
        fixture
            .repo
            .resolve_target(
                installation.id,
                &fixture.issuer,
                &fixture.client_id,
                IntegrationTarget::SnaplinkUser(subject),
            )
            .await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn integration_attachments_require_finalized_bot_owned_workspace_scoped_blobs() {
    let fixture = fixture().await;
    let installation = install(&fixture).await;
    let blobs = BlobRepo::new(fixture.pool.clone());
    let region = "us-west-2";
    WorkspaceRepo::new(fixture.pool.clone())
        .set_region_code(fixture.workspace, Some(region))
        .await
        .expect("set fixture storage region");

    let unfinished = scoped_blob(
        &blobs,
        fixture.bot,
        fixture.workspace,
        region,
        "unfinished-integration-attachment",
        false,
    )
    .await;
    let other_owner = scoped_blob(
        &blobs,
        fixture.owner,
        fixture.workspace,
        region,
        "other-owner-integration-attachment",
        true,
    )
    .await;
    let other_workspace = WorkspaceRepo::new(fixture.pool.clone())
        .create(
            format!("Other blob workspace {}", Uuid::new_v4()),
            format!("other-blob-workspace-{}", Uuid::new_v4()),
            fixture.owner,
        )
        .await
        .unwrap()
        .id;
    let cross_workspace = scoped_blob(
        &blobs,
        fixture.bot,
        other_workspace,
        "eu-west-1",
        "cross-workspace-integration-attachment",
        true,
    )
    .await;

    for (index, blob) in [unfinished, other_owner, cross_workspace]
        .iter()
        .enumerate()
    {
        let key = Uuid::new_v4();
        let mut request = notification(
            &installation,
            key,
            [u8::try_from(index + 40).unwrap(); 32],
            fixture.allowed_room,
            "attachment policy probe",
        );
        request.blocks = vec![file_block(blob)];
        let error = fixture
            .repo
            .publish(request)
            .await
            .expect_err("unavailable integration attachment must fail closed");
        assert!(matches!(error, Error::Forbidden(_)));
        assert_eq!(
            projection_counts(&fixture, installation.id, key).await,
            (0, 0, 0)
        );
    }

    let valid = scoped_blob(
        &blobs,
        fixture.bot,
        fixture.workspace,
        region,
        "valid-integration-attachment",
        true,
    )
    .await;
    assert_eq!(
        blobs.storage_scope(valid.id).await.unwrap(),
        Some(BlobStorageScope {
            workspace_id: Some(fixture.workspace),
            storage_region: Some(region.into()),
        })
    );
    let key = Uuid::new_v4();
    let mut request = notification(
        &installation,
        key,
        [49; 32],
        fixture.allowed_room,
        "valid scoped attachment",
    );
    request.blocks = vec![file_block(&valid)];
    let published = fixture
        .repo
        .publish(request)
        .await
        .expect("finalized bot-owned scoped attachment publishes");
    assert_eq!(published.message.sender_id, fixture.bot);
    assert_eq!(
        projection_counts(&fixture, installation.id, key).await,
        (1, 1, 1)
    );
}
