//! Stream categories & discovery repository (Twitch-style browse-by-category).
//!
//! Backs `migrations/0050_stream_categories.sql`. Lets a live stream be filed
//! under at most one slug-addressable [`StreamCategory`] (Gaming, Music, …) and
//! tagged with free-form tags, then browsed by category. Purely additive: the
//! existing `streams` table is never touched — three disjoint association tables
//! key off a stream's uuid id:
//!
//! * `stream_categories` — the small catalog (seeded by the migration).
//! * `stream_category_assignments` — at most ONE category per stream
//!   (`stream_id` PRIMARY KEY, so [`assign`](StreamCategoryRepo::assign) upserts).
//! * `stream_tags` — many tags per stream (composite PK dedupes).
//!
//! Discovery ([`streams_in_category`](StreamCategoryRepo::streams_in_category))
//! joins assignments to `streams` and filters on the live predicate
//! (`streams.status = 'live'`, matching [`StreamRepo`](crate::StreamRepo)),
//! returning the live stream ids — the caller re-hydrates each via `StreamRepo`.

use aero_common::StreamCategoryId;
use serde::Serialize;
use sqlx::PgPool;

/// One discovery category — a slug-addressable bucket a stream can be filed under.
///
/// A storage-layer projection of a `stream_categories` row. `Serialize` so a
/// handler can hand the row straight back as JSON.
#[derive(Debug, Clone, Serialize)]
pub struct StreamCategory {
    /// The category's unique id.
    pub id: StreamCategoryId,
    /// URL-friendly handle (e.g. `gaming`), unique across the catalog.
    pub slug: String,
    /// Human-readable display name (e.g. `Gaming`).
    pub name: String,
    /// Ordering hint for listing (lower sorts first).
    pub sort: i32,
}

/// The columns a [`StreamCategory`] is built from, in select order.
const COLUMNS: &str = "id, slug, name, sort";

type CategoryRow = (uuid::Uuid, String, String, i32);

fn row_to_category(r: CategoryRow) -> StreamCategory {
    let (id, slug, name, sort) = r;
    StreamCategory {
        id: StreamCategoryId::from_uuid(id),
        slug,
        name,
        sort,
    }
}

/// Repository over the stream-discovery tables (categories, assignments, tags).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`StreamCategoryRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct StreamCategoryRepo {
    pool: PgPool,
}

impl StreamCategoryRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// List the whole category catalog, ordered by `sort` then `name`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_categories(&self) -> Result<Vec<StreamCategory>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM stream_categories ORDER BY sort ASC, name ASC");
        let rows = sqlx::query_as::<_, CategoryRow>(&sql)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_category).collect())
    }

    /// Resolve a category by its `slug`, or `None` if no such category exists.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn category_by_slug(
        &self,
        slug: &str,
    ) -> Result<Option<StreamCategory>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM stream_categories WHERE slug = $1");
        let row = sqlx::query_as::<_, CategoryRow>(&sql)
            .bind(slug)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_category))
    }

    /// File `stream` under `category`, replacing any existing assignment (a stream
    /// belongs to at most one category — the `stream_id` PRIMARY KEY enforces it,
    /// so this upserts via `ON CONFLICT (stream_id) DO UPDATE`).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert.
    pub async fn assign(
        &self,
        stream: uuid::Uuid,
        category: StreamCategoryId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO stream_category_assignments (stream_id, category_id)
               VALUES ($1, $2)
               ON CONFLICT (stream_id) DO UPDATE SET category_id = EXCLUDED.category_id",
        )
        .bind(stream)
        .bind(category.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Remove `stream`'s category assignment (idempotent — a no-op if unassigned).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn clear_assignment(&self, stream: uuid::Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM stream_category_assignments WHERE stream_id = $1")
            .bind(stream)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The category `stream` is filed under, or `None` if unassigned.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn category_of(
        &self,
        stream: uuid::Uuid,
    ) -> Result<Option<StreamCategoryId>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid,)>(
            "SELECT category_id FROM stream_category_assignments WHERE stream_id = $1",
        )
        .bind(stream)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id,)| StreamCategoryId::from_uuid(id)))
    }

    /// Add `tag` to `stream` (idempotent — the composite PK dedupes a re-add).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn add_tag(&self, stream: uuid::Uuid, tag: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO stream_tags (stream_id, tag) VALUES ($1, $2)
               ON CONFLICT (stream_id, tag) DO NOTHING",
        )
        .bind(stream)
        .bind(tag)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Remove `tag` from `stream`. Returns `true` iff a row was removed.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn remove_tag(&self, stream: uuid::Uuid, tag: &str) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM stream_tags WHERE stream_id = $1 AND tag = $2")
            .bind(stream)
            .bind(tag)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// List `stream`'s tags, alphabetically.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn tags_for(&self, stream: uuid::Uuid) -> Result<Vec<String>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (String,)>(
            "SELECT tag FROM stream_tags WHERE stream_id = $1 ORDER BY tag ASC",
        )
        .bind(stream)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(t,)| t).collect())
    }

    /// The ids of the **live** streams filed under `category`, newest first,
    /// capped at `limit`. Joins `stream_category_assignments` to `streams` and
    /// reuses the live predicate (`status = 'live'`) so only currently-live
    /// streams surface; the caller re-reads each via
    /// [`StreamRepo`](crate::StreamRepo).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn streams_in_category(
        &self,
        category_id: StreamCategoryId,
        limit: i64,
    ) -> Result<Vec<uuid::Uuid>, sqlx::Error> {
        let limit = limit.clamp(1, 200);
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT s.id
                FROM stream_category_assignments a
                JOIN streams s ON s.id = a.stream_id
               WHERE a.category_id = $1 AND s.status = 'live'
               ORDER BY s.started_at DESC NULLS LAST, s.id DESC
               LIMIT $2",
        )
        .bind(category_id.to_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored stream_category
/// ```
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

    /// Insert a throwaway live stream owned by a fresh participant, returning its
    /// uuid id. Self-contained so the test cleans up after itself.
    async fn live_stream(p: &PgPool) -> uuid::Uuid {
        let owner = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(owner)
            .bind(format!("stream-cat-owner-{owner}"))
            .execute(p)
            .await
            .expect("insert participant");
        let stream = uuid::Uuid::new_v4();
        sqlx::query(
            r"INSERT INTO streams (id, owner_id, title, stream_key, status, protocol)
               VALUES ($1, $2, 'test', $3, 'live', 'rtmp')",
        )
        .bind(stream)
        .bind(owner)
        .bind(format!("key-{stream}"))
        .execute(p)
        .await
        .expect("insert stream");
        stream
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn categories_are_seeded_and_slug_addressable() {
        let repo = StreamCategoryRepo::new(pool());
        let all = repo.list_categories().await.unwrap();
        assert!(
            all.iter().any(|c| c.slug == "gaming"),
            "the gaming seed is present"
        );
        let gaming = repo
            .category_by_slug("gaming")
            .await
            .unwrap()
            .expect("gaming resolves");
        assert_eq!(gaming.name, "Gaming");
        assert!(
            repo.category_by_slug("does-not-exist").await.unwrap().is_none(),
            "unknown slug resolves to None"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn assign_upserts_clears_and_discovery_is_live_only() {
        let p = pool();
        let repo = StreamCategoryRepo::new(p.clone());
        let stream = live_stream(&p).await;
        let gaming = repo.category_by_slug("gaming").await.unwrap().expect("gaming");
        let music = repo.category_by_slug("music").await.unwrap().expect("music");

        // assign → category_of reflects it; appears in discovery.
        repo.assign(stream, gaming.id).await.unwrap();
        assert_eq!(repo.category_of(stream).await.unwrap(), Some(gaming.id));
        assert!(
            repo.streams_in_category(gaming.id, 50).await.unwrap().contains(&stream),
            "the live stream shows up in its category"
        );

        // re-assign upserts (one category per stream) — moves to music.
        repo.assign(stream, music.id).await.unwrap();
        assert_eq!(repo.category_of(stream).await.unwrap(), Some(music.id));
        assert!(
            !repo.streams_in_category(gaming.id, 50).await.unwrap().contains(&stream),
            "no longer in the old category"
        );

        // ended streams drop out of discovery.
        sqlx::query("UPDATE streams SET status = 'ended' WHERE id = $1")
            .bind(stream)
            .execute(&p)
            .await
            .unwrap();
        assert!(
            !repo.streams_in_category(music.id, 50).await.unwrap().contains(&stream),
            "an ended stream is not discoverable"
        );

        // clear is idempotent.
        repo.clear_assignment(stream).await.unwrap();
        assert_eq!(repo.category_of(stream).await.unwrap(), None);
        repo.clear_assignment(stream).await.unwrap();

        sqlx::query("DELETE FROM streams WHERE id = $1").bind(stream).execute(&p).await.ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn tags_add_dedupe_list_remove() {
        let p = pool();
        let repo = StreamCategoryRepo::new(p.clone());
        let stream = live_stream(&p).await;

        repo.add_tag(stream, "speedrun").await.unwrap();
        repo.add_tag(stream, "speedrun").await.unwrap(); // dedupes
        repo.add_tag(stream, "fps").await.unwrap();
        let tags = repo.tags_for(stream).await.unwrap();
        assert_eq!(tags, vec!["fps".to_string(), "speedrun".to_string()]);

        assert!(repo.remove_tag(stream, "fps").await.unwrap());
        assert!(!repo.remove_tag(stream, "fps").await.unwrap(), "second remove is a no-op");
        assert_eq!(repo.tags_for(stream).await.unwrap(), vec!["speedrun".to_string()]);

        sqlx::query("DELETE FROM stream_tags WHERE stream_id = $1").bind(stream).execute(&p).await.ok();
        sqlx::query("DELETE FROM streams WHERE id = $1").bind(stream).execute(&p).await.ok();
    }
}
