//! Rolling-upgrade proof for machine/human integration issuer separation.

use super::*;

#[tokio::test]
#[ignore = "requires a brand-new, disposable Postgres database"]
async fn migration_0233_backfills_and_constrains_human_identity_issuer() {
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
        "integration issuer migration refuses to mutate a non-empty database"
    );

    Migrator::new(migrations_through(232).0.as_path())
        .await
        .expect("load migrations through 0232")
        .run(&pool)
        .await
        .expect("migrate disposable database through 0232");
    assert_eq!(applied_version(&pool).await, 232);

    let owner = ParticipantId::new();
    let bot = ParticipantId::new();
    for (participant, kind, name) in [
        (owner, "human", "integration migration owner"),
        (bot, "bot", "integration migration bot"),
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
        .expect("insert migration participant");
    }
    let workspace = WorkspaceId::new();
    let mut tenant = pool.begin().await.expect("begin migration tenant fixture");
    sqlx::query(
        "INSERT INTO workspaces (id, name, slug, created_by)
         VALUES ($1, 'Integration issuer migration', $2, $3)",
    )
    .bind(workspace.to_uuid())
    .bind(format!("integration-issuer-migration-{workspace}"))
    .bind(owner.to_uuid())
    .execute(&mut *tenant)
    .await
    .expect("insert migration workspace");
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, 'owner')",
    )
    .bind(workspace.to_uuid())
    .bind(owner.to_uuid())
    .execute(&mut *tenant)
    .await
    .expect("insert migration workspace owner");
    tenant
        .commit()
        .await
        .expect("commit migration tenant fixture");
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, participant_id, role)
         VALUES ($1, $2, 'member')",
    )
    .bind(workspace.to_uuid())
    .bind(bot.to_uuid())
    .execute(&pool)
    .await
    .expect("insert migration bot membership");
    sqlx::query(
        "INSERT INTO bots (id, owner_id, name, workspace_id)
         VALUES ($1, $2, 'integration migration bot', $3)",
    )
    .bind(bot.to_uuid())
    .bind(owner.to_uuid())
    .bind(workspace.to_uuid())
    .execute(&pool)
    .await
    .expect("insert migration bot");

    let installation = Uuid::new_v4();
    let legacy_issuer = format!("https://legacy-machine-sso.test/{installation}");
    sqlx::query(
        "INSERT INTO integration_installations
             (id, workspace_id, bot_id, issuer, client_id, name, created_by)
         VALUES ($1, $2, $3, $4, 'legacy-client', 'legacy installation', $5)",
    )
    .bind(installation)
    .bind(workspace.to_uuid())
    .bind(bot.to_uuid())
    .bind(&legacy_issuer)
    .bind(owner.to_uuid())
    .execute(&pool)
    .await
    .expect("insert pre-0233 installation");

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
        .expect("0233 separates integration issuers");
    assert_eq!(applied_version(&pool).await, latest_version);

    let migrated: (String, String) = sqlx::query_as(
        "SELECT issuer, user_identity_issuer
           FROM integration_installations
          WHERE id = $1",
    )
    .bind(installation)
    .fetch_one(&pool)
    .await
    .expect("load migrated installation");
    assert_eq!(migrated, (legacy_issuer.clone(), legacy_issuer));

    let rolling_installation = Uuid::new_v4();
    let rolling_issuer = format!("https://rolling-machine-sso.test/{rolling_installation}");
    sqlx::query(
        "INSERT INTO integration_installations
             (id, workspace_id, bot_id, issuer, client_id, name, created_by)
         VALUES ($1, $2, $3, $4, 'rolling-client', 'old writer installation', $5)",
    )
    .bind(rolling_installation)
    .bind(workspace.to_uuid())
    .bind(bot.to_uuid())
    .bind(&rolling_issuer)
    .bind(owner.to_uuid())
    .execute(&pool)
    .await
    .expect("0232 writer remains compatible after 0233");
    let rolling_human_issuer: String = sqlx::query_scalar(
        "SELECT user_identity_issuer FROM integration_installations WHERE id = $1",
    )
    .bind(rolling_installation)
    .fetch_one(&pool)
    .await
    .expect("load rolling-upgrade installation");
    assert_eq!(rolling_human_issuer, rolling_issuer);

    let null_rejected = sqlx::query(
        "UPDATE integration_installations SET user_identity_issuer = NULL WHERE id = $1",
    )
    .bind(installation)
    .execute(&pool)
    .await
    .expect_err("human issuer is required after migration");
    assert!(null_rejected.as_database_error().is_some());

    let malformed_rejected = sqlx::query(
        "UPDATE integration_installations SET user_identity_issuer = ' bad' WHERE id = $1",
    )
    .bind(installation)
    .execute(&pool)
    .await
    .expect_err("human issuer check rejects non-exact values");
    assert_eq!(
        malformed_rejected
            .as_database_error()
            .and_then(|database| database.constraint()),
        Some("integration_installations_user_identity_issuer_valid")
    );
}
