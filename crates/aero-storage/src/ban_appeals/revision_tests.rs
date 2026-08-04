use super::*;
use crate::{NewStream, RoomRepo, StreamModRepo, StreamModeratorRepo, StreamRepo, WorkspaceRepo};
use aero_common::{RoomKind, StreamProtocol, WorkspaceRole};
use sqlx::postgres::PgConnectOptions;
use time::Duration;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(6)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

async fn tagged_pool(application_name: &str) -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    let options = url
        .parse::<PgConnectOptions>()
        .expect("valid DATABASE_URL")
        .application_name(application_name);
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("connect tagged appeal test pool")
}

async fn wait_until_tagged_query_waits_on_lock(pool: &PgPool, application_name: &str) {
    for _ in 0..100 {
        let waiting = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
                 SELECT 1
                   FROM pg_stat_activity
                  WHERE datname = current_database()
                    AND application_name = $1
                    AND wait_event_type = 'Lock'
             )",
        )
        .bind(application_name)
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("tagged appeal operation never reached its expected lock wait");
}

async fn participant(pool: &PgPool, label: &str) -> ParticipantId {
    let id = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(id.to_uuid())
        .bind(format!("appeal-revision-{label}-{id}"))
        .execute(pool)
        .await
        .expect("insert participant");
    id
}

async fn stream(pool: &PgPool, owner: ParticipantId, label: &str) -> Ulid {
    let id = Ulid::new();
    sqlx::query(
        r"INSERT INTO streams
                (id, owner_id, title, stream_key, status, protocol)
          VALUES ($1, $2, $3, $4, 'idle', 'rtmp')",
    )
    .bind(Uuid::from_u128(id.0))
    .bind(owner.to_uuid())
    .bind(format!("appeal-revision-{label}-{id}"))
    .bind(format!("appeal-revision-{label}-key-{id}"))
    .execute(pool)
    .await
    .expect("insert stream");
    id
}

async fn room_linked_stream(
    pool: &PgPool,
    owner: ParticipantId,
    viewer: ParticipantId,
) -> (Ulid, Uuid, Uuid) {
    let suffix = Ulid::new();
    let workspace = WorkspaceRepo::new(pool.clone())
        .create(
            format!("appeal-room-{suffix}"),
            format!("appeal-room-{suffix}"),
            owner,
        )
        .await
        .expect("create appeal workspace")
        .id;
    WorkspaceRepo::new(pool.clone())
        .add_member(workspace, viewer, WorkspaceRole::Member)
        .await
        .expect("add appellant to workspace");
    let room = RoomRepo::new(pool.clone())
        .create_in_workspace(
            workspace,
            RoomKind::Group,
            Some(format!("appeal-room-{suffix}")),
            owner,
        )
        .await
        .expect("create appeal room")
        .id;
    RoomRepo::new(pool.clone())
        .add_member(room, viewer)
        .await
        .expect("add appellant to room");
    let stream = StreamRepo::new(pool.clone())
        .create(NewStream {
            owner_id: owner,
            room_id: Some(room),
            title: format!("appeal-room-{suffix}"),
            protocol: StreamProtocol::Rtmp,
            stream_key: Some(format!("appeal-room-key-{suffix}")),
        })
        .await
        .expect("create room-linked stream")
        .id;
    (stream, workspace.to_uuid(), room.to_uuid())
}

#[tokio::test]
async fn ban_appeal_page_and_reason_envelopes_are_bounded() {
    assert_eq!(bounded_appeal_page(0, -1), (1, 0));
    assert_eq!(
        bounded_appeal_page(i64::MAX, i64::MAX),
        (MAX_APPEAL_PAGE_SIZE, MAX_APPEAL_PAGE_OFFSET)
    );

    let repo = BanAppealRepo::new(pool());
    let empty = repo
        .submit_appeal(Ulid::new(), ParticipantId::new(), "  ")
        .await;
    assert!(matches!(empty, Err(AppealError::Invalid(_))));
    let too_long = repo
        .submit_appeal(
            Ulid::new(),
            ParticipantId::new(),
            &"界".repeat(MAX_APPEAL_REASON_CHARS + 1),
        )
        .await;
    assert!(matches!(too_long, Err(AppealError::Invalid(_))));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn ban_appeal_duplicate_is_idempotent_and_old_revision_cannot_lift_reban() {
    let pool = pool();
    let moderation = StreamModRepo::new(pool.clone());
    let appeals = BanAppealRepo::new(pool.clone());
    let owner = participant(&pool, "aba-owner").await;
    let viewer = participant(&pool, "aba-viewer").await;
    let stream = stream(&pool, owner, "aba").await;
    moderation
        .ban_authorized(stream, viewer, owner, Some("ban-a"), None)
        .await
        .unwrap();
    let first = appeals
        .submit_appeal(stream, viewer, "please")
        .await
        .unwrap();
    let duplicate = appeals
        .submit_appeal(stream, viewer, "duplicate request")
        .await
        .unwrap();
    assert_eq!(first, duplicate, "one pending appeal per ban revision");

    let old_revision = appeals.get(first).await.unwrap().unwrap().ban_revision;
    assert!(old_revision.is_some());
    assert!(moderation
        .unban_authorized(stream, viewer, owner)
        .await
        .unwrap());
    moderation
        .ban_authorized(stream, viewer, owner, Some("ban-b"), None)
        .await
        .unwrap();
    let new_revision: i64 = sqlx::query_scalar(
        "SELECT ban_revision FROM stream_bans WHERE stream_id = $1 AND participant_id = $2",
    )
    .bind(Uuid::from_u128(stream.0))
    .bind(viewer.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_ne!(old_revision, Some(new_revision));

    assert!(appeals
        .review_authorized(first, owner, true, Some("old appeal accepted"))
        .await
        .unwrap());
    let surviving: (i64, Option<String>) = sqlx::query_as(
        "SELECT ban_revision, reason FROM stream_bans WHERE stream_id = $1 AND participant_id = $2",
    )
    .bind(Uuid::from_u128(stream.0))
    .bind(viewer.to_uuid())
    .fetch_one(&pool)
    .await
    .expect("replacement ban survives approval of old appeal");
    assert_eq!(surviving, (new_revision, Some("ban-b".into())));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn ban_appeal_submit_linearizes_after_unban() {
    let pool = pool();
    let moderation = StreamModRepo::new(pool.clone());
    let owner = participant(&pool, "unban-owner").await;
    let viewer = participant(&pool, "unban-viewer").await;
    let stream = stream(&pool, owner, "unban-race").await;
    moderation
        .ban_authorized(stream, viewer, owner, None, None)
        .await
        .unwrap();

    let mut unban = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM streams WHERE id = $1 FOR UPDATE")
        .bind(Uuid::from_u128(stream.0))
        .execute(&mut *unban)
        .await
        .unwrap();
    set_live_governance_actor(&mut unban, owner).await.unwrap();
    sqlx::query("DELETE FROM stream_bans WHERE stream_id = $1 AND participant_id = $2")
        .bind(Uuid::from_u128(stream.0))
        .bind(viewer.to_uuid())
        .execute(&mut *unban)
        .await
        .unwrap();

    let application_name = format!("appeal-submit-unban-{viewer}");
    let raced = BanAppealRepo::new(tagged_pool(&application_name).await);
    let submit = tokio::spawn(async move { raced.submit_appeal(stream, viewer, "raced").await });
    wait_until_tagged_query_waits_on_lock(&pool, &application_name).await;
    unban.commit().await.unwrap();

    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(3), submit)
            .await
            .expect("submit race completed")
            .expect("submit task"),
        Err(AppealError::NotBanned)
    ));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn ban_appeal_submit_linearizes_after_expiry_sweep() {
    let pool = pool();
    let moderation = StreamModRepo::new(pool.clone());
    let owner = participant(&pool, "sweep-owner").await;
    let viewer = participant(&pool, "sweep-viewer").await;
    let stream = stream(&pool, owner, "sweep-race").await;
    let before_expiry = OffsetDateTime::now_utc();
    moderation
        .ban_authorized(
            stream,
            viewer,
            owner,
            None,
            Some(before_expiry + Duration::milliseconds(30)),
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut sweep = pool.begin().await.unwrap();
    sqlx::query(
        "DELETE FROM stream_bans WHERE stream_id = $1 AND participant_id = $2 AND until < NOW()",
    )
    .bind(Uuid::from_u128(stream.0))
    .bind(viewer.to_uuid())
    .execute(&mut *sweep)
    .await
    .unwrap();

    // The database-clock predicate can reject the already-expired row without
    // waiting for the sweep's uncommitted delete; either serialization point
    // produces the same NotBanned result.
    let result = BanAppealRepo::new(pool.clone())
        .submit_appeal(stream, viewer, "raced sweep")
        .await;
    assert!(matches!(result, Err(AppealError::NotBanned)));
    sweep.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn ban_appeal_submit_uses_database_clock_for_expiry() {
    let pool = pool();
    let owner = participant(&pool, "clock-owner").await;
    let viewer = participant(&pool, "clock-viewer").await;
    let stream = stream(&pool, owner, "clock-expiry").await;
    StreamModRepo::new(pool.clone())
        .ban_authorized(
            stream,
            viewer,
            owner,
            None,
            Some(OffsetDateTime::now_utc() - Duration::seconds(1)),
        )
        .await
        .unwrap();

    assert!(matches!(
        BanAppealRepo::new(pool)
            .submit_appeal(stream, viewer, "too late")
            .await,
        Err(AppealError::NotBanned)
    ));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn ban_appeal_submit_follows_room_scope_revocation_lock_order() {
    let pool = pool();
    let owner = participant(&pool, "room-owner").await;
    let viewer = participant(&pool, "room-viewer").await;
    let (stream, workspace_id, room_id) = room_linked_stream(&pool, owner, viewer).await;
    StreamModRepo::new(pool.clone())
        .ban_authorized(stream, viewer, owner, None, None)
        .await
        .unwrap();

    // Hold the canonical workspace-first revocation transaction. Submission
    // must wait here, then observe the completed membership removal before it
    // can acquire the stream row.
    let mut revocation = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace_id)
        .execute(&mut *revocation)
        .await
        .unwrap();
    sqlx::query("DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
        .bind(room_id)
        .bind(viewer.to_uuid())
        .execute(&mut *revocation)
        .await
        .unwrap();
    sqlx::query("DELETE FROM workspace_members WHERE workspace_id = $1 AND participant_id = $2")
        .bind(workspace_id)
        .bind(viewer.to_uuid())
        .execute(&mut *revocation)
        .await
        .unwrap();

    let application_name = format!("appeal-submit-room-revoke-{viewer}");
    let raced = BanAppealRepo::new(tagged_pool(&application_name).await);
    let submit =
        tokio::spawn(async move { raced.submit_appeal(stream, viewer, "raced access").await });
    wait_until_tagged_query_waits_on_lock(&pool, &application_name).await;
    revocation.commit().await.unwrap();

    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(3), submit)
            .await
            .expect("room-scope submit completed")
            .expect("submit task"),
        Err(AppealError::Access(AeroError::Forbidden(_)))
    ));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM ban_appeals WHERE stream_id = $1")
        .bind(Uuid::from_u128(stream.0))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "revoked appellant cannot persist an appeal");

    // The trigger repeats the same scope check for a raw writer that supplies
    // the required actor context.
    let mut raw = pool.begin().await.unwrap();
    set_live_governance_actor(&mut raw, viewer).await.unwrap();
    let raw_insert = sqlx::query(
        "INSERT INTO ban_appeals (id, stream_id, appellant_id, appeal_reason)
         VALUES ($1, $2, $3, 'raw revoked appeal')",
    )
    .bind(BanAppealId::new().to_uuid())
    .bind(Uuid::from_u128(stream.0))
    .bind(viewer.to_uuid())
    .execute(&mut *raw)
    .await;
    assert!(raw_insert.is_err());
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn ban_appeal_list_rechecks_authority_in_its_read_transaction() {
    let pool = pool();
    let moderation = StreamModRepo::new(pool.clone());
    let moderators = StreamModeratorRepo::new(pool.clone());
    let owner = participant(&pool, "list-owner").await;
    let moderator = participant(&pool, "list-moderator").await;
    let viewer = participant(&pool, "list-viewer").await;
    let stream = stream(&pool, owner, "list-race").await;
    moderators
        .add_authorized(stream, moderator, owner)
        .await
        .unwrap();
    moderation
        .ban_authorized(stream, viewer, owner, None, None)
        .await
        .unwrap();
    BanAppealRepo::new(pool.clone())
        .submit_appeal(stream, viewer, "private reason")
        .await
        .unwrap();

    let mut revocation = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM streams WHERE id = $1 FOR UPDATE")
        .bind(Uuid::from_u128(stream.0))
        .execute(&mut *revocation)
        .await
        .unwrap();
    set_live_governance_actor(&mut revocation, owner)
        .await
        .unwrap();
    sqlx::query("DELETE FROM stream_moderators WHERE stream_id = $1 AND participant_id = $2")
        .bind(Uuid::from_u128(stream.0))
        .bind(moderator.to_uuid())
        .execute(&mut *revocation)
        .await
        .unwrap();

    let application_name = format!("appeal-list-revoke-{moderator}");
    let raced = BanAppealRepo::new(tagged_pool(&application_name).await);
    let list = tokio::spawn(async move {
        raced
            .list_pending_authorized(stream, moderator, i64::MAX, i64::MAX)
            .await
    });
    wait_until_tagged_query_waits_on_lock(&pool, &application_name).await;
    revocation.commit().await.unwrap();

    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(3), list)
            .await
            .expect("list race completed")
            .expect("list task"),
        Err(AeroError::Forbidden(_))
    ));
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn ban_appeal_raw_sql_cannot_forge_scope_revision_or_review() {
    let pool = pool();
    let moderation = StreamModRepo::new(pool.clone());
    let owner = participant(&pool, "raw-owner").await;
    let viewer = participant(&pool, "raw-viewer").await;
    let outsider = participant(&pool, "raw-outsider").await;
    let stream = stream(&pool, owner, "raw").await;

    let missing_context = sqlx::query(
        r"INSERT INTO ban_appeals (id, stream_id, appellant_id, appeal_reason)
           VALUES ($1, $2, $3, 'forged')",
    )
    .bind(BanAppealId::new().to_uuid())
    .bind(Uuid::from_u128(stream.0))
    .bind(owner.to_uuid())
    .execute(&pool)
    .await;
    assert!(
        missing_context.is_err(),
        "copying a participant UUID cannot replace appeal actor context"
    );

    let mut no_ban = pool.begin().await.unwrap();
    set_live_governance_actor(&mut no_ban, outsider)
        .await
        .unwrap();
    let without_ban = sqlx::query(
        r"INSERT INTO ban_appeals (id, stream_id, appellant_id, appeal_reason)
           VALUES ($1, $2, $3, 'forged')",
    )
    .bind(BanAppealId::new().to_uuid())
    .bind(Uuid::from_u128(stream.0))
    .bind(outsider.to_uuid())
    .execute(&mut *no_ban)
    .await;
    assert!(
        without_ban.is_err(),
        "an authenticated pending appeal still requires an active ban"
    );
    no_ban.rollback().await.unwrap();

    moderation
        .ban_authorized(stream, viewer, owner, None, None)
        .await
        .unwrap();
    let missing_context_with_ban = sqlx::query(
        r"INSERT INTO ban_appeals (id, stream_id, appellant_id, appeal_reason)
           VALUES ($1, $2, $3, 'forged')",
    )
    .bind(BanAppealId::new().to_uuid())
    .bind(Uuid::from_u128(stream.0))
    .bind(viewer.to_uuid())
    .execute(&pool)
    .await;
    assert!(
        missing_context_with_ban.is_err(),
        "an active ban does not waive appeal actor context"
    );

    let appeal = BanAppealId::new();
    let mut legacy = pool.begin().await.unwrap();
    set_live_governance_actor(&mut legacy, viewer)
        .await
        .unwrap();
    let canonical_created_at: OffsetDateTime = sqlx::query_scalar(
        r"INSERT INTO ban_appeals
                (id, stream_id, appellant_id, appeal_reason, created_at)
           VALUES ($1, $2, $3, ' legacy writer ', '2000-01-01T00:00:00Z')
           RETURNING created_at",
    )
    .bind(appeal.to_uuid())
    .bind(Uuid::from_u128(stream.0))
    .bind(viewer.to_uuid())
    .fetch_one(&mut *legacy)
    .await
    .expect("old valid writer is revision-bound by the trigger");
    legacy.commit().await.unwrap();
    assert!(
        canonical_created_at > OffsetDateTime::now_utc() - Duration::seconds(10),
        "the DB replaces caller-controlled appeal ordering timestamps"
    );
    let bound: (String, Option<i64>, i64) = sqlx::query_as(
        r"SELECT appeal.appeal_reason, appeal.ban_revision, ban.ban_revision
            FROM ban_appeals AS appeal
            JOIN stream_bans AS ban
              ON ban.stream_id = appeal.stream_id
             AND ban.participant_id = appeal.appellant_id
           WHERE appeal.id = $1",
    )
    .bind(appeal.to_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(bound.0, "legacy writer");
    assert_eq!(bound.1, Some(bound.2));

    let raw_delete = sqlx::query("DELETE FROM ban_appeals WHERE id = $1")
        .bind(appeal.to_uuid())
        .execute(&pool)
        .await;
    assert!(
        raw_delete.is_err(),
        "an active appellant's appeal is immutable audit history"
    );

    let tamper = sqlx::query("UPDATE ban_appeals SET appeal_reason = 'changed' WHERE id = $1")
        .bind(appeal.to_uuid())
        .execute(&pool)
        .await;
    assert!(tamper.is_err(), "appeal submission scope is immutable");

    let mut forged = pool.begin().await.unwrap();
    set_live_governance_actor(&mut forged, outsider)
        .await
        .unwrap();
    let forged_review = sqlx::query(
        r"UPDATE ban_appeals
              SET status = 'approved',
                  reviewed_by = $2,
                  reviewed_at = now()
            WHERE id = $1",
    )
    .bind(appeal.to_uuid())
    .bind(outsider.to_uuid())
    .execute(&mut *forged)
    .await;
    assert!(
        forged_review.is_err(),
        "an outsider cannot raw-SQL review a stream appeal"
    );
    forged.rollback().await.unwrap();
}
