use super::*;
use crate::WorkspaceRepo;
use aero_common::Error;

#[test]
fn installation_input_is_bounded_and_rooms_are_deduplicated() {
    assert!(validate_installation_fields(
        "https://machine-sso.test",
        "https://human-sso.test",
        "erp",
        "ERP",
        &[]
    )
    .is_ok());
    assert!(
        validate_installation_fields(" bad", "https://human-sso.test", "erp", "ERP", &[]).is_err()
    );
    assert!(
        validate_installation_fields("https://machine-sso.test", " bad", "erp", "ERP", &[])
            .is_err()
    );
    assert!(validate_installation_fields(
        "https://machine-sso.test",
        "https://human-sso.test",
        "",
        "ERP",
        &[]
    )
    .is_err());
    assert!(validate_installation_fields(
        "https://machine-sso.test",
        "https://human-sso.test",
        "erp",
        "\n",
        &[]
    )
    .is_err());
    let room = RoomId::new();
    assert_eq!(canonical_room_ids(&[room, room]).unwrap(), vec![room]);
    assert!(canonical_room_ids(&vec![RoomId::new(); MAX_INTEGRATION_ROOMS + 1]).is_err());

    let subject = "human-readable-snaplink-subject";
    let fingerprint = IntegrationTarget::SnaplinkUser(subject.into()).key();
    assert!(fingerprint.starts_with("sha256:"));
    assert_eq!(fingerprint.len(), "sha256:".len() + 64);
    assert!(!fingerprint.contains(subject));
    assert_eq!(
        fingerprint,
        IntegrationTarget::SnaplinkUser(subject.into()).key()
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn integration_installation_requires_admin_and_enforces_room_allowlist() {
    let fixture = fixture().await;
    let unauthorized = fixture
        .repo
        .create(installation_input(
            &fixture,
            fixture.member,
            fixture.bot,
            &format!("unauthorized-{}", Uuid::new_v4()),
            vec![fixture.allowed_room],
        ))
        .await
        .expect_err("ordinary workspace members cannot create installations");
    assert!(matches!(unauthorized, Error::Forbidden(_)));

    let installation = fixture
        .repo
        .create(installation_input(
            &fixture,
            fixture.owner,
            fixture.bot,
            &fixture.client_id,
            vec![fixture.allowed_room, fixture.allowed_room],
        ))
        .await
        .expect("owner creates installation");
    assert_eq!(installation.room_ids, vec![fixture.allowed_room]);
    assert_eq!(installation.created_by, Some(fixture.owner));
    assert_eq!(installation.issuer, fixture.issuer);
    assert_eq!(
        installation.user_identity_issuer,
        fixture.user_identity_issuer
    );

    let authorized = fixture
        .repo
        .authorize_client(installation.id, &fixture.issuer, &fixture.client_id)
        .await
        .expect("exact issuer/client is authorized");
    assert_eq!(authorized.bot_id, fixture.bot);
    for (issuer, client_id) in [
        ("https://wrong-issuer.test", fixture.client_id.as_str()),
        (fixture.issuer.as_str(), "wrong-client"),
    ] {
        let error = fixture
            .repo
            .authorize_client(installation.id, issuer, client_id)
            .await
            .expect_err("credentials are exact-match bindings");
        assert!(matches!(error, Error::Forbidden(_)));
    }

    let allowed = fixture
        .repo
        .resolve_target(
            installation.id,
            &fixture.issuer,
            &fixture.client_id,
            IntegrationTarget::Room(fixture.allowed_room),
        )
        .await
        .expect("whitelisted room resolves");
    assert_eq!(allowed.room_id, fixture.allowed_room);
    assert!(matches!(
        allowed.target,
        IntegrationTarget::Room(room) if room == fixture.allowed_room
    ));

    let denied = fixture
        .repo
        .resolve_target(
            installation.id,
            &fixture.issuer,
            &fixture.client_id,
            IntegrationTarget::Room(fixture.unlisted_room),
        )
        .await
        .expect_err("bot membership alone cannot bypass the installation allowlist");
    assert!(matches!(denied, Error::Forbidden(_)));

    let key = Uuid::new_v4();
    let bypass = fixture
        .repo
        .publish(notification(
            &installation,
            key,
            [1; 32],
            fixture.unlisted_room,
            "must not publish",
        ))
        .await
        .expect_err("publish revalidates the allowlist transactionally");
    assert!(matches!(bypass, Error::Forbidden(_)));
    assert_eq!(
        projection_counts(&fixture, installation.id, key).await,
        (0, 0, 0)
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn integration_identical_concurrent_replay_has_one_message_receipt_and_outbox() {
    let fixture = fixture().await;
    let installation = install(&fixture).await;
    let key = Uuid::new_v4();
    let request = notification(
        &installation,
        key,
        [7; 32],
        fixture.allowed_room,
        "ERP invoice approved",
    );
    assert!(fixture
        .repo
        .replay(&request)
        .await
        .expect("a fresh key has no fast replay")
        .is_none());

    let (first, second) = tokio::join!(
        fixture.repo.publish(request.clone()),
        fixture.repo.publish(request.clone())
    );
    let first = first.expect("first concurrent publish");
    let second = second.expect("identical concurrent replay");
    assert_eq!(first.message.id, second.message.id);
    assert_eq!(first.outbox_id, second.outbox_id);
    assert_ne!(first.deduplicated, second.deduplicated);
    let fast_replay = fixture
        .repo
        .replay(&request)
        .await
        .expect("load fast replay")
        .expect("completed key has a canonical replay");
    assert!(fast_replay.deduplicated);
    assert_eq!(fast_replay.message.id, first.message.id);
    assert_eq!(fast_replay.outbox_id, first.outbox_id);
    assert_eq!(
        projection_counts(&fixture, installation.id, key).await,
        (1, 1, 1)
    );

    let receipt: (Vec<u8>, Uuid, Uuid) = sqlx::query_as(
        r"SELECT request_hash, message_id, outbox_id
            FROM integration_notification_receipts
           WHERE installation_id = $1 AND idempotency_key = $2",
    )
    .bind(installation.id)
    .bind(key)
    .fetch_one(&fixture.pool)
    .await
    .expect("load canonical integration receipt");
    assert_eq!(receipt.0, vec![7; 32]);
    assert_eq!(receipt.1, first.message.id.to_uuid());
    assert_eq!(receipt.2, first.outbox_id);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn integration_receipts_do_not_block_authorized_workspace_deletion() {
    let fixture = fixture().await;
    let installation = install(&fixture).await;
    fixture
        .repo
        .publish(notification(
            &installation,
            Uuid::new_v4(),
            [9; 32],
            fixture.allowed_room,
            "notification before workspace erasure",
        ))
        .await
        .expect("publish before workspace erasure");

    let deleted = WorkspaceRepo::new(fixture.pool.clone())
        .delete_authorized(fixture.workspace, fixture.owner)
        .await
        .expect("integration receipts must cascade during workspace erasure");
    assert!(deleted);
    let receipts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM integration_notification_receipts WHERE installation_id = $1",
    )
    .bind(installation.id)
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(receipts, 0);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn integration_reusing_key_for_different_request_conflicts_without_side_effects() {
    let fixture = fixture().await;
    let installation = install(&fixture).await;
    let key = Uuid::new_v4();
    let canonical = fixture
        .repo
        .publish(notification(
            &installation,
            key,
            [11; 32],
            fixture.allowed_room,
            "canonical ERP notification",
        ))
        .await
        .expect("publish canonical request");

    let conflicting_request = notification(
        &installation,
        key,
        [12; 32],
        fixture.allowed_room,
        "different request using the same key",
    );
    let fast_conflict = fixture
        .repo
        .replay(&conflicting_request)
        .await
        .expect_err("fast replay rejects a different request");
    assert!(matches!(fast_conflict, Error::Conflict(_)));
    let conflict = fixture
        .repo
        .publish(conflicting_request)
        .await
        .expect_err("publish rejects a different request");
    assert!(matches!(conflict, Error::Conflict(_)));
    assert_eq!(
        projection_counts(&fixture, installation.id, key).await,
        (1, 1, 1)
    );

    let receipt_message: Uuid = sqlx::query_scalar(
        r"SELECT message_id
            FROM integration_notification_receipts
           WHERE installation_id = $1 AND idempotency_key = $2",
    )
    .bind(installation.id)
    .bind(key)
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(receipt_message, canonical.message.id.to_uuid());
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn integration_client_and_bot_rotation_preserve_receipts_but_deactivation_denies_access() {
    let fixture = fixture().await;
    let installation = install(&fixture).await;
    let key = Uuid::new_v4();
    let canonical = fixture
        .repo
        .publish(notification(
            &installation,
            key,
            [21; 32],
            fixture.allowed_room,
            "notification before credential rotation",
        ))
        .await
        .expect("publish before rotation");

    let rotated_client_id = format!("erp-rotated-{}", Uuid::new_v4());
    let client_rotated = fixture
        .repo
        .update(
            fixture.workspace,
            installation.id,
            fixture.owner,
            UpdateIntegrationInstallation {
                client_id: Some(rotated_client_id.clone()),
                ..UpdateIntegrationInstallation::default()
            },
        )
        .await
        .expect("rotate integration client id");
    assert!(matches!(
        fixture
            .repo
            .authorize_client(installation.id, &fixture.issuer, &fixture.client_id)
            .await,
        Err(Error::Forbidden(_))
    ));
    assert_eq!(
        fixture
            .repo
            .authorize_client(installation.id, &fixture.issuer, &rotated_client_id)
            .await
            .expect("rotated client authenticates")
            .bot_id,
        fixture.bot
    );

    let bot_rotated = fixture
        .repo
        .update(
            fixture.workspace,
            installation.id,
            fixture.owner,
            UpdateIntegrationInstallation {
                bot_id: Some(fixture.replacement_bot),
                ..UpdateIntegrationInstallation::default()
            },
        )
        .await
        .expect("rotate integration bot");
    assert_eq!(bot_rotated.client_id, client_rotated.client_id);
    assert_eq!(
        fixture
            .repo
            .authorize_client(installation.id, &fixture.issuer, &rotated_client_id)
            .await
            .expect("rotated installation authenticates")
            .bot_id,
        fixture.replacement_bot
    );

    let replay_request = notification(
        &bot_rotated,
        key,
        [21; 32],
        fixture.allowed_room,
        "notification before credential rotation",
    );
    let replay = fixture
        .repo
        .replay(&replay_request)
        .await
        .expect("canonical receipt survives client and bot rotation")
        .expect("rotated credentials find the completed receipt");
    assert!(replay.deduplicated);
    assert_eq!(replay.message.id, canonical.message.id);
    assert_eq!(replay.message.sender_id, fixture.bot);
    assert_eq!(replay.outbox_id, canonical.outbox_id);
    assert_eq!(
        projection_counts(&fixture, installation.id, key).await,
        (1, 1, 1)
    );

    let disabled = fixture
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
        .expect("disable installation");
    assert!(!disabled.active);
    assert!(matches!(
        fixture
            .repo
            .authorize_client(installation.id, &fixture.issuer, &rotated_client_id)
            .await,
        Err(Error::Forbidden(_))
    ));
    let fast_denied = fixture
        .repo
        .replay(&notification(
            &disabled,
            key,
            [21; 32],
            fixture.allowed_room,
            "notification before credential rotation",
        ))
        .await
        .expect_err("disabled installations cannot fast-replay");
    assert!(matches!(fast_denied, Error::Forbidden(_)));
    let denied = fixture
        .repo
        .publish(notification(
            &disabled,
            key,
            [21; 32],
            fixture.allowed_room,
            "notification before credential rotation",
        ))
        .await
        .expect_err("disabled installations cannot publish or replay");
    assert!(matches!(denied, Error::Forbidden(_)));
    assert_eq!(
        projection_counts(&fixture, installation.id, key).await,
        (1, 1, 1)
    );
}
