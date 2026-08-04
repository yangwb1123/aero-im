use super::*;

fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_lazy(&url)
        .expect("valid DATABASE_URL")
}

#[tokio::test]
#[ignore = "requires live Postgres"]
async fn effective_member_batch_rejects_every_inactive_state() {
    let pool = pool();
    let repo = WorkspaceRepo::new(pool.clone());
    let owner = ParticipantId::new();
    let active = ParticipantId::new();
    let deleted = ParticipantId::new();
    let deactivated = ParticipantId::new();
    let pending_2fa = ParticipantId::new();
    for participant in [owner, active, deleted, deactivated, pending_2fa] {
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(participant.to_uuid())
            .bind(format!("effective-batch-{participant}"))
            .execute(&pool)
            .await
            .unwrap();
    }
    let workspace = WorkspaceId::new();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        r"INSERT INTO workspaces (id, name, slug, created_by)
           VALUES ($1, 'Effective batch', $2, $3)",
    )
    .bind(workspace.to_uuid())
    .bind(format!("effective-batch-{workspace}"))
    .bind(owner.to_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    for (participant, role) in [
        (owner, "owner"),
        (active, "member"),
        (deleted, "member"),
        (deactivated, "member"),
        (pending_2fa, "member"),
    ] {
        sqlx::query(
            r"INSERT INTO workspace_members
                  (workspace_id, participant_id, role, joined_at)
               VALUES ($1, $2, $3, now())",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(role)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();

    assert!(repo
        .all_effective_members(workspace, &[owner, active, active])
        .await
        .unwrap());
    sqlx::query("UPDATE participants SET deleted_at = now() WHERE id = $1")
        .bind(deleted.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r"INSERT INTO workspace_deactivations
              (workspace_id, participant_id, deactivated_by)
           VALUES ($1, $2, $3)",
    )
    .bind(workspace.to_uuid())
    .bind(deactivated.to_uuid())
    .bind(owner.to_uuid())
    .execute(&pool)
    .await
    .unwrap();
    assert!(!repo
        .all_effective_members(workspace, &[active, deleted])
        .await
        .unwrap());
    assert!(!repo
        .all_effective_members(workspace, &[active, deactivated])
        .await
        .unwrap());

    sqlx::query(
        r"INSERT INTO totp_secrets
              (participant_id, secret, activated, activated_at)
           VALUES ($1, $2, true, now())",
    )
    .bind(owner.to_uuid())
    .bind(format!("secret-{owner}"))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE workspaces SET require_2fa = true WHERE id = $1")
        .bind(workspace.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        r"INSERT INTO totp_secrets
              (participant_id, secret, activated, activated_at)
           VALUES ($1, $2, true, now())",
    )
    .bind(active.to_uuid())
    .bind(format!("secret-{active}"))
    .execute(&pool)
    .await
    .unwrap();
    assert!(repo
        .all_effective_members(workspace, &[owner, active])
        .await
        .unwrap());
    assert!(!repo
        .all_effective_members(workspace, &[active, pending_2fa])
        .await
        .unwrap());
}
