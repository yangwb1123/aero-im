//! Scheduled-stream repository (live-event announcements).
//!
//! Backs `migrations/0029_scheduled_streams.sql`. A workspace member announces an
//! upcoming live stream ahead of time (title, optional description, optional
//! associated room, scheduled-for time); members list the upcoming announcements
//! and the creator can cancel one. This is purely the announcement/lifecycle
//! record — actually going live still uses the existing `/api/streams` ingest
//! path, so nothing here touches media transport.
//!
//! Purely additive: a NEW [`ScheduledStreamRepo`]; no existing repo is touched.
//! The [`ScheduledStream`] model lives here (and is re-exported from the crate
//! root) rather than in `aero-common`, since it is a storage-layer projection.
//! Mirrors the [`crate::scheduled`] repo's shape (a repo over a time-ordered
//! table with creator-scoped cancel + RFC 3339 time handling).

use aero_common::{ParticipantId, RoomId, ScheduledStreamId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;

/// Lifecycle status of a scheduled stream, mirroring the `status` text column's
/// domain (`scheduled` | `live` | `canceled` | `ended`).
pub const STATUS_SCHEDULED: &str = "scheduled";
/// Status once the announced stream has started.
pub const STATUS_LIVE: &str = "live";
/// Status once the creator has canceled the (not-yet-started) announcement.
pub const STATUS_CANCELED: &str = "canceled";
/// Status once the announced stream has finished.
pub const STATUS_ENDED: &str = "ended";

/// One scheduled live-stream announcement (upcoming, live, canceled, or ended).
///
/// A storage-layer projection of a `scheduled_streams` row. `Serialize` so a
/// handler can hand the row straight back as JSON; the timestamps render as
/// RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct ScheduledStream {
    /// The announcement's unique id.
    pub id: ScheduledStreamId,
    /// The tenant the announcement belongs to.
    pub workspace_id: WorkspaceId,
    /// Optional room the upcoming stream is associated with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_id: Option<RoomId>,
    /// Human-readable title of the upcoming stream.
    pub title: String,
    /// Optional longer description / agenda.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// When the stream is expected to go live (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub scheduled_for: time::OffsetDateTime,
    /// The participant who created the announcement.
    pub created_by: ParticipantId,
    /// Lifecycle status: `scheduled` | `live` | `canceled` | `ended`.
    pub status: String,
    /// When the announcement was created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// The columns a [`ScheduledStream`] is built from, in select order. Shared by
/// every query so the row decoding stays in one place.
const COLUMNS: &str =
    "id, workspace_id, room_id, title, description, scheduled_for, created_by, status, created_at";

type Row = (
    uuid::Uuid,
    uuid::Uuid,
    Option<uuid::Uuid>,
    String,
    Option<String>,
    time::OffsetDateTime,
    uuid::Uuid,
    String,
    time::OffsetDateTime,
);

fn row_to_model(r: Row) -> ScheduledStream {
    let (id, workspace_id, room_id, title, description, scheduled_for, created_by, status, created_at) =
        r;
    ScheduledStream {
        id: ScheduledStreamId::from_uuid(id),
        workspace_id: WorkspaceId::from_uuid(workspace_id),
        room_id: room_id.map(RoomId::from_uuid),
        title,
        description,
        scheduled_for,
        created_by: ParticipantId::from_uuid(created_by),
        status,
        created_at,
    }
}

/// Repository over the `scheduled_streams` table (live-event announcements).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`ScheduledStreamRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct ScheduledStreamRepo {
    pool: PgPool,
}

impl ScheduledStreamRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persist a new scheduled-stream announcement, returning its generated id.
    /// The caller is responsible for workspace-membership + not-in-the-past
    /// validation. The row starts in status [`STATUS_SCHEDULED`].
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn create(
        &self,
        ws: WorkspaceId,
        room: Option<RoomId>,
        title: &str,
        description: Option<String>,
        scheduled_for: time::OffsetDateTime,
        created_by: ParticipantId,
    ) -> Result<ScheduledStreamId, sqlx::Error> {
        let id = ScheduledStreamId::new();
        sqlx::query(
            r"INSERT INTO scheduled_streams
                  (id, workspace_id, room_id, title, description, scheduled_for, created_by)
               VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id.to_uuid())
        .bind(ws.to_uuid())
        .bind(room.map(|r| r.to_uuid()))
        .bind(title)
        .bind(description)
        .bind(scheduled_for)
        .bind(created_by.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List a workspace's still-upcoming announcements: `status = 'scheduled'`
    /// and `scheduled_for >= now`, soonest first. A canceled, started, ended, or
    /// already-past announcement is excluded.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_upcoming(
        &self,
        ws: WorkspaceId,
        now: time::OffsetDateTime,
    ) -> Result<Vec<ScheduledStream>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM scheduled_streams
              WHERE workspace_id = $1
                AND status = 'scheduled'
                AND scheduled_for >= $2
              ORDER BY scheduled_for ASC, id ASC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(ws.to_uuid())
            .bind(now)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Fetch a single announcement by id, regardless of status, or `None` if no
    /// such row exists.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(
        &self,
        id: ScheduledStreamId,
    ) -> Result<Option<ScheduledStream>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM scheduled_streams WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// Cancel one of the creator's own still-scheduled announcements. Returns
    /// `true` iff a row was canceled — creator-scoped and only while still
    /// `scheduled`, so a member cannot cancel another's announcement, nor one
    /// that has already gone live / ended / been canceled.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn cancel(
        &self,
        id: ScheduledStreamId,
        by: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE scheduled_streams
                 SET status = 'canceled'
               WHERE id = $1
                 AND created_by = $2
                 AND status = 'scheduled'",
        )
        .bind(id.to_uuid())
        .bind(by.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Transition a still-scheduled announcement to `live` (the announced stream
    /// has started). Returns `true` iff a row was updated (it existed and was
    /// still `scheduled`).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn mark_live(&self, id: ScheduledStreamId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE scheduled_streams
                 SET status = 'live'
               WHERE id = $1
                 AND status = 'scheduled'",
        )
        .bind(id.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Transition a `scheduled`-or-`live` announcement to `ended` (the announced
    /// stream has finished). Returns `true` iff a row was updated.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn mark_ended(&self, id: ScheduledStreamId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE scheduled_streams
                 SET status = 'ended'
               WHERE id = $1
                 AND status IN ('scheduled', 'live')",
        )
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
///   cargo test -p aero-storage --lib -- --ignored scheduled_stream
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    /// The reserved all-zero default workspace, guaranteed to exist by migration
    /// 0006's backfill — reused so the announcement rows are well-scoped.
    const DEFAULT_WS: &str = "00000000-0000-0000-0000-000000000000";

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    // Create a throwaway creator participant so the test is self-contained.
    async fn creator(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("sched-stream-creator-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    fn default_ws() -> WorkspaceId {
        WorkspaceId::from_uuid(uuid::Uuid::parse_str(DEFAULT_WS).expect("valid uuid"))
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn scheduled_stream_create_list_cancel() {
        let p = pool();
        let repo = ScheduledStreamRepo::new(p.clone());
        let ws = default_ws();
        let creator = creator(&p).await;
        let now = time::OffsetDateTime::now_utc();

        // A future announcement, and a past-time one (which must NOT be upcoming).
        let future = now + time::Duration::hours(1);
        let id = repo
            .create(ws, None, "Launch keynote", Some("Q3 roadmap".into()), future, creator)
            .await
            .unwrap();
        let past = now - time::Duration::hours(1);
        let past_id = repo
            .create(ws, None, "Yesterday's stream", None, past, creator)
            .await
            .unwrap();

        // The future one appears in the upcoming list; the past one does not.
        let upcoming = repo.list_upcoming(ws, now).await.unwrap();
        assert!(
            upcoming.iter().any(|s| s.id == id),
            "upcoming list includes the future announcement"
        );
        assert!(
            !upcoming.iter().any(|s| s.id == past_id),
            "upcoming list excludes a past-time announcement"
        );
        // The fetched row round-trips its fields.
        let got = repo.get(id).await.unwrap().expect("row exists");
        assert_eq!(got.title, "Launch keynote");
        assert_eq!(got.status, STATUS_SCHEDULED);
        assert_eq!(got.created_by, creator);

        // Cancel is creator-scoped: a stranger can't cancel; the creator can, once.
        let stranger = ParticipantId::new();
        assert!(
            !repo.cancel(id, stranger).await.unwrap(),
            "stranger cannot cancel"
        );
        assert!(repo.cancel(id, creator).await.unwrap(), "creator cancels");
        assert!(
            !repo.cancel(id, creator).await.unwrap(),
            "second cancel is a no-op"
        );

        // After cancel it leaves the upcoming list.
        let after = repo.list_upcoming(ws, now).await.unwrap();
        assert!(
            !after.iter().any(|s| s.id == id),
            "canceled announcement leaves the upcoming list"
        );
        assert_eq!(
            repo.get(id).await.unwrap().expect("row still exists").status,
            STATUS_CANCELED,
            "canceled row carries the canceled status"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM scheduled_streams WHERE created_by = $1")
            .bind(creator.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
