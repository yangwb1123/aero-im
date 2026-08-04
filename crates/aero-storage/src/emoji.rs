//! Workspace custom-emoji repository.
//!
//! Backs `migrations/0021_custom_emoji.sql`. A workspace defines named custom
//! emoji (`:shipit:`) backed by an already-uploaded image blob; members
//! reference them by name in messages/reactions and the client resolves
//! name → image URL (the blob is served by the existing `GET /api/blobs/:id`).
//!
//! Purely additive: a NEW [`EmojiRepo`]; no existing repo is touched. The `id`
//! is a ULID stored as UUID. Everything is scoped by `workspace_id`, so one
//! tenant's emoji can never surface in another's listing, and a `(workspace_id,
//! name)` UNIQUE constraint enforces "one image per name per workspace" — the
//! same name may freely exist in two different workspaces.

use aero_common::{BlobId, EmojiId, Error, ParticipantId, WorkspaceId};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};

/// One workspace custom emoji, mirroring a `custom_emoji` row.
#[derive(Debug, Clone, Serialize)]
pub struct CustomEmoji {
    pub id: EmojiId,
    pub workspace_id: WorkspaceId,
    /// The short name referenced as `:name:` (validated by
    /// [`is_valid_emoji_name`] before insert).
    pub name: String,
    /// The already-uploaded image blob backing this emoji.
    pub blob_id: BlobId,
    /// The administrator who registered it (`None` for legacy rows).
    pub created_by: Option<ParticipantId>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// Largest number of emoji a single listing returns. A workspace's emoji set is
/// small by nature, but the cap keeps the response bounded.
const MAX_LIST: i64 = 1000;

/// Maximum length of an emoji name, in characters.
const MAX_NAME_LEN: usize = 64;

/// Whether `name` is a valid custom-emoji short name: 1..=64 characters of
/// lowercase ASCII letters, digits, `_`, or `-`. Pure, so it unit-tests without
/// a database and is reused by the HTTP layer to reject bad names before any
/// query runs. Matching the `:name:` convention, the surrounding colons are
/// **not** part of the stored name.
#[must_use]
pub fn is_valid_emoji_name(name: &str) -> bool {
    let len = name.chars().count();
    (1..=MAX_NAME_LEN).contains(&len)
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

#[derive(Clone)]
pub struct EmojiRepo {
    pool: PgPool,
}

/// Row shape shared by the `SELECT` queries below.
type EmojiRow = (
    uuid::Uuid,
    uuid::Uuid,
    String,
    uuid::Uuid,
    Option<uuid::Uuid>,
    time::OffsetDateTime,
);

fn row_to_emoji((id, ws, name, blob, by, at): EmojiRow) -> CustomEmoji {
    CustomEmoji {
        id: EmojiId::from_uuid(id),
        workspace_id: WorkspaceId::from_uuid(ws),
        name,
        blob_id: BlobId::from_uuid(blob),
        created_by: by.map(ParticipantId::from_uuid),
        created_at: at,
    }
}

impl EmojiRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Register a custom emoji while the caller remains an effective workspace
    /// Owner/Admin and the backing blob remains a usable image in that tenant.
    ///
    /// The workspace governance row is locked before the blob and insert. This
    /// makes a concurrent demotion, deactivation, mandatory-2FA change, or
    /// cleanup reservation visible before commit rather than trusting an HTTP
    /// preflight decision.
    ///
    /// # Errors
    /// Returns [`Error::Invalid`] for an invalid name, [`Error::Forbidden`]
    /// unless `actor` is a current effective Owner/Admin, [`Error::NotFound`]
    /// when the blob is missing, unfinished, queued for deletion, not an image,
    /// or belongs to another workspace, and [`Error::Conflict`] when the name
    /// already exists in this workspace.
    pub async fn create_authorized(
        &self,
        workspace: WorkspaceId,
        name: &str,
        blob_id: BlobId,
        actor: ParticipantId,
    ) -> Result<CustomEmoji, Error> {
        let name = name.trim();
        if !is_valid_emoji_name(name) {
            return Err(Error::Invalid(
                "emoji name must be 1-64 chars of lowercase [a-z0-9_-]".into(),
            ));
        }

        let mut tx = self.pool.begin().await?;
        crate::workspace::authz::assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        lock_usable_emoji_blob(&mut tx, workspace, blob_id.to_uuid()).await?;

        let id = EmojiId::new();
        let created_at = time::OffsetDateTime::now_utc();
        let row = sqlx::query_as::<_, EmojiRow>(
            r"INSERT INTO custom_emoji (id, workspace_id, name, blob_id, created_by, created_at)
               VALUES ($1, $2, $3, $4, $5, $6)
               ON CONFLICT (workspace_id, name) DO NOTHING
               RETURNING id, workspace_id, name, blob_id, created_by, created_at",
        )
        .bind(id.to_uuid())
        .bind(workspace.to_uuid())
        .bind(name)
        .bind(blob_id.to_uuid())
        .bind(actor.to_uuid())
        .bind(created_at)
        .fetch_optional(&mut *tx)
        .await?;
        let row = row.ok_or_else(|| {
            Error::Conflict(format!("emoji ':{name}:' already exists in this workspace"))
        })?;
        tx.commit().await?;
        Ok(row_to_emoji(row))
    }

    /// List a workspace's custom emoji, alphabetically by name. Always filtered
    /// to `workspace` — tenant-scoped.
    pub async fn list(&self, workspace: WorkspaceId) -> Result<Vec<CustomEmoji>, sqlx::Error> {
        let rows = sqlx::query_as::<_, EmojiRow>(
            r"SELECT id, workspace_id, name, blob_id, created_by, created_at
               FROM custom_emoji
               WHERE workspace_id = $1
               ORDER BY name ASC
               LIMIT $2",
        )
        .bind(workspace.to_uuid())
        .bind(MAX_LIST)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_emoji).collect())
    }

    /// Resolve a single emoji by its name within a workspace (the client's
    /// name → image lookup), or `None` if unknown.
    pub async fn get_by_name(
        &self,
        workspace: WorkspaceId,
        name: &str,
    ) -> Result<Option<CustomEmoji>, sqlx::Error> {
        let row = sqlx::query_as::<_, EmojiRow>(
            r"SELECT id, workspace_id, name, blob_id, created_by, created_at
               FROM custom_emoji
               WHERE workspace_id = $1 AND name = $2",
        )
        .bind(workspace.to_uuid())
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_emoji))
    }

    /// Fetch a single emoji by id.
    pub async fn get(&self, id: EmojiId) -> Result<Option<CustomEmoji>, sqlx::Error> {
        let row = sqlx::query_as::<_, EmojiRow>(
            r"SELECT id, workspace_id, name, blob_id, created_by, created_at
               FROM custom_emoji
               WHERE id = $1",
        )
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_to_emoji))
    }

    /// Delete an emoji under a transactionally current Owner/Admin decision.
    ///
    /// This legacy route identifies a row only by id. A caller with no effective
    /// access to the resolved workspace receives the same opaque not-found
    /// result as a missing id; an effective member without an administrative
    /// role receives forbidden. The workspace is locked before the resource.
    ///
    /// # Errors
    /// Returns [`Error::NotFound`] for a missing/cross-tenant row,
    /// [`Error::Forbidden`] unless an effective same-workspace caller is a
    /// current Owner/Admin, and propagates storage failures.
    pub async fn delete_authorized(&self, id: EmojiId, actor: ParticipantId) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        let resolved_workspace = sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT workspace_id FROM custom_emoji WHERE id = $1",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::NotFound("emoji".into()))?;
        let workspace = WorkspaceId::from_uuid(resolved_workspace);

        lock_workspace(&mut tx, workspace).await?;
        if !crate::workspace::members::effective_workspace_access_in_tx(&mut tx, workspace, actor)
            .await?
        {
            return Err(Error::NotFound("emoji".into()));
        }
        crate::workspace::authz::assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;

        let locked = sqlx::query_scalar::<_, bool>(
            "SELECT true
               FROM custom_emoji
              WHERE id = $1 AND workspace_id = $2
              FOR UPDATE",
        )
        .bind(id.to_uuid())
        .bind(workspace.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !locked {
            return Err(Error::NotFound("emoji".into()));
        }

        let result = sqlx::query(
            "DELETE FROM custom_emoji
              WHERE id = $1 AND workspace_id = $2",
        )
        .bind(id.to_uuid())
        .bind(workspace.to_uuid())
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Err(Error::NotFound("emoji".into()));
        }
        tx.commit().await?;
        Ok(())
    }
}

/// Lock and validate the existing blob availability contract for emoji writes.
///
/// Kept crate-visible so the UUID compatibility repository enforces exactly the
/// same boundary instead of drifting into a second definition.
pub(crate) async fn lock_usable_emoji_blob(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    blob_id: uuid::Uuid,
) -> Result<(), Error> {
    sqlx::query_scalar::<_, uuid::Uuid>("SELECT id FROM blobs WHERE id = $1 FOR UPDATE")
        .bind(blob_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| Error::NotFound("usable custom emoji image blob in workspace".into()))?;
    let available = sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT blob.id
           FROM blobs blob
          WHERE blob.id = $1
            AND blob.workspace_id = $2
            AND blob.kind = 'image'
            AND blob.finalized_at IS NOT NULL
            AND NOT EXISTS (
                SELECT 1
                  FROM blob_gc_queue queued
                 WHERE queued.blob_id = blob.id
            )",
    )
    .bind(blob_id)
    .bind(workspace.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .is_some();
    if available {
        Ok(())
    } else {
        Err(Error::NotFound(
            "usable custom emoji image blob in workspace".into(),
        ))
    }
}

async fn lock_workspace(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
) -> Result<(), Error> {
    let exists =
        sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .is_some();
    if exists {
        Ok(())
    } else {
        Err(Error::NotFound(format!("workspace {workspace}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names_are_lowercase_alnum_underscore_hyphen() {
        assert!(is_valid_emoji_name("shipit"));
        assert!(is_valid_emoji_name("party_parrot"));
        assert!(is_valid_emoji_name("thumbs-up"));
        assert!(is_valid_emoji_name("a1b2"));
        assert!(is_valid_emoji_name("x")); // single char
        assert!(is_valid_emoji_name(&"a".repeat(64))); // max length
    }

    #[test]
    fn invalid_names_are_rejected() {
        assert!(!is_valid_emoji_name("")); // empty
        assert!(!is_valid_emoji_name("ShipIt")); // uppercase
        assert!(!is_valid_emoji_name("party parrot")); // space
        assert!(!is_valid_emoji_name(":shipit:")); // colons are not part of the name
        assert!(!is_valid_emoji_name("emoji!")); // punctuation
        assert!(!is_valid_emoji_name("caf\u{e9}")); // non-ASCII
        assert!(!is_valid_emoji_name(&"a".repeat(65))); // too long
    }

    #[test]
    fn name_length_counts_characters_not_bytes() {
        // A 64-char ASCII name is the boundary; 65 is over.
        assert!(is_valid_emoji_name(&"z".repeat(64)));
        assert!(!is_valid_emoji_name(&"z".repeat(65)));
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored emoji_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use std::time::Duration;

    use aero_common::{BlobId, ParticipantId, WorkspaceId, WorkspaceRole};

    use crate::{BlobRepo, WorkspaceEmojiRepo, WorkspaceRepo};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn fixture(p: &PgPool) -> (WorkspaceId, ParticipantId, BlobId) {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(actor.to_uuid())
            .bind(format!("emoji-actor-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");
        let ws = WorkspaceId::new();
        let mut tx = p.begin().await.expect("begin workspace fixture");
        sqlx::query("INSERT INTO workspaces (id, name, slug, created_by, created_at) VALUES ($1,$2,$3,$4, now())")
            .bind(ws.to_uuid())
            .bind("Emoji Test WS")
            .bind(format!("emoji-{ws}"))
            .bind(actor.to_uuid())
            .execute(&mut *tx)
            .await
            .expect("insert workspace");
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1, $2, 'owner')",
        )
        .bind(ws.to_uuid())
        .bind(actor.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert workspace owner");
        tx.commit().await.expect("commit workspace fixture");
        let blob = BlobId::new();
        sqlx::query(
            r"INSERT INTO blobs
                 (id, owner_id, workspace_id, kind, name, mime, size, storage_key,
                  created_at, finalized_at)
               VALUES ($1, $2, $3, 'image', $4, 'image/png', 12, $5, now(), now())",
        )
        .bind(blob.to_uuid())
        .bind(actor.to_uuid())
        .bind(ws.to_uuid())
        .bind(format!("emoji-{blob}.png"))
        .bind(format!("pending:{blob}"))
        .execute(p)
        .await
        .expect("insert blob");
        (ws, actor, blob)
    }

    async fn participant(p: &PgPool, label: &str) -> ParticipantId {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(actor.to_uuid())
            .bind(format!("emoji-{label}-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");
        actor
    }

    async fn blob(
        p: &PgPool,
        owner: ParticipantId,
        workspace: WorkspaceId,
        label: &str,
        kind: &str,
        finalized: bool,
    ) -> BlobId {
        let id = BlobId::new();
        sqlx::query(
            "INSERT INTO blobs
                 (id, owner_id, workspace_id, kind, name, mime, size, storage_key,
                  finalized_at)
             VALUES ($1, $2, $3, $4, $5, $6, 16, $7,
                     CASE WHEN $8 THEN now() ELSE NULL END)",
        )
        .bind(id.to_uuid())
        .bind(owner.to_uuid())
        .bind(workspace.to_uuid())
        .bind(kind)
        .bind(format!("{label}-{id}"))
        .bind(if kind == "image" {
            "image/png"
        } else {
            "application/pdf"
        })
        .bind(format!("emoji-test:{id}"))
        .bind(finalized)
        .execute(p)
        .await
        .expect("insert scoped blob");
        id
    }

    fn constraint(error: &sqlx::Error) -> Option<&str> {
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn emoji_create_list_get_delete_roundtrip() {
        let p = pool();
        let repo = EmojiRepo::new(p.clone());
        let (ws, actor, blob) = fixture(&p).await;

        let created = repo
            .create_authorized(ws, "shipit", blob, actor)
            .await
            .unwrap();
        let id = created.id;

        let list = repo.list(ws).await.unwrap();
        assert_eq!(list.len(), 1, "exactly one emoji listed");
        assert_eq!(list[0].name, "shipit");
        assert_eq!(list[0].blob_id, blob);
        assert_eq!(list[0].created_by, Some(actor));

        let by_name = repo.get_by_name(ws, "shipit").await.unwrap();
        assert!(
            by_name.is_some(),
            "get_by_name resolves the registered name"
        );
        assert_eq!(by_name.unwrap().id, id);

        assert!(repo.get(id).await.unwrap().is_some(), "get by id");
        repo.delete_authorized(id, actor)
            .await
            .expect("delete removes the row");
        assert!(matches!(
            repo.delete_authorized(id, actor).await,
            Err(Error::NotFound(_))
        ));
        assert!(repo.get_by_name(ws, "shipit").await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn emoji_duplicate_name_in_same_workspace_is_rejected() {
        let p = pool();
        let repo = EmojiRepo::new(p.clone());
        let (ws, actor, blob) = fixture(&p).await;

        repo.create_authorized(ws, "dup", blob, actor)
            .await
            .expect("first registration succeeds");
        assert!(matches!(
            repo.create_authorized(ws, "dup", blob, actor).await,
            Err(Error::Conflict(_))
        ));
        assert_eq!(repo.list(ws).await.unwrap().len(), 1);
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn emoji_same_name_in_two_workspaces_both_allowed() {
        let p = pool();
        let repo = EmojiRepo::new(p.clone());
        let (ws_a, actor_a, blob_a) = fixture(&p).await;
        let (ws_b, actor_b, blob_b) = fixture(&p).await;

        repo.create_authorized(ws_a, "wave", blob_a, actor_a)
            .await
            .unwrap();
        repo.create_authorized(ws_b, "wave", blob_b, actor_b)
            .await
            .unwrap();

        let list_a = repo.list(ws_a).await.unwrap();
        assert!(list_a.iter().all(|e| e.workspace_id == ws_a));
        assert_eq!(list_a.len(), 1);
        let list_b = repo.list(ws_b).await.unwrap();
        assert!(list_b.iter().all(|e| e.workspace_id == ws_b));
        assert_eq!(list_b.len(), 1);
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migration 0204 applied"]
    async fn emoji_blob_scope_admin_rechecks_and_gc_lifecycle_are_enforced() {
        let p = pool();
        let repo = EmojiRepo::new(p.clone());
        let uuid_repo = WorkspaceEmojiRepo::new(p.clone());
        let workspaces = WorkspaceRepo::new(p.clone());

        let (workspace, owner, valid_blob) = fixture(&p).await;
        let (other_workspace, other_owner, foreign_blob) = fixture(&p).await;
        let admin = participant(&p, "admin").await;
        let member = participant(&p, "member").await;
        workspaces
            .add_member(workspace, admin, WorkspaceRole::Admin)
            .await
            .unwrap();
        workspaces
            .add_member(workspace, member, WorkspaceRole::Member)
            .await
            .unwrap();

        let pending_blob = blob(&p, owner, workspace, "pending", "image", false).await;
        let document_blob = blob(&p, owner, workspace, "document", "document", true).await;
        let queued_blob = blob(&p, owner, workspace, "queued", "image", true).await;
        sqlx::query(
            "INSERT INTO blob_gc_queue (blob_id, force_delete)
             VALUES ($1, FALSE)",
        )
        .bind(queued_blob.to_uuid())
        .execute(&p)
        .await
        .unwrap();

        assert!(matches!(
            repo.create_authorized(workspace, "member_denied", valid_blob, member)
                .await,
            Err(Error::Forbidden(_))
        ));
        for (name, unavailable) in [
            ("foreign", foreign_blob),
            ("pending", pending_blob),
            ("document", document_blob),
            ("queued", queued_blob),
        ] {
            assert!(matches!(
                repo.create_authorized(workspace, name, unavailable, admin)
                    .await,
                Err(Error::NotFound(_))
            ));
        }
        let rejected_count: i64 = sqlx::query_scalar(
            "SELECT count(*)
               FROM custom_emoji
              WHERE workspace_id = $1
                AND name IN ('member_denied', 'foreign', 'pending', 'document', 'queued')",
        )
        .bind(workspace.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(rejected_count, 0, "every rejected create is a zero-write");
        assert!(
            !BlobRepo::new(p.clone())
                .is_accessible_by(valid_blob, member)
                .await
                .unwrap(),
            "a bare workspace blob is not readable by another member"
        );

        for (table, expected_constraint) in [
            ("custom_emoji", "custom_emoji_blob_workspace_containment"),
            (
                "workspace_emoji",
                "workspace_emoji_blob_workspace_containment",
            ),
        ] {
            for (suffix, unavailable) in [("cross", foreign_blob), ("pending", pending_blob)] {
                let id = uuid::Uuid::new_v4();
                let sql = format!(
                    "INSERT INTO {table}
                         (id, workspace_id, name, blob_id, created_by)
                     VALUES ($1, $2, $3, $4, $5)"
                );
                let error = sqlx::query(&sql)
                    .bind(id)
                    .bind(workspace.to_uuid())
                    .bind(format!("raw_{suffix}_{id}"))
                    .bind(unavailable.to_uuid())
                    .bind(admin.to_uuid())
                    .execute(&p)
                    .await
                    .expect_err("database trigger rejects bypassed blob containment");
                assert_eq!(constraint(&error), Some(expected_constraint));
            }
        }

        let custom = repo
            .create_authorized(workspace, "valid_custom", valid_blob, admin)
            .await
            .unwrap();
        let uuid_custom = uuid_repo
            .create_authorized(workspace, "valid_uuid", valid_blob.to_uuid(), admin)
            .await
            .unwrap();
        assert!(
            BlobRepo::new(p.clone())
                .has_live_references(valid_blob)
                .await
                .unwrap(),
            "both emoji tables keep an ordinary GC cleanup from deleting bytes"
        );
        assert!(
            BlobRepo::new(p.clone())
                .is_accessible_by(valid_blob, member)
                .await
                .unwrap(),
            "workspace members can fetch a registered emoji image"
        );
        assert!(
            !BlobRepo::new(p.clone())
                .is_accessible_by(valid_blob, other_owner)
                .await
                .unwrap(),
            "emoji download access never crosses workspace boundaries"
        );

        let raw_update = sqlx::query("UPDATE custom_emoji SET blob_id = $2 WHERE id = $1")
            .bind(custom.id.to_uuid())
            .bind(foreign_blob.to_uuid())
            .execute(&p)
            .await
            .expect_err("raw update cannot move an emoji to a foreign blob");
        assert_eq!(
            constraint(&raw_update),
            Some("custom_emoji_blob_workspace_containment")
        );

        assert!(matches!(
            repo.delete_authorized(custom.id, member).await,
            Err(Error::Forbidden(_))
        ));
        assert!(matches!(
            repo.delete_authorized(custom.id, other_owner).await,
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            uuid_repo
                .delete_authorized(uuid_custom.id, other_workspace, other_owner)
                .await,
            Err(Error::NotFound(_))
        ));

        let mut demotion = p.begin().await.unwrap();
        crate::ownership::lock_membership_governance(&mut demotion)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .execute(&mut *demotion)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE workspace_members
                SET role = 'member'
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(admin.to_uuid())
        .execute(&mut *demotion)
        .await
        .unwrap();

        let raced_repo = repo.clone();
        let mut raced_create = tokio::spawn(async move {
            raced_repo
                .create_authorized(workspace, "raced_create", valid_blob, admin)
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut raced_create)
                .await
                .is_err(),
            "emoji create must wait behind the workspace revocation lock"
        );
        demotion.commit().await.unwrap();
        assert!(matches!(
            raced_create.await.unwrap(),
            Err(Error::Forbidden(_))
        ));
        let raced_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM custom_emoji
              WHERE workspace_id = $1 AND name = 'raced_create'",
        )
        .bind(workspace.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(raced_count, 0);

        workspaces
            .change_member_role_authorized(workspace, owner, admin, WorkspaceRole::Admin)
            .await
            .unwrap();
        let raced_delete_target = repo
            .create_authorized(workspace, "raced_delete", valid_blob, admin)
            .await
            .unwrap();

        let mut second_demotion = p.begin().await.unwrap();
        crate::ownership::lock_membership_governance(&mut second_demotion)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .execute(&mut *second_demotion)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE workspace_members
                SET role = 'member'
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(admin.to_uuid())
        .execute(&mut *second_demotion)
        .await
        .unwrap();

        let raced_repo = repo.clone();
        let mut raced_delete = tokio::spawn(async move {
            raced_repo
                .delete_authorized(raced_delete_target.id, admin)
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut raced_delete)
                .await
                .is_err(),
            "emoji delete must wait behind the workspace revocation lock"
        );
        second_demotion.commit().await.unwrap();
        assert!(matches!(
            raced_delete.await.unwrap(),
            Err(Error::Forbidden(_))
        ));
        assert!(
            repo.get(raced_delete_target.id).await.unwrap().is_some(),
            "revoked delete leaves the emoji intact"
        );

        sqlx::query("DELETE FROM blobs WHERE id = $1")
            .bind(valid_blob.to_uuid())
            .execute(&p)
            .await
            .unwrap();
        assert!(repo.get(custom.id).await.unwrap().is_none());
        assert!(uuid_repo.get(uuid_custom.id).await.unwrap().is_none());
    }
}
