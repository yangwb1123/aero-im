use aero_common::{Error, ParticipantId, WorkspaceId, WorkspaceRole};
use sqlx::{PgPool, Row};
use std::time::Duration;

use super::*;
use crate::{DeactivationRepo, ParticipantRepo, SsoRepo, WorkspaceRepo};

struct Fixture {
    pool: PgPool,
    repo: IdentityMigrationRepo,
    workspace: WorkspaceId,
    owner: ParticipantId,
    admin: ParticipantId,
    participant: ParticipantId,
    source: ExternalIdentityKey,
}

impl Fixture {
    async fn create() -> Self {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(8)
            .connect_lazy(&url)
            .expect("valid DATABASE_URL");
        let marker = uuid::Uuid::new_v4();
        let owner = insert_participant(&pool, "human", "identity-owner").await;
        let admin = insert_participant(&pool, "human", "identity-admin").await;
        let participant = insert_participant(&pool, "human", "identity-subject").await;
        let workspaces = WorkspaceRepo::new(pool.clone());
        let workspace = workspaces
            .create(
                format!("Identity migration {marker}"),
                format!("identity-migration-{marker}"),
                owner,
            )
            .await
            .expect("create workspace")
            .id;
        workspaces
            .add_member(workspace, admin, WorkspaceRole::Admin)
            .await
            .expect("add admin");
        workspaces
            .add_member(workspace, participant, WorkspaceRole::Member)
            .await
            .expect("add migration subject");
        let source = ExternalIdentityKey::new(
            format!("https://old-idp.example/{marker}"),
            format!("old-subject-{marker}"),
        );
        SsoRepo::new(pool.clone())
            .link(
                &source.issuer,
                &source.subject,
                participant,
                Some("identity@example.test"),
            )
            .await
            .expect("link source identity");

        Self {
            repo: IdentityMigrationRepo::new(pool.clone()),
            pool,
            workspace,
            owner,
            admin,
            participant,
            source,
        }
    }

    fn target(&self, label: &str) -> ExternalIdentityKey {
        ExternalIdentityKey::new(
            format!("https://new-idp.example/{}", self.workspace),
            format!("{label}-{}", uuid::Uuid::new_v4()),
        )
    }

    fn request(
        &self,
        actor_id: ParticipantId,
        to: ExternalIdentityKey,
        retire_source: bool,
    ) -> IdentityMigrationRequest {
        IdentityMigrationRequest {
            workspace_id: self.workspace,
            actor_id,
            participant_id: self.participant,
            from: self.source.clone(),
            to,
            retire_source,
        }
    }
}

async fn insert_participant(pool: &PgPool, kind: &str, prefix: &str) -> ParticipantId {
    let participant = ParticipantId::new();
    sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, $2, $3)")
        .bind(participant.to_uuid())
        .bind(kind)
        .bind(format!("{prefix}-{participant}"))
        .execute(pool)
        .await
        .expect("insert participant");
    participant
}

async fn single_connection_pool() -> (PgPool, i32) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect dedicated identity-race pool");
    let backend_pid = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&pool)
        .await
        .expect("read dedicated identity-race backend pid");
    (pool, backend_pid)
}

async fn wait_for_lock_wait(observer: &PgPool, backend_pid: i32, operation: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting = sqlx::query_scalar::<_, bool>(
                r"SELECT COALESCE(
                       (SELECT wait_event_type = 'Lock'
                          FROM pg_stat_activity
                         WHERE pid = $1),
                       FALSE
                   )",
            )
            .bind(backend_pid)
            .fetch_one(observer)
            .await
            .expect("inspect identity-race backend wait state");
            if waiting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{operation} did not reach its workspace lock wait"));
}

async fn identity_owner(pool: &PgPool, key: &ExternalIdentityKey) -> Option<ParticipantId> {
    sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT participant_id FROM sso_identities WHERE issuer = $1 AND subject = $2",
    )
    .bind(&key.issuer)
    .bind(&key.subject)
    .fetch_optional(pool)
    .await
    .expect("load identity")
    .map(ParticipantId::from_uuid)
}

async fn replace_source_with_tombstone(
    fixture: &Fixture,
    former_participant: ParticipantId,
    reason: &str,
) {
    sqlx::query("DELETE FROM sso_identities WHERE issuer = $1 AND subject = $2")
        .bind(&fixture.source.issuer)
        .bind(&fixture.source.subject)
        .execute(&fixture.pool)
        .await
        .expect("remove source identity");
    sqlx::query(
        r"INSERT INTO sso_identity_tombstones
              (issuer, subject, former_participant_id, reason)
           VALUES ($1, $2, $3, $4)",
    )
    .bind(&fixture.source.issuer)
    .bind(&fixture.source.subject)
    .bind(former_participant.to_uuid())
    .bind(reason)
    .execute(&fixture.pool)
    .await
    .expect("insert source tombstone");
}

async fn migration_audit_count(fixture: &Fixture) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r"SELECT COUNT(*)
            FROM audit_events
           WHERE workspace_id = $1 AND action = $2 AND target = $3",
    )
    .bind(fixture.workspace.to_uuid())
    .bind(IDENTITY_MIGRATED_AUDIT_ACTION)
    .bind(fixture.participant.to_string())
    .fetch_one(&fixture.pool)
    .await
    .expect("count identity migration audits")
}

#[tokio::test]
#[ignore = "requires a freshly migrated PostgreSQL database"]
async fn identity_migration_adds_self_alias_and_audits_without_subjects() {
    let fixture = Fixture::create().await;
    let target = fixture.target("alias");
    let outcome = fixture
        .repo
        .migrate(fixture.request(fixture.participant, target.clone(), false))
        .await
        .expect("add self-service alias");
    assert_eq!(outcome.participant_id, fixture.participant);
    assert!(outcome.target_created);
    assert!(!outcome.source_retired);
    assert_eq!(outcome.scim_rows_updated, 0);
    assert_eq!(
        identity_owner(&fixture.pool, &fixture.source).await,
        Some(fixture.participant)
    );
    assert_eq!(
        identity_owner(&fixture.pool, &target).await,
        Some(fixture.participant)
    );

    let repeated = fixture
        .repo
        .migrate(fixture.request(fixture.participant, target.clone(), false))
        .await
        .expect("same-participant target is idempotent");
    assert!(!repeated.target_created);

    let audit = sqlx::query(
        r"SELECT target, detail::text AS detail
            FROM audit_events
           WHERE workspace_id = $1 AND action = $2 AND target = $3
           ORDER BY created_at DESC
           LIMIT 1",
    )
    .bind(fixture.workspace.to_uuid())
    .bind(IDENTITY_MIGRATED_AUDIT_ACTION)
    .bind(fixture.participant.to_string())
    .fetch_one(&fixture.pool)
    .await
    .expect("load migration audit");
    let audit_target: String = audit.get("target");
    let audit_detail: String = audit.get("detail");
    assert_eq!(audit_target, fixture.participant.to_string());
    assert!(!audit_detail.contains(&fixture.source.subject));
    assert!(!audit_detail.contains(&target.subject));
}

#[tokio::test]
#[ignore = "requires a freshly migrated PostgreSQL database"]
async fn identity_migration_retires_source_tombstone_and_updates_only_matching_scim_alias() {
    let fixture = Fixture::create().await;
    let target = fixture.target("retire");
    sqlx::query(
        r"INSERT INTO scim_users
              (workspace_id, participant_id, user_name, external_id, active)
           VALUES ($1, $2, $3, $4, true)",
    )
    .bind(fixture.workspace.to_uuid())
    .bind(fixture.participant.to_uuid())
    .bind(format!("identity-scim-{}", fixture.participant))
    .bind(&fixture.source.subject)
    .execute(&fixture.pool)
    .await
    .expect("insert SCIM mapping");

    let outcome = fixture
        .repo
        .migrate(fixture.request(fixture.participant, target.clone(), true))
        .await
        .expect("retire source identity");
    assert!(outcome.source_retired);
    assert!(outcome.target_created);
    assert_eq!(outcome.scim_rows_updated, 1);
    assert_eq!(identity_owner(&fixture.pool, &fixture.source).await, None);
    assert_eq!(
        identity_owner(&fixture.pool, &target).await,
        Some(fixture.participant)
    );

    let tombstone = sqlx::query_as::<_, (uuid::Uuid, String)>(
        r"SELECT former_participant_id, reason
            FROM sso_identity_tombstones
           WHERE issuer = $1 AND subject = $2",
    )
    .bind(&fixture.source.issuer)
    .bind(&fixture.source.subject)
    .fetch_one(&fixture.pool)
    .await
    .expect("load source tombstone");
    assert_eq!(tombstone.0, fixture.participant.to_uuid());
    assert_eq!(tombstone.1, IDENTITY_MIGRATED_REASON);
    let external_id = sqlx::query_scalar::<_, Option<String>>(
        "SELECT external_id FROM scim_users WHERE workspace_id = $1 AND participant_id = $2",
    )
    .bind(fixture.workspace.to_uuid())
    .bind(fixture.participant.to_uuid())
    .fetch_one(&fixture.pool)
    .await
    .expect("load SCIM external id");
    assert_eq!(external_id.as_deref(), Some(target.subject.as_str()));

    assert_eq!(migration_audit_count(&fixture).await, 1);
    let replay = fixture
        .repo
        .migrate(fixture.request(fixture.participant, target, true))
        .await
        .expect("network retry of completed retire is idempotent");
    assert_eq!(
        replay,
        IdentityMigrationOutcome {
            participant_id: fixture.participant,
            target_created: false,
            source_retired: true,
            scim_rows_updated: 0,
        }
    );
    assert_eq!(
        migration_audit_count(&fixture).await,
        1,
        "completed replay must not duplicate the original audit"
    );
}

#[tokio::test]
#[ignore = "requires a freshly migrated PostgreSQL database"]
async fn identity_migration_retire_replay_rejects_forged_or_incomplete_state() {
    let erased = Fixture::create().await;
    let erased_target = erased.target("erased-replay");
    SsoRepo::new(erased.pool.clone())
        .link(
            &erased_target.issuer,
            &erased_target.subject,
            erased.participant,
            None,
        )
        .await
        .expect("link erased-state target");
    replace_source_with_tombstone(&erased, erased.participant, "account_erased").await;
    assert!(matches!(
        erased
            .repo
            .migrate(erased.request(erased.participant, erased_target, true))
            .await,
        Err(Error::Conflict(_))
    ));

    let wrong_former = Fixture::create().await;
    let wrong_former_target = wrong_former.target("wrong-former-replay");
    SsoRepo::new(wrong_former.pool.clone())
        .link(
            &wrong_former_target.issuer,
            &wrong_former_target.subject,
            wrong_former.participant,
            None,
        )
        .await
        .expect("link wrong-former target");
    let other = insert_participant(&wrong_former.pool, "human", "identity-former-other").await;
    replace_source_with_tombstone(&wrong_former, other, IDENTITY_MIGRATED_REASON).await;
    assert!(matches!(
        wrong_former
            .repo
            .migrate(wrong_former.request(wrong_former.participant, wrong_former_target, true,))
            .await,
        Err(Error::Conflict(_))
    ));

    let missing_target = Fixture::create().await;
    let absent = missing_target.target("missing-replay");
    replace_source_with_tombstone(
        &missing_target,
        missing_target.participant,
        IDENTITY_MIGRATED_REASON,
    )
    .await;
    assert!(matches!(
        missing_target
            .repo
            .migrate(missing_target.request(missing_target.participant, absent, true))
            .await,
        Err(Error::Conflict(_))
    ));

    let wrong_target = Fixture::create().await;
    let wrong_target_key = wrong_target.target("wrong-target-replay");
    let wrong_owner =
        insert_participant(&wrong_target.pool, "human", "identity-target-other").await;
    SsoRepo::new(wrong_target.pool.clone())
        .link(
            &wrong_target_key.issuer,
            &wrong_target_key.subject,
            wrong_owner,
            None,
        )
        .await
        .expect("link target to another participant");
    replace_source_with_tombstone(
        &wrong_target,
        wrong_target.participant,
        IDENTITY_MIGRATED_REASON,
    )
    .await;
    assert!(matches!(
        wrong_target
            .repo
            .migrate(wrong_target.request(wrong_target.participant, wrong_target_key, true))
            .await,
        Err(Error::Conflict(_))
    ));

    assert_eq!(migration_audit_count(&erased).await, 0);
    assert_eq!(migration_audit_count(&wrong_former).await, 0);
    assert_eq!(migration_audit_count(&missing_target).await, 0);
    assert_eq!(migration_audit_count(&wrong_target).await, 0);
}

#[tokio::test]
#[ignore = "requires a freshly migrated PostgreSQL database"]
async fn identity_migration_retire_replay_rechecks_self_and_effective_membership() {
    let fixture = Fixture::create().await;
    let target = fixture.target("policy-replay");
    fixture
        .repo
        .migrate(fixture.request(fixture.participant, target.clone(), true))
        .await
        .expect("complete initial retire");

    assert!(matches!(
        fixture
            .repo
            .migrate(fixture.request(fixture.admin, target.clone(), true))
            .await,
        Err(Error::Forbidden(_))
    ));
    DeactivationRepo::new(fixture.pool.clone())
        .deactivate(fixture.workspace, fixture.participant, fixture.owner)
        .await
        .expect("deactivate migration subject");
    assert!(matches!(
        fixture
            .repo
            .migrate(fixture.request(fixture.participant, target.clone(), true))
            .await,
        Err(Error::Forbidden(_))
    ));
    DeactivationRepo::new(fixture.pool.clone())
        .reactivate(fixture.workspace, fixture.participant)
        .await
        .expect("reactivate migration subject");

    sqlx::query("DELETE FROM workspace_members WHERE workspace_id = $1 AND participant_id = $2")
        .bind(fixture.workspace.to_uuid())
        .bind(fixture.participant.to_uuid())
        .execute(&fixture.pool)
        .await
        .expect("remove migration subject membership");
    assert!(matches!(
        fixture
            .repo
            .migrate(fixture.request(fixture.participant, target, true))
            .await,
        Err(Error::Forbidden(_))
    ));

    let deleted = Fixture::create().await;
    let deleted_target = deleted.target("deleted-replay");
    deleted
        .repo
        .migrate(deleted.request(deleted.participant, deleted_target.clone(), true))
        .await
        .expect("complete retire before deletion marker");
    sqlx::query("UPDATE participants SET deleted_at = NOW() WHERE id = $1")
        .bind(deleted.participant.to_uuid())
        .execute(&deleted.pool)
        .await
        .expect("mark participant deleted");
    assert!(matches!(
        deleted
            .repo
            .migrate(deleted.request(deleted.participant, deleted_target, true))
            .await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
#[ignore = "requires a freshly migrated PostgreSQL database"]
async fn identity_migration_rejects_target_owned_by_another_participant_without_merging() {
    let fixture = Fixture::create().await;
    let other = insert_participant(&fixture.pool, "human", "identity-other").await;
    let target = fixture.target("conflict");
    SsoRepo::new(fixture.pool.clone())
        .link(&target.issuer, &target.subject, other, None)
        .await
        .expect("link conflicting target");

    let error = fixture
        .repo
        .migrate(fixture.request(fixture.participant, target.clone(), false))
        .await
        .expect_err("a target owned by another participant must conflict");
    assert!(matches!(error, Error::Conflict(_)));
    assert_eq!(identity_owner(&fixture.pool, &target).await, Some(other));
    assert_eq!(
        identity_owner(&fixture.pool, &fixture.source).await,
        Some(fixture.participant)
    );
}

#[tokio::test]
#[ignore = "requires a freshly migrated PostgreSQL database"]
async fn identity_migration_rejects_tombstoned_target() {
    let fixture = Fixture::create().await;
    let target = fixture.target("tombstoned");
    sqlx::query(
        r"INSERT INTO sso_identity_tombstones
              (issuer, subject, former_participant_id, reason)
           VALUES ($1, $2, $3, 'account_erased')",
    )
    .bind(&target.issuer)
    .bind(&target.subject)
    .bind(fixture.participant.to_uuid())
    .execute(&fixture.pool)
    .await
    .expect("insert target tombstone");

    let error = fixture
        .repo
        .migrate(fixture.request(fixture.participant, target.clone(), false))
        .await
        .expect_err("tombstoned target must remain unavailable");
    assert!(matches!(error, Error::Conflict(_)));
    assert_eq!(identity_owner(&fixture.pool, &target).await, None);
    assert_eq!(
        identity_owner(&fixture.pool, &fixture.source).await,
        Some(fixture.participant)
    );
}

#[tokio::test]
#[ignore = "requires a freshly migrated PostgreSQL database"]
async fn identity_migration_rejects_admin_binding_another_participant() {
    let fixture = Fixture::create().await;
    let target = fixture.target("admin-takeover");
    let error = fixture
        .repo
        .migrate(fixture.request(fixture.admin, target.clone(), false))
        .await
        .expect_err("an administrator cannot migrate another global identity");
    assert!(matches!(error, Error::Forbidden(_)));
    assert_eq!(identity_owner(&fixture.pool, &target).await, None);
}

#[tokio::test]
#[ignore = "requires a freshly migrated PostgreSQL database"]
async fn identity_migration_concurrent_alias_requests_are_idempotent() {
    let fixture = Fixture::create().await;
    let target = fixture.target("concurrent");
    let request = fixture.request(fixture.participant, target.clone(), false);
    let (first, second) = tokio::join!(
        fixture.repo.migrate(request.clone()),
        fixture.repo.migrate(request),
    );
    let first = first.expect("first concurrent migration");
    let second = second.expect("second concurrent migration");
    assert_ne!(first.target_created, second.target_created);
    assert_eq!(first.participant_id, fixture.participant);
    assert_eq!(second.participant_id, fixture.participant);

    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM sso_identities WHERE issuer = $1 AND subject = $2",
    )
    .bind(&target.issuer)
    .bind(&target.subject)
    .fetch_one(&fixture.pool)
    .await
    .expect("count target identities");
    assert_eq!(count, 1);
    assert_eq!(
        identity_owner(&fixture.pool, &target).await,
        Some(fixture.participant)
    );
}

#[tokio::test]
#[ignore = "requires a freshly migrated PostgreSQL database"]
async fn identity_migration_requires_live_human_workspace_member() {
    let fixture = Fixture::create().await;

    let nonmember = insert_participant(&fixture.pool, "human", "identity-nonmember").await;
    let nonmember_source = fixture.target("nonmember-source");
    SsoRepo::new(fixture.pool.clone())
        .link(
            &nonmember_source.issuer,
            &nonmember_source.subject,
            nonmember,
            None,
        )
        .await
        .expect("link nonmember source");
    let nonmember_error = fixture
        .repo
        .migrate(IdentityMigrationRequest {
            workspace_id: fixture.workspace,
            actor_id: nonmember,
            participant_id: nonmember,
            from: nonmember_source,
            to: fixture.target("nonmember-target"),
            retire_source: false,
        })
        .await
        .expect_err("nonmember migration must fail");
    assert!(matches!(nonmember_error, Error::Forbidden(_)));

    let bot = insert_participant(&fixture.pool, "bot", "identity-bot").await;
    WorkspaceRepo::new(fixture.pool.clone())
        .add_member(fixture.workspace, bot, WorkspaceRole::Member)
        .await
        .expect("add bot membership");
    let bot_source = fixture.target("bot-source");
    SsoRepo::new(fixture.pool.clone())
        .link(&bot_source.issuer, &bot_source.subject, bot, None)
        .await
        .expect("link bot source");
    let bot_error = fixture
        .repo
        .migrate(IdentityMigrationRequest {
            workspace_id: fixture.workspace,
            actor_id: bot,
            participant_id: bot,
            from: bot_source,
            to: fixture.target("bot-target"),
            retire_source: false,
        })
        .await
        .expect_err("non-human migration must fail");
    assert!(matches!(bot_error, Error::NotFound(_)));
}

#[tokio::test]
#[ignore = "requires a freshly migrated PostgreSQL database"]
async fn participant_erasure_shares_workspace_identity_order_with_migration_and_jit() {
    // Queue migration ahead of erasure on the same workspace. Erasure's global
    // governance fence and explicit aggregate locks must both precede its sorted
    // identity advisory locks, so releasing the blocker cannot form a
    // workspace <-> identity cycle.
    let fixture = Fixture::create().await;
    let target = fixture.target("erasure-race");
    let request = fixture.request(fixture.participant, target.clone(), false);
    let (migration_pool, migration_pid) = single_connection_pool().await;
    let (erasure_pool, erasure_pid) = single_connection_pool().await;
    let mut workspace_blocker = fixture.pool.begin().await.expect("begin workspace blocker");
    sqlx::query("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(fixture.workspace.to_uuid())
        .execute(&mut *workspace_blocker)
        .await
        .expect("lock migration workspace");

    let migration = tokio::spawn(async move {
        IdentityMigrationRepo::new(migration_pool)
            .migrate(request)
            .await
    });
    wait_for_lock_wait(&fixture.pool, migration_pid, "identity migration").await;
    let participant = fixture.participant;
    let erasure = tokio::spawn(async move {
        ParticipantRepo::new(erasure_pool)
            .delete_participant(participant)
            .await
    });
    wait_for_lock_wait(&fixture.pool, erasure_pid, "participant erasure").await;
    workspace_blocker
        .rollback()
        .await
        .expect("release migration workspace blocker");

    let (migration, erasure) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(migration, erasure)
    })
    .await
    .expect("erasure and identity migration must not deadlock");
    let migration = migration
        .expect("identity migration task")
        .expect("identity migration result");
    assert_eq!(migration.participant_id, fixture.participant);
    assert!(migration.target_created);
    assert!(erasure
        .expect("participant erasure task")
        .expect("participant erasure result"));
    assert_eq!(identity_owner(&fixture.pool, &fixture.source).await, None);
    assert_eq!(identity_owner(&fixture.pool, &target).await, None);

    // Repeat the choreography with an existing-identity OIDC login. The login
    // enters workspace -> identity -> participant, and erasure must remain in
    // the same aggregate order while retaining its cross-workspace fence.
    let fixture = Fixture::create().await;
    let (jit_pool, jit_pid) = single_connection_pool().await;
    let (erasure_pool, erasure_pid) = single_connection_pool().await;
    let mut workspace_blocker = fixture
        .pool
        .begin()
        .await
        .expect("begin JIT workspace blocker");
    sqlx::query("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(fixture.workspace.to_uuid())
        .execute(&mut *workspace_blocker)
        .await
        .expect("lock JIT workspace");

    let issuer = fixture.source.issuer.clone();
    let subject = fixture.source.subject.clone();
    let workspace = fixture.workspace;
    let jit = tokio::spawn(async move {
        SsoRepo::new(jit_pool)
            .resolve_or_provision_human(
                &issuer,
                &subject,
                "existing identity during erasure race",
                Some("race@example.test"),
                workspace,
            )
            .await
    });
    wait_for_lock_wait(&fixture.pool, jit_pid, "OIDC JIT").await;
    let participant = fixture.participant;
    let erasure = tokio::spawn(async move {
        ParticipantRepo::new(erasure_pool)
            .delete_participant(participant)
            .await
    });
    wait_for_lock_wait(&fixture.pool, erasure_pid, "participant erasure").await;
    workspace_blocker
        .rollback()
        .await
        .expect("release JIT workspace blocker");

    let (jit, erasure) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(jit, erasure)
    })
    .await
    .expect("erasure and OIDC JIT must not deadlock");
    assert_eq!(
        jit.expect("OIDC JIT task").expect("OIDC JIT result"),
        fixture.participant
    );
    assert!(erasure
        .expect("participant erasure task")
        .expect("participant erasure result"));
    assert_eq!(identity_owner(&fixture.pool, &fixture.source).await, None);
}
