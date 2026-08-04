use super::ParticipantRepo;
use crate::message::{MessageRepo, NewMessage};
use crate::{BanAppealRepo, MessageReportRepo, RoomRepo, StreamModRepo, WorkspaceRepo};
use aero_common::{Block, ParticipantId, RoomKind, WorkspaceRole};
use sqlx::PgPool;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

async fn participant(pool: &PgPool) -> ParticipantId {
    let id = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(id.to_uuid())
        .bind(format!("erasure-actor-{id}"))
        .execute(pool)
        .await
        .expect("insert participant");
    id
}

/// GDPR erasure must delete moderation reports / ban appeals AUTHORED BY the
/// erased user (their free-text `reason` / `appeal_reason` is PII), while
/// keeping rows authored by OTHERS — including reports ABOUT the erased user —
/// as other users' governance records. None of the three tables cascades on a
/// tombstone erasure (`ban_appeals` / `message_reports` have no FK;
/// `user_reports`'
/// `reporter_id` cascade never fires because the participant is tombstoned, not
/// hard-deleted), so the explicit DELETEs in `delete_participant` are load-bearing.
#[tokio::test]
#[ignore = "requires running Postgres with migrations applied"]
async fn erasure_clears_authored_moderation_reports() {
    let pool = pool();
    let participants = ParticipantRepo::new(pool.clone());
    let erased = participant(&pool).await;
    let other = participant(&pool).await;
    let stream_owner = participant(&pool).await;

    // Use governed paths so each appeal is bound to a real, locked ban
    // incarnation. The erased user's appeal is deleted; the other's survives.
    let appeal_stream = ulid::Ulid::new();
    sqlx::query(
        r"INSERT INTO streams
                (id, owner_id, title, stream_key, status, protocol)
          VALUES ($1, $2, $3, $4, 'idle', 'rtmp')",
    )
    .bind(uuid::Uuid::from_u128(appeal_stream.0))
    .bind(stream_owner.to_uuid())
    .bind(format!("erasure-appeal-{appeal_stream}"))
    .bind(format!("erasure-appeal-key-{appeal_stream}"))
    .execute(&pool)
    .await
    .expect("appeal stream");
    let moderation = StreamModRepo::new(pool.clone());
    let appeals = BanAppealRepo::new(pool.clone());
    for (appellant, reason) in [(erased, "my appeal"), (other, "their appeal")] {
        moderation
            .ban_authorized(appeal_stream, appellant, stream_owner, None, None)
            .await
            .expect("stream ban");
        appeals
            .submit_appeal(appeal_stream, appellant, reason)
            .await
            .expect("ban appeal");
    }
    let other_appellant = other.to_uuid();

    // Use a live message and effective memberships so the fixture remains
    // valid under the raw-SQL message-report containment trigger.
    let report_workspace = WorkspaceRepo::new(pool.clone())
        .create(
            format!("erasure-report-{stream_owner}"),
            format!("erasure-report-{stream_owner}"),
            stream_owner,
        )
        .await
        .expect("report workspace")
        .id;
    let workspaces = WorkspaceRepo::new(pool.clone());
    let rooms = RoomRepo::new(pool.clone());
    for reporter in [erased, other] {
        workspaces
            .add_member(report_workspace, reporter, WorkspaceRole::Member)
            .await
            .expect("reporter workspace member");
    }
    let report_room = rooms
        .create_in_workspace(
            report_workspace,
            RoomKind::Group,
            Some(format!("erasure-report-{stream_owner}")),
            stream_owner,
        )
        .await
        .expect("report room")
        .id;
    for reporter in [erased, other] {
        rooms
            .add_member(report_room, reporter)
            .await
            .expect("reporter room member");
    }
    let reported_message = MessageRepo::new(pool.clone())
        .insert(NewMessage {
            room_id: report_room,
            sender_id: stream_owner,
            blocks: vec![Block::text("shared moderation fixture")],
            reply_to: None,
            metadata: serde_json::json!({}),
            expires_at: None,
        })
        .await
        .expect("reported message");
    let reports = MessageReportRepo::new(pool.clone());
    reports
        .report_authorized(report_room, reported_message.id, erased, "my report")
        .await
        .expect("message report by erased participant");
    reports
        .report_authorized(report_room, reported_message.id, other, "their report")
        .await
        .expect("message report by other participant");

    // A report BY the erased user is deleted; one ABOUT them is retained.
    sqlx::query(
        "INSERT INTO user_reports (reporter_id, reported_id, reason) VALUES ($1,$2,'rude')",
    )
    .bind(erased.to_uuid())
    .bind(other.to_uuid())
    .execute(&pool)
    .await
    .expect("user_report by erased participant");
    sqlx::query(
        "INSERT INTO user_reports (reporter_id, reported_id, reason) VALUES ($1,$2,'rude')",
    )
    .bind(other.to_uuid())
    .bind(erased.to_uuid())
    .execute(&pool)
    .await
    .expect("user_report about erased participant");

    for participant_id in [erased, other] {
        sqlx::query("INSERT INTO export_jobs (participant_id) VALUES ($1)")
            .bind(participant_id.to_uuid())
            .execute(&pool)
            .await
            .expect("export_job");
    }

    // A block BY the erased user is deleted; one ABOUT them is retained.
    sqlx::query("INSERT INTO user_blocks (blocker_id, blocked_id) VALUES ($1,$2)")
        .bind(erased.to_uuid())
        .bind(other.to_uuid())
        .execute(&pool)
        .await
        .expect("block by erased participant");
    sqlx::query("INSERT INTO user_blocks (blocker_id, blocked_id) VALUES ($1,$2)")
        .bind(other.to_uuid())
        .bind(erased.to_uuid())
        .execute(&pool)
        .await
        .expect("block about erased participant");

    assert!(participants
        .delete_participant(erased)
        .await
        .expect("erase"));

    for (table, column) in [
        ("ban_appeals", "appellant_id"),
        ("message_reports", "reporter_id"),
        ("user_reports", "reporter_id"),
        ("export_jobs", "participant_id"),
        ("user_blocks", "blocker_id"),
    ] {
        let count: i64 =
            sqlx::query_scalar(&format!("SELECT count(*) FROM {table} WHERE {column} = $1"))
                .bind(erased.to_uuid())
                .fetch_one(&pool)
                .await
                .expect("count erased participant's rows");
        assert_eq!(
            count, 0,
            "{table} authored by the erased user must be deleted"
        );
    }

    for (table, column, value) in [
        ("ban_appeals", "appellant_id", other_appellant),
        ("message_reports", "reporter_id", other.to_uuid()),
        ("user_reports", "reported_id", erased.to_uuid()),
        ("export_jobs", "participant_id", other.to_uuid()),
        ("user_blocks", "blocked_id", erased.to_uuid()),
    ] {
        let count: i64 =
            sqlx::query_scalar(&format!("SELECT count(*) FROM {table} WHERE {column} = $1"))
                .bind(value)
                .fetch_one(&pool)
                .await
                .expect("count retained rows");
        assert_eq!(
            count, 1,
            "{table} authored by or concerning others must be kept"
        );
    }
}
