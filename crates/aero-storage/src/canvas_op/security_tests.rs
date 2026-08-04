use super::*;
use aero_common::{RoomKind, WorkspaceRole};

struct Fixture {
    owner: ParticipantId,
    member: ParticipantId,
    outsider: ParticipantId,
    room: RoomId,
    other_room: RoomId,
    canvas: CanvasId,
}

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect_lazy(&url)
        .expect("connect_lazy accepts the configured URL")
}

async fn participant(pool: &PgPool, label: &str) -> ParticipantId {
    let id = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(id.to_uuid())
        .bind(format!("canvas-op-{label}-{id}"))
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn fixture(pool: &PgPool) -> Fixture {
    let owner = participant(pool, "owner").await;
    let member = participant(pool, "member").await;
    let outsider = participant(pool, "outsider").await;
    let workspaces = crate::WorkspaceRepo::new(pool.clone());
    let workspace = workspaces
        .create(
            format!("Canvas op {owner}"),
            format!("canvas-op-{owner}"),
            owner,
        )
        .await
        .unwrap()
        .id;
    for participant in [member, outsider] {
        workspaces
            .add_member(workspace, participant, WorkspaceRole::Member)
            .await
            .unwrap();
    }
    let rooms = crate::RoomRepo::new(pool.clone());
    let room = rooms
        .create_in_workspace(
            workspace,
            RoomKind::Channel,
            Some("canvas-op-primary".into()),
            owner,
        )
        .await
        .unwrap()
        .id;
    rooms.add_member(room, member).await.unwrap();
    let other_room = rooms
        .create_in_workspace(
            workspace,
            RoomKind::Channel,
            Some("canvas-op-other".into()),
            owner,
        )
        .await
        .unwrap()
        .id;
    let canvas = crate::CanvasRepo::new(pool.clone())
        .create_canvas_authorized(room, owner, "Ops", &serde_json::json!([]))
        .await
        .unwrap()
        .id;
    Fixture {
        owner,
        member,
        outsider,
        room,
        other_room,
        canvas,
    }
}

fn constraint(error: &sqlx::Error) -> Option<&str> {
    error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::constraint)
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn append_is_gapfree_idempotent_outboxed_and_path_scoped() {
    let pool = pool();
    let repo = CanvasOpRepo::new(pool.clone());
    let fixture = fixture(&pool).await;
    let client_id = Uuid::new_v4();
    let payload = serde_json::json!({"type":"set_text","text":"once"});
    let first = repo
        .append_canvas_op_authorized(
            fixture.room,
            fixture.canvas,
            fixture.owner,
            client_id,
            &payload,
            Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        )
        .await
        .unwrap();
    let retry = repo
        .append_canvas_op_authorized(
            fixture.room,
            fixture.canvas,
            fixture.owner,
            client_id,
            &payload,
            None,
        )
        .await
        .unwrap();
    assert!(first.inserted);
    assert!(!retry.inserted);
    assert_eq!(retry.op.id, first.op.id);
    assert_eq!(retry.op.seq, first.op.seq);
    assert_eq!(retry.outbox_id, first.outbox_id);

    let (event_id, message_id, kind, subject, event): (
        Uuid,
        Uuid,
        String,
        String,
        serde_json::Value,
    ) = sqlx::query_as(
        "SELECT event_id, message_id, event_kind, subject, payload
           FROM event_outbox WHERE id = $1",
    )
    .bind(first.outbox_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(event_id, first.op.id);
    assert_eq!(message_id, first.op.id);
    assert_eq!(kind, "canvas_op");
    assert_eq!(subject, format!("im.room.{}", fixture.room));
    assert_eq!(event["canvas_id"], fixture.canvas.to_string());
    assert_eq!(event["op_seq"], 1);
    assert_eq!(event["op"], payload);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM event_outbox WHERE message_id = $1")
            .bind(first.op.id)
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );

    assert!(matches!(
        repo.append_canvas_op_authorized(
            fixture.room,
            fixture.canvas,
            fixture.owner,
            client_id,
            &serde_json::json!({"type":"set_text","text":"different"}),
            None,
        )
        .await,
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        repo.append_canvas_op_authorized(
            fixture.other_room,
            fixture.canvas,
            fixture.owner,
            Uuid::new_v4(),
            &serde_json::json!({"type":"wrong_room"}),
            None,
        )
        .await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        repo.list_canvas_ops_authorized(fixture.other_room, fixture.canvas, fixture.owner, 0, 10,)
            .await,
        Err(Error::NotFound(_))
    ));

    let second = repo
        .append_canvas_op_authorized(
            fixture.room,
            fixture.canvas,
            fixture.member,
            Uuid::new_v4(),
            &serde_json::json!({"type":"add_note","text":"two"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(second.op.seq, 2);
    let rows = repo
        .list_canvas_ops_authorized(fixture.room, fixture.canvas, fixture.member, 0, 100)
        .await
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|operation| operation.seq)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );

    let concurrent_id = Uuid::new_v4();
    let left_repo = repo.clone();
    let right_repo = repo.clone();
    let left_payload = serde_json::json!({"type":"add_note","note_id":"race","text":"once"});
    let right_payload = left_payload.clone();
    let (left, right) = tokio::join!(
        left_repo.append_canvas_op_authorized(
            fixture.room,
            fixture.canvas,
            fixture.owner,
            concurrent_id,
            &left_payload,
            None,
        ),
        right_repo.append_canvas_op_authorized(
            fixture.room,
            fixture.canvas,
            fixture.owner,
            concurrent_id,
            &right_payload,
            None,
        ),
    );
    let left = left.unwrap();
    let right = right.unwrap();
    assert_ne!(left.inserted, right.inserted);
    assert_eq!(left.op.id, right.op.id);
    assert_eq!(left.op.seq, 3);
    assert_eq!(left.outbox_id, right.outbox_id);
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn raw_op_and_outbox_guards_reject_nonmembers_and_forged_identity() {
    let pool = pool();
    let repo = CanvasOpRepo::new(pool.clone());
    let fixture = fixture(&pool).await;
    let valid = repo
        .append_canvas_op_authorized(
            fixture.room,
            fixture.canvas,
            fixture.owner,
            Uuid::new_v4(),
            &serde_json::json!({"type":"valid"}),
            None,
        )
        .await
        .unwrap();
    let error = sqlx::query("UPDATE canvas_ops SET author_id = $2 WHERE id = $1")
        .bind(valid.op.id)
        .bind(fixture.member.to_uuid())
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(
        constraint(&error),
        Some("canvas_ops_identity_immutable_chk")
    );
    let error = sqlx::query("UPDATE event_outbox SET event_kind = 'message' WHERE id = $1")
        .bind(valid.outbox_id)
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(
        constraint(&error),
        Some("event_outbox_canvas_op_identity_immutable_chk")
    );

    let before_seq: i64 = sqlx::query_scalar("SELECT op_seq FROM channel_canvases WHERE id = $1")
        .bind(fixture.canvas.to_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut raw = pool.begin().await.unwrap();
    let forged_seq: i64 = sqlx::query_scalar(
        "UPDATE channel_canvases
            SET op_seq = op_seq + 1, updated_at = now()
          WHERE id = $1
          RETURNING op_seq",
    )
    .bind(fixture.canvas.to_uuid())
    .fetch_one(&mut *raw)
    .await
    .unwrap();
    let error = sqlx::query(
        "INSERT INTO canvas_ops
             (id, canvas_id, seq, author_id, client_op_id, op)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::new_v4())
    .bind(fixture.canvas.to_uuid())
    .bind(forged_seq)
    .bind(fixture.outsider.to_uuid())
    .bind(Uuid::new_v4())
    .bind(serde_json::json!({"type":"forged"}))
    .execute(&mut *raw)
    .await
    .unwrap_err();
    assert_eq!(constraint(&error), Some("canvas_ops_author_scope_chk"));
    raw.rollback().await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT op_seq FROM channel_canvases WHERE id = $1")
            .bind(fixture.canvas.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap(),
        before_seq
    );

    let forged = Uuid::new_v4();
    let error = sqlx::query(
        "INSERT INTO event_outbox
             (id, event_id, message_id, event_kind, aggregate_version, subject, payload)
         VALUES ($1, $2, $2, 'canvas_op', 1, $3, $4)",
    )
    .bind(Uuid::new_v4())
    .bind(forged)
    .bind(format!("im.room.{}", fixture.other_room))
    .bind(serde_json::json!({
        "kind": "canvas_op",
        "room_id": fixture.other_room,
        "canvas_id": fixture.canvas,
        "op_id": forged,
        "op_seq": 1,
        "author_id": fixture.owner,
        "op": {"type": "forged"}
    }))
    .execute(&pool)
    .await
    .unwrap_err();
    assert_eq!(constraint(&error), Some("event_outbox_canvas_op_scope_chk"));

    let mut no_outbox = pool.begin().await.unwrap();
    let missing_outbox_op = Uuid::new_v4();
    let seq: i64 = sqlx::query_scalar(
        "UPDATE channel_canvases
            SET op_seq = op_seq + 1, updated_at = now()
          WHERE id = $1
          RETURNING op_seq",
    )
    .bind(fixture.canvas.to_uuid())
    .fetch_one(&mut *no_outbox)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO canvas_ops
             (id, canvas_id, seq, author_id, client_op_id, op)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(missing_outbox_op)
    .bind(fixture.canvas.to_uuid())
    .bind(seq)
    .bind(fixture.owner.to_uuid())
    .bind(Uuid::new_v4())
    .bind(serde_json::json!({"type":"missing_outbox"}))
    .execute(&mut *no_outbox)
    .await
    .unwrap();
    let error = no_outbox.commit().await.unwrap_err();
    assert_eq!(constraint(&error), Some("canvas_ops_durable_outbox_chk"));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM canvas_ops WHERE id = $1")
            .bind(missing_outbox_op)
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT op_seq FROM channel_canvases WHERE id = $1")
            .bind(fixture.canvas.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap(),
        before_seq
    );
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn concurrent_revocation_causes_zero_op_and_zero_outbox_write() {
    let pool = pool();
    let repo = CanvasOpRepo::new(pool.clone());
    let fixture = fixture(&pool).await;
    let before_ops: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM canvas_ops WHERE canvas_id = $1")
            .bind(fixture.canvas.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    let subject = format!("im.room.{}", fixture.room);
    let before_outbox: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM event_outbox
          WHERE event_kind = 'canvas_op' AND subject = $1",
    )
    .bind(&subject)
    .fetch_one(&pool)
    .await
    .unwrap();

    let mut revoke = pool.begin().await.unwrap();
    sqlx::query("DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
        .bind(fixture.room.to_uuid())
        .bind(fixture.member.to_uuid())
        .execute(&mut *revoke)
        .await
        .unwrap();
    let append_repo = repo.clone();
    let room = fixture.room;
    let canvas = fixture.canvas;
    let member = fixture.member;
    let append = tokio::spawn(async move {
        append_repo
            .append_canvas_op_authorized(
                room,
                canvas,
                member,
                Uuid::new_v4(),
                &serde_json::json!({"type":"revoked"}),
                None,
            )
            .await
    });
    tokio::task::yield_now().await;
    revoke.commit().await.unwrap();

    assert!(matches!(append.await.unwrap(), Err(Error::Forbidden(_))));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM canvas_ops WHERE canvas_id = $1")
            .bind(fixture.canvas.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap(),
        before_ops
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM event_outbox
              WHERE event_kind = 'canvas_op' AND subject = $1"
        )
        .bind(subject)
        .fetch_one(&pool)
        .await
        .unwrap(),
        before_outbox
    );
}
