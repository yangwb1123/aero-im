//! Rolling-upgrade regression proof for the workspace-bot membership repair.

use super::*;

#[tokio::test]
#[ignore = "requires a brand-new, disposable Postgres database"]
async fn migration_0228_backfills_workspace_bot_membership() {
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must select a disposable database");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect disposable database");
    let public_relations: i64 = sqlx::query_scalar(
        "SELECT count(*)
           FROM pg_class relation
           JOIN pg_namespace namespace ON namespace.oid = relation.relnamespace
          WHERE namespace.nspname = 'public'
            AND relation.relkind IN ('r', 'p', 'v', 'm')",
    )
    .fetch_one(&pool)
    .await
    .expect("inspect disposable database");
    assert_eq!(
        public_relations, 0,
        "bot-membership regression refuses to mutate a non-empty database"
    );

    let through_0227_directory = migrations_through(227);
    Migrator::new(through_0227_directory.0.as_path())
        .await
        .expect("load migrations through 0227")
        .run(&pool)
        .await
        .expect("migrate disposable database through 0227");
    assert_eq!(applied_version(&pool).await, 227);

    let owner = ParticipantId::new();
    let missing_bot = ParticipantId::new();
    let guest_bot = ParticipantId::new();
    for (participant, kind, name) in [
        (owner, "human", "bot migration owner"),
        (missing_bot, "bot", "missing membership bot"),
        (guest_bot, "bot", "guest-shaped bot"),
    ] {
        sqlx::query(
            "INSERT INTO participants (id, kind, display_name, created_by)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(participant.to_uuid())
        .bind(kind)
        .bind(name)
        .bind((participant != owner).then_some(owner.to_uuid()))
        .execute(&pool)
        .await
        .expect("insert participant");
    }
    let workspace = WorkspaceId::new();
    let mut tenant = pool.begin().await.expect("begin tenant fixture");
    sqlx::query(
        "INSERT INTO workspaces (id, name, slug, created_by)
         VALUES ($1, 'Bot migration', $2, $3)",
    )
    .bind(workspace.to_uuid())
    .bind(format!("bot-migration-{workspace}"))
    .bind(owner.to_uuid())
    .execute(&mut *tenant)
    .await
    .expect("insert workspace");
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, 'owner')",
    )
    .bind(workspace.to_uuid())
    .bind(owner.to_uuid())
    .execute(&mut *tenant)
    .await
    .expect("insert effective owner");
    tenant.commit().await.expect("commit tenant fixture");

    for (bot, name) in [
        (missing_bot, "missing membership bot"),
        (guest_bot, "guest-shaped bot"),
    ] {
        sqlx::query(
            "INSERT INTO bots (id, owner_id, name, workspace_id)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(bot.to_uuid())
        .bind(owner.to_uuid())
        .bind(name)
        .bind(workspace.to_uuid())
        .execute(&pool)
        .await
        .expect("insert pre-0228 workspace bot");
    }
    sqlx::query(
        "INSERT INTO workspace_members
             (workspace_id, participant_id, role, is_guest)
         VALUES ($1, $2, 'guest', true)",
    )
    .bind(workspace.to_uuid())
    .bind(guest_bot.to_uuid())
    .execute(&pool)
    .await
    .expect("insert guest-shaped legacy bot membership");

    let current_source = Migrator::new(migration_source().as_path())
        .await
        .expect("load full migration chain");
    let latest_version = current_source
        .iter()
        .map(|migration| migration.version)
        .max()
        .expect("migration chain is non-empty");
    current_source
        .run(&pool)
        .await
        .expect("0228 repairs every live workspace bot");
    assert_eq!(applied_version(&pool).await, latest_version);

    for bot in [missing_bot, guest_bot] {
        let membership = sqlx::query_as::<_, (String, bool)>(
            "SELECT role, is_guest
               FROM workspace_members
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(bot.to_uuid())
        .fetch_one(&pool)
        .await
        .expect("repaired bot membership");
        assert_eq!(membership, ("member".into(), false));
        assert!(
            sqlx::query_scalar::<_, bool>("SELECT aero_effective_workspace_access($1, $2)")
                .bind(workspace.to_uuid())
                .bind(bot.to_uuid())
                .fetch_one(&pool)
                .await
                .expect("effective bot access")
        );
    }
}
