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

use aero_common::{BlobId, EmojiId, ParticipantId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;

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
    /// The member who registered it (`None` if the creator was later removed,
    /// since `created_by` is `ON DELETE SET NULL`-free but nullable).
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

fn row_to_emoji(
    (id, ws, name, blob, by, at): EmojiRow,
) -> CustomEmoji {
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

    /// Register a custom emoji for a workspace. Returns the new id, or `None`
    /// when the `(workspace_id, name)` pair already exists — relying on the
    /// UNIQUE constraint (via `ON CONFLICT DO NOTHING`) so a duplicate is a
    /// clean `None` rather than a database error the caller must classify.
    pub async fn create(
        &self,
        workspace: WorkspaceId,
        name: &str,
        blob_id: BlobId,
        created_by: ParticipantId,
    ) -> Result<Option<EmojiId>, sqlx::Error> {
        let id = EmojiId::new();
        let created_at = time::OffsetDateTime::now_utc();
        let row = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"INSERT INTO custom_emoji (id, workspace_id, name, blob_id, created_by, created_at)
               VALUES ($1, $2, $3, $4, $5, $6)
               ON CONFLICT (workspace_id, name) DO NOTHING
               RETURNING id",
        )
        .bind(id.to_uuid())
        .bind(workspace.to_uuid())
        .bind(name)
        .bind(blob_id.to_uuid())
        .bind(created_by.to_uuid())
        .bind(created_at)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(uid,)| EmojiId::from_uuid(uid)))
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

    /// Fetch a single emoji by id (used for the delete authorization check —
    /// the caller must be its creator or a workspace admin).
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

    /// Delete an emoji by id. Returns `true` if a row was removed (`false` when
    /// it was already gone — e.g. a concurrent delete).
    pub async fn delete(&self, id: EmojiId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(r"DELETE FROM custom_emoji WHERE id = $1")
            .bind(id.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
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
    use aero_common::{BlobId, ParticipantId, WorkspaceId};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    // Create a throwaway participant, workspace, and image blob so each test is
    // self-contained (custom_emoji.blob_id REFERENCES blobs(id)).
    async fn fixture(p: &PgPool) -> (WorkspaceId, ParticipantId, BlobId) {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(actor.to_uuid())
            .bind(format!("emoji-actor-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");
        let ws = WorkspaceId::new();
        sqlx::query("INSERT INTO workspaces (id, name, slug, created_by, created_at) VALUES ($1,$2,$3,$4, now())")
            .bind(ws.to_uuid())
            .bind("Emoji Test WS")
            .bind(format!("emoji-{ws}"))
            .bind(actor.to_uuid())
            .execute(p)
            .await
            .expect("insert workspace");
        let blob = BlobId::new();
        sqlx::query(
            r"INSERT INTO blobs (id, owner_id, kind, name, mime, size, storage_key, created_at, finalized_at)
               VALUES ($1, $2, 'image', $3, 'image/png', 12, $4, now(), now())",
        )
        .bind(blob.to_uuid())
        .bind(actor.to_uuid())
        .bind(format!("emoji-{blob}.png"))
        .bind(format!("pending:{blob}"))
        .execute(p)
        .await
        .expect("insert blob");
        (ws, actor, blob)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn emoji_create_list_get_delete_roundtrip() {
        let p = pool();
        let repo = EmojiRepo::new(p.clone());
        let (ws, actor, blob) = fixture(&p).await;

        let id = repo
            .create(ws, "shipit", blob, actor)
            .await
            .unwrap()
            .expect("first create returns an id");

        let list = repo.list(ws).await.unwrap();
        assert_eq!(list.len(), 1, "exactly one emoji listed");
        assert_eq!(list[0].name, "shipit");
        assert_eq!(list[0].blob_id, blob);
        assert_eq!(list[0].created_by, Some(actor));

        let by_name = repo.get_by_name(ws, "shipit").await.unwrap();
        assert!(by_name.is_some(), "get_by_name resolves the registered name");
        assert_eq!(by_name.unwrap().id, id);

        assert!(repo.get(id).await.unwrap().is_some(), "get by id");
        assert!(repo.delete(id).await.unwrap(), "delete removes the row");
        assert!(!repo.delete(id).await.unwrap(), "second delete is a no-op");
        assert!(repo.get_by_name(ws, "shipit").await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn emoji_duplicate_name_in_same_workspace_is_rejected() {
        let p = pool();
        let repo = EmojiRepo::new(p.clone());
        let (ws, actor, blob) = fixture(&p).await;

        let first = repo.create(ws, "dup", blob, actor).await.unwrap();
        assert!(first.is_some(), "first registration succeeds");
        let second = repo.create(ws, "dup", blob, actor).await.unwrap();
        assert!(second.is_none(), "duplicate name in same workspace yields None");
        // And only one row actually exists.
        assert_eq!(repo.list(ws).await.unwrap().len(), 1);
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn emoji_same_name_in_two_workspaces_both_allowed() {
        let p = pool();
        let repo = EmojiRepo::new(p.clone());
        let (ws_a, actor_a, blob_a) = fixture(&p).await;
        let (ws_b, actor_b, blob_b) = fixture(&p).await;

        let a = repo.create(ws_a, "wave", blob_a, actor_a).await.unwrap();
        let b = repo.create(ws_b, "wave", blob_b, actor_b).await.unwrap();
        assert!(a.is_some() && b.is_some(), "same name in distinct workspaces both succeed");

        // Listing is tenant-scoped: each workspace sees only its own "wave".
        let list_a = repo.list(ws_a).await.unwrap();
        assert!(list_a.iter().all(|e| e.workspace_id == ws_a));
        assert_eq!(list_a.len(), 1);
        let list_b = repo.list(ws_b).await.unwrap();
        assert!(list_b.iter().all(|e| e.workspace_id == ws_b));
        assert_eq!(list_b.len(), 1);
    }
}
