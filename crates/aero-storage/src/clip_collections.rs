//! Clip collections / playlists — per-user named, ordered groups of stream
//! clips (YouTube-playlist-style).
//!
//! Backs `migrations/0097_clip_collections.sql`. A participant creates a named
//! collection (`clip_collections`) and then adds their clips to it with a
//! `position` ordering so the UI can present a playlist
//! (`clip_collection_items`). Removing a clip from `stream_clips` automatically
//! removes it from every collection it belongs to (ON DELETE CASCADE).
//!
//! Authorization is the caller's job: `delete` takes the `creator_id` it was
//! given, so a caller can only delete their own collections. Clip-level auth
//! (only the clip's creator can add/remove it) is left to the server handler.

use aero_common::{ClipCollectionId, ClipId, ParticipantId};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// One clip collection — a named, ordered group of stream clips.
///
/// A storage-layer projection of a `clip_collections` row. `Serialize` so a
/// handler can hand the row straight back as JSON; `created_at` is RFC 3339.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct CollectionRow {
    /// The collection's unique id (UUID-backed).
    pub id: uuid::Uuid,
    /// The participant who created the collection.
    pub creator_id: uuid::Uuid,
    /// Human-readable title.
    pub title: String,
    /// Optional longer description.
    pub description: Option<String>,
    /// When the collection was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// Repository over the `clip_collections` + `clip_collection_items` tables.
///
/// Cheap to clone — wraps a [`PgPool`] (an `Arc` internally), so feature
/// modules build one inline via [`ClipCollectionRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct ClipCollectionRepo {
    pub pg: PgPool,
}

impl ClipCollectionRepo {
    /// Build a repo over the given pool.
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Create a new collection owned by `creator`, returning its generated id.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn create(
        &self,
        creator: ParticipantId,
        title: &str,
        description: Option<&str>,
    ) -> Result<ClipCollectionId, sqlx::Error> {
        let row: (uuid::Uuid,) = sqlx::query_as(
            r"INSERT INTO clip_collections (creator_id, title, description)
               VALUES ($1, $2, $3)
               RETURNING id",
        )
        .bind(creator.to_uuid())
        .bind(title)
        .bind(description)
        .fetch_one(&self.pg)
        .await?;
        Ok(ClipCollectionId::from_uuid(row.0))
    }

    /// Fetch one collection by id, or `None` if it does not exist.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(&self, id: ClipCollectionId) -> Result<Option<CollectionRow>, sqlx::Error> {
        sqlx::query_as::<_, CollectionRow>(
            r"SELECT id, creator_id, title, description, created_at
                FROM clip_collections
               WHERE id = $1",
        )
        .bind(id.to_uuid())
        .fetch_optional(&self.pg)
        .await
    }

    /// All collections owned by `creator`, newest first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_creator(
        &self,
        creator: ParticipantId,
    ) -> Result<Vec<CollectionRow>, sqlx::Error> {
        sqlx::query_as::<_, CollectionRow>(
            r"SELECT id, creator_id, title, description, created_at
                FROM clip_collections
               WHERE creator_id = $1
               ORDER BY created_at DESC, id DESC",
        )
        .bind(creator.to_uuid())
        .fetch_all(&self.pg)
        .await
    }

    /// Add a clip to a collection at the given position. If the
    /// (collection, clip) pair already exists this is a no-op.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn add_clip(
        &self,
        collection: ClipCollectionId,
        clip: ClipId,
        position: i32,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO clip_collection_items (collection_id, clip_id, position)
               VALUES ($1, $2, $3)
               ON CONFLICT (collection_id, clip_id) DO UPDATE SET position = EXCLUDED.position",
        )
        .bind(collection.to_uuid())
        .bind(clip.to_uuid())
        .bind(position)
        .execute(&self.pg)
        .await?;
        Ok(())
    }

    /// Remove a clip from a collection. Returns `true` iff the item was
    /// present and removed; a second call (or a wrong collection/clip pair)
    /// is a no-op returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn remove_clip(
        &self,
        collection: ClipCollectionId,
        clip: ClipId,
    ) -> Result<bool, sqlx::Error> {
        let r = sqlx::query(
            r"DELETE FROM clip_collection_items
               WHERE collection_id = $1 AND clip_id = $2",
        )
        .bind(collection.to_uuid())
        .bind(clip.to_uuid())
        .execute(&self.pg)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Ordered list of clip ids in a collection (ascending by position, then
    /// by `added_at`).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_clips(
        &self,
        collection: ClipCollectionId,
    ) -> Result<Vec<ClipId>, sqlx::Error> {
        let rows: Vec<(uuid::Uuid,)> = sqlx::query_as(
            r"SELECT clip_id
                FROM clip_collection_items
               WHERE collection_id = $1
               ORDER BY position ASC, added_at ASC",
        )
        .bind(collection.to_uuid())
        .fetch_all(&self.pg)
        .await?;
        Ok(rows.into_iter().map(|(u,)| ClipId::from_uuid(u)).collect())
    }

    /// Delete a collection by id, scoped to `creator`. Returns `true` iff a
    /// row was removed — `false` when the id is unknown or belongs to a
    /// different creator. Items are cascade-deleted by the FK.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn delete(
        &self,
        id: ClipCollectionId,
        creator: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let r = sqlx::query(
            r"DELETE FROM clip_collections WHERE id = $1 AND creator_id = $2",
        )
        .bind(id.to_uuid())
        .bind(creator.to_uuid())
        .execute(&self.pg)
        .await?;
        Ok(r.rows_affected() > 0)
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn make_participant(pg: &PgPool, label: &str) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query(
            "INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)",
        )
        .bind(id.to_uuid())
        .bind(format!("{label}-{id}"))
        .execute(pg)
        .await
        .expect("insert participant");
        id
    }

    async fn make_clip(pg: &PgPool, creator: ParticipantId) -> ClipId {
        let id = ClipId::new();
        let stream = uuid::Uuid::new_v4();
        sqlx::query(
            "INSERT INTO stream_clips (id, stream_id, creator_id, title, start_secs, end_secs) \
             VALUES ($1, $2, $3, 'test clip', 0, 10)",
        )
        .bind(id.to_uuid())
        .bind(stream)
        .bind(creator.to_uuid())
        .execute(pg)
        .await
        .expect("insert clip");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn clip_collection_create_get_list_add_remove_delete() {
        let pg = pool();
        let repo = ClipCollectionRepo::new(pg.clone());
        let creator = make_participant(&pg, "coll-creator").await;
        let stranger = make_participant(&pg, "coll-stranger").await;
        let clip1 = make_clip(&pg, creator).await;
        let clip2 = make_clip(&pg, creator).await;

        // create → get + list
        let cid = repo
            .create(creator, "My Highlights", Some("best clips"))
            .await
            .unwrap();
        let row = repo.get(cid).await.unwrap().expect("collection present");
        assert_eq!(row.title, "My Highlights");
        assert_eq!(row.description.as_deref(), Some("best clips"));

        let listed = repo.list_for_creator(creator).await.unwrap();
        assert!(listed.iter().any(|r| r.id == cid.to_uuid()));

        // add_clip → list_clips shows them ordered
        repo.add_clip(cid, clip1, 0).await.unwrap();
        repo.add_clip(cid, clip2, 1).await.unwrap();
        let clips = repo.list_clips(cid).await.unwrap();
        assert_eq!(clips, vec![clip1, clip2]);

        // remove_clip
        assert!(repo.remove_clip(cid, clip1).await.unwrap());
        assert!(!repo.remove_clip(cid, clip1).await.unwrap(), "second remove is no-op");
        let clips = repo.list_clips(cid).await.unwrap();
        assert_eq!(clips, vec![clip2]);

        // delete (scoped to creator)
        assert!(!repo.delete(cid, stranger).await.unwrap(), "stranger cannot delete");
        assert!(repo.delete(cid, creator).await.unwrap());
        assert!(repo.get(cid).await.unwrap().is_none(), "gone after delete");

        // cleanup
        for c in [clip1, clip2] {
            sqlx::query("DELETE FROM stream_clips WHERE id = $1")
                .bind(c.to_uuid())
                .execute(&pg)
                .await
                .ok();
        }
    }
}
