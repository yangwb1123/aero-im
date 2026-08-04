use super::*;

use aero_common::{Error, WorkspaceRole};

use super::machine_safety::{blob_probe, commit_blob};
use crate::{BlobRepo, SsoRepo, WorkspaceRepo};

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn trusted_issuer_rotation_preserves_receipts_and_blob_ledger() {
    let fixture = fixture().await;
    let installation = install(&fixture).await;
    let message_key = Uuid::new_v4();
    let message_hash = [73; 32];
    let canonical = fixture
        .repo
        .publish(notification(
            &installation,
            message_key,
            message_hash,
            fixture.allowed_room,
            "before trusted issuer rotation",
        ))
        .await
        .unwrap();

    let blobs = BlobRepo::new(fixture.pool.clone());
    let blob = scoped_blob(
        &blobs,
        fixture.bot,
        fixture.workspace,
        "us-west-2",
        "issuer-rotation-ledger-blob",
        false,
    )
    .await;
    let blob_probe = blob_probe(&installation, Uuid::new_v4(), [74; 32]);
    commit_blob(
        &fixture,
        &installation,
        &blob_probe,
        &blob,
        Some("us-west-2:issuer-rotation-ledger-blob".into()),
    )
    .await;

    let old_issuer = installation.issuer.clone();
    let old_client = installation.client_id.clone();
    let new_issuer = format!("https://new-snaplink.test/{}", Uuid::new_v4());
    let new_client = format!("rotated-client-{}", Uuid::new_v4());
    let rotated = fixture
        .repo
        .update(
            fixture.workspace,
            installation.id,
            fixture.owner,
            UpdateIntegrationInstallation {
                issuer: Some(new_issuer.clone()),
                client_id: Some(new_client.clone()),
                ..UpdateIntegrationInstallation::default()
            },
        )
        .await
        .expect("trusted control plane rotates issuer and client atomically");
    assert_eq!(rotated.id, installation.id);
    assert_eq!(rotated.issuer, new_issuer);
    assert_eq!(
        rotated.user_identity_issuer,
        installation.user_identity_issuer
    );
    assert_eq!(rotated.client_id, new_client);
    assert!(matches!(
        fixture
            .repo
            .authorize_client(installation.id, &old_issuer, &old_client)
            .await,
        Err(Error::Forbidden(_))
    ));
    fixture
        .repo
        .authorize_client(installation.id, &rotated.issuer, &rotated.client_id)
        .await
        .expect("new issuer and client are immediately active");

    let replay = fixture
        .repo
        .replay(&notification(
            &rotated,
            message_key,
            message_hash,
            fixture.allowed_room,
            "before trusted issuer rotation",
        ))
        .await
        .unwrap()
        .expect("notification receipt survives issuer rotation");
    assert_eq!(replay.message.id, canonical.message.id);
    let rotated_blob_probe = IntegrationBlobProbe {
        issuer: rotated.issuer.clone(),
        client_id: rotated.client_id.clone(),
        ..blob_probe.clone()
    };
    match fixture.repo.claim_blob(&rotated_blob_probe).await.unwrap() {
        IntegrationBlobClaim::Replay(replay) => assert_eq!(replay.blob.id, blob.id),
        _ => panic!("blob receipt must survive issuer rotation"),
    }
    let ledger_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM integration_blob_ledger
          WHERE installation_id = $1 AND blob_id = $2",
    )
    .bind(installation.id)
    .bind(blob.id.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(ledger_rows, 1);

    let detail: serde_json::Value = sqlx::query_scalar(
        "SELECT detail FROM audit_events
          WHERE workspace_id = $1 AND action = 'integration.installation.updated'
            AND target = $2 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(fixture.workspace.to_uuid())
    .bind(installation.id.to_string())
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(
        detail.get("issuer_rotated"),
        Some(&serde_json::Value::Bool(true))
    );
    let serialized = detail.to_string();
    assert!(!serialized.contains(&old_issuer));
    assert!(!serialized.contains(&rotated.issuer));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn human_identity_issuer_rotation_does_not_rotate_machine_credentials() {
    let fixture = fixture().await;
    let workspaces = WorkspaceRepo::new(fixture.pool.clone());
    let identities = SsoRepo::new(fixture.pool.clone());
    let subject = format!("issuer-rotation-user-{}", Uuid::new_v4());
    let original_target = participant(&fixture.pool, "original-human-issuer-target").await;
    let rotated_target = participant(&fixture.pool, "rotated-human-issuer-target").await;
    for target in [original_target, rotated_target] {
        workspaces
            .add_member(fixture.workspace, target, WorkspaceRole::Member)
            .await
            .expect("add human issuer fixture member");
    }
    identities
        .link(
            &fixture.user_identity_issuer,
            &subject,
            original_target,
            None,
        )
        .await
        .expect("bind original human issuer");
    let new_human_issuer = format!("https://new-human-snaplink.test/{}", Uuid::new_v4());
    identities
        .link(&new_human_issuer, &subject, rotated_target, None)
        .await
        .expect("bind rotated human issuer");

    let mut input = installation_input(
        &fixture,
        fixture.owner,
        fixture.bot,
        &fixture.client_id,
        Vec::new(),
    );
    input.allow_user_dm = true;
    let installation = fixture.repo.create(input).await.unwrap();
    let original = fixture
        .repo
        .resolve_target(
            installation.id,
            &fixture.issuer,
            &fixture.client_id,
            IntegrationTarget::SnaplinkUser(subject.clone()),
        )
        .await
        .expect("original human namespace resolves");
    assert_eq!(original.recipient, Some(original_target));

    let rotated = fixture
        .repo
        .update(
            fixture.workspace,
            installation.id,
            fixture.owner,
            UpdateIntegrationInstallation {
                user_identity_issuer: Some(new_human_issuer.clone()),
                ..UpdateIntegrationInstallation::default()
            },
        )
        .await
        .expect("administrator rotates human identity namespace");
    assert_eq!(rotated.issuer, fixture.issuer);
    assert_eq!(rotated.client_id, fixture.client_id);
    assert_eq!(rotated.user_identity_issuer, new_human_issuer);
    fixture
        .repo
        .authorize_client(rotated.id, &fixture.issuer, &fixture.client_id)
        .await
        .expect("same machine credentials remain authorized");
    let resolved = fixture
        .repo
        .resolve_target(
            rotated.id,
            &fixture.issuer,
            &fixture.client_id,
            IntegrationTarget::SnaplinkUser(subject),
        )
        .await
        .expect("rotated human namespace resolves");
    assert_eq!(resolved.recipient, Some(rotated_target));
}
