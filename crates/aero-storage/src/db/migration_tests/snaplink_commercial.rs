//! Rolling-upgrade proof for Billing/Audit service-identity separation.

use super::*;

#[tokio::test]
#[ignore = "requires a brand-new, disposable Postgres database"]
async fn migration_0237_backfills_before_installing_destination_guard() {
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
        "Snaplink credential migration refuses to mutate a non-empty database"
    );

    Migrator::new(migrations_through(236).0.as_path())
        .await
        .expect("load migrations through 0236")
        .run(&pool)
        .await
        .expect("migrate disposable database through 0236");
    assert_eq!(applied_version(&pool).await, 236);

    let workspace = WorkspaceId::new();
    sqlx::query(
        "INSERT INTO snaplink_commercial_bindings
                (workspace_id, tenant_id, client_id, source_system, revision, enabled)
         VALUES ($1, 'tenant-upgrade', 'legacy-shared-client', 'aero-im.upgrade', 1, TRUE)",
    )
    .bind(workspace.to_uuid())
    .execute(&pool)
    .await
    .expect("insert version-1 shared credential binding");

    let current = Migrator::new(migration_source().as_path())
        .await
        .expect("load full migration chain");
    current
        .run(&pool)
        .await
        .expect("0237 backfills while replacing the version-1 update guard");
    let migrated: (String, String, i64) = sqlx::query_as(
        "SELECT client_id, audit_client_id, revision
           FROM snaplink_commercial_bindings WHERE workspace_id = $1",
    )
    .bind(workspace.to_uuid())
    .fetch_one(&pool)
    .await
    .expect("read migrated binding");
    assert_eq!(
        migrated,
        (
            "legacy-shared-client".into(),
            "legacy-shared-client".into(),
            1
        )
    );

    sqlx::query(
        "UPDATE snaplink_commercial_bindings
            SET audit_client_id = 'dedicated-audit-client', revision = 2
          WHERE workspace_id = $1",
    )
    .bind(workspace.to_uuid())
    .execute(&pool)
    .await
    .expect("install dedicated Audit client at the next desired revision");
}
