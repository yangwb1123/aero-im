use super::*;

use aero_common::{Blob, FileKind, RoomKind, WorkspaceRole};
use sha2::Sha256;
use sqlx::PgPool;

use crate::{BlobRepo, BotRepo, NewBlob, RoomRepo, WorkspaceRepo};

pub(super) struct Fixture {
    pub(super) pool: PgPool,
    pub(super) repo: IntegrationRepo,
    pub(super) owner: ParticipantId,
    pub(super) member: ParticipantId,
    pub(super) workspace: WorkspaceId,
    pub(super) bot: ParticipantId,
    pub(super) replacement_bot: ParticipantId,
    pub(super) allowed_room: RoomId,
    pub(super) unlisted_room: RoomId,
    pub(super) issuer: String,
    pub(super) user_identity_issuer: String,
    pub(super) client_id: String,
}

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

pub(super) async fn participant(pool: &PgPool, label: &str) -> ParticipantId {
    let id = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(id.to_uuid())
        .bind(format!("{label}-{id}"))
        .execute(pool)
        .await
        .expect("insert integration fixture participant");
    id
}

pub(super) async fn fixture() -> Fixture {
    let pool = pool();
    let owner = participant(&pool, "integration-owner").await;
    let member = participant(&pool, "integration-member").await;
    let marker = Uuid::new_v4();
    let workspace_repo = WorkspaceRepo::new(pool.clone());
    let workspace = workspace_repo
        .create(
            format!("Integration {marker}"),
            format!("integration-{marker}"),
            owner,
        )
        .await
        .expect("create integration fixture workspace")
        .id;
    workspace_repo
        .add_member(workspace, member, WorkspaceRole::Member)
        .await
        .expect("enroll ordinary member");

    let bots = BotRepo::new(pool.clone());
    let (bot, _) = bots
        .create_authorized_with_token(owner, "integration-bot", None, Some(workspace))
        .await
        .expect("create integration bot");
    let (replacement_bot, _) = bots
        .create_authorized_with_token(owner, "replacement-integration-bot", None, Some(workspace))
        .await
        .expect("create replacement integration bot");

    let rooms = RoomRepo::new(pool.clone());
    let allowed_room = rooms
        .create_in_workspace(
            workspace,
            RoomKind::Channel,
            Some(format!("integration-allowed-{marker}")),
            owner,
        )
        .await
        .expect("create allowed integration room")
        .id;
    let unlisted_room = rooms
        .create_in_workspace(
            workspace,
            RoomKind::Channel,
            Some(format!("integration-unlisted-{marker}")),
            owner,
        )
        .await
        .expect("create unlisted integration room")
        .id;
    for room in [allowed_room, unlisted_room] {
        rooms
            .add_member(room, bot)
            .await
            .expect("enroll integration bot in room");
        rooms
            .add_member(room, replacement_bot)
            .await
            .expect("enroll replacement bot in room");
    }

    Fixture {
        repo: IntegrationRepo::new(pool.clone()),
        pool,
        owner,
        member,
        workspace,
        bot,
        replacement_bot,
        allowed_room,
        unlisted_room,
        issuer: format!("https://snaplink.test/{marker}"),
        user_identity_issuer: format!("https://human-snaplink.test/{marker}"),
        client_id: format!("erp-{marker}"),
    }
}

pub(super) fn installation_input(
    fixture: &Fixture,
    actor: ParticipantId,
    bot: ParticipantId,
    client_id: &str,
    room_ids: Vec<RoomId>,
) -> NewIntegrationInstallation {
    NewIntegrationInstallation {
        workspace_id: fixture.workspace,
        bot_id: bot,
        issuer: fixture.issuer.clone(),
        user_identity_issuer: fixture.user_identity_issuer.clone(),
        client_id: client_id.to_owned(),
        name: "ERP notifications".into(),
        allow_user_dm: false,
        room_ids,
        created_by: actor,
    }
}

pub(super) async fn install(fixture: &Fixture) -> IntegrationInstallation {
    fixture
        .repo
        .create(installation_input(
            fixture,
            fixture.owner,
            fixture.bot,
            &fixture.client_id,
            vec![fixture.allowed_room],
        ))
        .await
        .expect("create integration installation")
}

pub(super) fn notification(
    installation: &IntegrationInstallation,
    idempotency_key: Uuid,
    request_hash: [u8; 32],
    room: RoomId,
    text: &str,
) -> NewIntegrationNotification {
    NewIntegrationNotification {
        installation_id: installation.id,
        issuer: installation.issuer.clone(),
        client_id: installation.client_id.clone(),
        idempotency_key,
        request_hash,
        target: IntegrationTarget::Room(room),
        room_id: room,
        recipient: None,
        blocks: vec![Block::text(text)],
        traceparent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into()),
        lease_token: None,
    }
}

pub(super) fn resolved_notification(
    resolved: &ResolvedIntegrationTarget,
    idempotency_key: Uuid,
    request_hash: [u8; 32],
    blocks: Vec<Block>,
) -> NewIntegrationNotification {
    NewIntegrationNotification {
        installation_id: resolved.installation.id,
        issuer: resolved.installation.issuer.clone(),
        client_id: resolved.installation.client_id.clone(),
        idempotency_key,
        request_hash,
        target: resolved.target.clone(),
        room_id: resolved.room_id,
        recipient: resolved.recipient,
        blocks,
        traceparent: None,
        lease_token: None,
    }
}

pub(super) async fn scoped_blob(
    repo: &BlobRepo,
    owner: ParticipantId,
    workspace: WorkspaceId,
    region: &str,
    label: &str,
    finalize: bool,
) -> Blob {
    let reserved = repo
        .reserve_in_scope(
            NewBlob {
                owner_id: owner,
                kind: FileKind::Document,
                name: format!("{label}.pdf"),
                mime: "application/pdf".into(),
                size: 17,
                sha256: Some(hex::encode(<Sha256 as sha2::Digest>::digest(
                    format!("{label}-{owner}-{workspace}").as_bytes(),
                ))),
                storage_key: format!("pending:{label}"),
            },
            Some(workspace),
            Some(region),
        )
        .await
        .expect("reserve integration attachment");
    if finalize {
        repo.finalize(reserved.id, &format!("{region}:{label}"))
            .await
            .expect("finalize integration attachment")
            .expect("reservation remains available")
    } else {
        reserved
    }
}

pub(super) fn file_block(blob: &Blob) -> Block {
    Block::File {
        blob_id: blob.id,
        kind: blob.kind.clone(),
        name: blob.name.clone(),
        size: blob.size,
    }
}

pub(super) async fn projection_counts(
    fixture: &Fixture,
    installation: Uuid,
    idempotency_key: Uuid,
) -> (i64, i64, i64) {
    sqlx::query_as(
        r"SELECT
             (SELECT count(*)
                FROM messages message
               WHERE message.metadata ->> 'installation_id' = $1),
             (SELECT count(*)
                FROM integration_notification_receipts receipt
               WHERE receipt.installation_id = $2
                 AND receipt.idempotency_key = $3),
             (SELECT count(*)
                FROM event_outbox outbox
                JOIN messages message ON message.id = outbox.message_id
               WHERE message.metadata ->> 'installation_id' = $1
                 AND outbox.event_kind = 'message')",
    )
    .bind(installation.to_string())
    .bind(installation)
    .bind(idempotency_key)
    .fetch_one(&fixture.pool)
    .await
    .expect("count integration message projections")
}
