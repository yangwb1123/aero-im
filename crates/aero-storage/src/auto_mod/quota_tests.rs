use aero_common::{Error, ParticipantId};

use super::{
    AutoModRuleRepo, AutoModRuleWriteError, MAX_AUTO_MOD_RULES_PER_WORKSPACE,
    MAX_AUTO_MOD_RULES_PER_WORKSPACE_DB,
};
use crate::WorkspaceRepo;

fn pool(max_connections: u32) -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(max_connections)
        .connect_lazy(&url)
        .expect("valid DATABASE_URL")
}

async fn workspace_fixture(
    pool: &sqlx::PgPool,
    label: &str,
) -> (ParticipantId, aero_common::WorkspaceId) {
    let owner = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id,kind,display_name) VALUES ($1,'human',$2)")
        .bind(owner.to_uuid())
        .bind(format!("auto-mod-{label}-{owner}"))
        .execute(pool)
        .await
        .expect("participant");
    let workspace = WorkspaceRepo::new(pool.clone())
        .create(
            format!("Auto-mod {label} {owner}"),
            format!("auto-mod-{label}-{owner}"),
            owner,
        )
        .await
        .expect("workspace")
        .id;
    (owner, workspace)
}

async fn cleanup(pool: &sqlx::PgPool, owner: ParticipantId, workspace: aero_common::WorkspaceId) {
    sqlx::query("DELETE FROM workspaces WHERE id = $1")
        .bind(workspace.to_uuid())
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM participants WHERE id = $1")
        .bind(owner.to_uuid())
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn workspace_rule_quota_is_atomic_under_concurrent_creates() {
    let pool = pool(4);
    let (owner, workspace) = workspace_fixture(&pool, "quota").await;
    sqlx::query(
        "INSERT INTO auto_mod_rules
             (id, workspace_id, pattern, match_type, action, created_by)
         SELECT gen_random_uuid(), $1, 'seed-' || n, 'contains', 'block', $2
           FROM generate_series(1, $3) AS n",
    )
    .bind(workspace.to_uuid())
    .bind(owner.to_uuid())
    .bind(MAX_AUTO_MOD_RULES_PER_WORKSPACE_DB - 1)
    .execute(&pool)
    .await
    .expect("seed one slot below quota");

    let repo = AutoModRuleRepo::new(pool.clone());
    let first = repo.create_authorized(workspace, "CONTENDER-A", "contains", "block", owner);
    let second = repo.create_authorized(workspace, "CONTENDER-B", "contains", "block", owner);
    let (first, second) = tokio::join!(first, second);
    let mut succeeded = 0;
    let mut quota_rejected = 0;
    for result in [first, second] {
        match result {
            Ok(rule) => {
                succeeded += 1;
                assert!(
                    ["contender-a", "contender-b"].contains(&rule.pattern.as_str()),
                    "new patterns are normalized once at creation"
                );
            }
            Err(AutoModRuleWriteError::QuotaExceeded) => quota_rejected += 1,
            Err(error) => panic!("unexpected create result: {error}"),
        }
    }

    let count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM auto_mod_rules WHERE workspace_id = $1")
            .bind(workspace.to_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(succeeded, 1);
    assert_eq!(quota_rejected, 1);
    assert_eq!(count, MAX_AUTO_MOD_RULES_PER_WORKSPACE_DB);
    assert_eq!(
        repo.list_for_enforcement(workspace).await.unwrap().len(),
        MAX_AUTO_MOD_RULES_PER_WORKSPACE
    );

    cleanup(&pool, owner, workspace).await;
}

#[tokio::test]
#[ignore = "requires a migrated PostgreSQL database"]
async fn legacy_over_quota_rules_fail_closed_but_management_can_recover() {
    let pool = pool(2);
    let (owner, workspace) = workspace_fixture(&pool, "recovery").await;
    sqlx::query(
        "INSERT INTO auto_mod_rules
             (id, workspace_id, pattern, match_type, action, created_by)
         SELECT gen_random_uuid(), $1, 'LEGACY-' || n, 'contains', 'block', $2
           FROM generate_series(1, $3) AS n",
    )
    .bind(workspace.to_uuid())
    .bind(owner.to_uuid())
    .bind(MAX_AUTO_MOD_RULES_PER_WORKSPACE_DB + 1)
    .execute(&pool)
    .await
    .expect("seed legacy over-quota configuration");

    let repo = AutoModRuleRepo::new(pool.clone());
    assert!(matches!(
        repo.list_for_enforcement(workspace).await,
        Err(Error::Conflict(message)) if message.contains("exceeds the maximum")
    ));
    let recovery_page = repo.list_for_workspace(workspace).await.unwrap();
    assert_eq!(recovery_page.len(), MAX_AUTO_MOD_RULES_PER_WORKSPACE + 1);
    assert!(
        recovery_page
            .iter()
            .all(|rule| rule.pattern == rule.pattern.to_lowercase()),
        "legacy patterns are normalized once when loaded"
    );
    repo.delete_authorized(recovery_page[0].id, workspace, owner)
        .await
        .expect("management delete remains available");
    assert_eq!(
        repo.list_for_enforcement(workspace).await.unwrap().len(),
        MAX_AUTO_MOD_RULES_PER_WORKSPACE
    );
    assert!(matches!(
        repo.create_authorized(workspace, "still-full", "contains", "block", owner)
            .await,
        Err(AutoModRuleWriteError::QuotaExceeded)
    ));

    cleanup(&pool, owner, workspace).await;
}
