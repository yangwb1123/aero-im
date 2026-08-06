use super::*;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

async fn participant(pool: &PgPool, label: &str) -> ParticipantId {
    let participant = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(participant.to_uuid())
        .bind(format!("call-block-{label}-{participant}"))
        .execute(pool)
        .await
        .unwrap();
    participant
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn racing_block_completes_before_direct_call_can_start() {
    let pg = pool();
    let workspace = aero_common::WorkspaceId::from_uuid(uuid::Uuid::nil());
    let caller = participant(&pg, "caller").await;
    let target = participant(&pg, "callee").await;
    sqlx::query(
        r"INSERT INTO workspace_members
              (workspace_id, participant_id, role, joined_at)
          VALUES ($1, $2, 'member', NOW()),
                 ($1, $3, 'member', NOW())",
    )
    .bind(workspace.to_uuid())
    .bind(caller.to_uuid())
    .bind(target.to_uuid())
    .execute(&pg)
    .await
    .unwrap();
    let room = crate::DmRepo::new(pg.clone())
        .find_or_create_in_workspace(workspace, caller, target)
        .await
        .unwrap()
        .id;

    let mut blocking = pg.begin().await.unwrap();
    crate::user_blocks::lock_user_block_pair(&mut blocking, caller, target)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_blocks (blocker_id, blocked_id) VALUES ($1, $2)")
        .bind(target.to_uuid())
        .bind(caller.to_uuid())
        .execute(&mut *blocking)
        .await
        .unwrap();

    let repo = CallRepo::new(pg.clone());
    let mut waiting = tokio::spawn(async move {
        repo.start_authorized(CallId::new(), room, caller, CallKind::Audio, CallMode::P2p)
            .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut waiting)
            .await
            .is_err(),
        "call start must wait for the block-pair fence"
    );
    blocking.commit().await.unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
        .await
        .expect("waiting call completes")
        .expect("task does not panic");
    assert!(matches!(result, Err(Error::Forbidden(_))));

    let calls: i64 = sqlx::query_scalar("SELECT count(*) FROM call_sessions WHERE room_id = $1")
        .bind(room.to_uuid())
        .fetch_one(&pg)
        .await
        .unwrap();
    assert_eq!(calls, 0);
    sqlx::query("DELETE FROM rooms WHERE id = $1")
        .bind(room.to_uuid())
        .execute(&pg)
        .await
        .ok();
    sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
        .bind(vec![caller.to_uuid(), target.to_uuid()])
        .execute(&pg)
        .await
        .unwrap();
}
