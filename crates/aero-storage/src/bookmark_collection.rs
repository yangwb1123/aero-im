//! Bookmark collections / folders repository (per-user saved-items folders).
//!
//! Backs `migrations/0078_bookmark_collections.sql`. The flat per-user saved
//! list ([`BookmarkRepo`](crate::BookmarkRepo)) gains named, ordered folders
//! (Slack/Lark "Saved items" collections). A collection is per-user; a saved
//! message belongs to at most one collection via a nullable `collection_id` on
//! `bookmarks`. Deleting a collection nulls its items' assignment (the DB's
//! `ON DELETE SET NULL`) rather than removing the bookmarks. Purely additive: a
//! NEW [`BookmarkCollectionRepo`]; the assign/clear/list-by-collection helpers
//! touch the `bookmarks` table's new column but no existing method.

use aero_common::{BookmarkCollectionId, MessageId, ParticipantId};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use time::OffsetDateTime;

/// One bookmark collection: a per-user named, ordered folder of saved items.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookmarkCollection {
    pub id: BookmarkCollectionId,
    pub participant_id: ParticipantId,
    pub name: String,
    pub position: i32,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

type Row = (uuid::Uuid, uuid::Uuid, String, i32, OffsetDateTime);

fn row_to_model(r: Row) -> BookmarkCollection {
    let (id, participant_id, name, position, created_at) = r;
    BookmarkCollection {
        id: BookmarkCollectionId::from_uuid(id),
        participant_id: ParticipantId::from_uuid(participant_id),
        name,
        position,
        created_at,
    }
}

#[derive(Clone)]
pub struct BookmarkCollectionRepo {
    pool: PgPool,
}

impl BookmarkCollectionRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a new collection for a user, returning its generated id. The caller
    /// is responsible for trimming/validating `name`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn create(
        &self,
        participant: ParticipantId,
        name: &str,
        position: i32,
    ) -> Result<BookmarkCollectionId, sqlx::Error> {
        let id = BookmarkCollectionId::new();
        sqlx::query(
            r"INSERT INTO bookmark_collections (id, participant_id, name, position)
               VALUES ($1, $2, $3, $4)",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(name)
        .bind(position)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List a user's collections in sidebar order (by `position`, then oldest
    /// first). Always owner-scoped.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<BookmarkCollection>, sqlx::Error> {
        let rows = sqlx::query_as::<_, Row>(
            r"SELECT id, participant_id, name, position, created_at
               FROM bookmark_collections
              WHERE participant_id = $1
              ORDER BY position ASC, created_at ASC, id ASC",
        )
        .bind(participant.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Rename a collection (and/or reposition it). Owner-scoped: returns `true`
    /// iff a row belonging to `participant` was updated. `position = None` leaves
    /// the existing position untouched.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn rename(
        &self,
        id: BookmarkCollectionId,
        participant: ParticipantId,
        name: &str,
        position: Option<i32>,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE bookmark_collections
                 SET name = $3,
                     position = COALESCE($4, position)
               WHERE id = $1 AND participant_id = $2",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(name)
        .bind(position)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Delete a collection. Owner-scoped. The `bookmarks.collection_id` FK is
    /// `ON DELETE SET NULL`, so its saved items are NOT removed — they fall back
    /// into the un-foldered list. Returns `true` iff a row was deleted.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn delete(
        &self,
        id: BookmarkCollectionId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"DELETE FROM bookmark_collections WHERE id = $1 AND participant_id = $2",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Assign a saved message to one of the caller's collections. Owner-scoped on
    /// BOTH sides: the bookmark must be the caller's, and the collection must be
    /// the caller's (enforced via a sub-select so a user cannot file a message
    /// into another user's folder). Returns `true` iff a bookmark row was updated.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn assign(
        &self,
        participant: ParticipantId,
        message: MessageId,
        collection: BookmarkCollectionId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE bookmarks
                 SET collection_id = $3
               WHERE participant_id = $1
                 AND message_id = $2
                 AND EXISTS (
                       SELECT 1 FROM bookmark_collections c
                        WHERE c.id = $3 AND c.participant_id = $1
                 )",
        )
        .bind(participant.to_uuid())
        .bind(message.to_uuid())
        .bind(collection.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Clear a saved message's collection assignment (move it back to the flat
    /// list). Owner-scoped. Returns `true` iff a bookmark row was updated.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn clear(
        &self,
        participant: ParticipantId,
        message: MessageId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE bookmarks
                 SET collection_id = NULL
               WHERE participant_id = $1 AND message_id = $2",
        )
        .bind(participant.to_uuid())
        .bind(message.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored bookmark_collection_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::BookmarkRepo;
    use aero_common::RoomId;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// A self-contained (room, message, participant) fixture.
    async fn fixture(p: &PgPool) -> (RoomId, MessageId, ParticipantId) {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(actor.to_uuid())
            .bind(format!("bc-actor-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");
        let room = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
             VALUES ($1,'channel',$2,$3, now(), '00000000-0000-0000-0000-000000000000')",
        )
        .bind(room.to_uuid())
        .bind("bc-room")
        .bind(actor.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        let message = MessageId::new();
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks, searchable_text, created_at)
             VALUES ($1,$2,$3,'[{\"type\":\"text\",\"content\":\"hi\"}]'::jsonb,'hi', now())",
        )
        .bind(message.to_uuid())
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .execute(p)
        .await
        .expect("insert message");
        (room, message, actor)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn bookmark_collection_create_assign_list_by_collection() {
        let p = pool();
        let collections = BookmarkCollectionRepo::new(p.clone());
        let bookmarks = BookmarkRepo::new(p.clone());
        let (room, message, actor) = fixture(&p).await;

        // Create a collection; it appears in the owner's list.
        let cid = collections.create(actor, "Read later", 0).await.unwrap();
        let listed = collections.list_for(actor).await.unwrap();
        assert!(listed.iter().any(|c| c.id == cid && c.name == "Read later"));

        // Save the message, then file it into the collection.
        assert!(bookmarks.save(actor, message, room, None).await.unwrap(), "saved");
        assert!(collections.assign(actor, message, cid).await.unwrap(), "assigned");

        // list-by-collection returns only the assigned item...
        let in_coll = bookmarks.list_in_collection(actor, Some(cid), None).await.unwrap();
        assert_eq!(in_coll.len(), 1, "exactly the filed message");
        assert_eq!(in_coll[0].message.id, message);

        // ...and another (empty) collection returns nothing.
        let other = collections.create(actor, "Empty", 1).await.unwrap();
        assert!(
            bookmarks.list_in_collection(actor, Some(other), None).await.unwrap().is_empty(),
            "empty collection returns no items"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn bookmark_collection_delete_nulls_items() {
        let p = pool();
        let collections = BookmarkCollectionRepo::new(p.clone());
        let bookmarks = BookmarkRepo::new(p.clone());
        let (room, message, actor) = fixture(&p).await;

        let cid = collections.create(actor, "Folder", 0).await.unwrap();
        assert!(bookmarks.save(actor, message, room, None).await.unwrap());
        assert!(collections.assign(actor, message, cid).await.unwrap());

        // Deleting the collection nulls the item's assignment (ON DELETE SET NULL):
        // the bookmark survives and reappears in the flat (un-foldered) list.
        assert!(collections.delete(cid, actor).await.unwrap(), "deleted");
        assert!(
            bookmarks.list_in_collection(actor, Some(cid), None).await.unwrap().is_empty(),
            "no items remain under the deleted collection id"
        );
        // The bookmark is still present overall (flat list).
        assert!(
            bookmarks.list(actor, None).await.unwrap().iter().any(|s| s.message.id == message),
            "the saved message survived the collection delete"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn bookmark_collection_assign_is_owner_scoped() {
        let p = pool();
        let collections = BookmarkCollectionRepo::new(p.clone());
        let bookmarks = BookmarkRepo::new(p.clone());
        let (room, message, actor) = fixture(&p).await;

        // A stranger's collection cannot receive the actor's saved message.
        let stranger = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(stranger.to_uuid())
            .bind(format!("bc-stranger-{stranger}"))
            .execute(&p)
            .await
            .expect("insert stranger");
        let stranger_coll = collections.create(stranger, "Theirs", 0).await.unwrap();

        assert!(bookmarks.save(actor, message, room, None).await.unwrap());
        assert!(
            !collections.assign(actor, message, stranger_coll).await.unwrap(),
            "cannot file into another user's collection"
        );
    }
}
