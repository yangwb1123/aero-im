use super::*;

fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_lazy(&url)
        .expect("valid DATABASE_URL")
}

async fn participant(pool: &PgPool, label: &str) -> ParticipantId {
    let participant = ParticipantId::new();
    sqlx::query(
        "INSERT INTO participants (id, kind, display_name)
         VALUES ($1, 'human', $2)",
    )
    .bind(participant.to_uuid())
    .bind(format!("{label}-{participant}"))
    .execute(pool)
    .await
    .unwrap();
    participant
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn metadata_patch_waits_for_workspace_revocation_without_lock_inversion() {
    let pool = pool();
    let workspace = WorkspaceId::from_uuid(uuid::Uuid::nil());
    let alice = participant(&pool, "group-dm-metadata-alice").await;
    let bob = participant(&pool, "group-dm-metadata-bob").await;
    let carol = participant(&pool, "group-dm-metadata-carol").await;
    for member in [alice, bob, carol] {
        sqlx::query(
            "INSERT INTO workspace_members
                 (workspace_id, participant_id, role, joined_at)
             VALUES ($1, $2, 'member', now())",
        )
        .bind(workspace.to_uuid())
        .bind(member.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    }

    let room = RoomId::new();
    sqlx::query(
        "INSERT INTO rooms
             (id, kind, name, created_by, workspace_id, is_group_dm)
         VALUES ($1, 'group', 'before', $2, $3, false)",
    )
    .bind(room.to_uuid())
    .bind(alice.to_uuid())
    .bind(workspace.to_uuid())
    .execute(&pool)
    .await
    .unwrap();
    for member in [alice, bob, carol] {
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role, joined_at)
             VALUES ($1, $2, 'member', now())",
        )
        .bind(room.to_uuid())
        .bind(member.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query("UPDATE rooms SET is_group_dm = true WHERE id = $1")
        .bind(room.to_uuid())
        .execute(&pool)
        .await
        .unwrap();

    let mut revocation = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace.to_uuid())
        .execute(&mut *revocation)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO workspace_deactivations
             (workspace_id, participant_id, deactivated_by)
         VALUES ($1, $2, $3)",
    )
    .bind(workspace.to_uuid())
    .bind(alice.to_uuid())
    .bind(bob.to_uuid())
    .execute(&mut *revocation)
    .await
    .unwrap();

    let repo = GroupDmRepo::new(pool.clone());
    let mut patch = tokio::spawn(async move {
        repo.patch_metadata(room, alice, Some(Some("after")), None)
            .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut patch)
            .await
            .is_err(),
        "metadata patch must wait behind the workspace revocation fence"
    );

    // A workspace-first patch has not locked the room while waiting. This is
    // the deadlock regression: the governance transaction can safely continue
    // through its canonical workspace -> room order.
    sqlx::query("SET LOCAL lock_timeout = '500ms'")
        .execute(&mut *revocation)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM rooms WHERE id = $1 FOR UPDATE")
        .bind(room.to_uuid())
        .execute(&mut *revocation)
        .await
        .expect("waiting metadata patch must not already hold the room lock");
    revocation.commit().await.unwrap();

    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(2), patch)
            .await
            .expect("metadata patch completes after revocation")
            .expect("metadata task does not panic"),
        Err(GroupDmWriteError::Forbidden)
    ));
    let retained_name: Option<String> = sqlx::query_scalar("SELECT name FROM rooms WHERE id = $1")
        .bind(room.to_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(retained_name.as_deref(), Some("before"));

    sqlx::query(
        "DELETE FROM workspace_deactivations
          WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(workspace.to_uuid())
    .bind(alice.to_uuid())
    .execute(&pool)
    .await
    .ok();
    sqlx::query("DELETE FROM rooms WHERE id = $1")
        .bind(room.to_uuid())
        .execute(&pool)
        .await
        .ok();
    for member in [alice, bob, carol] {
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(member.to_uuid())
            .execute(&pool)
            .await
            .ok();
    }
}
