//! Approval-request repository (Lark 审批 / approvals-lite, single-approver MVP).
//!
//! Backs `migrations/0056_approvals.sql`. A requester opens an approval request
//! addressed to a single approver within a workspace (title + optional details);
//! the named approver then approves or denies it with an optional decision note.
//! Each row starts `pending` and transitions exactly once to `approved`/`denied`,
//! stamping `decided_at`.
//!
//! Creation and decision recheck effective workspace membership in the same
//! transaction as the write. Lists always carry an explicit workspace id, so a
//! path scoped to one tenant cannot return the caller's rows from another.
//!
//! Purely additive: a NEW [`ApprovalRepo`] over a NEW table; no existing repo is
//! touched. The [`Approval`] model lives here (and is re-exported from the crate
//! root) rather than in `aero-common`, since it is a storage-layer projection.

use aero_common::{ApprovalId, ParticipantId, WorkspaceId};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};

/// One approval request — a requester's ask addressed to a single approver,
/// pending that approver's decision.
///
/// A storage-layer projection of an `approvals` row. `Serialize` so a handler can
/// hand the row straight back as JSON; `created_at` renders as RFC 3339, and
/// `decided_at` as RFC 3339 or `null` while still pending.
#[derive(Debug, Clone, Serialize)]
pub struct Approval {
    /// The approval request's unique id.
    pub id: ApprovalId,
    /// The tenant the approval is scoped to.
    pub workspace_id: WorkspaceId,
    /// The participant who opened the request.
    pub requester_id: ParticipantId,
    /// The single participant asked to decide the request.
    pub approver_id: ParticipantId,
    /// Short human-readable title of what is being requested.
    pub title: String,
    /// Optional longer description, or `None` if the requester gave none.
    pub details: Option<String>,
    /// Lifecycle status: `pending`, `approved`, or `denied`.
    pub status: String,
    /// Optional note the approver left with their decision, or `None`.
    pub decision_note: Option<String>,
    /// When the request was decided, or `None` while still pending (RFC 3339 or
    /// `null` on the wire).
    #[serde(with = "time::serde::rfc3339::option")]
    pub decided_at: Option<time::OffsetDateTime>,
    /// When the request was created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// The columns an [`Approval`] is built from, in select order. Shared by every
/// query so the row decoding stays in one place.
const COLUMNS: &str = "id, workspace_id, requester_id, approver_id, title, details, status, \
                       decision_note, decided_at, created_at";

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: uuid::Uuid,
    workspace_id: uuid::Uuid,
    requester_id: uuid::Uuid,
    approver_id: uuid::Uuid,
    title: String,
    details: Option<String>,
    status: String,
    decision_note: Option<String>,
    decided_at: Option<time::OffsetDateTime>,
    created_at: time::OffsetDateTime,
}

fn row_to_model(r: Row) -> Approval {
    Approval {
        id: ApprovalId::from_uuid(r.id),
        workspace_id: WorkspaceId::from_uuid(r.workspace_id),
        requester_id: ParticipantId::from_uuid(r.requester_id),
        approver_id: ParticipantId::from_uuid(r.approver_id),
        title: r.title,
        details: r.details,
        status: r.status,
        decision_note: r.decision_note,
        decided_at: r.decided_at,
        created_at: r.created_at,
    }
}

/// A write was rejected before it could preserve approval tenant containment.
#[derive(Debug, thiserror::Error)]
pub enum ApprovalWriteError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("requester is not an effective workspace member")]
    RequesterNotMember,
    #[error("approver is not an effective workspace member")]
    ApproverNotMember,
    #[error("approval is missing, already decided, or not owned by this approver")]
    NotFound,
    #[error("invalid approval decision status")]
    InvalidStatus,
}

async fn is_effective_workspace_member(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        r"SELECT true
            FROM workspace_members membership
            JOIN workspaces workspace
              ON workspace.id = membership.workspace_id
            JOIN participants participant
              ON participant.id = membership.participant_id
             AND participant.deleted_at IS NULL
           WHERE membership.workspace_id = $1
             AND membership.participant_id = $2
             AND NOT EXISTS (
                 SELECT 1
                   FROM workspace_deactivations deactivated
                  WHERE deactivated.workspace_id = membership.workspace_id
                    AND deactivated.participant_id = membership.participant_id
             )
             AND (
                 participant.kind <> 'human'
                 OR NOT workspace.require_2fa
                 OR EXISTS (
                     SELECT 1
                       FROM totp_secrets totp
                      WHERE totp.participant_id = membership.participant_id
                        AND totp.activated
                 )
             )
           FOR SHARE OF membership, workspace, participant",
    )
    .bind(workspace.to_uuid())
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map(|row| row.is_some())
}

/// Repository over the `approvals` table (approval-request lifecycle).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`ApprovalRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct ApprovalRepo {
    pool: PgPool,
}

impl ApprovalRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Open a new `pending` approval request from `requester` to `approver` in
    /// `workspace`, returning its generated id. Both participants are checked
    /// against the current effective workspace boundary in the insert
    /// transaction.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn create(
        &self,
        workspace: WorkspaceId,
        requester: ParticipantId,
        approver: ParticipantId,
        title: &str,
        details: Option<&str>,
    ) -> Result<ApprovalId, ApprovalWriteError> {
        let mut tx = self.pool.begin().await?;
        if !is_effective_workspace_member(&mut tx, workspace, requester).await? {
            return Err(ApprovalWriteError::RequesterNotMember);
        }
        if !is_effective_workspace_member(&mut tx, workspace, approver).await? {
            return Err(ApprovalWriteError::ApproverNotMember);
        }
        let id = ApprovalId::new();
        sqlx::query(
            r"INSERT INTO approvals (id, workspace_id, requester_id, approver_id, title, details)
               VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id.to_uuid())
        .bind(workspace.to_uuid())
        .bind(requester.to_uuid())
        .bind(approver.to_uuid())
        .bind(title)
        .bind(details)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Fetch one approval request by id, or `None` if no such row exists.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(&self, id: ApprovalId) -> Result<Option<Approval>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM approvals WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// List approval requests addressed TO `approver` (their inbox), newest first.
    /// When `status` is `Some`, only rows in that status are returned (e.g.
    /// `Some("pending")` for the to-decide queue); `None` returns every request.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_approver(
        &self,
        workspace: WorkspaceId,
        approver: ParticipantId,
        status: Option<&str>,
    ) -> Result<Vec<Approval>, sqlx::Error> {
        let rows = if let Some(status) = status {
            let sql = format!(
                "SELECT {COLUMNS} FROM approvals
                  WHERE workspace_id = $1 AND approver_id = $2 AND status = $3
                  ORDER BY created_at DESC, id DESC"
            );
            sqlx::query_as::<_, Row>(&sql)
                .bind(workspace.to_uuid())
                .bind(approver.to_uuid())
                .bind(status)
                .fetch_all(&self.pool)
                .await?
        } else {
            let sql = format!(
                "SELECT {COLUMNS} FROM approvals
                  WHERE workspace_id = $1 AND approver_id = $2
                  ORDER BY created_at DESC, id DESC"
            );
            sqlx::query_as::<_, Row>(&sql)
                .bind(workspace.to_uuid())
                .bind(approver.to_uuid())
                .fetch_all(&self.pool)
                .await?
        };
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// List approval requests opened BY `requester` (their outbox), newest first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_requester(
        &self,
        workspace: WorkspaceId,
        requester: ParticipantId,
    ) -> Result<Vec<Approval>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS} FROM approvals
              WHERE workspace_id = $1 AND requester_id = $2
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(workspace.to_uuid())
            .bind(requester.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Decide a `pending` request: set its `status` (to `approved` / `denied`),
    /// record the optional `note`, stamp `decided_at`, and return the canonical
    /// row. The task row, approver identity, pending state, and current effective
    /// workspace membership are checked in one transaction.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn decide(
        &self,
        id: ApprovalId,
        approver: ParticipantId,
        status: &str,
        note: Option<&str>,
    ) -> Result<Approval, ApprovalWriteError> {
        if !matches!(status, "approved" | "denied") {
            return Err(ApprovalWriteError::InvalidStatus);
        }
        let mut tx = self.pool.begin().await?;
        let workspace = sqlx::query_scalar::<_, uuid::Uuid>(
            r"SELECT workspace_id
                FROM approvals
               WHERE id = $1
                 AND approver_id = $2
                 AND status = 'pending'
               FOR UPDATE",
        )
        .bind(id.to_uuid())
        .bind(approver.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .map(WorkspaceId::from_uuid)
        .ok_or(ApprovalWriteError::NotFound)?;
        if !is_effective_workspace_member(&mut tx, workspace, approver).await? {
            return Err(ApprovalWriteError::NotFound);
        }

        let sql = format!(
            "UPDATE approvals
                SET status = $3, decision_note = $4, decided_at = now()
              WHERE id = $1 AND approver_id = $2 AND status = 'pending'
          RETURNING {COLUMNS}"
        );
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .bind(approver.to_uuid())
            .bind(status)
            .bind(note)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(ApprovalWriteError::NotFound)?;
        tx.commit().await?;
        Ok(row_to_model(row))
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored approval
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    /// The reserved all-zero default workspace, guaranteed to exist by migration
    /// 0006's backfill — reused so the approval rows are well-scoped.
    const DEFAULT_WS: &str = "00000000-0000-0000-0000-000000000000";

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
            .bind(format!("approval-participant-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    fn default_ws() -> WorkspaceId {
        WorkspaceId::from_uuid(uuid::Uuid::parse_str(DEFAULT_WS).expect("valid uuid"))
    }

    async fn enroll(p: &PgPool, workspace: WorkspaceId, participant: ParticipantId) {
        sqlx::query(
            "INSERT INTO workspace_members
                 (workspace_id, participant_id, role, joined_at)
             VALUES ($1, $2, 'member', now())
             ON CONFLICT (workspace_id, participant_id) DO NOTHING",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .execute(p)
        .await
        .expect("enroll approval participant");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn create_approver_sees_pending_then_decide_approves() {
        let p = pool();
        let repo = ApprovalRepo::new(p.clone());
        let ws = default_ws();
        let requester = mk_participant(&p).await;
        let approver = mk_participant(&p).await;
        let stranger = mk_participant(&p).await;
        for participant in [requester, approver, stranger] {
            enroll(&p, ws, participant).await;
        }

        // create → approver sees it pending in their inbox.
        let id = repo
            .create(ws, requester, approver, "PTO request", Some("3 days"))
            .await
            .unwrap();
        let pending = repo
            .list_for_approver(ws, approver, Some("pending"))
            .await
            .unwrap();
        assert!(
            pending.iter().any(|a| a.id == id),
            "approver sees the pending request"
        );
        let got = repo.get(id).await.unwrap().expect("present");
        assert_eq!(got.status, "pending");
        assert_eq!(got.title, "PTO request");
        assert_eq!(got.details.as_deref(), Some("3 days"));
        assert!(got.decided_at.is_none());

        // A non-approver decide is a no-op (false) and leaves the row pending.
        assert!(matches!(
            repo.decide(id, stranger, "approved", None).await,
            Err(ApprovalWriteError::NotFound)
        ));
        assert_eq!(
            repo.get(id).await.unwrap().expect("present").status,
            "pending",
            "stranger's decide left the request pending"
        );

        sqlx::query(
            "INSERT INTO workspace_deactivations
                 (workspace_id, participant_id, deactivated_by)
             VALUES ($1, $2, $3)",
        )
        .bind(ws.to_uuid())
        .bind(approver.to_uuid())
        .bind(requester.to_uuid())
        .execute(&p)
        .await
        .unwrap();
        assert!(matches!(
            repo.decide(id, approver, "approved", Some("blocked")).await,
            Err(ApprovalWriteError::NotFound)
        ));
        sqlx::query(
            "DELETE FROM workspace_deactivations
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(ws.to_uuid())
        .bind(approver.to_uuid())
        .execute(&p)
        .await
        .unwrap();

        // The active named approver decides → approved, with a note and a stamp.
        let decided = repo
            .decide(id, approver, "approved", Some("ok"))
            .await
            .unwrap();
        assert!(matches!(
            repo.decide(id, approver, "denied", None).await,
            Err(ApprovalWriteError::NotFound)
        ));
        assert_eq!(decided.status, "approved");
        assert_eq!(decided.decision_note.as_deref(), Some("ok"));
        assert!(decided.decided_at.is_some());

        // requester sees the (now approved) request in their outbox.
        let outbox = repo.list_for_requester(ws, requester).await.unwrap();
        let mine = outbox
            .iter()
            .find(|a| a.id == id)
            .expect("requester sees it");
        assert_eq!(mine.status, "approved");

        let other_ws = WorkspaceId::new();
        let mut tx = p.begin().await.unwrap();
        sqlx::query(
            "INSERT INTO workspaces (id, name, slug, created_by)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(other_ws.to_uuid())
        .bind("approval-other")
        .bind(format!("approval-other-{other_ws}"))
        .bind(requester.to_uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
        for (participant, role) in [(requester, "owner"), (approver, "member")] {
            sqlx::query(
                "INSERT INTO workspace_members
                     (workspace_id, participant_id, role, joined_at)
                 VALUES ($1, $2, $3, now())",
            )
            .bind(other_ws.to_uuid())
            .bind(participant.to_uuid())
            .bind(role)
            .execute(&mut *tx)
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();
        let other_id = repo
            .create(other_ws, requester, approver, "Other tenant", None)
            .await
            .unwrap();
        assert!(
            !repo
                .list_for_approver(ws, approver, None)
                .await
                .unwrap()
                .iter()
                .any(|approval| approval.id == other_id),
            "workspace-scoped inbox cannot mix another tenant"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM approvals WHERE id = ANY($1)")
            .bind(vec![id.to_uuid(), other_id.to_uuid()])
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(other_ws.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query(
            "DELETE FROM workspace_members
              WHERE workspace_id = $1 AND participant_id = ANY($2)",
        )
        .bind(ws.to_uuid())
        .bind(vec![
            requester.to_uuid(),
            approver.to_uuid(),
            stranger.to_uuid(),
        ])
        .execute(&p)
        .await
        .ok();
        for who in [requester, approver, stranger] {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(who.to_uuid())
                .execute(&p)
                .await
                .ok();
        }
    }
}
