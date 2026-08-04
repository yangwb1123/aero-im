use super::*;
use aero_common::WorkspaceRole;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

async fn participant(pool: &PgPool) -> ParticipantId {
    let participant = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(participant.to_uuid())
        .bind(format!("scheduled-status-{participant}"))
        .execute(pool)
        .await
        .expect("insert participant");
    participant
}

async fn workspace(pool: &PgPool, owner: ParticipantId) -> WorkspaceId {
    let workspace = WorkspaceId::new();
    let mut tx = pool.begin().await.expect("begin workspace fixture");
    sqlx::query("INSERT INTO workspaces (id, name, slug, created_by) VALUES ($1, $2, $3, $4)")
        .bind(workspace.to_uuid())
        .bind(format!("scheduled-status-{workspace}"))
        .bind(format!("scheduled-status-{workspace}"))
        .bind(owner.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert workspace");
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, $3)",
    )
    .bind(workspace.to_uuid())
    .bind(owner.to_uuid())
    .bind(WorkspaceRole::Owner.as_str())
    .execute(&mut *tx)
    .await
    .expect("insert workspace owner");
    tx.commit().await.expect("commit workspace fixture");
    workspace
}

#[tokio::test]
#[ignore = "requires live Postgres with migrations through 0223"]
async fn scheduled_stream_status_domain_and_transitions_are_fenced() {
    let pool = pool();
    let repo = ScheduledStreamRepo::new(pool.clone());
    let owner = participant(&pool).await;
    let workspace = workspace(&pool, owner).await;
    let future = time::OffsetDateTime::now_utc() + time::Duration::hours(1);

    let constraint = sqlx::query_as::<_, (String, bool)>(
        "SELECT pg_get_constraintdef(oid), convalidated
           FROM pg_constraint
          WHERE conrelid = 'scheduled_streams'::regclass
            AND conname = 'scheduled_streams_status_domain'",
    )
    .fetch_one(&pool)
    .await
    .expect("status domain exists");
    assert!(constraint.0.contains("'scheduled'::text"));
    assert!(constraint.0.contains("'live'::text"));
    assert!(constraint.0.contains("'canceled'::text"));
    assert!(constraint.0.contains("'ended'::text"));
    assert!(!constraint.1, "rolling domain constraint remains NOT VALID");

    for initial_status in ["live", "evil"] {
        let error = sqlx::query(
            r"INSERT INTO scheduled_streams
                  (id, workspace_id, title, scheduled_for, created_by, status)
               VALUES ($1, $2, 'forged initial status', $3, $4, $5)",
        )
        .bind(ScheduledStreamId::new().to_uuid())
        .bind(workspace.to_uuid())
        .bind(future)
        .bind(owner.to_uuid())
        .bind(initial_status)
        .execute(&pool)
        .await
        .expect_err("every row must start scheduled");
        assert_eq!(
            error
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::constraint),
            Some("scheduled_streams_initial_status")
        );
    }

    let canceled = repo
        .create(workspace, None, "cancel me", None, future, owner)
        .await
        .unwrap();
    repo.cancel(canceled, owner).await.unwrap();
    let backward = sqlx::query("UPDATE scheduled_streams SET status = 'live' WHERE id = $1")
        .bind(canceled.to_uuid())
        .execute(&pool)
        .await
        .expect_err("canceled rows are terminal");
    assert_eq!(
        backward
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("scheduled_streams_status_transition")
    );

    let lifecycle = repo
        .create(workspace, None, "go live", None, future, owner)
        .await
        .unwrap();
    assert!(repo.mark_live(lifecycle).await.unwrap());
    assert!(repo.mark_ended(lifecycle).await.unwrap());
    assert!(!repo.mark_live(lifecycle).await.unwrap());

    let invalid = repo
        .create(workspace, None, "invalid update", None, future, owner)
        .await
        .unwrap();
    let invalid = sqlx::query("UPDATE scheduled_streams SET status = 'evil' WHERE id = $1")
        .bind(invalid.to_uuid())
        .execute(&pool)
        .await
        .expect_err("arbitrary status updates are rejected");
    assert_eq!(
        invalid
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("scheduled_streams_status_transition")
    );
}
