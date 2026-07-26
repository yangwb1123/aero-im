//! Workspace announcement / banner repository (admin-posted, workspace-wide).
//!
//! Backs `migrations/0038_workspace_announcements.sql`. A workspace admin posts a
//! short banner ([`create`](AnnouncementRepo::create)); every member of the
//! workspace reads the currently *active* ones
//! ([`list_active`](AnnouncementRepo::list_active)), newest first; an admin may
//! delete one early ([`delete`](AnnouncementRepo::delete)). A banner is active
//! until its optional `expires_at` passes — the pure [`is_active`] decision (also
//! applied in SQL) is the single source of truth for that rule.
//!
//! Every read/mutate method is workspace-scoped (`workspace_id` in the `WHERE`),
//! so a caller can only ever see or delete banners for the tenant they pass in;
//! the HTTP layer ([`crate`]'s server counterpart) gates `create`/`delete` on the
//! admin role and `list_active` on workspace membership. Purely additive: a NEW
//! [`AnnouncementRepo`]; no existing repo is touched. The [`Announcement`] model
//! lives here (and is re-exported from the crate root) rather than in
//! `aero-common`, since it is a storage-layer projection.

use aero_common::{AnnouncementId, ParticipantId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;

/// Whether a banner with the given `expires_at` is still active as of `now`.
///
/// `None` (no expiry) is always active. Otherwise active only while `expires_at`
/// is strictly in the future (`expires_at > now`). Pure, so the boundary
/// behaviour is unit-tested offline.
#[must_use]
pub fn is_active(expires_at: Option<OffsetDateTime>, now: OffsetDateTime) -> bool {
    match expires_at {
        None => true,
        Some(at) => at > now,
    }
}

/// One workspace announcement — an admin-posted, workspace-wide banner.
///
/// A storage-layer projection of a `workspace_announcements` row. `Serialize` so
/// a handler can hand the row straight back as JSON; both timestamps render as
/// RFC 3339 (`expires_at` as `null` when the banner never expires).
#[derive(Debug, Clone, Serialize)]
pub struct Announcement {
    /// The announcement's unique id.
    pub id: AnnouncementId,
    /// The tenant the announcement is scoped to (only its members read it).
    pub workspace_id: WorkspaceId,
    /// The banner text shown to members.
    pub body: String,
    /// The admin who posted the announcement.
    pub created_by: ParticipantId,
    /// When the announcement was posted (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Optional auto-expiry; `None` means the banner never expires (RFC 3339 /
    /// `null` on the wire). See [`is_active`].
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
}

/// The columns an [`Announcement`] is built from, in select order. Shared by
/// every query so the row decoding stays in one place.
const COLUMNS: &str = "id, workspace_id, body, created_by, created_at, expires_at";

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: uuid::Uuid,
    workspace_id: uuid::Uuid,
    body: String,
    created_by: uuid::Uuid,
    created_at: OffsetDateTime,
    expires_at: Option<OffsetDateTime>,
}

fn row_to_model(r: Row) -> Announcement {
    Announcement {
        id: AnnouncementId::from_uuid(r.id),
        workspace_id: WorkspaceId::from_uuid(r.workspace_id),
        body: r.body,
        created_by: ParticipantId::from_uuid(r.created_by),
        created_at: r.created_at,
        expires_at: r.expires_at,
    }
}

/// Repository over the `workspace_announcements` table (admin-posted banners).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`AnnouncementRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct AnnouncementRepo {
    pool: PgPool,
}

impl AnnouncementRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persist a new announcement in `workspace`, returning its generated id. The
    /// caller is responsible for the admin-role check and body validation;
    /// `expires_at` is the optional auto-expiry (`None` ⇒ never expires).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn create(
        &self,
        workspace: WorkspaceId,
        body: &str,
        created_by: ParticipantId,
        expires_at: Option<OffsetDateTime>,
    ) -> Result<AnnouncementId, sqlx::Error> {
        let id = AnnouncementId::new();
        sqlx::query(
            r"INSERT INTO workspace_announcements (id, workspace_id, body, created_by, expires_at)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id.to_uuid())
        .bind(workspace.to_uuid())
        .bind(body)
        .bind(created_by.to_uuid())
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List the *active* announcements in `workspace` as of `now`, newest first.
    /// Workspace-scoped, and filtered to live banners — a row with a past
    /// `expires_at` is excluded (mirrors the pure [`is_active`] rule in SQL).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_active(
        &self,
        workspace: WorkspaceId,
        now: OffsetDateTime,
    ) -> Result<Vec<Announcement>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM workspace_announcements
              WHERE workspace_id = $1 AND (expires_at IS NULL OR expires_at > $2)
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(workspace.to_uuid())
            .bind(now)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Fetch one announcement by id, regardless of expiry. Unlike
    /// [`list_active`](Self::list_active) this does NOT filter out expired rows, so
    /// a handler can echo back a freshly-created banner even when it was created
    /// already-expired (e.g. a zero-second TTL). Returns `None` if no such row.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(&self, id: AnnouncementId) -> Result<Option<Announcement>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM workspace_announcements WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// Delete an announcement by id within `workspace`. Returns `true` iff a row
    /// was removed — workspace-scoped, so a banner from another tenant (or an
    /// unknown id) is a no-op returning `false`, as is a second delete.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn delete(
        &self,
        id: AnnouncementId,
        workspace: WorkspaceId,
    ) -> Result<bool, sqlx::Error> {
        let result =
            sqlx::query("DELETE FROM workspace_announcements WHERE id = $1 AND workspace_id = $2")
                .bind(id.to_uuid())
                .bind(workspace.to_uuid())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod is_active_tests {
    use super::is_active;
    use time::{Duration, OffsetDateTime};

    #[test]
    fn no_expiry_is_always_active() {
        let now = OffsetDateTime::now_utc();
        assert!(is_active(None, now), "a banner with no expiry never expires");
    }

    #[test]
    fn future_expiry_is_active_past_expiry_is_not() {
        let now = OffsetDateTime::now_utc();
        assert!(
            is_active(Some(now + Duration::hours(1)), now),
            "a future expiry is still active"
        );
        assert!(
            !is_active(Some(now - Duration::hours(1)), now),
            "a past expiry is no longer active"
        );
        // Boundary: an expiry exactly at `now` is not active (strict `>`).
        assert!(!is_active(Some(now), now), "expiry at now is not active");
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored announcement
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use time::Duration;

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

    /// Create a throwaway author participant so the test is self-contained.
    async fn author(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("announcement-author-{id}"))
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
    async fn announcement_create_list_active_delete() {
        let p = pool();
        let repo = AnnouncementRepo::new(p.clone());
        let ws = default_ws();
        let author = author(&p).await;
        let now = OffsetDateTime::now_utc();

        // create (no expiry) → shows in list_active.
        let live = repo
            .create(ws, "all-hands at 3pm", author, None)
            .await
            .unwrap();
        // create (already expired) → must NOT show in list_active.
        let expired = repo
            .create(ws, "old notice", author, Some(now - Duration::hours(1)))
            .await
            .unwrap();

        let active = repo.list_active(ws, now).await.unwrap();
        assert!(
            active.iter().any(|a| a.id == live),
            "non-expiring banner is active"
        );
        assert!(
            !active.iter().any(|a| a.id == expired),
            "a past-expiry banner is not active"
        );
        let found = active.iter().find(|a| a.id == live).expect("present");
        assert_eq!(found.body, "all-hands at 3pm");
        assert_eq!(found.created_by, author);

        // delete removes (and is workspace-scoped + idempotent).
        assert!(repo.delete(live, ws).await.unwrap(), "delete removes the row");
        assert!(
            !repo.delete(live, ws).await.unwrap(),
            "second delete is a no-op"
        );
        assert!(
            !repo.list_active(ws, now).await.unwrap().iter().any(|a| a.id == live),
            "deleted banner leaves the active list"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM workspace_announcements WHERE created_by = $1")
            .bind(author.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
