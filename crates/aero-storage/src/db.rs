//! Postgres pool and migrations.
//!
//! Resilience improvements (roadmap 9th analysis 方向二):
//! - `min_connections(2)` — warm start, no cold-start acquire latency
//! - `test_before_acquire(true)` — verify connection health before handing it out
//! - `after_connect` callback — sets `statement_timeout = '10s'` on each connection
//!   to prevent a single slow query from occupying the slot indefinitely

use std::time::Duration;

use sqlx::postgres::PgPoolOptions;

pub type PgPool = sqlx::PgPool;

/// Default timeout for acquiring a connection from the pool.
const POOL_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

/// Idle timeout: a connection sitting unused for this long is closed and the
/// slot released back to Postgres.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// Minimum idle connections kept warm. Avoids cold-start acquire latency on
/// the first request after a period of low traffic.
const POOL_MIN_CONNECTIONS: u32 = 2;

/// Per-query statement timeout set on every connection via `after_connect`.
/// Prevents a single slow query from occupying a pool slot for minutes.
/// 10 seconds covers the vast majority of normal queries; the search path
/// (which may run slower trigram scans) currently has its own code-level
/// guard (`MAX_SEARCH_LIMIT`) so it stays within this window.
const STATEMENT_TIMEOUT_SQL: &str = "SET statement_timeout = '10000'";

pub async fn connect_pg(url: &str, max_conns: u32) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_conns)
        .min_connections(POOL_MIN_CONNECTIONS)
        .acquire_timeout(POOL_ACQUIRE_TIMEOUT)
        .idle_timeout(POOL_IDLE_TIMEOUT)
        .test_before_acquire(true)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::query(STATEMENT_TIMEOUT_SQL).execute(conn).await?;
                Ok(())
            })
        })
        .connect(url)
        .await
}

/// Runs migrations embedded from the workspace `migrations/` directory.
///
/// Adding a migration requires rebuilding the executable before invoking this
/// function; an already-built binary cannot discover a new SQL file at runtime.
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("../../migrations").run(pool).await
}

#[cfg(test)]
mod migration_tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use aero_common::{
        BlobId, CanvasId, MessageId, ParticipantId, RoomId, ScheduledMessageId, WorkspaceId,
    };
    use sqlx::{migrate::Migrator, postgres::PgPoolOptions, PgPool};
    use uuid::Uuid;

    struct TemporaryMigrationDirectory(PathBuf);

    impl Drop for TemporaryMigrationDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn migration_source() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations")
    }

    fn migrations_through(max_version: u16) -> TemporaryMigrationDirectory {
        let target = std::env::temp_dir().join(format!(
            "aero-im-migrations-through-{max_version:04}-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir(&target).expect("create temporary migration directory");

        for entry in fs::read_dir(migration_source()).expect("read migration source") {
            let entry = entry.expect("read migration entry");
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(version) = file_name
                .split_once('_')
                .and_then(|(prefix, _)| prefix.parse::<u16>().ok())
            else {
                continue;
            };
            if path.extension().and_then(|extension| extension.to_str()) == Some("sql")
                && version <= max_version
            {
                fs::copy(&path, target.join(file_name)).expect("copy immutable migration");
            }
        }

        TemporaryMigrationDirectory(target)
    }

    async fn applied_version(pool: &PgPool) -> i64 {
        sqlx::query_scalar(
            "SELECT COALESCE(MAX(version), 0)
               FROM _sqlx_migrations
              WHERE success",
        )
        .fetch_one(pool)
        .await
        .expect("read migration ledger")
    }

    #[tokio::test]
    #[ignore = "requires a brand-new, disposable Postgres database"]
    async fn rolling_upgrade_fences_are_atomic_before_0176_reasserts_them() {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must select a disposable database");
        let pool = PgPoolOptions::new()
            .max_connections(5)
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
            "upgrade regression refuses to mutate a non-empty database"
        );

        let through_0171_directory = migrations_through(171);
        Migrator::new(through_0171_directory.0.as_path())
            .await
            .expect("load migrations through 0171")
            .run(&pool)
            .await
            .expect("migrate disposable database through 0171");
        assert_eq!(applied_version(&pool).await, 171);

        let participant = ParticipantId::new();
        let pre_tombstoned_participant = ParticipantId::new();
        let workspace_successor = ParticipantId::new();
        let workspace_a = WorkspaceId::new();
        let workspace_b = WorkspaceId::new();
        let room_b = RoomId::new();
        let legacy_blob = BlobId::new();
        let polluted_message = aero_common::MessageId::new();

        sqlx::query(
            "INSERT INTO participants (id, kind, display_name)
             VALUES ($1, 'human', 'rolling upgrade')",
        )
        .bind(participant.to_uuid())
        .execute(&pool)
        .await
        .expect("insert participant");
        sqlx::query(
            "INSERT INTO participants (id, kind, display_name, deleted_at)
             VALUES ($1, 'human', '[deleted]', now())",
        )
        .bind(pre_tombstoned_participant.to_uuid())
        .execute(&pool)
        .await
        .expect("insert participant erased before the scoped-profile migration");
        sqlx::query(
            "INSERT INTO participants (id, kind, display_name)
             VALUES ($1, 'human', 'rolling upgrade workspace successor')",
        )
        .bind(workspace_successor.to_uuid())
        .execute(&pool)
        .await
        .expect("insert retained effective workspace successor");
        for (workspace, suffix) in [(workspace_a, "a"), (workspace_b, "b")] {
            sqlx::query(
                "INSERT INTO workspaces (id, name, slug, created_by)
                 VALUES ($1, $2, $3, $4)",
            )
            .bind(workspace.to_uuid())
            .bind(format!("Upgrade {suffix}"))
            .bind(format!("upgrade-{suffix}-{workspace}"))
            .bind(participant.to_uuid())
            .execute(&pool)
            .await
            .expect("insert workspace");
            // Model a recoverable pre-0200 tenant: the creator is already a
            // retained ordinary member, but an old writer left no owner edge.
            // Migration 0200 may promote this existing reader; it must not
            // invent membership for an ownerless workspace with no members.
            sqlx::query(
                "INSERT INTO workspace_members
                     (workspace_id, participant_id, role)
                 VALUES ($1, $2, 'member')",
            )
            .bind(workspace.to_uuid())
            .bind(participant.to_uuid())
            .execute(&pool)
            .await
            .expect("insert recoverable legacy workspace membership");
            sqlx::query(
                "INSERT INTO workspace_members
                     (workspace_id, participant_id, role)
                 VALUES ($1, $2, 'member')",
            )
            .bind(workspace.to_uuid())
            .bind(workspace_successor.to_uuid())
            .execute(&pool)
            .await
            .expect("insert existing effective successor membership");
        }
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id)
             VALUES ($1, 'group', 'polluted', $2, $3)",
        )
        .bind(room_b.to_uuid())
        .bind(participant.to_uuid())
        .bind(workspace_b.to_uuid())
        .execute(&pool)
        .await
        .expect("insert room");
        sqlx::query(
            "INSERT INTO blobs
                 (id, owner_id, kind, name, mime, size, storage_key, finalized_at)
             VALUES ($1, $2, 'document', 'legacy.txt', 'text/plain', 1, $3, now())",
        )
        .bind(legacy_blob.to_uuid())
        .bind(participant.to_uuid())
        .bind(format!("upgrade/{legacy_blob}"))
        .execute(&pool)
        .await
        .expect("insert pre-scope blob");
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(polluted_message.to_uuid())
        .bind(room_b.to_uuid())
        .bind(participant.to_uuid())
        .bind(serde_json::json!([{
            "type": "file",
            "blob_id": legacy_blob.to_string(),
            "kind": "document",
            "name": "legacy.txt",
            "size": 1
        }]))
        .execute(&pool)
        .await
        .expect("0171 permits the historical unscoped reference");
        let copied: i64 =
            sqlx::query_scalar("SELECT rows_copied FROM backfill_messages_partition(100, $1)")
                .bind(Uuid::nil())
                .fetch_one(&pool)
                .await
                .expect("backfill polluted row into partition shadow");
        assert_eq!(copied, 1);

        sqlx::query(
            "INSERT INTO participant_ai_profiles
                 (participant_id, workspace_id, summary)
             VALUES ($1, $2, 'tenant A profile')",
        )
        .bind(participant.to_uuid())
        .bind(workspace_a.to_uuid())
        .execute(&pool)
        .await
        .expect("insert pre-0173 AI profile");
        sqlx::query(
            "INSERT INTO participant_ai_profiles
                 (participant_id, workspace_id, summary)
             VALUES ($1, $2, 'must be erased during migration')",
        )
        .bind(pre_tombstoned_participant.to_uuid())
        .bind(workspace_a.to_uuid())
        .execute(&pool)
        .await
        .expect("insert legacy profile for an already-tombstoned participant");

        let canvas = CanvasId::new();
        sqlx::query(
            "INSERT INTO channel_canvases (id, room_id, author_id, title)
             VALUES ($1, $2, $3, 'rolling canvas')",
        )
        .bind(canvas.to_uuid())
        .bind(room_b.to_uuid())
        .bind(participant.to_uuid())
        .execute(&pool)
        .await
        .expect("insert pre-0175 canvas");

        let through_0175_directory = migrations_through(175);
        let mut through_0175 = Migrator::new(through_0175_directory.0.as_path())
            .await
            .expect("load migrations through 0175");
        // SQLx's session advisory lock is intentionally released only on a
        // successful run. This isolated database deliberately exercises two
        // migration failures in one process, so rely on 0172's table locks and
        // avoid retaining a failed connection's advisory lock between retries.
        through_0175.set_locking(false);
        let live_pollution_error = through_0175
            .run(&pool)
            .await
            .expect_err("0172 must reject pollution in canonical messages");
        assert!(
            live_pollution_error
                .to_string()
                .contains("blob workspace audit"),
            "migration failure names the attachment audit: {live_pollution_error}"
        );
        assert_eq!(applied_version(&pool).await, 171);

        sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(polluted_message.to_uuid())
            .execute(&pool)
            .await
            .expect("remove canonical polluted row");
        let shadow_pollution_error = through_0175
            .run(&pool)
            .await
            .expect_err("0172 must also reject pollution in the partition shadow");
        assert!(
            shadow_pollution_error
                .to_string()
                .contains("messages_partitioned"),
            "migration failure identifies the polluted shadow: {shadow_pollution_error}"
        );
        assert_eq!(applied_version(&pool).await, 171);

        sqlx::query("DELETE FROM messages_partitioned WHERE id = $1")
            .bind(polluted_message.to_uuid())
            .execute(&pool)
            .await
            .expect("remove shadow polluted row");
        through_0175
            .run(&pool)
            .await
            .expect("clean 0171 database upgrades through 0175");
        assert_eq!(applied_version(&pool).await, 175);

        let guarded_parents: i64 = sqlx::query_scalar(
            "SELECT count(*)
               FROM pg_trigger
              WHERE tgname = 'messages_enforce_blob_workspace_scope'
                AND tgrelid IN (
                    'messages'::regclass,
                    'messages_partitioned'::regclass
                )
                AND NOT tgisinternal",
        )
        .fetch_one(&pool)
        .await
        .expect("inspect blob-scope triggers");
        assert_eq!(
            guarded_parents, 2,
            "both live and partition-cutover candidate are guarded"
        );

        let scoped_blob = BlobId::new();
        sqlx::query(
            "INSERT INTO blobs
                 (id, owner_id, workspace_id, kind, name, mime, size, storage_key, finalized_at)
             VALUES ($1, $2, $3, 'document', 'scoped.txt', 'text/plain', 1, $4, now())",
        )
        .bind(scoped_blob.to_uuid())
        .bind(participant.to_uuid())
        .bind(workspace_a.to_uuid())
        .bind(format!("upgrade/{scoped_blob}"))
        .execute(&pool)
        .await
        .expect("insert scoped blob after 0172");
        let old_cross_workspace_write = sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(aero_common::MessageId::new().to_uuid())
        .bind(room_b.to_uuid())
        .bind(participant.to_uuid())
        .bind(serde_json::json!([{
            "type": "file",
            "blob_id": scoped_blob.to_string(),
            "kind": "document",
            "name": "scoped.txt",
            "size": 1
        }]))
        .execute(&pool)
        .await
        .expect_err("0172 trigger rejects a previous binary's cross-workspace write");
        assert_eq!(
            old_cross_workspace_write
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .as_deref(),
            Some("23514")
        );

        let legacy_profile_rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM participant_ai_profiles")
                .fetch_one(&pool)
                .await
                .expect("old AI reader is valid but empty");
        let scoped_profile_rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM participant_ai_profiles_scoped")
                .fetch_one(&pool)
                .await
                .expect("new AI table preserves tenant profile");
        assert_eq!(legacy_profile_rows, 0);
        assert_eq!(scoped_profile_rows, 1);
        let pre_tombstoned_profile_rows: i64 = sqlx::query_scalar(
            "SELECT count(*)
               FROM participant_ai_profiles_scoped
              WHERE participant_id = $1",
        )
        .bind(pre_tombstoned_participant.to_uuid())
        .fetch_one(&pool)
        .await
        .expect("inspect already-tombstoned subject cleanup");
        assert_eq!(
            pre_tombstoned_profile_rows, 0,
            "0173 removes profiles belonging to subjects erased before migration"
        );
        let old_upsert_error = sqlx::query(
            "INSERT INTO participant_ai_profiles
                 (participant_id, workspace_id, summary)
             VALUES ($1, $2, 'unsafe old write')
             ON CONFLICT (participant_id)
             DO UPDATE SET summary = EXCLUDED.summary",
        )
        .bind(participant.to_uuid())
        .bind(workspace_a.to_uuid())
        .execute(&pool)
        .await
        .expect_err("old AI single-key upsert must fail closed");
        assert!(
            old_upsert_error.as_database_error().is_some(),
            "old AI write returns an observable PostgreSQL error"
        );

        let canvas_op = Uuid::new_v4();
        let client_op_id: Uuid = sqlx::query_scalar(
            "INSERT INTO canvas_ops (id, canvas_id, seq, author_id, op)
             VALUES ($1, $2, 1, $3, '{\"type\":\"insert\"}'::jsonb)
             RETURNING client_op_id",
        )
        .bind(canvas_op)
        .bind(canvas.to_uuid())
        .bind(participant.to_uuid())
        .fetch_one(&pool)
        .await
        .expect("0175 accepts an old canvas INSERT");
        assert_eq!(
            client_op_id, canvas_op,
            "legacy canvas operation gets a stable fallback retry id"
        );

        sqlx::query("UPDATE participants SET deleted_at = now() WHERE id = $1")
            .bind(participant.to_uuid())
            .execute(&pool)
            .await
            .expect("old erasure path tombstones participant");
        let profiles_after_tombstone: i64 = sqlx::query_scalar(
            "SELECT count(*)
               FROM participant_ai_profiles_scoped
              WHERE participant_id = $1",
        )
        .bind(participant.to_uuid())
        .fetch_one(&pool)
        .await
        .expect("inspect scoped erasure");
        assert_eq!(
            profiles_after_tombstone, 0,
            "0173 trigger keeps old tombstone-based GDPR erasure safe"
        );

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
            .expect("current chain preserves the 0172/0173/0175 rollout fences");
        assert_eq!(applied_version(&pool).await, latest_version);

        let retained_memberships: i64 = sqlx::query_scalar(
            "SELECT count(*)
               FROM workspace_members
              WHERE workspace_id IN ($1, $2)",
        )
        .bind(workspace_a.to_uuid())
        .bind(workspace_b.to_uuid())
        .fetch_one(&pool)
        .await
        .expect("inspect repaired legacy workspace memberships");
        assert_eq!(
            retained_memberships, 4,
            "0200 repairs existing readers without inventing tenant membership"
        );
        let repaired_owners: i64 = sqlx::query_scalar(
            "SELECT count(*)
               FROM workspace_members membership
               JOIN participants participant
                 ON participant.id = membership.participant_id
                AND participant.deleted_at IS NULL
              WHERE membership.workspace_id IN ($1, $2)
                AND membership.participant_id = $3
                AND membership.role = 'owner'
                AND NOT membership.is_guest",
        )
        .bind(workspace_a.to_uuid())
        .bind(workspace_b.to_uuid())
        .bind(workspace_successor.to_uuid())
        .fetch_one(&pool)
        .await
        .expect("inspect repaired legacy workspace owners");
        assert_eq!(
            repaired_owners, 2,
            "0227 deterministically promotes the retained effective successor"
        );
        let tombstoned_owner_edges: i64 = sqlx::query_scalar(
            "SELECT count(*)
               FROM workspace_members
              WHERE workspace_id IN ($1, $2)
                AND participant_id = $3
                AND role = 'owner'",
        )
        .bind(workspace_a.to_uuid())
        .bind(workspace_b.to_uuid())
        .bind(participant.to_uuid())
        .fetch_one(&pool)
        .await
        .expect("inspect demoted tombstoned owner edges");
        assert_eq!(
            tombstoned_owner_edges, 0,
            "0227 must not retain a tombstoned participant as a raw owner"
        );
    }

    #[tokio::test]
    #[ignore = "requires a brand-new, disposable Postgres database"]
    async fn migration_0192_repairs_attempted_cross_room_scheduled_replies() {
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
            "upgrade regression refuses to mutate a non-empty database"
        );

        let through_0191_directory = migrations_through(191);
        Migrator::new(through_0191_directory.0.as_path())
            .await
            .expect("load migrations through 0191")
            .run(&pool)
            .await
            .expect("migrate disposable database through 0191");
        assert_eq!(applied_version(&pool).await, 191);

        let actor = ParticipantId::new();
        let workspace = WorkspaceId::new();
        let parent_room = RoomId::new();
        let scheduled_room = RoomId::new();
        let parent = MessageId::new();
        let scheduled = ScheduledMessageId::new();
        sqlx::query(
            "INSERT INTO participants (id, kind, display_name)
             VALUES ($1, 'human', '0192 rolling upgrade')",
        )
        .bind(actor.to_uuid())
        .execute(&pool)
        .await
        .expect("participant");
        sqlx::query(
            "INSERT INTO workspaces (id, name, slug, created_by)
             VALUES ($1, '0192 upgrade', $2, $3)",
        )
        .bind(workspace.to_uuid())
        .bind(format!("migration-0192-{workspace}"))
        .bind(actor.to_uuid())
        .execute(&pool)
        .await
        .expect("workspace");
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1, $2, 'member')",
        )
        .bind(workspace.to_uuid())
        .bind(actor.to_uuid())
        .execute(&pool)
        .await
        .expect("recoverable legacy workspace membership");
        for room in [parent_room, scheduled_room] {
            sqlx::query(
                "INSERT INTO rooms (id, kind, created_by, workspace_id)
                 VALUES ($1, 'group', $2, $3)",
            )
            .bind(room.to_uuid())
            .bind(actor.to_uuid())
            .bind(workspace.to_uuid())
            .execute(&pool)
            .await
            .expect("room");
        }
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks)
             VALUES ($1, $2, $3, '[{\"type\":\"text\",\"content\":\"root\"}]'::jsonb)",
        )
        .bind(parent.to_uuid())
        .bind(parent_room.to_uuid())
        .bind(actor.to_uuid())
        .execute(&pool)
        .await
        .expect("parent message");
        sqlx::query(
            "INSERT INTO scheduled_messages
                 (id, room_id, sender_id, blocks, reply_to, scheduled_at,
                  available_at, attempts)
             VALUES (
                 $1, $2, $3, '[{\"type\":\"text\",\"content\":\"reply\"}]'::jsonb,
                 $4, now() + interval '1 hour', now() + interval '1 hour', 1
             )",
        )
        .bind(scheduled.to_uuid())
        .bind(scheduled_room.to_uuid())
        .bind(actor.to_uuid())
        .bind(parent.to_uuid())
        .execute(&pool)
        .await
        .expect("pre-0192 attempted cross-room scheduled reply");

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
            .expect("0192 cleans an attempted historical row without weakening the fence");
        assert_eq!(applied_version(&pool).await, latest_version);
        let repaired: (Option<Uuid>, i32) = sqlx::query_as(
            "SELECT reply_to, attempts
               FROM scheduled_messages
              WHERE id = $1",
        )
        .bind(scheduled.to_uuid())
        .fetch_one(&pool)
        .await
        .expect("repaired scheduled row");
        assert_eq!(
            repaired,
            (None, 1),
            "migration clears only the invalid parent edge and preserves attempts"
        );
    }

    mod bot_membership;
    mod integration_user_issuer;
}
