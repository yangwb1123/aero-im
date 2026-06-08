//! Channel-canvas repository (per-channel collaborative documents).
//!
//! Backs `migrations/0048_channel_canvas.sql`. A channel ("room") can own
//! several canvases — Slack Canvas / Lark Docs-in-channel. Each is a titled rich
//! document whose body is a JSON array of arbitrary blocks, stored verbatim as
//! JSONB and deliberately NOT coupled to [`aero_common::Block`]: the repo accepts
//! and returns an opaque [`serde_json::Value`].
//!
//! This repo owns only the canvas CRUD; it does NOT enforce room membership —
//! the server layer gates every route on
//! [`ImService::assert_room_access`](aero_im_core::ImService::assert_room_access)
//! (a canvas is collaborative, so any member with room access may edit it).
//! Purely additive: a NEW [`CanvasRepo`]; no existing repo is touched. The
//! [`Canvas`] model lives here (and is re-exported from the crate root) rather
//! than in `aero-common`, since it is a storage-layer projection.

use aero_common::{CanvasId, ParticipantId, RoomId};
use serde::Serialize;
use sqlx::PgPool;

/// One channel canvas — a per-channel collaborative document.
///
/// A storage-layer projection of a `channel_canvases` row. `Serialize` so a
/// handler can hand the row straight back as JSON; `created_at` / `updated_at`
/// render as RFC 3339, and `blocks` is the raw stored payload (a JSON array of
/// blocks).
#[derive(Debug, Clone, Serialize)]
pub struct Canvas {
    /// The canvas's unique id.
    pub id: CanvasId,
    /// The channel (room) the canvas belongs to.
    pub room_id: RoomId,
    /// The participant who originally created the canvas.
    pub author_id: ParticipantId,
    /// Human-readable title of the document.
    pub title: String,
    /// The document body (an arbitrary JSON array of blocks).
    pub blocks: serde_json::Value,
    /// When the canvas was created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    /// When the canvas was last edited (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: time::OffsetDateTime,
}

/// The columns a [`Canvas`] is built from, in select order. Shared by every query
/// so the row decoding stays in one place.
const COLUMNS: &str = "id, room_id, author_id, title, blocks, created_at, updated_at";

type Row = (
    uuid::Uuid,
    uuid::Uuid,
    uuid::Uuid,
    String,
    serde_json::Value,
    time::OffsetDateTime,
    time::OffsetDateTime,
);

fn row_to_model(r: Row) -> Canvas {
    let (id, room_id, author_id, title, blocks, created_at, updated_at) = r;
    Canvas {
        id: CanvasId::from_uuid(id),
        room_id: RoomId::from_uuid(room_id),
        author_id: ParticipantId::from_uuid(author_id),
        title,
        blocks,
        created_at,
        updated_at,
    }
}

/// Repository over the `channel_canvases` table (per-channel documents).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`CanvasRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct CanvasRepo {
    pool: PgPool,
}

impl CanvasRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persist a new canvas authored by `author` in `room`, returning its
    /// generated id. The caller is responsible for room-access and title/blocks
    /// validation.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn create(
        &self,
        room: RoomId,
        author: ParticipantId,
        title: &str,
        blocks: &serde_json::Value,
    ) -> Result<CanvasId, sqlx::Error> {
        let id = CanvasId::new();
        sqlx::query(
            r"INSERT INTO channel_canvases (id, room_id, author_id, title, blocks)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(author.to_uuid())
        .bind(title)
        .bind(blocks)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Fetch one canvas by id, or `None` if no such row exists. Not scoped — the
    /// caller (server layer) asserts room access against the returned `room_id`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(&self, id: CanvasId) -> Result<Option<Canvas>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM channel_canvases WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// List `room`'s canvases, newest edit first (`updated_at DESC`).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_room(&self, room: RoomId) -> Result<Vec<Canvas>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM channel_canvases
              WHERE room_id = $1
              ORDER BY updated_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(room.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Replace a canvas's `title` and `blocks`, bumping `updated_at` to now.
    /// Returns `true` iff a row was updated (`false` for an unknown id).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn update(
        &self,
        id: CanvasId,
        title: &str,
        blocks: &serde_json::Value,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE channel_canvases
                 SET title = $2, blocks = $3, updated_at = now()
               WHERE id = $1",
        )
        .bind(id.to_uuid())
        .bind(title)
        .bind(blocks)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Delete a canvas by id. Returns `true` iff a row was removed — a second
    /// delete (or an unknown id) is a no-op returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn delete(&self, id: CanvasId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM channel_canvases WHERE id = $1")
            .bind(id.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored canvas
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

    /// Create a throwaway author participant so the test is self-contained.
    async fn author(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("canvas-author-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn canvas_create_get_list_update_delete() {
        let p = pool();
        let repo = CanvasRepo::new(p.clone());
        let room = RoomId::new();
        let other_room = RoomId::new();
        let author = author(&p).await;
        let blocks = serde_json::json!([{ "type": "heading", "text": "Plan" }]);

        // create → get returns the row.
        let id = repo.create(room, author, "Q3 Plan", &blocks).await.unwrap();
        let got = repo.get(id).await.unwrap().expect("created canvas exists");
        assert_eq!(got.id, id);
        assert_eq!(got.room_id, room);
        assert_eq!(got.author_id, author);
        assert_eq!(got.title, "Q3 Plan");
        assert_eq!(got.blocks, blocks);

        // list_for_room shows it; a different room does not.
        let listed = repo.list_for_room(room).await.unwrap();
        assert!(listed.iter().any(|c| c.id == id), "room list shows the canvas");
        assert!(
            !repo.list_for_room(other_room).await.unwrap().iter().any(|c| c.id == id),
            "another room's list does not show it"
        );

        // update replaces title + blocks and bumps updated_at.
        let new_blocks = serde_json::json!([{ "type": "text", "text": "shipped" }]);
        assert!(repo.update(id, "Q3 Plan (final)", &new_blocks).await.unwrap());
        let after = repo.get(id).await.unwrap().expect("still present");
        assert_eq!(after.title, "Q3 Plan (final)");
        assert_eq!(after.blocks, new_blocks);
        assert!(after.updated_at >= got.updated_at, "updated_at moved forward");

        // delete: first removes, second is a no-op; unknown id is false too.
        assert!(repo.delete(id).await.unwrap(), "first delete removes");
        assert!(!repo.delete(id).await.unwrap(), "second delete is a no-op");
        assert!(!repo.update(id, "x", &new_blocks).await.unwrap(), "update on gone id is false");
        assert!(repo.get(id).await.unwrap().is_none(), "deleted canvas is gone");

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM channel_canvases WHERE room_id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(author.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
