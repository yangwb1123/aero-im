//! Cross-surface regression tests for the effective workspace/room boundary.
//!
//! These are deliberately separate from the historical crate-level DB fixture:
//! they verify that service methods do not silently fall back to retained
//! membership rows when an account is deleted, workspace-deactivated, or has
//! not satisfied mandatory 2FA.

#![allow(clippy::unwrap_used)]

use std::{fmt::Debug, sync::Arc};

use aero_common::{
    Block, CallKind, CallMode, Error, Message, Participant, ParticipantId, Result, Room, RoomKind,
    Workspace, WorkspaceRole,
};
use aero_storage::{
    hash_token, participant::NewHuman, AiJobRepo, CallRepo, DeactivationRepo, MessageRepo,
    ParticipantRepo, ReactionRepo, ReceiptRepo, RoomMemberRole, RoomRepo, TotpRepo, WebhookRepo,
    WorkspaceRepo,
};
use sqlx::PgPool;

use super::ImService;
use crate::test_util::MockBus;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero@localhost/aero_test".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_lazy(&url)
        .expect("valid DATABASE_URL")
}

fn service(pool: &PgPool) -> ImService {
    ImService::new(
        RoomRepo::new(pool.clone()),
        MessageRepo::new(pool.clone()),
        ParticipantRepo::new(pool.clone()),
        ReceiptRepo::new(pool.clone()),
        ReactionRepo::new(pool.clone()),
        CallRepo::new(pool.clone()),
        AiJobRepo::new(pool.clone()),
        Arc::new(MockBus::default()),
    )
    // Do not wire the legacy DeactivationRepo/TotpRepo diagnostic seams here:
    // effective authorization must be complete through WorkspaceRepo alone.
    .with_workspaces(WorkspaceRepo::new(pool.clone()))
}

async fn participant(repo: &ParticipantRepo, label: &str) -> Participant {
    let id = ParticipantId::new();
    repo.create_human(NewHuman {
        email: format!("{label}-{id}@effective-authz.test"),
        display_name: label.into(),
        password_hash: "test-only".into(),
    })
    .await
    .unwrap()
}

struct Fixture {
    pool: PgPool,
    service: ImService,
    workspace_repo: WorkspaceRepo,
    owner: Participant,
    actor: Participant,
    target: Participant,
    workspace: Workspace,
    room: Room,
    joinable_room: Room,
    message: Message,
    client_message_id: uuid::Uuid,
    request_hash: [u8; 32],
}

async fn fixture() -> Fixture {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let workspace_repo = WorkspaceRepo::new(pool.clone());
    let owner = participant(&participants, "effective-owner").await;
    let actor = participant(&participants, "effective-actor").await;
    let target = participant(&participants, "effective-target").await;
    let workspace = workspace_repo
        .create(
            "Effective authorization".into(),
            format!("effective-{}", ParticipantId::new()),
            owner.id,
        )
        .await
        .unwrap();
    for member in [actor.id, target.id] {
        workspace_repo
            .add_member(workspace.id, member, WorkspaceRole::Member)
            .await
            .unwrap();
    }

    let service = service(&pool);
    let room = service
        .create_room_in_workspace(
            actor.id,
            workspace.id,
            RoomKind::Channel,
            Some("actor room".into()),
        )
        .await
        .unwrap();
    service
        .add_member(actor.id, room.id, target.id)
        .await
        .unwrap();
    RoomRepo::new(pool.clone())
        .change_channel_member_role_authorized(room.id, actor.id, target.id, RoomMemberRole::Owner)
        .await
        .unwrap();
    let joinable_room = service
        .create_room_in_workspace(
            owner.id,
            workspace.id,
            RoomKind::Channel,
            Some("joinable room".into()),
        )
        .await
        .unwrap();

    let client_message_id = uuid::Uuid::new_v4();
    let request_hash = [41; 32];
    let message = service
        .send_message_idempotent(
            actor.id,
            room.id,
            vec![Block::text("effective access")],
            None,
            None,
            client_message_id,
            request_hash,
        )
        .await
        .unwrap()
        .message;
    service
        .toggle_reaction(actor.id, message.id, "✅")
        .await
        .unwrap();

    Fixture {
        pool,
        service,
        workspace_repo,
        owner,
        actor,
        target,
        workspace,
        room,
        joinable_room,
        message,
        client_message_id,
        request_hash,
    }
}

fn assert_forbidden<T: Debug>(result: &Result<T>, operation: &str) {
    assert!(
        matches!(result, Err(Error::Forbidden(_))),
        "{operation} must fail with Forbidden, got {result:?}"
    );
}

#[tokio::test]
#[ignore = "requires a freshly migrated Postgres database"]
async fn deactivation_blocks_every_caller_facing_service_surface() {
    let f = fixture().await;
    DeactivationRepo::new(f.pool.clone())
        .deactivate(f.workspace.id, f.actor.id, f.owner.id)
        .await
        .unwrap();

    assert_forbidden(
        &f.service.assert_room_access(f.actor.id, f.room.id).await,
        "room access",
    );
    assert_forbidden(
        &f.service
            .create_room_in_workspace(
                f.actor.id,
                f.workspace.id,
                RoomKind::Channel,
                Some("denied".into()),
            )
            .await,
        "create room",
    );
    assert_forbidden(
        &f.service
            .send_message_idempotent(
                f.actor.id,
                f.room.id,
                vec![Block::text("effective access")],
                None,
                None,
                f.client_message_id,
                f.request_hash,
            )
            .await,
        "idempotent replay",
    );
    assert_forbidden(
        &f.service
            .mark_read(f.actor.id, f.room.id, f.message.id)
            .await,
        "mark read",
    );
    assert_forbidden(
        &f.service.typing(f.actor.id, f.room.id, true).await,
        "typing",
    );
    assert_forbidden(
        &f.service
            .toggle_reaction(f.actor.id, f.message.id, "✅")
            .await,
        "reaction",
    );
    assert_forbidden(
        &f.service
            .start_call(
                f.actor.id,
                f.room.id,
                CallKind::Audio,
                CallMode::P2p,
                "test-sdp".into(),
            )
            .await,
        "start call",
    );
    assert_forbidden(
        &f.service.archive_channel(f.actor.id, f.room.id, true).await,
        "archive channel",
    );
    assert_forbidden(
        &f.service
            .set_channel_meta(
                f.actor.id,
                f.room.id,
                Some(Some("denied".into())),
                None,
                None,
            )
            .await,
        "channel metadata",
    );
    assert_forbidden(
        &f.service
            .set_room_post_policy(f.actor.id, f.room.id, "admins")
            .await,
        "post policy",
    );
    assert_forbidden(
        &f.service.history(f.actor.id, f.room.id, None, 20).await,
        "history",
    );
    assert_forbidden(
        &f.service
            .add_member(f.actor.id, f.room.id, f.target.id)
            .await,
        "add member",
    );
    assert_forbidden(
        &f.service.join_channel(f.actor.id, f.joinable_room.id).await,
        "join channel",
    );
    assert_forbidden(
        &f.service
            .list_workspace_channels(f.actor.id, f.workspace.id, None)
            .await,
        "list workspace channels",
    );
    assert!(
        f.service
            .list_my_rooms(f.actor.id)
            .await
            .unwrap()
            .is_empty(),
        "retained room membership must not appear in the caller's room list"
    );
    assert!(
        f.service
            .reactions_for_accessible(f.actor.id, &[f.message.id])
            .await
            .unwrap()
            .is_empty(),
        "batch reactions must apply the same effective access boundary"
    );

    // A leave only removes privilege. Keeping this path available lets a user
    // discard a retained room-membership edge after access was revoked.
    f.service
        .leave_channel(f.actor.id, f.room.id)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires a freshly migrated Postgres database"]
async fn mandatory_2fa_and_deleted_account_gate_lists_and_batch_reads() {
    let f = fixture().await;
    let totp = TotpRepo::new(f.pool.clone());
    for owner in [f.owner.id, f.target.id] {
        totp.upsert_secret(owner, "JBSWY3DPEHPK3PXP").await.unwrap();
        assert!(totp.activate(owner).await.unwrap());
    }
    f.workspace_repo
        .set_require_2fa(f.workspace.id, true)
        .await
        .unwrap();

    assert_forbidden(
        &f.service.assert_room_access(f.actor.id, f.room.id).await,
        "mandatory 2FA room access",
    );
    assert_forbidden(
        &f.service
            .list_workspace_channels(f.actor.id, f.workspace.id, None)
            .await,
        "mandatory 2FA workspace list",
    );
    assert!(f
        .service
        .list_my_rooms(f.actor.id)
        .await
        .unwrap()
        .is_empty());
    assert!(f
        .service
        .reactions_for_accessible(f.actor.id, &[f.message.id])
        .await
        .unwrap()
        .is_empty());
    assert!(
        RoomRepo::new(f.pool.clone())
            .rooms_for_in_workspace(f.actor.id, f.workspace.id)
            .await
            .unwrap()
            .is_empty(),
        "the scoped storage path used by GET /rooms must not bypass 2FA"
    );

    totp.upsert_secret(f.actor.id, "JBSWY3DPEHPK3PXP")
        .await
        .unwrap();
    assert!(totp.activate(f.actor.id).await.unwrap());
    f.service
        .assert_room_access(f.actor.id, f.room.id)
        .await
        .unwrap();
    assert_eq!(f.service.list_my_rooms(f.actor.id).await.unwrap().len(), 1);
    assert!(f
        .service
        .reactions_for_accessible(f.actor.id, &[f.message.id])
        .await
        .unwrap()
        .contains_key(&f.message.id));

    // Preserve memberships while deleting the account so the regression proves
    // the active-account predicate, not an incidental cascade/removal.
    sqlx::query("UPDATE participants SET deleted_at = now() WHERE id = $1")
        .bind(f.actor.id.to_uuid())
        .execute(&f.pool)
        .await
        .unwrap();
    assert_forbidden(
        &f.service.assert_room_access(f.actor.id, f.room.id).await,
        "deleted account room access",
    );
    assert!(f
        .service
        .list_my_rooms(f.actor.id)
        .await
        .unwrap()
        .is_empty());
    assert!(f
        .service
        .reactions_for_accessible(f.actor.id, &[f.message.id])
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
#[ignore = "requires a freshly migrated Postgres database"]
async fn mandatory_2fa_blocks_humans_but_allows_active_incoming_webhook_bots() {
    let f = fixture().await;
    // Migration 0194 requires every channel to retain an effective owner when
    // mandatory 2FA turns on. Enroll the fixture's fallback owners, while
    // deliberately leaving `actor` unenrolled for the human-denial assertion.
    let totp = TotpRepo::new(f.pool.clone());
    for owner in [f.owner.id, f.target.id] {
        totp.upsert_secret(owner, "JBSWY3DPEHPK3PXP").await.unwrap();
        assert!(totp.activate(owner).await.unwrap());
    }
    f.workspace_repo
        .set_require_2fa(f.workspace.id, true)
        .await
        .unwrap();

    assert_forbidden(
        &f.service.assert_room_access(f.actor.id, f.room.id).await,
        "unenrolled human in mandatory-2FA workspace",
    );

    // The human creator must still satisfy mandatory 2FA before registering a
    // hook. Only the resulting service identity is exempt from TOTP enrollment.
    let created = WebhookRepo::new(f.pool.clone())
        .create_incoming(
            f.room.id,
            "mandatory-2fa webhook bot",
            &hash_token(&format!("mandatory-2fa-hook-{}", f.room.id)),
            None,
            f.target.id,
        )
        .await
        .unwrap();
    let has_totp = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
             SELECT 1 FROM totp_secrets WHERE participant_id = $1
         )",
    )
    .bind(created.bot_id.to_uuid())
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert!(
        !has_totp,
        "service identities must not receive dummy TOTP rows"
    );

    assert_eq!(
        f.workspace_repo
            .effective_member_role(f.workspace.id, created.bot_id)
            .await
            .unwrap(),
        Some(WorkspaceRole::Member)
    );
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT aero_effective_workspace_access($1, $2)")
            .bind(f.workspace.id.to_uuid())
            .bind(created.bot_id.to_uuid())
            .fetch_one(&f.pool)
            .await
            .unwrap(),
        "the transaction helper must apply mandatory 2FA only to humans"
    );
    f.service
        .assert_room_access(created.bot_id, f.room.id)
        .await
        .unwrap();

    let sent = f
        .service
        .send_message(
            created.bot_id,
            f.room.id,
            vec![Block::text("service identity can post")],
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(sent.sender_id, created.bot_id);

    let rooms = RoomRepo::new(f.pool.clone())
        .rooms_for_in_workspace(created.bot_id, f.workspace.id)
        .await
        .unwrap();
    assert!(
        rooms.iter().any(|room| room.id == f.room.id),
        "effective room lists must agree with assert_room_access"
    );
    let delivery_members = RoomRepo::new(f.pool.clone())
        .delivery_members(f.room.id)
        .await
        .unwrap();
    assert!(
        delivery_members.contains(&created.bot_id),
        "effective delivery fan-out must retain active service identities"
    );

    // The TOTP exemption is not an authorization bypass: deactivation still
    // removes the bot from every effective-access surface.
    sqlx::query(
        "INSERT INTO workspace_deactivations
             (workspace_id, participant_id, deactivated_by)
         VALUES ($1, $2, $3)",
    )
    .bind(f.workspace.id.to_uuid())
    .bind(created.bot_id.to_uuid())
    .bind(f.owner.id.to_uuid())
    .execute(&f.pool)
    .await
    .unwrap();
    assert_forbidden(
        &f.service
            .assert_room_access(created.bot_id, f.room.id)
            .await,
        "deactivated incoming webhook bot",
    );
    assert!(
        !sqlx::query_scalar::<_, bool>("SELECT aero_effective_workspace_access($1, $2)")
            .bind(f.workspace.id.to_uuid())
            .bind(created.bot_id.to_uuid())
            .fetch_one(&f.pool)
            .await
            .unwrap()
    );
}
