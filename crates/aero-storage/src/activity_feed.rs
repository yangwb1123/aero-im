//! Activity-feed repository (general-purpose, per-participant).
//!
//! Backs `migrations/0062_activity_feed.sql`. A durable, per-participant feed of
//! NON-message events — the first being "a creator you follow went live". The
//! existing notification inbox is message+room-scoped (both columns NOT NULL) and
//! so cannot carry a creator-feed entry, so this repo owns a deliberately general
//! shape: a `kind` discriminator, an optional `actor` (who triggered it) and
//! `subject` (what it is about), and a human `summary`.
//!
//! Every read/mutate method is recipient-scoped (`participant_id` in the `WHERE`),
//! so a caller can only ever see or mutate their own feed. The fan-out
//! ([`ActivityFeedRepo::insert`]) is the write path — e.g. the go-live bot inserts
//! one row per follower. Purely additive: a NEW [`ActivityFeedRepo`]; no existing
//! repo is touched. The [`ActivityEntry`] model lives here (and is re-exported
//! from the crate root) rather than in `aero-common`, since it is a storage-layer
//! projection.

use aero_common::{ActivityId, ParticipantId};
use serde::Serialize;
use sqlx::PgPool;
use ulid::Ulid;

/// One activity-feed entry — a durable notice delivered to a single participant.
///
/// A storage-layer projection of an `activity_feed` row. `Serialize` so a handler
/// can hand the row straight back as JSON; timestamps render as RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct ActivityEntry {
    /// The entry's unique id (ULID-backed, so it sorts by creation time).
    pub id: ActivityId,
    /// The recipient the entry belongs to (and is scoped to).
    pub participant_id: ParticipantId,
    /// Short discriminator for the entry, e.g. `"stream_live"`.
    pub kind: String,
    /// The entity that triggered the entry (e.g. the creator who went live), if any.
    pub actor_id: Option<ParticipantId>,
    /// The entity the entry is about (e.g. the stream id), if any. An opaque ULID
    /// with no foreign key, so it may outlive the row it references.
    pub subject_id: Option<Ulid>,
    /// Human-readable summary rendered in the feed.
    pub summary: String,
    /// When the entry was created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    /// When the recipient marked the entry read, or `None` if still unread.
    #[serde(with = "time::serde::rfc3339::option")]
    pub read_at: Option<time::OffsetDateTime>,
}

/// The columns an [`ActivityEntry`] is built from, in select order. Shared by every
/// query so the row decoding stays in one place.
const COLUMNS: &str = "id, participant_id, kind, actor_id, subject_id, summary, created_at, read_at";

type Row = (
    uuid::Uuid,
    uuid::Uuid,
    String,
    Option<uuid::Uuid>,
    Option<uuid::Uuid>,
    String,
    time::OffsetDateTime,
    Option<time::OffsetDateTime>,
);

fn row_to_model(r: Row) -> ActivityEntry {
    let (id, participant_id, kind, actor_id, subject_id, summary, created_at, read_at) = r;
    ActivityEntry {
        id: ActivityId::from_uuid(id),
        participant_id: ParticipantId::from_uuid(participant_id),
        kind,
        actor_id: actor_id.map(ParticipantId::from_uuid),
        subject_id: subject_id.map(|u| Ulid(u.as_u128())),
        summary,
        created_at,
        read_at,
    }
}

/// Repository over the `activity_feed` table (general-purpose per-participant feed).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules and bots build one inline via [`ActivityFeedRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct ActivityFeedRepo {
    pool: PgPool,
}

impl ActivityFeedRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Append an entry to `participant`'s feed, returning its generated id.
    ///
    /// `actor` is the entity that triggered the entry (e.g. the creator who went
    /// live) and `subject` is what it is about (e.g. the stream id); both are
    /// optional and stored as opaque uuids with no foreign key. The caller owns
    /// any de-duplication — this method always inserts.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn insert(
        &self,
        participant: ParticipantId,
        kind: &str,
        actor: Option<ParticipantId>,
        subject: Option<Ulid>,
        summary: &str,
    ) -> Result<ActivityId, sqlx::Error> {
        let id = ActivityId::new();
        sqlx::query(
            r"INSERT INTO activity_feed
                  (id, participant_id, kind, actor_id, subject_id, summary)
               VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id.to_uuid())
        .bind(participant.to_uuid())
        .bind(kind)
        .bind(actor.map(|a| a.to_uuid()))
        .bind(subject.map(|s| uuid::Uuid::from_u128(s.0)))
        .bind(summary)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List `participant`'s feed, newest first. Recipient-scoped — only the
    /// caller's own rows are returned. `before` is an exclusive keyset cursor: pass
    /// the oldest id from the previous page to fetch the next, or `None` for the
    /// first page. `limit` caps the page size.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list(
        &self,
        participant: ParticipantId,
        before: Option<ActivityId>,
        limit: i64,
    ) -> Result<Vec<ActivityEntry>, sqlx::Error> {
        // ULID-backed ids sort by time, and the uuid byte order preserves that
        // ordering, so `id < before` / `id DESC` is a correct newest-first keyset.
        let sql = format!(
            "SELECT {COLUMNS}
               FROM activity_feed
              WHERE participant_id = $1 AND ($2::uuid IS NULL OR id < $2)
              ORDER BY id DESC
              LIMIT $3"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(participant.to_uuid())
            .bind(before.map(|b| b.to_uuid()))
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Count `participant`'s unread feed entries (`read_at IS NULL`).
    /// Recipient-scoped.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn unread_count(&self, participant: ParticipantId) -> Result<i64, sqlx::Error> {
        let (count,): (i64,) = sqlx::query_as(
            r"SELECT COUNT(*) FROM activity_feed
               WHERE participant_id = $1 AND read_at IS NULL",
        )
        .bind(participant.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// Mark `participant`'s unread entries read, returning how many rows changed.
    /// Recipient-scoped. When `before` is `Some`, only entries with `id <= before`
    /// (that one and everything older) are marked, so a client can "mark read up to
    /// here"; when `None`, the whole unread feed is marked. Already-read rows are
    /// left untouched (so the count reflects the actual transition).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn mark_read(
        &self,
        participant: ParticipantId,
        before: Option<ActivityId>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE activity_feed SET read_at = now()
               WHERE participant_id = $1
                 AND read_at IS NULL
                 AND ($2::uuid IS NULL OR id <= $2)",
        )
        .bind(participant.to_uuid())
        .bind(before.map(|b| b.to_uuid()))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored activity_feed
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

    /// Create a throwaway participant so the test is self-contained.
    async fn mk_participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("activity-feed-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn activity_feed_insert_list_count_mark_read_recipient_scoped() {
        let p = pool();
        let repo = ActivityFeedRepo::new(p.clone());
        let me = mk_participant(&p).await;
        let actor = mk_participant(&p).await;
        let other = mk_participant(&p).await;
        let subject = Ulid::new();

        // Insert three entries for `me` (oldest → newest).
        let a = repo
            .insert(me, "stream_live", Some(actor), Some(subject), "first is live")
            .await
            .unwrap();
        let b = repo
            .insert(me, "stream_live", Some(actor), Some(subject), "second is live")
            .await
            .unwrap();
        let c = repo
            .insert(me, "stream_live", Some(actor), Some(subject), "third is live")
            .await
            .unwrap();
        // An entry for someone else must never leak into `me`'s feed.
        repo.insert(other, "stream_live", Some(actor), None, "not mine")
            .await
            .unwrap();

        // List newest first; recipient-scoped.
        let listed = repo.list(me, None, 10).await.unwrap();
        assert_eq!(listed.len(), 3, "only my three entries");
        assert_eq!(listed[0].id, c, "newest first");
        assert_eq!(listed[2].id, a, "oldest last");
        assert_eq!(listed[0].actor_id, Some(actor));
        assert_eq!(listed[0].subject_id, Some(subject));
        assert!(listed.iter().all(|e| e.read_at.is_none()), "all start unread");

        // Keyset pagination: `before = c` skips the newest.
        let page = repo.list(me, Some(c), 10).await.unwrap();
        assert_eq!(page.len(), 2, "before-cursor excludes the newest");
        assert_eq!(page[0].id, b);

        // Unread count reflects all three.
        assert_eq!(repo.unread_count(me).await.unwrap(), 3);

        // Mark read up to `b` (b and older) → a + b flip; c stays unread.
        let marked = repo.mark_read(me, Some(b)).await.unwrap();
        assert_eq!(marked, 2, "b and a marked");
        assert_eq!(repo.unread_count(me).await.unwrap(), 1, "c still unread");

        // A second mark of the same range is a no-op (already read).
        assert_eq!(repo.mark_read(me, Some(b)).await.unwrap(), 0);

        // Mark all remaining read → c flips; feed fully read.
        assert_eq!(repo.mark_read(me, None).await.unwrap(), 1);
        assert_eq!(repo.unread_count(me).await.unwrap(), 0);
        let after = repo.list(me, None, 10).await.unwrap();
        assert!(after.iter().all(|e| e.read_at.is_some()), "all read now");

        // The other participant's feed is untouched.
        assert_eq!(repo.unread_count(other).await.unwrap(), 1);

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM activity_feed WHERE participant_id = $1 OR participant_id = $2")
            .bind(me.to_uuid())
            .bind(other.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
