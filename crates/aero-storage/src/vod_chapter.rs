//! VOD chapter / marker repository (timestamped table-of-contents on a recording).
//!
//! Backs `migrations/0081_vod_chapters.sql`. A [`Vod`](aero_common::Vod) (0025) is
//! a finalized live stream; a [`VodChapter`] annotates a timestamp within it
//! ("Intro", "Boss fight", …) so a viewer can jump to a section — playback reuses
//! the VOD's existing HLS playlist with a client-side seek to `start_secs`, no
//! media is processed here (mirroring [`ClipRepo`](crate::ClipRepo)).
//!
//! Owner-gating (the VOD's stream owner) is enforced by the HTTP layer; this repo
//! persists what it is given. [`VodChapterRepo::delete`] is creator-scoped at the
//! SQL layer. Purely additive: a NEW [`VodChapterRepo`]; no existing repo is
//! touched. `vod_id` cascades with its recording (a chapter has no meaning without
//! its VOD).

use aero_common::{ParticipantId, VodChapterId, VodId};
use serde::Serialize;
use sqlx::PgPool;

/// One VOD chapter — a storage-layer projection of a `vod_chapters` row.
#[derive(Debug, Clone, Serialize)]
pub struct VodChapter {
    /// The chapter's unique id.
    pub id: VodChapterId,
    /// The VOD / recording this chapter annotates.
    pub vod_id: VodId,
    /// Offset into the recording, in whole seconds (`>= 0`).
    pub start_secs: i32,
    /// Human-readable chapter title.
    pub title: String,
    /// The participant who created the chapter (the VOD's owner).
    pub created_by: ParticipantId,
    /// When the chapter was created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

const COLUMNS: &str = "id, vod_id, start_secs, title, created_by, created_at";

type Row = (
    uuid::Uuid,
    uuid::Uuid,
    i32,
    String,
    uuid::Uuid,
    time::OffsetDateTime,
);

fn row_to_model(r: Row) -> VodChapter {
    let (id, vod_id, start_secs, title, created_by, created_at) = r;
    VodChapter {
        id: VodChapterId::from_uuid(id),
        vod_id: VodId::from_uuid(vod_id),
        start_secs,
        title,
        created_by: ParticipantId::from_uuid(created_by),
        created_at,
    }
}

/// Repository over the `vod_chapters` table.
///
/// Cheap to clone — wraps a [`PgPool`]; feature modules build one inline via
/// [`VodChapterRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct VodChapterRepo {
    pool: PgPool,
}

impl VodChapterRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Add a chapter to `vod`, returning its generated id. The caller has already
    /// resolved the VOD, checked ownership, and validated `start_secs >= 0` +
    /// a non-empty `title`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn create(
        &self,
        vod: VodId,
        start_secs: i32,
        title: &str,
        created_by: ParticipantId,
    ) -> Result<VodChapterId, sqlx::Error> {
        let id = VodChapterId::new();
        sqlx::query(
            r"INSERT INTO vod_chapters (id, vod_id, start_secs, title, created_by)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id.to_uuid())
        .bind(vod.to_uuid())
        .bind(start_secs)
        .bind(title)
        .bind(created_by.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// A VOD's chapters, ordered by position in the recording (`start_secs`).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_vod(&self, vod: VodId) -> Result<Vec<VodChapter>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM vod_chapters
              WHERE vod_id = $1
              ORDER BY start_secs ASC, created_at ASC, id ASC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(vod.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Fetch one chapter by id, or `None` if no such row exists.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(&self, id: VodChapterId) -> Result<Option<VodChapter>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM vod_chapters WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// Delete a chapter, scoped to its creator. Returns `true` iff a row was
    /// removed — creator-scoped, so a caller can never delete a chapter they did
    /// not author, and a second delete (or a stranger's) is a no-op (`false`).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn delete(
        &self,
        id: VodChapterId,
        creator: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result =
            sqlx::query("DELETE FROM vod_chapters WHERE id = $1 AND created_by = $2")
                .bind(id.to_uuid())
                .bind(creator.to_uuid())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored vod_chapter_
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

    /// A self-contained (owner, stream, vod) triple so the chapter tests stand alone.
    async fn fixture(p: &PgPool) -> (ParticipantId, VodId) {
        let owner = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(owner.to_uuid())
            .bind(format!("vodchap-owner-{owner}"))
            .execute(p)
            .await
            .expect("insert participant");
        let stream_id = ulid::Ulid::new();
        sqlx::query(
            "INSERT INTO streams (id, owner_id, title, stream_key, status, protocol, created_at)
             VALUES ($1,$2,$3,$4,'ended','whip', now())",
        )
        .bind(uuid::Uuid::from_u128(stream_id.0))
        .bind(owner.to_uuid())
        .bind("vodchap-stream")
        .bind(format!("key-{stream_id}"))
        .execute(p)
        .await
        .expect("insert stream");
        let vod = VodId::new();
        sqlx::query(
            "INSERT INTO stream_recordings (id, stream_id, owner_id, title, hls_path, created_at)
             VALUES ($1,$2,$3,$4,$5, now())",
        )
        .bind(vod.to_uuid())
        .bind(uuid::Uuid::from_u128(stream_id.0))
        .bind(owner.to_uuid())
        .bind("vodchap-vod")
        .bind(format!("/hls/{stream_id}/index.m3u8"))
        .execute(p)
        .await
        .expect("insert vod");
        (owner, vod)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn vod_chapter_create_list_ordered_delete() {
        let p = pool();
        let repo = VodChapterRepo::new(p.clone());
        let (owner, vod) = fixture(&p).await;
        let stranger = ParticipantId::new();

        // Insert out of order; list must come back ordered by start_secs.
        let c_mid = repo.create(vod, 120, "Boss fight", owner).await.unwrap();
        let c_first = repo.create(vod, 0, "Intro", owner).await.unwrap();
        let c_last = repo.create(vod, 600, "Outro", owner).await.unwrap();

        let listed = repo.list_for_vod(vod).await.unwrap();
        let ids: Vec<_> = listed.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![c_first, c_mid, c_last], "ordered by start_secs");
        assert_eq!(listed[0].title, "Intro");

        // get round-trips.
        let got = repo.get(c_mid).await.unwrap().expect("present");
        assert_eq!(got.start_secs, 120);
        assert_eq!(got.vod_id, vod);

        // A stranger's delete is a no-op; the creator's succeeds; the second no-op.
        assert!(!repo.delete(c_mid, stranger).await.unwrap(), "stranger cannot delete");
        assert!(repo.delete(c_mid, owner).await.unwrap(), "creator deletes");
        assert!(!repo.delete(c_mid, owner).await.unwrap(), "second delete is a no-op");
        assert_eq!(repo.list_for_vod(vod).await.unwrap().len(), 2);

        // Cleanup (chapters cascade with the recording).
        sqlx::query("DELETE FROM stream_recordings WHERE owner_id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
