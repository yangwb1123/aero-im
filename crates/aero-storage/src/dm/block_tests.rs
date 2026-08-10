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
        .bind(format!("dm-block-{label}-{participant}"))
        .execute(pool)
        .await
        .unwrap();
    participant
}

async fn enroll(
    pool: &PgPool,
    workspace: WorkspaceId,
    first: ParticipantId,
    second: ParticipantId,
) {
    sqlx::query(
        r"INSERT INTO workspace_members
              (workspace_id, participant_id, role, joined_at)
          VALUES ($1, $2, 'member', NOW()),
                 ($1, $3, 'member', NOW())",
    )
    .bind(workspace.to_uuid())
    .bind(first.to_uuid())
    .bind(second.to_uuid())
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn committed_or_racing_block_prevents_dm_creation() {
    let pg = pool();
    let workspace = WorkspaceId::from_uuid(uuid::Uuid::nil());
    let alice = participant(&pg, "alice").await;
    let bob = participant(&pg, "bob").await;
    enroll(&pg, workspace, alice, bob).await;

    crate::BlockRepo::new(pg.clone())
        .block(alice, bob)
        .await
        .unwrap();
    let repo = DmRepo::new(pg.clone());
    assert!(matches!(
        repo.find_or_create_in_workspace(workspace, alice, bob)
            .await,
        Err(DmWriteError::Blocked)
    ));
    crate::BlockRepo::new(pg.clone())
        .unblock(alice, bob)
        .await
        .unwrap();

    let mut blocking = pg.begin().await.unwrap();
    crate::user_blocks::lock_user_block_pair(&mut blocking, alice, bob)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_blocks (blocker_id, blocked_id) VALUES ($1, $2)")
        .bind(bob.to_uuid())
        .bind(alice.to_uuid())
        .execute(&mut *blocking)
        .await
        .unwrap();

    let mut waiting = tokio::spawn(async move {
        repo.find_or_create_in_workspace(workspace, alice, bob)
            .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut waiting)
            .await
            .is_err(),
        "DM open must wait for the block-pair fence"
    );
    blocking.commit().await.unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
        .await
        .expect("waiting DM completes")
        .expect("task does not panic");
    assert!(matches!(result, Err(DmWriteError::Blocked)));

    let direct_rooms: i64 =
        sqlx::query_scalar("SELECT count(*) FROM rooms WHERE kind = 'direct' AND created_by = $1")
            .bind(alice.to_uuid())
            .fetch_one(&pg)
            .await
            .unwrap();
    assert_eq!(direct_rooms, 0);
    // 0227 owner guard: drop memberships first so the raw participant DELETE
    // satisfies `participant_workspace_raw_owner_guard` (same pattern as
    // call/security_tests.rs).
    sqlx::query("DELETE FROM workspace_members WHERE participant_id = ANY($1)")
        .bind(vec![alice.to_uuid(), bob.to_uuid()])
        .execute(&pg)
        .await
        .unwrap();
    sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
        .bind(vec![alice.to_uuid(), bob.to_uuid()])
        .execute(&pg)
        .await
        .unwrap();
}
