//! Channel-bookmark repository (per-channel header links / pinned resources).
//!
//! Backs `migrations/0049_channel_bookmarks.sql`. A member adds a titled URL
//! (with an optional emoji) to a room's header bar; members list them in display
//! order, and any can be edited or removed. This is DISTINCT from message pins
//! ([`PinRepo`](crate::PinRepo), which pin a message) and from personal saved
//! items ([`BookmarkRepo`](crate::BookmarkRepo), which are per-user save-for-
//! later) — a channel bookmark belongs to the channel itself.
//!
//! Every public operation owns a transaction-scoped effective-access recheck and
//! binds global bookmark ids to the request's live channel. A cross-room id is an
//! opaque not-found; a member whose access was revoked is forbidden.

use aero_common::{ChannelBookmarkId, Error, ParticipantId, RoomId};
use serde::Serialize;
use sqlx::PgPool;

const MAX_TITLE_CHARS: usize = 256;
const MAX_URL_CHARS: usize = 2_048;
const MAX_EMOJI_CHARS: usize = 64;

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

/// Optional fields for one authorized channel-bookmark update.
#[derive(Debug, Clone, Copy)]
pub struct ChannelBookmarkPatch<'a> {
    /// Replacement title; omitted to retain the current title.
    pub title: Option<&'a str>,
    /// Replacement URL; omitted to retain the current URL.
    pub url: Option<&'a str>,
    /// Omitted to retain the emoji, `Some(None)` to clear it.
    pub emoji: Option<Option<&'a str>>,
    /// Replacement display order; omitted to retain it.
    pub position: Option<i32>,
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

    /// Persist a new bookmark while `creator` remains an effective member of the
    /// live channel, returning the exact committed row.
    ///
    /// # Errors
    pub async fn add_channel_bookmark_authorized(
        &self,
        room: RoomId,
        creator: ParticipantId,
        title: &str,
        url: &str,
        emoji: Option<&str>,
        position: i32,
    ) -> Result<ChannelBookmark, Error> {
        let title = validate_required(title, MAX_TITLE_CHARS, "title")?;
        let url = validate_required(url, MAX_URL_CHARS, "url")?;
        let emoji = validate_emoji(emoji)?;
        let mut tx = self.pool.begin().await?;
        crate::canvas::lock_live_channel_access(&mut tx, room, creator).await?;
        let id = ChannelBookmarkId::new();
        let sql = format!(
            "INSERT INTO channel_bookmarks
                 (id, room_id, title, url, emoji, created_by, position)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING {COLUMNS}"
        );
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .bind(room.to_uuid())
            .bind(title)
            .bind(url)
            .bind(emoji.as_deref())
            .bind(creator.to_uuid())
            .bind(position)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(row_to_model(row))
    }

    /// List a live channel's bookmarks in display order while the actor's
    /// effective access remains locked.
    ///
    /// # Errors
    pub async fn list_channel_bookmarks_authorized(
        &self,
        room: RoomId,
        actor: ParticipantId,
    ) -> Result<Vec<ChannelBookmark>, Error> {
        let mut tx = self.pool.begin().await?;
        crate::canvas::lock_live_channel_access(&mut tx, room, actor).await?;
        let sql = format!(
            "SELECT {COLUMNS}
               FROM channel_bookmarks
              WHERE room_id = $1
              ORDER BY position ASC, created_at ASC, id ASC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(room.to_uuid())
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Fetch one path-bound bookmark while effective live-channel access remains
    /// locked. Missing and cross-room ids share one not-found result.
    ///
    /// # Errors
    pub async fn get_channel_bookmark_authorized(
        &self,
        room: RoomId,
        id: ChannelBookmarkId,
        actor: ParticipantId,
    ) -> Result<ChannelBookmark, Error> {
        let mut tx = self.pool.begin().await?;
        crate::canvas::lock_live_channel_access(&mut tx, room, actor).await?;
        let sql = format!("SELECT {COLUMNS} FROM channel_bookmarks WHERE id = $1 AND room_id = $2");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .bind(room.to_uuid())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| Error::NotFound(format!("channel bookmark {id}")))?;
        tx.commit().await?;
        Ok(row_to_model(row))
    }

    /// Update a path-bound bookmark while the actor's effective channel access
    /// remains locked. `emoji = None` leaves it unchanged; `Some(None)` clears
    /// it; `Some(Some(value))` replaces it.
    ///
    /// # Errors
    pub async fn update_channel_bookmark_authorized(
        &self,
        room: RoomId,
        id: ChannelBookmarkId,
        actor: ParticipantId,
        patch: ChannelBookmarkPatch<'_>,
    ) -> Result<ChannelBookmark, Error> {
        let ChannelBookmarkPatch {
            title,
            url,
            emoji,
            position,
        } = patch;
        if title.is_none() && url.is_none() && emoji.is_none() && position.is_none() {
            return Err(Error::Invalid("bookmark patch is empty".into()));
        }
        let title = title
            .map(|value| validate_required(value, MAX_TITLE_CHARS, "title"))
            .transpose()?;
        let url = url
            .map(|value| validate_required(value, MAX_URL_CHARS, "url"))
            .transpose()?;
        let emoji = emoji.map(validate_emoji).transpose()?;

        let mut tx = self.pool.begin().await?;
        crate::canvas::lock_live_channel_access(&mut tx, room, actor).await?;
        let exists = sqlx::query_scalar::<_, bool>(
            "SELECT true
               FROM channel_bookmarks
              WHERE id = $1 AND room_id = $2
              FOR UPDATE",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !exists {
            return Err(Error::NotFound(format!("channel bookmark {id}")));
        }

        let sql = format!(
            r"UPDATE channel_bookmarks
                 SET title = COALESCE($3, title),
                     url = COALESCE($4, url),
                     emoji = CASE WHEN $5 THEN $6 ELSE emoji END,
                     position = COALESCE($7, position)
               WHERE id = $1 AND room_id = $2
               RETURNING {COLUMNS}"
        );
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .bind(room.to_uuid())
            .bind(title.as_deref())
            .bind(url.as_deref())
            .bind(emoji.is_some())
            .bind(emoji.as_ref().and_then(Option::as_deref))
            .bind(position)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(row_to_model(row))
    }

    /// Delete a path-bound bookmark while effective live-channel access remains
    /// locked. Missing/already-deleted/cross-room ids are opaque not-found.
    ///
    /// # Errors
    pub async fn delete_channel_bookmark_authorized(
        &self,
        room: RoomId,
        id: ChannelBookmarkId,
        actor: ParticipantId,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        crate::canvas::lock_live_channel_access(&mut tx, room, actor).await?;
        let deleted = sqlx::query_scalar::<_, uuid::Uuid>(
            "DELETE FROM channel_bookmarks
              WHERE id = $1 AND room_id = $2
              RETURNING id",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        if deleted.is_none() {
            return Err(Error::NotFound(format!("channel bookmark {id}")));
        }
        tx.commit().await?;
        Ok(())
    }
}

fn validate_required(raw: &str, max_chars: usize, field: &str) -> Result<String, Error> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(Error::Invalid(format!("{field} must not be empty")));
    }
    if value.chars().count() > max_chars {
        return Err(Error::Invalid(format!("{field} too long")));
    }
    Ok(value.to_owned())
}

fn validate_emoji(raw: Option<&str>) -> Result<Option<String>, Error> {
    let value = raw.map(str::trim).filter(|value| !value.is_empty());
    if value.is_some_and(|value| value.chars().count() > MAX_EMOJI_CHARS) {
        return Err(Error::Invalid("emoji too long".into()));
    }
    Ok(value.map(str::to_owned))
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored channel_bookmark
/// ```
#[cfg(test)]
#[path = "channel_bookmark/security_tests.rs"]
mod security_tests;

#[cfg(any())] // Replaced by transaction-fence PG tests in this change.
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
