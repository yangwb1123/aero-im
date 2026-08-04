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
use sqlx::{PgPool, Postgres, Transaction};

/// Lifecycle status of a scheduled stream, mirroring the `status` text column's
/// domain (`scheduled` | `live` | `canceled` | `ended`).
pub const STATUS_SCHEDULED: &str = "scheduled";
/// Status once the announced stream has started.
pub const STATUS_LIVE: &str = "live";
/// Status once the creator has canceled the (not-yet-started) announcement.
pub const STATUS_CANCELED: &str = "canceled";
/// Status once the announced stream has finished.
pub const STATUS_ENDED: &str = "ended";
pub const MAX_SCHEDULED_STREAM_TITLE_CHARS: usize = 256;
pub const MAX_SCHEDULED_STREAM_DESCRIPTION_CHARS: usize = 4_000;
pub const MAX_UPCOMING_SCHEDULED_STREAMS_PAGE: i64 = 200;
pub const MAX_SCHEDULED_STREAMS_PER_CREATOR: i64 = 100;

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
    let (
        id,
        workspace_id,
        room_id,
        title,
        description,
        scheduled_for,
        created_by,
        status,
        created_at,
    ) = r;
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

/// A scheduled-stream write failed its tenant-containment checks.
#[derive(Debug, thiserror::Error)]
pub enum ScheduledStreamWriteError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("creator is not an effective workspace member")]
    CreatorNotMember,
    #[error("creator cannot access the requested room in this workspace")]
    RoomNotAccessible,
    #[error("scheduled stream is missing, no longer mutable, or not owned by this creator")]
    NotFound,
    #[error("scheduled stream input is invalid: {0}")]
    InvalidInput(String),
    #[error("scheduled stream limit reached")]
    LimitReached,
}

async fn has_effective_workspace_access(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT aero_effective_workspace_access($1, $2)")
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .fetch_one(&mut **tx)
        .await
}

async fn has_effective_room_access(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    room: RoomId,
    participant: ParticipantId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT aero_effective_room_access($1, $2, $3)")
        .bind(room.to_uuid())
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .fetch_one(&mut **tx)
        .await
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
    /// Effective workspace and optional room membership are locked and checked
    /// in the same transaction as the insert. The row starts in status
    /// [`STATUS_SCHEDULED`].
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
    ) -> Result<ScheduledStreamId, ScheduledStreamWriteError> {
        if title.trim().is_empty() || title.chars().count() > MAX_SCHEDULED_STREAM_TITLE_CHARS {
            return Err(ScheduledStreamWriteError::InvalidInput(format!(
                "title must contain 1..={MAX_SCHEDULED_STREAM_TITLE_CHARS} characters"
            )));
        }
        if description
            .as_deref()
            .is_some_and(|value| value.chars().count() > MAX_SCHEDULED_STREAM_DESCRIPTION_CHARS)
        {
            return Err(ScheduledStreamWriteError::InvalidInput(format!(
                "description must be at most {MAX_SCHEDULED_STREAM_DESCRIPTION_CHARS} characters"
            )));
        }
        if scheduled_for <= time::OffsetDateTime::now_utc() {
            return Err(ScheduledStreamWriteError::InvalidInput(
                "scheduled_for must be in the future".into(),
            ));
        }

        let mut tx = self.pool.begin().await?;
        crate::ownership::lock_membership_governance(&mut tx).await?;
        let workspace_exists =
            sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
                .bind(ws.to_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        if !workspace_exists {
            return Err(ScheduledStreamWriteError::CreatorNotMember);
        }
        if let Some(room) = room {
            if !has_effective_room_access(&mut tx, ws, room, created_by).await? {
                return Err(ScheduledStreamWriteError::RoomNotAccessible);
            }
        } else if !has_effective_workspace_access(&mut tx, ws, created_by).await? {
            return Err(ScheduledStreamWriteError::CreatorNotMember);
        }
        let still_future =
            sqlx::query_scalar::<_, bool>("SELECT $1::timestamptz > clock_timestamp()")
                .bind(scheduled_for)
                .fetch_one(&mut *tx)
                .await?;
        if !still_future {
            return Err(ScheduledStreamWriteError::InvalidInput(
                "scheduled_for must be in the future".into(),
            ));
        }
        let active = sqlx::query_scalar::<_, i64>(
            r"SELECT count(*)
                FROM scheduled_streams
               WHERE workspace_id = $1
                 AND created_by = $2
                 AND status = 'scheduled'
                 AND scheduled_for > clock_timestamp()",
        )
        .bind(ws.to_uuid())
        .bind(created_by.to_uuid())
        .fetch_one(&mut *tx)
        .await?;
        if active >= MAX_SCHEDULED_STREAMS_PER_CREATOR {
            return Err(ScheduledStreamWriteError::LimitReached);
        }

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
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
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
        limit: i64,
    ) -> Result<Vec<ScheduledStream>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM scheduled_streams
              WHERE workspace_id = $1
                AND status = 'scheduled'
                AND scheduled_for >= $2
              ORDER BY scheduled_for ASC, id ASC
              LIMIT $3"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(ws.to_uuid())
            .bind(now)
            .bind(limit.clamp(1, MAX_UPCOMING_SCHEDULED_STREAMS_PAGE))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Fetch a single announcement by id, regardless of status, or `None` if no
    /// such row exists.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(&self, id: ScheduledStreamId) -> Result<Option<ScheduledStream>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM scheduled_streams WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// Cancel one of the creator's own still-scheduled announcements. The
    /// creator and optional room relationship are revalidated while the row is
    /// locked. Missing, unauthorized, or non-scheduled rows all return the same
    /// opaque [`ScheduledStreamWriteError::NotFound`].
    ///
    /// # Errors
    /// Returns [`ScheduledStreamWriteError`] for either a database failure or a
    /// failed authorization/state check.
    pub async fn cancel(
        &self,
        id: ScheduledStreamId,
        by: ParticipantId,
    ) -> Result<(), ScheduledStreamWriteError> {
        let mut tx = self.pool.begin().await?;
        // Resolve the authorization scope without locking the announcement.
        // The canonical helper must acquire workspace/room/member locks before
        // this aggregate row, matching revocation and tenant-cascade writers.
        let resolved = sqlx::query_as::<_, (uuid::Uuid, Option<uuid::Uuid>, uuid::Uuid, String)>(
            r"SELECT workspace_id, room_id, created_by, status
                FROM scheduled_streams
               WHERE id = $1",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some((workspace, room, created_by, status)) = resolved else {
            return Err(ScheduledStreamWriteError::NotFound);
        };
        if created_by != by.to_uuid() || status != STATUS_SCHEDULED {
            return Err(ScheduledStreamWriteError::NotFound);
        }

        let workspace = WorkspaceId::from_uuid(workspace);
        let allowed = if let Some(room) = room {
            has_effective_room_access(&mut tx, workspace, RoomId::from_uuid(room), by).await?
        } else {
            has_effective_workspace_access(&mut tx, workspace, by).await?
        };
        if !allowed {
            return Err(ScheduledStreamWriteError::NotFound);
        }

        let locked = sqlx::query_as::<_, (uuid::Uuid, Option<uuid::Uuid>, uuid::Uuid, String)>(
            r"SELECT workspace_id, room_id, created_by, status
                FROM scheduled_streams
               WHERE id = $1
               FOR UPDATE",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        if locked != Some((workspace.to_uuid(), room, created_by, status)) {
            // The unlocked values only selected the lock route. Revalidate the
            // aggregate identity and lifecycle state after acquiring its lock.
            return Err(ScheduledStreamWriteError::NotFound);
        }

        let result = sqlx::query(
            r"UPDATE scheduled_streams
                 SET status = 'canceled'
               WHERE id = $1
                 AND created_by = $2
                 AND status = 'scheduled'",
        )
        .bind(id.to_uuid())
        .bind(by.to_uuid())
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Err(ScheduledStreamWriteError::NotFound);
        }
        tx.commit().await?;
        Ok(())
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

#[cfg(test)]
#[path = "scheduled_stream/audit_tests.rs"]
mod audit_tests;

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored scheduled_stream
/// ```
#[cfg(test)]
mod db_tests {
    use aero_common::{RoomKind, StreamProtocol, WorkspaceRole};
    use sqlx::postgres::PgConnectOptions;

    use crate::{stream::NewStream, MarkLiveOutcome, RoomRepo, StreamRepo, StreamWriteError};

    use super::*;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn tagged_pool(application_name: &str) -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        let options = url
            .parse::<PgConnectOptions>()
            .expect("valid DATABASE_URL")
            .application_name(application_name);
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("connect tagged stream test pool")
    }

    async fn wait_until_tagged_query_waits_on_lock(pool: &PgPool, application_name: &str) {
        for _ in 0..100 {
            let waiting = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                     SELECT 1
                       FROM pg_stat_activity
                      WHERE datname = current_database()
                        AND application_name = $1
                        AND wait_event_type = 'Lock'
                 )",
            )
            .bind(application_name)
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("tagged stream transaction never reached its expected lock wait");
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

    async fn workspace(p: &PgPool, creator: ParticipantId, label: &str) -> WorkspaceId {
        let id = WorkspaceId::new();
        let mut tx = p.begin().await.expect("begin workspace fixture");
        sqlx::query("INSERT INTO workspaces (id, name, slug, created_by) VALUES ($1, $2, $3, $4)")
            .bind(id.to_uuid())
            .bind(label)
            .bind(format!("{label}-{id}"))
            .bind(creator.to_uuid())
            .execute(&mut *tx)
            .await
            .expect("insert workspace");
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1, $2, $3)",
        )
        .bind(id.to_uuid())
        .bind(creator.to_uuid())
        .bind(WorkspaceRole::Owner.as_str())
        .execute(&mut *tx)
        .await
        .expect("insert workspace owner");
        tx.commit().await.expect("commit workspace fixture");
        id
    }

    async fn member(p: &PgPool, workspace: WorkspaceId, participant: ParticipantId) {
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role) \
             VALUES ($1, $2, $3)",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(WorkspaceRole::Member.as_str())
        .execute(p)
        .await
        .expect("insert workspace member");
    }

    async fn room(p: &PgPool, workspace: WorkspaceId, participant: ParticipantId) -> RoomId {
        RoomRepo::new(p.clone())
            .create_in_workspace(
                workspace,
                RoomKind::Group,
                Some("stream room".into()),
                participant,
            )
            .await
            .expect("insert room with its owner atomically")
            .id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn scheduled_stream_create_list_cancel() {
        let p = pool();
        let repo = ScheduledStreamRepo::new(p.clone());
        let creator = creator(&p).await;
        let ws = workspace(&p, creator, "scheduled-basic").await;
        let now = time::OffsetDateTime::now_utc();

        // A future announcement is accepted; past schedules are rejected at the
        // repository boundary rather than becoming permanently dead rows.
        let future = now + time::Duration::hours(1);
        let id = repo
            .create(
                ws,
                None,
                "Launch keynote",
                Some("Q3 roadmap".into()),
                future,
                creator,
            )
            .await
            .unwrap();
        assert!(matches!(
            repo.create(
                ws,
                None,
                "Yesterday's stream",
                None,
                now - time::Duration::hours(1),
                creator,
            )
            .await,
            Err(ScheduledStreamWriteError::InvalidInput(_))
        ));

        // The future one appears in the bounded upcoming list.
        let upcoming = repo.list_upcoming(ws, now, 100).await.unwrap();
        assert!(
            upcoming.iter().any(|s| s.id == id),
            "upcoming list includes the future announcement"
        );
        // The fetched row round-trips its fields.
        let got = repo.get(id).await.unwrap().expect("row exists");
        assert_eq!(got.title, "Launch keynote");
        assert_eq!(got.status, STATUS_SCHEDULED);
        assert_eq!(got.created_by, creator);

        // Cancel is creator-scoped: a stranger can't cancel; the creator can, once.
        let stranger = ParticipantId::new();
        assert!(
            matches!(
                repo.cancel(id, stranger).await,
                Err(ScheduledStreamWriteError::NotFound)
            ),
            "stranger cannot cancel"
        );
        repo.cancel(id, creator).await.expect("creator cancels");
        assert!(
            matches!(
                repo.cancel(id, creator).await,
                Err(ScheduledStreamWriteError::NotFound)
            ),
            "second cancel is rejected"
        );

        // After cancel it leaves the upcoming list.
        let after = repo.list_upcoming(ws, now, 100).await.unwrap();
        assert!(
            !after.iter().any(|s| s.id == id),
            "canceled announcement leaves the upcoming list"
        );
        assert_eq!(
            repo.get(id)
                .await
                .unwrap()
                .expect("row still exists")
                .status,
            STATUS_CANCELED,
            "canceled row carries the canceled status"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM scheduled_streams WHERE created_by = $1")
            .bind(creator.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(ws.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(creator.to_uuid())
            .execute(&p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn optional_room_links_are_transactionally_tenant_contained() {
        let p = pool();
        let scheduled = ScheduledStreamRepo::new(p.clone());
        let streams = StreamRepo::new(p.clone());
        let owner = creator(&p).await;
        let actor = creator(&p).await;
        let outsider = creator(&p).await;
        let ws_a = workspace(&p, owner, "stream-scope-a").await;
        let ws_b = workspace(&p, owner, "stream-scope-b").await;
        member(&p, ws_a, actor).await;
        member(&p, ws_b, actor).await;
        member(&p, ws_a, outsider).await;
        let room_a = room(&p, ws_a, actor).await;
        let room_b = room(&p, ws_b, actor).await;
        let future = time::OffsetDateTime::now_utc() + time::Duration::hours(1);

        let linked = streams
            .create(NewStream {
                owner_id: actor,
                room_id: Some(room_a),
                title: "contained".into(),
                protocol: StreamProtocol::Rtmp,
                stream_key: None,
            })
            .await
            .expect("valid room-linked stream");
        let unscoped = streams
            .create(NewStream {
                owner_id: outsider,
                room_id: None,
                title: "roomless".into(),
                protocol: StreamProtocol::Whip,
                stream_key: None,
            })
            .await
            .expect("NULL room remains legal");
        assert!(matches!(
            streams
                .create(NewStream {
                    owner_id: outsider,
                    room_id: Some(room_a),
                    title: "not a room member".into(),
                    protocol: StreamProtocol::Srt,
                    stream_key: None,
                })
                .await,
            Err(StreamWriteError::RoomNotAccessible)
        ));

        let announcement = scheduled
            .create(ws_a, Some(room_a), "valid", None, future, actor)
            .await
            .expect("valid scheduled stream");
        assert!(matches!(
            scheduled
                .create(ws_a, Some(room_b), "cross tenant", None, future, actor)
                .await,
            Err(ScheduledStreamWriteError::RoomNotAccessible)
        ));

        sqlx::query(
            "INSERT INTO totp_secrets
                 (participant_id, secret, activated, activated_at)
             VALUES ($1, 'scheduled-stream-workspace-owner', true, now())",
        )
        .bind(owner.to_uuid())
        .execute(&p)
        .await
        .expect("enroll workspace owner");
        sqlx::query("UPDATE workspaces SET require_2fa = true WHERE id = $1")
            .bind(ws_a.to_uuid())
            .execute(&p)
            .await
            .expect("require 2fa");
        assert!(matches!(
            streams
                .create(NewStream {
                    owner_id: actor,
                    room_id: Some(room_a),
                    title: "missing 2fa".into(),
                    protocol: StreamProtocol::Rtmp,
                    stream_key: None,
                })
                .await,
            Err(StreamWriteError::RoomNotAccessible)
        ));
        sqlx::query("UPDATE workspaces SET require_2fa = false WHERE id = $1")
            .bind(ws_a.to_uuid())
            .execute(&p)
            .await
            .expect("restore 2fa policy");

        // Simulate access being revoked after an HTTP preflight. The same lock
        // used by repository writes fences a concurrent deactivation insert;
        // once revocation commits, later ingest/cancel rechecks reject it.
        assert!(
            sqlx::query_scalar::<_, bool>("SELECT aero_effective_room_access($1, $2, $3)")
                .bind(room_a.to_uuid())
                .bind(actor.to_uuid())
                .bind(ws_a.to_uuid())
                .fetch_one(&p)
                .await
                .expect("preflight")
        );
        let mut guarded_write = p.begin().await.expect("begin guarded write");
        assert!(
            sqlx::query_scalar::<_, bool>("SELECT aero_effective_room_access($1, $2, $3)")
                .bind(room_a.to_uuid())
                .bind(actor.to_uuid())
                .bind(ws_a.to_uuid())
                .fetch_one(&mut *guarded_write)
                .await
                .expect("transactional access check")
        );
        let revoke_pool = p.clone();
        let mut revoke = tokio::spawn(async move {
            sqlx::query(
                "INSERT INTO workspace_deactivations \
                 (workspace_id, participant_id, deactivated_by) VALUES ($1, $2, $3)",
            )
            .bind(ws_a.to_uuid())
            .bind(actor.to_uuid())
            .bind(owner.to_uuid())
            .execute(&revoke_pool)
            .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut revoke)
                .await
                .is_err(),
            "revocation waits for the guarded write transaction"
        );
        guarded_write.commit().await.expect("commit guarded write");
        tokio::time::timeout(std::time::Duration::from_secs(2), revoke)
            .await
            .expect("revocation unblocked")
            .expect("revoke task")
            .expect("revoke actor");
        assert_eq!(
            streams
                .mark_live(linked.id, "/hls/revoked/index.m3u8")
                .await
                .expect("opaque ingest result"),
            MarkLiveOutcome::NotFound
        );
        assert!(matches!(
            scheduled.cancel(announcement, actor).await,
            Err(ScheduledStreamWriteError::NotFound)
        ));

        // Direct SQL cannot bypass the repository containment checks.
        let raw_stream = sqlx::query("UPDATE streams SET room_id = $1 WHERE id = $2")
            .bind(room_a.to_uuid())
            .bind(uuid::Uuid::from_u128(unscoped.id.0))
            .execute(&p)
            .await;
        assert!(
            raw_stream.is_err(),
            "stream trigger rejects inaccessible room"
        );
        let raw_scheduled = sqlx::query("UPDATE scheduled_streams SET room_id = $1 WHERE id = $2")
            .bind(room_b.to_uuid())
            .bind(announcement.to_uuid())
            .execute(&p)
            .await;
        assert!(
            raw_scheduled.is_err(),
            "scheduled trigger rejects cross-workspace room"
        );

        sqlx::query("DELETE FROM streams WHERE owner_id IN ($1, $2)")
            .bind(actor.to_uuid())
            .bind(outsider.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM workspaces WHERE id IN ($1, $2)")
            .bind(ws_a.to_uuid())
            .bind(ws_b.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id IN ($1, $2, $3)")
            .bind(actor.to_uuid())
            .bind(outsider.to_uuid())
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn room_linked_transitions_take_workspace_before_aggregate_locks() {
        let p = pool();
        let owner = creator(&p).await;
        let workspace = workspace(&p, owner, "stream-lock-order").await;
        let room = room(&p, workspace, owner).await;
        let stream = StreamRepo::new(p.clone())
            .create(NewStream {
                owner_id: owner,
                room_id: Some(room),
                title: "lock ordered".into(),
                protocol: StreamProtocol::Whip,
                stream_key: None,
            })
            .await
            .expect("create linked stream");
        let announcement = ScheduledStreamRepo::new(p.clone())
            .create(
                workspace,
                Some(room),
                "lock ordered",
                None,
                time::OffsetDateTime::now_utc() + time::Duration::hours(1),
                owner,
            )
            .await
            .expect("create linked announcement");

        let mark_application = format!("stream-mark-lock-order-{owner}");
        let mark_repo = StreamRepo::new(tagged_pool(&mark_application).await);
        let mut governance = p.begin().await.unwrap();
        sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .execute(&mut *governance)
            .await
            .unwrap();
        let mark = tokio::spawn(async move {
            mark_repo
                .mark_live(stream.id, "/hls/lock-order/index.m3u8")
                .await
        });
        wait_until_tagged_query_waits_on_lock(&p, &mark_application).await;
        sqlx::query("SET LOCAL lock_timeout = '500ms'")
            .execute(&mut *governance)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM streams WHERE id = $1 FOR UPDATE")
            .bind(uuid::Uuid::from_u128(stream.id.0))
            .execute(&mut *governance)
            .await
            .expect("workspace-blocked mark-live must not already hold the stream row");
        governance.commit().await.unwrap();
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(3), mark)
                .await
                .expect("mark-live completed")
                .expect("mark-live task")
                .expect("mark-live write"),
            MarkLiveOutcome::Started(_)
        ));

        let cancel_application = format!("scheduled-cancel-lock-order-{owner}");
        let cancel_repo = ScheduledStreamRepo::new(tagged_pool(&cancel_application).await);
        let mut governance = p.begin().await.unwrap();
        sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .execute(&mut *governance)
            .await
            .unwrap();
        let cancel = tokio::spawn(async move { cancel_repo.cancel(announcement, owner).await });
        wait_until_tagged_query_waits_on_lock(&p, &cancel_application).await;
        sqlx::query("SET LOCAL lock_timeout = '500ms'")
            .execute(&mut *governance)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM scheduled_streams WHERE id = $1 FOR UPDATE")
            .bind(announcement.to_uuid())
            .execute(&mut *governance)
            .await
            .expect("workspace-blocked cancel must not already hold the scheduled row");
        governance.commit().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), cancel)
            .await
            .expect("cancel completed")
            .expect("cancel task")
            .expect("cancel write");

        sqlx::query("DELETE FROM stream_go_live_outbox WHERE stream_id = $1")
            .bind(uuid::Uuid::from_u128(stream.id.0))
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM streams WHERE id = $1")
            .bind(uuid::Uuid::from_u128(stream.id.0))
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(workspace.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migrations through 0223"]
    async fn scheduled_stream_creation_uses_wall_clock_after_workspace_lock_wait() {
        let p = pool();
        let creator = creator(&p).await;
        let workspace = workspace(&p, creator, "scheduled-wall-clock").await;
        let mut workspace_guard = p.begin().await.unwrap();
        sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .execute(&mut *workspace_guard)
            .await
            .unwrap();

        let announcement = ScheduledStreamId::new();
        let scheduled_for = time::OffsetDateTime::now_utc() + time::Duration::milliseconds(700);
        let raw_pool = p.clone();
        let mut raw_insert = tokio::spawn(async move {
            sqlx::query(
                r"INSERT INTO scheduled_streams
                      (id, workspace_id, title, scheduled_for, created_by)
                   VALUES ($1, $2, 'waited announcement', $3, $4)",
            )
            .bind(announcement.to_uuid())
            .bind(workspace.to_uuid())
            .bind(scheduled_for)
            .bind(creator.to_uuid())
            .execute(&raw_pool)
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !raw_insert.is_finished(),
            "raw insert waits on the canonical workspace"
        );
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        workspace_guard.commit().await.unwrap();
        let error = tokio::time::timeout(std::time::Duration::from_secs(3), &mut raw_insert)
            .await
            .expect("raw insert unblocked")
            .expect("raw insert task")
            .expect_err("wall-clock time must reject the waited insert");
        assert_eq!(
            error
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .as_deref(),
            Some("22023")
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migrations through 0223"]
    async fn scheduled_stream_legacy_text_does_not_block_lifecycle_updates() {
        let p = pool();
        let creator = creator(&p).await;
        let workspace = workspace(&p, creator, "scheduled-legacy-text").await;
        let mut tx = p.begin().await.unwrap();
        sqlx::query(
            "CREATE TEMP TABLE legacy_scheduled_stream
                 (LIKE scheduled_streams INCLUDING ALL)
             ON COMMIT DROP",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        let announcement = ScheduledStreamId::new();
        sqlx::query(
            r"INSERT INTO legacy_scheduled_stream
                  (id, workspace_id, title, description, scheduled_for, created_by)
               VALUES ($1, $2, 'legacy', $3, $4, $5)",
        )
        .bind(announcement.to_uuid())
        .bind(workspace.to_uuid())
        .bind("x".repeat(MAX_SCHEDULED_STREAM_DESCRIPTION_CHARS + 1))
        .bind(time::OffsetDateTime::now_utc() + time::Duration::hours(1))
        .bind(creator.to_uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            r"CREATE TRIGGER legacy_scheduled_stream_fence
                 BEFORE INSERT OR UPDATE ON legacy_scheduled_stream
                 FOR EACH ROW EXECUTE FUNCTION scheduled_stream_resource_fence()",
        )
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE legacy_scheduled_stream
                SET status = 'canceled'
              WHERE id = $1",
        )
        .bind(announcement.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("unchanged legacy text must not block cancellation");
        tx.commit().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migrations through 0223"]
    async fn scheduled_stream_inputs_pages_and_per_creator_work_are_bounded() {
        let p = pool();
        let repo = ScheduledStreamRepo::new(p.clone());
        let creator = creator(&p).await;
        let workspace = workspace(&p, creator, "scheduled-resource-bounds").await;
        let base = time::OffsetDateTime::now_utc() + time::Duration::hours(1);

        assert!(matches!(
            repo.create(
                workspace,
                None,
                "oversized",
                Some("x".repeat(MAX_SCHEDULED_STREAM_DESCRIPTION_CHARS + 1)),
                base,
                creator,
            )
            .await,
            Err(ScheduledStreamWriteError::InvalidInput(_))
        ));

        for index in 0..MAX_SCHEDULED_STREAMS_PER_CREATOR {
            repo.create(
                workspace,
                None,
                &format!("bounded-{index}"),
                None,
                base + time::Duration::minutes(index),
                creator,
            )
            .await
            .unwrap();
        }
        assert!(matches!(
            repo.create(
                workspace,
                None,
                "one too many",
                None,
                base + time::Duration::days(1),
                creator,
            )
            .await,
            Err(ScheduledStreamWriteError::LimitReached)
        ));
        assert_eq!(
            repo.list_upcoming(workspace, time::OffsetDateTime::now_utc(), 7)
                .await
                .unwrap()
                .len(),
            7
        );

        let raw = sqlx::query(
            r"INSERT INTO scheduled_streams
                  (id, workspace_id, title, description, scheduled_for, created_by)
               VALUES ($1, $2, 'raw', $3, $4, $5)",
        )
        .bind(ScheduledStreamId::new().to_uuid())
        .bind(workspace.to_uuid())
        .bind("x".repeat(MAX_SCHEDULED_STREAM_DESCRIPTION_CHARS + 1))
        .bind(base)
        .bind(creator.to_uuid())
        .execute(&p)
        .await
        .expect_err("raw oversized description must be rejected");
        assert_eq!(
            raw.as_database_error()
                .and_then(sqlx::error::DatabaseError::code)
                .as_deref(),
            Some("22023")
        );
    }
}
