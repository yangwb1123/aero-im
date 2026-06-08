//! Approval-request repository (Lark 审批 / approvals-lite, single-approver MVP).
//!
//! Backs `migrations/0056_approvals.sql`. A requester opens an approval request
//! addressed to a single approver within a workspace (title + optional details);
//! the named approver then approves or denies it with an optional decision note.
//! Each row starts `pending` and transitions exactly once to `approved`/`denied`,
//! stamping `decided_at`.
//!
//! [`decide`](ApprovalRepo::decide) is approver-scoped at the SQL layer: only the
//! named approver can decide (the `approver_id` predicate), and only while the
//! request is still `pending` (the `status = 'pending'` guard) — so a stranger's
//! decision, or a second decision, is a no-op returning `false`. Workspace
//! membership of both parties is enforced at the API layer.
//!
//! Purely additive: a NEW [`ApprovalRepo`] over a NEW table; no existing repo is
//! touched. The [`Approval`] model lives here (and is re-exported from the crate
//! root) rather than in `aero-common`, since it is a storage-layer projection.

use aero_common::{ApprovalId, ParticipantId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;

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

type Row = (
    uuid::Uuid,
    uuid::Uuid,
    uuid::Uuid,
    uuid::Uuid,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<time::OffsetDateTime>,
    time::OffsetDateTime,
);

fn row_to_model(r: Row) -> Approval {
    let (
        id,
        workspace_id,
        requester_id,
        approver_id,
        title,
        details,
        status,
        decision_note,
        decided_at,
        created_at,
    ) = r;
    Approval {
        id: ApprovalId::from_uuid(id),
        workspace_id: WorkspaceId::from_uuid(workspace_id),
        requester_id: ParticipantId::from_uuid(requester_id),
        approver_id: ParticipantId::from_uuid(approver_id),
        title,
        details,
        status,
        decision_note,
        decided_at,
        created_at,
    }
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
    /// `workspace`, returning its generated id. The caller is responsible for the
    /// workspace-membership checks (of both parties) and for title validation.
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
    ) -> Result<ApprovalId, sqlx::Error> {
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
        .execute(&self.pool)
        .await?;
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
        approver: ParticipantId,
        status: Option<&str>,
    ) -> Result<Vec<Approval>, sqlx::Error> {
        let rows = if let Some(status) = status {
            let sql = format!(
                "SELECT {COLUMNS} FROM approvals
                  WHERE approver_id = $1 AND status = $2
                  ORDER BY created_at DESC, id DESC"
            );
            sqlx::query_as::<_, Row>(&sql)
                .bind(approver.to_uuid())
                .bind(status)
                .fetch_all(&self.pool)
                .await?
        } else {
            let sql = format!(
                "SELECT {COLUMNS} FROM approvals
                  WHERE approver_id = $1
                  ORDER BY created_at DESC, id DESC"
            );
            sqlx::query_as::<_, Row>(&sql)
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
        requester: ParticipantId,
    ) -> Result<Vec<Approval>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS} FROM approvals
              WHERE requester_id = $1
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(requester.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Decide a `pending` request: set its `status` (to `approved` / `denied`),
    /// record the optional `note`, stamp `decided_at`, and return `true` iff a
    /// pending row was changed. Approver-scoped: the `approver_id = $2` predicate
    /// means only the named approver can decide (a stranger's decide is a no-op
    /// `false`), and the `status = 'pending'` guard makes a second decide a no-op
    /// too — so the decision is idempotent and race-safe. Validating the target
    /// status string is the caller's concern.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn decide(
        &self,
        id: ApprovalId,
        approver: ParticipantId,
        status: &str,
        note: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE approvals
                 SET status = $3, decision_note = $4, decided_at = now()
               WHERE id = $1 AND approver_id = $2 AND status = 'pending'",
        )
        .bind(id.to_uuid())
        .bind(approver.to_uuid())
        .bind(status)
        .bind(note)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
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

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn create_approver_sees_pending_then_decide_approves() {
        let p = pool();
        let repo = ApprovalRepo::new(p.clone());
        let ws = default_ws();
        let requester = mk_participant(&p).await;
        let approver = mk_participant(&p).await;
        let stranger = mk_participant(&p).await;

        // create → approver sees it pending in their inbox.
        let id = repo
            .create(ws, requester, approver, "PTO request", Some("3 days"))
            .await
            .unwrap();
        let pending = repo.list_for_approver(approver, Some("pending")).await.unwrap();
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
        assert!(
            !repo.decide(id, stranger, "approved", None).await.unwrap(),
            "a non-approver cannot decide"
        );
        assert_eq!(
            repo.get(id).await.unwrap().expect("present").status,
            "pending",
            "stranger's decide left the request pending"
        );

        // The named approver decides → approved, with a note and a decision stamp.
        assert!(
            repo.decide(id, approver, "approved", Some("ok")).await.unwrap(),
            "the named approver decides"
        );
        assert!(
            !repo.decide(id, approver, "denied", None).await.unwrap(),
            "second decide is a no-op"
        );
        let decided = repo.get(id).await.unwrap().expect("present");
        assert_eq!(decided.status, "approved");
        assert_eq!(decided.decision_note.as_deref(), Some("ok"));
        assert!(decided.decided_at.is_some());

        // requester sees the (now approved) request in their outbox.
        let outbox = repo.list_for_requester(requester).await.unwrap();
        let mine = outbox.iter().find(|a| a.id == id).expect("requester sees it");
        assert_eq!(mine.status, "approved");

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM approvals WHERE id = $1")
            .bind(id.to_uuid())
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
