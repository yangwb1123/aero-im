use super::*;

use aero_common::{Blob, BlobId, FileKind, MessageId};

fn assert_redacted(value: &impl std::fmt::Debug, secrets: &[&str]) {
    let rendered = format!("{value:?}");
    assert!(
        rendered.contains("[REDACTED]"),
        "Debug output must make redaction explicit: {rendered}"
    );
    for secret in secrets {
        assert!(
            !rendered.contains(secret),
            "Debug output exposed a sensitive value: {rendered}"
        );
    }
}

#[test]
fn integration_debug_output_redacts_external_identity_content_and_credentials() {
    let issuer = "https://issuer.secret.example/tenant";
    let user_identity_issuer = "https://human-issuer.secret.example/tenant";
    let client_id = "client-secret-marker";
    let subject = "snaplink-subject-secret-marker";
    let block_content = "message-block-secret-marker";
    let traceparent = "traceparent-secret-marker";
    let storage_key = "vault-storage-key-secret-marker";
    let content_sha256 = "sha256-secret-marker";
    let lease_token = Uuid::new_v4();
    let lease = lease_token.to_string();
    let installation_id = Uuid::new_v4();
    let workspace_id = WorkspaceId::new();
    let bot_id = ParticipantId::new();
    let room_id = RoomId::new();
    let target = IntegrationTarget::SnaplinkUser(subject.into());
    let installation = IntegrationInstallation {
        id: installation_id,
        workspace_id,
        bot_id,
        issuer: issuer.into(),
        user_identity_issuer: user_identity_issuer.into(),
        client_id: client_id.into(),
        name: "redaction fixture".into(),
        active: true,
        allow_user_dm: true,
        room_ids: vec![room_id],
        created_by: Some(ParticipantId::new()),
        created_at: time::OffsetDateTime::UNIX_EPOCH,
        updated_at: time::OffsetDateTime::UNIX_EPOCH,
    };
    let secrets = [
        issuer,
        user_identity_issuer,
        client_id,
        subject,
        block_content,
        traceparent,
    ];

    assert_redacted(&target, &secrets);
    assert_redacted(&installation, &secrets);
    assert_redacted(
        &NewIntegrationInstallation {
            workspace_id,
            bot_id,
            issuer: issuer.into(),
            user_identity_issuer: user_identity_issuer.into(),
            client_id: client_id.into(),
            name: "redaction fixture".into(),
            allow_user_dm: true,
            room_ids: vec![room_id],
            created_by: ParticipantId::new(),
        },
        &secrets,
    );
    assert_redacted(
        &UpdateIntegrationInstallation {
            issuer: Some(issuer.into()),
            user_identity_issuer: Some(user_identity_issuer.into()),
            client_id: Some(client_id.into()),
            ..UpdateIntegrationInstallation::default()
        },
        &secrets,
    );

    let prepared = PreparedIntegrationTarget {
        installation: installation.clone(),
        target: target.clone(),
        room_id: None,
        recipient: Some(ParticipantId::new()),
    };
    assert_redacted(&prepared, &secrets);
    assert_redacted(
        &IntegrationNotificationClaim::Acquired {
            lease_token,
            target: prepared,
        },
        &[issuer, client_id, subject, &lease],
    );

    let idempotency_key = Uuid::new_v4();
    let request_hash = [0x5a; 32];
    assert_redacted(
        &IntegrationReplayProbe {
            installation_id,
            issuer: issuer.into(),
            client_id: client_id.into(),
            idempotency_key,
            request_hash,
            target: target.clone(),
        },
        &secrets,
    );
    assert_redacted(
        &NewIntegrationNotification {
            installation_id,
            issuer: issuer.into(),
            client_id: client_id.into(),
            idempotency_key,
            request_hash,
            target: target.clone(),
            room_id,
            recipient: None,
            blocks: vec![Block::text(block_content)],
            traceparent: Some(traceparent.into()),
            lease_token: Some(lease_token),
        },
        &[
            issuer,
            client_id,
            subject,
            block_content,
            traceparent,
            &lease,
        ],
    );

    let blob_probe = IntegrationBlobProbe {
        installation_id,
        issuer: issuer.into(),
        client_id: client_id.into(),
        idempotency_key,
        request_hash,
        target: target.clone(),
    };
    assert_redacted(&blob_probe, &secrets);
    assert_redacted(
        &IntegrationBlobClaim::Acquired {
            lease_token,
            target: PreparedIntegrationTarget {
                installation: installation.clone(),
                target: target.clone(),
                room_id: None,
                recipient: Some(ParticipantId::new()),
            },
        },
        &[issuer, client_id, subject, &lease],
    );
    assert_redacted(
        &IntegrationBlobCommit {
            installation_id,
            issuer: issuer.into(),
            client_id: client_id.into(),
            idempotency_key,
            request_hash,
            target: target.clone(),
            room_id,
            recipient: None,
            lease_token,
            blob_id: BlobId::new(),
            content_sha256: content_sha256.into(),
            storage_key: Some(storage_key.into()),
        },
        &[
            issuer,
            client_id,
            subject,
            content_sha256,
            storage_key,
            &lease,
        ],
    );

    let message = Message {
        id: MessageId::new(),
        room_id,
        sender_id: bot_id,
        blocks: vec![Block::text(block_content)],
        reply_to: None,
        metadata: serde_json::json!({"secret": block_content}),
        created_at: time::OffsetDateTime::UNIX_EPOCH,
        edited_at: None,
        deleted_at: None,
        recalled_at: None,
        recalled_by: None,
        expires_at: None,
        version: 1,
    };
    assert_redacted(
        &IntegrationPublishOutcome {
            message,
            outbox_id: Uuid::new_v4(),
            deduplicated: false,
        },
        &[block_content],
    );

    let blob = Blob {
        id: BlobId::new(),
        owner_id: bot_id,
        workspace_id: Some(workspace_id),
        storage_region: Some("vault-region-secret-marker".into()),
        kind: FileKind::Document,
        name: "secret-name.pdf".into(),
        mime: "application/pdf".into(),
        size: 1,
        sha256: Some(content_sha256.into()),
        storage_key: storage_key.into(),
        created_at: time::OffsetDateTime::UNIX_EPOCH,
        finalized_at: Some(time::OffsetDateTime::UNIX_EPOCH),
    };
    let outcome = IntegrationBlobOutcome {
        blob,
        deduplicated: false,
    };
    assert_redacted(&outcome, &[content_sha256, storage_key]);
    assert_redacted(
        &IntegrationBlobCommitResolution::Completed(Box::new(outcome)),
        &[content_sha256, storage_key],
    );
}
