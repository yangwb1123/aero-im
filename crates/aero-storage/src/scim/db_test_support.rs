use aero_common::{ParticipantId, WorkspaceId};
use sqlx::PgPool;

use crate::workspace::WorkspaceRepo;

pub(super) fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&url)
        .expect("connect_lazy never fails on a well-formed URL")
}

pub(super) async fn new_participant(pool: &PgPool) -> ParticipantId {
    let id = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
        .bind(id.to_uuid())
        .bind(format!("scim-user-{id}"))
        .execute(pool)
        .await
        .expect("insert participant");
    id
}

pub(super) async fn new_workspace(repo: &WorkspaceRepo, owner: ParticipantId) -> WorkspaceId {
    repo.create(
        "SCIM WS".into(),
        format!("scim-{}", WorkspaceId::new()),
        owner,
    )
    .await
    .expect("create workspace")
    .id
}
