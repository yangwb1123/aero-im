//! Channel-bookmark repository (per-channel header links / pinned resources).
//!
//! Backs `migrations/0049_channel_bookmarks.sql`. A member adds a titled URL
//! (with an optional emoji) to a room's header bar; members list them in display
//! order, and any can be edited or removed. This is DISTINCT from message pins
//! ([`PinRepo`](crate::PinRepo), which pin a message) and from personal saved
//! items ([`BookmarkRepo`](crate::BookmarkRepo), which are per-user save-for-
//! later) — a channel bookmark belongs to the channel itself.
//!
//! Every method is room-scoped: the caller (the server handler) is responsible
//! for asserting room access before reading or mutating, mirroring the other
//! room-scoped repos. Purely additive: a NEW [`ChannelBookmarkRepo`]; no existing
//! repo is touched. The [`ChannelBookmark`] model lives here (and is re-exported
//! from the crate root) rather than in `aero-common`, since it is a storage-layer
//! projection.

use aero_common::{ChannelBookmarkId, ParticipantId, RoomId};
use serde::Serialize;
use sqlx::PgPool;

/// One channel bookmark — a per-channel header link / pinned resource.
///
/// A storage-layer projection of a `channel_bookmarks` row. `Serialize` so a
/// handler can hand the row straight back as JSON; `created_at` renders as
/// RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct ChannelBookmark {
    /// The bookmark's unique id.
    pub id: ChannelBookmarkId,
    /// The room (channel) the bookmark is shown in.
    pub room_id: RoomId,
    /// Human-readable label rendered in the header.
    pub title: String,
    /// The link the bookmark points at.
    pub url: String,
    /// Optional leading emoji shown next to the title.
    pub emoji: Option<String>,
    /// The member who created the bookmark.
    pub created_by: ParticipantId,
    /// Display order within the channel header (ascending; ties broken by
    /// `created_at`).
    pub position: i32,
    /// When the bookmark was created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// The columns a [`ChannelBookmark`] is built from, in select order. Shared by
/// every query so the row decoding stays in one place.
const COLUMNS: &str = "id, room_id, title, url, emoji, created_by, position, created_at";

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: uuid::Uuid,
    room_id: uuid::Uuid,
    title: String,
    url: String,
    emoji: Option<String>,
    created_by: uuid::Uuid,
    position: i32,
    created_at: time::OffsetDateTime,
}

fn row_to_model(r: Row) -> ChannelBookmark {
    ChannelBookmark {
        id: ChannelBookmarkId::from_uuid(r.id),
        room_id: RoomId::from_uuid(r.room_id),
        title: r.title,
        url: r.url,
        emoji: r.emoji,
        created_by: ParticipantId::from_uuid(r.created_by),
        position: r.position,
        created_at: r.created_at,
    }
}

/// Repository over the `channel_bookmarks` table (per-channel header links).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`ChannelBookmarkRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct ChannelBookmarkRepo {
    pool: PgPool,
}

impl ChannelBookmarkRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persist a new bookmark in `room`, created by `creator`, returning its
    /// generated id. The caller is responsible for room-access and
    /// title/url validation.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn add(
        &self,
        room: RoomId,
        creator: ParticipantId,
        title: &str,
        url: &str,
        emoji: Option<&str>,
        position: i32,
    ) -> Result<ChannelBookmarkId, sqlx::Error> {
        let id = ChannelBookmarkId::new();
        sqlx::query(
            r"INSERT INTO channel_bookmarks
                  (id, room_id, title, url, emoji, created_by, position)
               VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(title)
        .bind(url)
        .bind(emoji)
        .bind(creator.to_uuid())
        .bind(position)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List `room`'s bookmarks in display order (by `position`, then
    /// `created_at`). Room-scoped — the caller asserts room access first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_room(
        &self,
        room: RoomId,
    ) -> Result<Vec<ChannelBookmark>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM channel_bookmarks
              WHERE room_id = $1
              ORDER BY position ASC, created_at ASC, id ASC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(room.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Fetch one bookmark by id, or `None` if no such row exists. The caller
    /// resolves the owning room from the returned row to assert room access.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(
        &self,
        id: ChannelBookmarkId,
    ) -> Result<Option<ChannelBookmark>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM channel_bookmarks WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// Update a bookmark's title, url, emoji, and position in place. Returns
    /// `true` iff a row was changed (so a missing id is a no-op returning
    /// `false`). The caller asserts room access (via [`get`](Self::get)) first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn update(
        &self,
        id: ChannelBookmarkId,
        title: &str,
        url: &str,
        emoji: Option<&str>,
        position: i32,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE channel_bookmarks
                 SET title = $2, url = $3, emoji = $4, position = $5
               WHERE id = $1",
        )
        .bind(id.to_uuid())
        .bind(title)
        .bind(url)
        .bind(emoji)
        .bind(position)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Delete a bookmark by id. Returns `true` iff a row was removed, so a second
    /// delete (or an unknown id) is a no-op returning `false`. The caller asserts
    /// room access (via [`get`](Self::get)) first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn delete(&self, id: ChannelBookmarkId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query("DELETE FROM channel_bookmarks WHERE id = $1")
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
///   cargo test -p aero-storage --lib -- --ignored channel_bookmark
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
            .bind(format!("channel-bookmark-creator-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn channel_bookmark_add_list_get_update_delete() {
        let p = pool();
        let repo = ChannelBookmarkRepo::new(p.clone());
        let room = RoomId::new();
        let creator = creator(&p).await;

        // add → list shows it in position order.
        let first = repo
            .add(room, creator, "Docs", "https://docs.example", Some("📚"), 1)
            .await
            .unwrap();
        let second = repo
            .add(room, creator, "Wiki", "https://wiki.example", None, 0)
            .await
            .unwrap();
        let listed = repo.list_for_room(room).await.unwrap();
        assert_eq!(listed.len(), 2, "both bookmarks listed");
        assert_eq!(listed[0].id, second, "lower position sorts first");
        assert_eq!(listed[1].id, first);
        assert_eq!(listed[1].emoji.as_deref(), Some("📚"));

        // get (existing) works; get (unknown) is None.
        let got = repo.get(first).await.unwrap().expect("present");
        assert_eq!(got.title, "Docs");
        assert_eq!(got.room_id, room);
        assert_eq!(got.created_by, creator);
        assert!(
            repo.get(ChannelBookmarkId::new()).await.unwrap().is_none(),
            "unknown id resolves to None"
        );

        // update changes the row and clears the emoji.
        assert!(
            repo.update(first, "Handbook", "https://hb.example", None, 5)
                .await
                .unwrap(),
            "update changes a row"
        );
        let after = repo.get(first).await.unwrap().expect("present");
        assert_eq!(after.title, "Handbook");
        assert_eq!(after.url, "https://hb.example");
        assert_eq!(after.emoji, None);
        assert_eq!(after.position, 5);
        assert!(
            !repo
                .update(ChannelBookmarkId::new(), "x", "y", None, 0)
                .await
                .unwrap(),
            "update of unknown id is a no-op"
        );

        // delete removes the row; a second delete is a no-op.
        assert!(repo.delete(first).await.unwrap(), "owner deletes");
        assert!(
            !repo.delete(first).await.unwrap(),
            "second delete is a no-op"
        );
        assert!(repo.delete(second).await.unwrap());
        assert!(
            repo.list_for_room(room).await.unwrap().is_empty(),
            "deleted bookmarks leave the list"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM channel_bookmarks WHERE room_id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(creator.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
