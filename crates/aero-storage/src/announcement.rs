//! Workspace announcement / banner repository (admin-posted, workspace-wide).
//!
//! Backs migrations 0038 and 0207. A workspace admin posts a short banner
//! ([`create_authorized`](AnnouncementRepo::create_authorized)); every effective
//! member reads the currently *active* ones
//! ([`list_active_authorized`](AnnouncementRepo::list_active_authorized)), newest
//! first; an admin may delete one early
//! ([`delete_authorized`](AnnouncementRepo::delete_authorized)). A banner is
//! active until its optional `expires_at` passes — the pure [`is_active`]
//! decision (also applied in SQL) is the single source of truth for that rule.
//!
//! Authorization is transaction-owned rather than delegated to a stale HTTP
//! preflight. Create/delete lock the workspace and recheck current effective
//! Owner/Admin status in the same transaction as the row and audit writes.
//! Listing holds the effective membership boundary through the query. The
//! [`Announcement`] model lives here (and is re-exported from the crate root)
//! rather than in `aero-common`, since it is a storage-layer projection.

use aero_common::{AnnouncementId, Error, ParticipantId, WorkspaceId};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};
use time::OffsetDateTime;

const MAX_BODY_CHARS: usize = 2_000;

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

    /// Persist a new announcement while `created_by` remains an effective
    /// workspace Owner/Admin, returning the exact row inserted by the
    /// transaction.
    ///
    /// The body is stored trimmed and must contain 1..=2000 Unicode scalar
    /// values. An explicit expiry must be strictly later than the transaction's
    /// timestamp. The announcement and its `announcement.create` audit record
    /// commit atomically.
    ///
    /// # Errors
    /// Returns [`Error::Invalid`] for an invalid body/expiry,
    /// [`Error::NotFound`] for a missing workspace, [`Error::Forbidden`] unless
    /// the actor is a current effective Owner/Admin, and propagates storage
    /// errors.
    pub async fn create_authorized(
        &self,
        workspace: WorkspaceId,
        body: &str,
        created_by: ParticipantId,
        expires_at: Option<OffsetDateTime>,
    ) -> Result<Announcement, Error> {
        let body = body.trim();
        if body.is_empty() {
            return Err(Error::Invalid("announcement body must not be empty".into()));
        }
        if body.chars().count() > MAX_BODY_CHARS {
            return Err(Error::Invalid("announcement body too long".into()));
        }

        let mut tx = self.pool.begin().await?;
        crate::workspace::authz::assert_effective_admin_in_tx(&mut tx, workspace, created_by)
            .await?;
        let transaction_now = sqlx::query_scalar::<_, OffsetDateTime>("SELECT CURRENT_TIMESTAMP")
            .fetch_one(&mut *tx)
            .await?;
        if expires_at.is_some_and(|expires_at| expires_at <= transaction_now) {
            return Err(Error::Invalid(
                "announcement expiry must be in the future".into(),
            ));
        }

        let id = AnnouncementId::new();
        let sql = format!(
            "INSERT INTO workspace_announcements
                 (id, workspace_id, body, created_by, expires_at)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING {COLUMNS}"
        );
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .bind(workspace.to_uuid())
            .bind(body)
            .bind(created_by.to_uuid())
            .bind(expires_at)
            .fetch_one(&mut *tx)
            .await?;
        let announcement = row_to_model(row);
        crate::audit::AuditRepo::append_in_tx(
            &mut tx,
            workspace,
            Some(created_by),
            "announcement.create",
            Some(&id.to_string()),
            serde_json::json!({
                "expires_at": announcement.expires_at,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(announcement)
    }

    /// List the *active* announcements in `workspace` as of `now`, newest first.
    /// Effective workspace membership is rechecked and held in the same
    /// transaction as the tenant-scoped query. A row with a past `expires_at` is
    /// excluded (mirrors the pure [`is_active`] rule in SQL).
    ///
    /// # Errors
    /// Returns [`Error::NotFound`] for a missing workspace,
    /// [`Error::Forbidden`] when effective access has been revoked, and
    /// propagates storage errors.
    pub async fn list_active_authorized(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
        now: OffsetDateTime,
    ) -> Result<Vec<Announcement>, Error> {
        let mut tx = self.pool.begin().await?;
        assert_effective_member_in_tx(&mut tx, workspace, participant).await?;
        let sql = format!(
            "SELECT {COLUMNS}
               FROM workspace_announcements
              WHERE workspace_id = $1 AND (expires_at IS NULL OR expires_at > $2)
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(workspace.to_uuid())
            .bind(now)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Delete an announcement while `actor` remains an effective Owner/Admin.
    ///
    /// The resource predicate binds both `id` and `workspace`; a missing,
    /// already-deleted, or cross-tenant id therefore has one opaque not-found
    /// result. The delete and its `announcement.delete` audit record commit
    /// atomically.
    ///
    /// # Errors
    /// Returns [`Error::NotFound`] for a missing workspace/resource,
    /// [`Error::Forbidden`] unless the actor is a current effective Owner/Admin,
    /// and propagates storage errors.
    pub async fn delete_authorized(
        &self,
        id: AnnouncementId,
        workspace: WorkspaceId,
        actor: ParticipantId,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        crate::workspace::authz::assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        let sql = format!(
            "DELETE FROM workspace_announcements
              WHERE id = $1 AND workspace_id = $2
              RETURNING {COLUMNS}"
        );
        let deleted = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .bind(workspace.to_uuid())
            .fetch_optional(&mut *tx)
            .await?
            .map(row_to_model)
            .ok_or_else(|| Error::NotFound(format!("announcement {id}")))?;
        crate::audit::AuditRepo::append_in_tx(
            &mut tx,
            workspace,
            Some(actor),
            "announcement.delete",
            Some(&id.to_string()),
            serde_json::json!({
                "created_by": deleted.created_by,
                "expires_at": deleted.expires_at,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn assert_effective_member_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Result<(), Error> {
    let exists =
        sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR SHARE")
            .bind(workspace.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .is_some();
    if !exists {
        return Err(Error::NotFound(format!("workspace {workspace}")));
    }
    if !crate::workspace::members::effective_workspace_access_in_tx(tx, workspace, participant)
        .await?
    {
        return Err(Error::Forbidden("workspace member required".into()));
    }
    Ok(())
}

#[cfg(test)]
mod is_active_tests {
    use super::is_active;
    use time::{Duration, OffsetDateTime};

    #[test]
    fn no_expiry_is_always_active() {
        let now = OffsetDateTime::now_utc();
        assert!(
            is_active(None, now),
            "a banner with no expiry never expires"
        );
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
    use aero_common::{Error, WorkspaceRole};
    use time::Duration;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn participant(p: &PgPool, label: &str) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("announcement-{label}-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    fn constraint(error: &sqlx::Error) -> Option<&str> {
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint)
    }

    async fn create_workspace(
        p: &PgPool,
        owner: ParticipantId,
        label: &str,
    ) -> (WorkspaceId, crate::WorkspaceRepo) {
        let repo = crate::WorkspaceRepo::new(p.clone());
        let workspace = repo
            .create(
                format!("Announcement {label} {owner}"),
                format!("announcement-{label}-{owner}"),
                owner,
            )
            .await
            .expect("create workspace")
            .id;
        (workspace, repo)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn authorized_announcements_are_tenant_scoped_validated_and_audited() {
        let p = pool();
        let repo = AnnouncementRepo::new(p.clone());
        let owner = participant(&p, "owner").await;
        let member = participant(&p, "member").await;
        let other_owner = participant(&p, "other-owner").await;
        let (workspace, workspaces) = create_workspace(&p, owner, "primary").await;
        let (other_workspace, _) = create_workspace(&p, other_owner, "other").await;
        workspaces
            .add_member(workspace, member, WorkspaceRole::Member)
            .await
            .unwrap();
        let now = OffsetDateTime::now_utc();

        assert!(matches!(
            repo.create_authorized(workspace, " \n ", owner, None).await,
            Err(Error::Invalid(_))
        ));
        let too_long = "界".repeat(MAX_BODY_CHARS + 1);
        assert!(matches!(
            repo.create_authorized(workspace, &too_long, owner, None)
                .await,
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            repo.create_authorized(
                workspace,
                "already expired",
                owner,
                Some(now - Duration::seconds(1)),
            )
            .await,
            Err(Error::Invalid(_))
        ));

        let live = repo
            .create_authorized(
                workspace,
                "  all-hands at 3pm  ",
                owner,
                Some(now + Duration::hours(1)),
            )
            .await
            .unwrap();
        assert_eq!(live.workspace_id, workspace);
        assert_eq!(live.created_by, owner);
        assert_eq!(live.body, "all-hands at 3pm");

        let active = repo
            .list_active_authorized(workspace, member, now)
            .await
            .unwrap();
        assert!(
            active.iter().any(|announcement| announcement.id == live.id),
            "effective members see active banners"
        );
        let create_audits: i64 = sqlx::query_scalar(
            "SELECT count(*)
               FROM audit_events
              WHERE workspace_id = $1
                AND actor_id = $2
                AND action = 'announcement.create'
                AND target = $3",
        )
        .bind(workspace.to_uuid())
        .bind(owner.to_uuid())
        .bind(live.id.to_string())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(create_audits, 1, "create and audit commit together");

        assert!(matches!(
            repo.delete_authorized(live.id, other_workspace, other_owner)
                .await,
            Err(Error::NotFound(_))
        ));
        let retained: bool = sqlx::query_scalar(
            "SELECT EXISTS(
                 SELECT 1 FROM workspace_announcements
                  WHERE id = $1 AND workspace_id = $2
             )",
        )
        .bind(live.id.to_uuid())
        .bind(workspace.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert!(retained, "cross-tenant delete is an opaque no-op");
        let leaked_delete_audit: i64 = sqlx::query_scalar(
            "SELECT count(*)
               FROM audit_events
              WHERE workspace_id = $1
                AND action = 'announcement.delete'
                AND target = $2",
        )
        .bind(other_workspace.to_uuid())
        .bind(live.id.to_string())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(leaked_delete_audit, 0);

        let raw_cross_tenant = sqlx::query(
            "INSERT INTO workspace_announcements
                 (id, workspace_id, body, created_by, expires_at)
             VALUES ($1, $2, 'forged', $3, now() + interval '1 hour')",
        )
        .bind(AnnouncementId::new().to_uuid())
        .bind(other_workspace.to_uuid())
        .bind(owner.to_uuid())
        .execute(&p)
        .await
        .expect_err("raw SQL cannot forge a creator from another workspace");
        assert_eq!(
            constraint(&raw_cross_tenant),
            Some("workspace_announcements_creator_scope_chk")
        );

        let raw_non_admin = sqlx::query(
            "INSERT INTO workspace_announcements
                 (id, workspace_id, body, created_by, expires_at)
             VALUES ($1, $2, 'forged member', $3, now() + interval '1 hour')",
        )
        .bind(AnnouncementId::new().to_uuid())
        .bind(workspace.to_uuid())
        .bind(member.to_uuid())
        .execute(&p)
        .await
        .expect_err("raw SQL requires an effective workspace admin");
        assert_eq!(
            constraint(&raw_non_admin),
            Some("workspace_announcements_creator_scope_chk")
        );

        let raw_identity_change = sqlx::query(
            "UPDATE workspace_announcements
                SET workspace_id = $2, created_by = $3
              WHERE id = $1",
        )
        .bind(live.id.to_uuid())
        .bind(other_workspace.to_uuid())
        .bind(other_owner.to_uuid())
        .execute(&p)
        .await
        .expect_err("retained tenant identity is immutable");
        assert_eq!(
            constraint(&raw_identity_change),
            Some("workspace_announcements_identity_immutable_chk")
        );

        let raw_expired = sqlx::query(
            "INSERT INTO workspace_announcements
                 (id, workspace_id, body, created_by, expires_at)
             VALUES ($1, $2, 'expired', $3, CURRENT_TIMESTAMP)",
        )
        .bind(AnnouncementId::new().to_uuid())
        .bind(workspace.to_uuid())
        .bind(owner.to_uuid())
        .execute(&p)
        .await
        .expect_err("raw SQL observes the strict future-expiry boundary");
        assert_eq!(
            constraint(&raw_expired),
            Some("workspace_announcements_expiry_future_chk")
        );

        repo.delete_authorized(live.id, workspace, owner)
            .await
            .unwrap();
        let delete_audits: i64 = sqlx::query_scalar(
            "SELECT count(*)
               FROM audit_events
              WHERE workspace_id = $1
                AND actor_id = $2
                AND action = 'announcement.delete'
                AND target = $3",
        )
        .bind(workspace.to_uuid())
        .bind(owner.to_uuid())
        .bind(live.id.to_string())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(delete_audits, 1, "delete and audit commit together");
        assert!(matches!(
            repo.delete_authorized(live.id, workspace, owner).await,
            Err(Error::NotFound(_))
        ));
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn admin_revocation_races_leave_zero_writes_and_zero_deletes() {
        let p = pool();
        let repo = AnnouncementRepo::new(p.clone());
        let owner = participant(&p, "race-owner").await;
        let create_admin = participant(&p, "race-create-admin").await;
        let delete_admin = participant(&p, "race-delete-admin").await;
        let (workspace, workspaces) = create_workspace(&p, owner, "race").await;
        workspaces
            .add_member(workspace, create_admin, WorkspaceRole::Admin)
            .await
            .unwrap();
        workspaces
            .add_member(workspace, delete_admin, WorkspaceRole::Admin)
            .await
            .unwrap();

        // Simulate a successful route preflight followed by a committed
        // demotion. The repository must wait behind the workspace fence and
        // observe the new role before inserting either resource or audit row.
        let mut demotion = p.begin().await.unwrap();
        crate::ownership::lock_membership_governance(&mut demotion)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .execute(&mut *demotion)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE workspace_members
                SET role = 'member'
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(create_admin.to_uuid())
        .execute(&mut *demotion)
        .await
        .unwrap();

        let raced_repo = repo.clone();
        let mut raced_create = tokio::spawn(async move {
            raced_repo
                .create_authorized(workspace, "must not commit", create_admin, None)
                .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut raced_create)
                .await
                .is_err(),
            "create waits behind the workspace revocation lock"
        );
        demotion.commit().await.unwrap();
        assert!(matches!(
            raced_create.await.unwrap(),
            Err(Error::Forbidden(_))
        ));
        let raced_create_rows: i64 = sqlx::query_scalar(
            "SELECT count(*)
               FROM workspace_announcements
              WHERE workspace_id = $1 AND body = 'must not commit'",
        )
        .bind(workspace.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(raced_create_rows, 0);
        let raced_create_audits: i64 = sqlx::query_scalar(
            "SELECT count(*)
               FROM audit_events
              WHERE workspace_id = $1
                AND actor_id = $2
                AND action = 'announcement.create'",
        )
        .bind(workspace.to_uuid())
        .bind(create_admin.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(raced_create_audits, 0);

        let delete_target = repo
            .create_authorized(workspace, "retained history", delete_admin, None)
            .await
            .unwrap();
        let mut removal = p.begin().await.unwrap();
        crate::ownership::lock_membership_governance(&mut removal)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .execute(&mut *removal)
            .await
            .unwrap();
        sqlx::query(
            "DELETE FROM workspace_members
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(delete_admin.to_uuid())
        .execute(&mut *removal)
        .await
        .unwrap();

        let raced_repo = repo.clone();
        let mut raced_delete = tokio::spawn(async move {
            raced_repo
                .delete_authorized(delete_target.id, workspace, delete_admin)
                .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut raced_delete)
                .await
                .is_err(),
            "delete waits behind the workspace revocation lock"
        );
        removal.commit().await.unwrap();
        assert!(matches!(
            raced_delete.await.unwrap(),
            Err(Error::Forbidden(_))
        ));

        let retained: bool = sqlx::query_scalar(
            "SELECT EXISTS(
                 SELECT 1
                   FROM workspace_announcements
                  WHERE id = $1 AND workspace_id = $2
             )",
        )
        .bind(delete_target.id.to_uuid())
        .bind(workspace.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert!(
            retained,
            "creator membership removal preserves historical announcements"
        );
        let raced_delete_audits: i64 = sqlx::query_scalar(
            "SELECT count(*)
               FROM audit_events
              WHERE workspace_id = $1
                AND actor_id = $2
                AND action = 'announcement.delete'
                AND target = $3",
        )
        .bind(workspace.to_uuid())
        .bind(delete_admin.to_uuid())
        .bind(delete_target.id.to_string())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(raced_delete_audits, 0);
        assert!(matches!(
            repo.list_active_authorized(workspace, delete_admin, OffsetDateTime::now_utc())
                .await,
            Err(Error::Forbidden(_))
        ));

        let owner_view = repo
            .list_active_authorized(workspace, owner, OffsetDateTime::now_utc())
            .await
            .unwrap();
        assert!(
            owner_view
                .iter()
                .any(|announcement| announcement.id == delete_target.id),
            "another effective member still sees retained history"
        );
    }
}
