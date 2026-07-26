//! Live-stream clip repository (viewer-marked shareable highlight ranges).
//!
//! Backs `migrations/0057_clips.sql`. A viewer marks a timestamped
//! `[start_secs, end_secs]` range of a live stream / VOD to share; playback reuses
//! the stream's existing HLS playlist with a client-side seek to that range — no
//! media is processed here, so a [`Clip`] is just metadata pointing at a stream
//! plus an in/out point. This mirrors the [`VodRepo`](crate::VodRepo) pattern (a
//! stream-referencing record whose `stream_id` is a plain column, not a cascading
//! FK, so a clip outlives a pruned stream).
//!
//! Purely additive: a NEW [`ClipRepo`]; no existing repo is touched. The [`Clip`]
//! model lives here (and is re-exported from the crate root) rather than in
//! `aero-common`, since it is a storage-layer projection — like
//! [`SavedSearch`](crate::SavedSearch). Range validation (`0 <= start < end` and a
//! max duration) is the SERVER handler's job; this repo persists what it is given.
//! [`ClipRepo::delete`] is creator-scoped, so a caller can only ever remove their
//! own clips.

use aero_common::{ClipId, ParticipantId};
use serde::Serialize;
use sqlx::PgPool;

/// One live-stream clip — a viewer-marked `[start_secs, end_secs]` range of a
/// stream / VOD, shared for playback over the stream's existing HLS playlist.
///
/// A storage-layer projection of a `stream_clips` row. `Serialize` so a handler
/// can hand the row straight back as JSON; `created_at` renders as RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct Clip {
    /// The clip's unique id.
    pub id: ClipId,
    /// The stream / VOD this clip is taken from (a plain reference, not a hard FK —
    /// a clip may outlive its stream row).
    pub stream_id: ulid::Ulid,
    /// The participant who created the clip.
    pub creator_id: ParticipantId,
    /// Human-readable title the creator gave the clip.
    pub title: String,
    /// Inclusive start offset into the stream, in whole seconds.
    pub start_secs: i32,
    /// Exclusive end offset into the stream, in whole seconds (`> start_secs`).
    pub end_secs: i32,
    /// When the clip was created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    /// Optional short slug for the shareable public URL (e.g. `/clips/{slug}`).
    /// `None` until the creator calls the share endpoint.
    pub share_slug: Option<String>,
    /// Number of times this clip has been fetched via the public slug route.
    pub view_count: i64,
}

/// The columns a [`Clip`] is built from, in select order. Shared by every query so
/// the row decoding stays in one place.
const COLUMNS: &str =
    "id, stream_id, creator_id, title, start_secs, end_secs, created_at, share_slug, view_count";

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: uuid::Uuid,
    stream_id: uuid::Uuid,
    creator_id: uuid::Uuid,
    title: String,
    start_secs: i32,
    end_secs: i32,
    created_at: time::OffsetDateTime,
    share_slug: Option<String>,
    view_count: i64,
}

fn row_to_model(r: Row) -> Clip {
    Clip {
        id: r.id,
        stream_id: r.stream_id,
        creator_id: r.creator_id,
        title: r.title,
        start_secs: r.start_secs,
        end_secs: r.end_secs,
        created_at: r.created_at,
        share_slug: r.share_slug,
        view_count: r.view_count,
    }

}

/// Derive a URL-safe base32 slug from a byte slice. Uses the crockford
/// base32 alphabet (no padding, lowercase). Returns the first 12 characters
/// (60 bits), which is sufficient for a human-readable public URL slug while
/// keeping it short enough to type.
fn base32_slug(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
    let mut out = String::with_capacity(12);
    let mut buf: u64 = 0;
    let mut bits: u32 = 0;
    for &b in bytes {
        buf = (buf << 8) | u64::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let idx = ((buf >> bits) & 0x1f) as usize;
            out.push(ALPHABET[idx] as char);
            if out.len() >= 12 {
                return out;
            }
        }
    }
    out
}

/// Repository over the `stream_clips` table (live-stream clips).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// the feature module builds one inline via [`ClipRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct ClipRepo {
    pool: PgPool,
}

impl ClipRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persist a new clip on `stream`, returning its generated id. The caller has
    /// already checked the stream exists and validated the `[start, end]` range.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn create(
        &self,
        stream: ulid::Ulid,
        creator: ParticipantId,
        title: &str,
        start_secs: i32,
        end_secs: i32,
    ) -> Result<ClipId, sqlx::Error> {
        let id = ClipId::new();
        sqlx::query(
            r"INSERT INTO stream_clips
                  (id, stream_id, creator_id, title, start_secs, end_secs)
               VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id.to_uuid())
        .bind(uuid::Uuid::from_u128(stream.0))
        .bind(creator.to_uuid())
        .bind(title)
        .bind(start_secs)
        .bind(end_secs)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Fetch one clip by id, or `None` if no such clip exists.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(&self, id: ClipId) -> Result<Option<Clip>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM stream_clips WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// A stream's clips, newest first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_stream(&self, stream: ulid::Ulid) -> Result<Vec<Clip>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM stream_clips
              WHERE stream_id = $1
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(uuid::Uuid::from_u128(stream.0))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Generate (or return the existing) share slug for a clip, returning the
    /// slug string. The slug is derived from the first 12 URL-safe base32 chars
    /// of the clip's UUID bytes. Because the clip id is unique the slug is
    /// also unique — a conflict can only happen when the row already has one,
    /// in which case `WHERE share_slug IS NULL` prevents the UPDATE and
    /// `RETURNING share_slug` returns `None`; the caller then re-reads via
    /// [`Self::get`] to obtain the existing slug.
    ///
    /// Returns `Ok(Some(slug))` when the slug was set (first call) or
    /// `Ok(None)` when the clip already had a slug (idempotent — the caller
    /// must re-read to get it).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn generate_share_slug(
        &self,
        id: ClipId,
    ) -> Result<Option<String>, sqlx::Error> {
        // Derive a deterministic slug from the clip's uuid bytes using base32
        // (RFC 4648 no-pad, lower-case). The first 12 chars cover 60 bits —
        // enough uniqueness for a human-readable shareable URL.
        let slug = {
            let bytes = id.to_uuid().as_bytes().to_vec();
            base32_slug(&bytes)
        };
        let row: Option<(Option<String>,)> = sqlx::query_as(
            r"UPDATE stream_clips
                 SET share_slug = $2
               WHERE id = $1 AND share_slug IS NULL
               RETURNING share_slug",
        )
        .bind(id.to_uuid())
        .bind(&slug)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(s,)| s))
    }

    /// Atomically increment the view count for a clip by 1.
    ///
    /// Called after successfully fetching a clip via its public share slug.
    /// No-op if the clip id does not exist (the caller has already resolved it).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn increment_view_count(&self, id: ClipId) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"UPDATE stream_clips SET view_count = view_count + 1 WHERE id = $1",
        )
        .bind(id.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch a clip by its share slug. Returns `None` if no clip has that slug.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get_by_slug(&self, slug: &str) -> Result<Option<Clip>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS} FROM stream_clips WHERE share_slug = $1"
        );
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(slug)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// Delete a clip, scoped to its creator. Returns `true` iff a row was removed —
    /// creator-scoped, so a caller can never delete another user's clip, and a
    /// second delete (or a stranger's) is a no-op returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn delete(&self, id: ClipId, creator: ParticipantId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM stream_clips WHERE id = $1 AND creator_id = $2")
            .bind(id.to_uuid())
            .bind(creator.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    // ----- Clip tag methods (migration 0110) -----

    /// Add a tag to a clip. The tag is normalised (trimmed + lowercased) before
    /// storage. Silently no-ops when the `(clip_id, tag)` pair already exists.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn add_tag(&self, clip_id: uuid::Uuid, tag: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO clip_tags (clip_id, tag) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        )
        .bind(clip_id)
        .bind(tag.trim().to_lowercase())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Remove a tag from a clip. Silently no-ops when the pair does not exist.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn remove_tag(&self, clip_id: uuid::Uuid, tag: &str) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM clip_tags WHERE clip_id = $1 AND tag = $2")
            .bind(clip_id)
            .bind(tag.trim().to_lowercase())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// List all tags on a clip, alphabetically.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_tags(&self, clip_id: uuid::Uuid) -> Result<Vec<String>, sqlx::Error> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT tag FROM clip_tags WHERE clip_id = $1 ORDER BY tag")
                .bind(clip_id)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(|(t,)| t).collect())
    }

    /// List clip ids bearing a tag, newest-tagged first. `limit` is clamped to
    /// `[1, 100]`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn clips_by_tag(
        &self,
        tag: &str,
        limit: i64,
    ) -> Result<Vec<uuid::Uuid>, sqlx::Error> {
        let limit = limit.clamp(1, 100);
        let rows: Vec<(uuid::Uuid,)> = sqlx::query_as(
            "SELECT clip_id FROM clip_tags WHERE tag = $1 ORDER BY created_at DESC LIMIT $2",
        )
        .bind(tag.trim().to_lowercase())
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
///   cargo test -p aero-storage --lib -- --ignored clip_
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

    /// Create a throwaway creator participant so the test is self-contained.
    async fn creator(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("clip-creator-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn clip_create_get_list_delete_creator_scoped() {
        let p = pool();
        let repo = ClipRepo::new(p.clone());
        let stream = ulid::Ulid::new();
        let owner = creator(&p).await;
        let stranger = ParticipantId::new();

        // create → get + list show it.
        let id = repo
            .create(stream, owner, "Best moment", 30, 75)
            .await
            .unwrap();

        let got = repo.get(id).await.unwrap().expect("clip exists");
        assert_eq!(got.id, id);
        assert_eq!(got.stream_id, stream);
        assert_eq!(got.creator_id, owner);
        assert_eq!(got.title, "Best moment");
        assert_eq!(got.start_secs, 30);
        assert_eq!(got.end_secs, 75);

        let listed = repo.list_for_stream(stream).await.unwrap();
        assert!(listed.iter().any(|c| c.id == id), "list shows the clip");

        // A stranger's delete is a no-op; the creator's first delete succeeds, the
        // second is a no-op.
        assert!(
            !repo.delete(id, stranger).await.unwrap(),
            "stranger cannot delete another user's clip"
        );
        assert!(repo.get(id).await.unwrap().is_some(), "still present");
        assert!(repo.delete(id, owner).await.unwrap(), "creator deletes");
        assert!(repo.get(id).await.unwrap().is_none(), "gone after delete");
        assert!(
            !repo.delete(id, owner).await.unwrap(),
            "second delete is a no-op"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM stream_clips WHERE creator_id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
