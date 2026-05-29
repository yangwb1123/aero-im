//! Workspace audit trail repository (ROADMAP 方向一 合规).
//!
//! Backs `migrations/0007_audit.sql`. An append-only record of security-relevant
//! workspace administration — member add/remove, role changes, workspace
//! creation — scoped by `workspace_id`. Listing is always tenant-scoped, so one
//! workspace's trail can never surface another's.
//!
//! Purely additive: a NEW [`AuditRepo`]; no existing repo is touched. The
//! `id` is a ULID stored as UUID (time-sortable), so reverse-chronological
//! listing is a keyset walk on `id` descending.

use aero_common::{AuditId, ParticipantId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;

/// One recorded administrative action.
#[derive(Debug, Clone, Serialize)]
pub struct AuditEvent {
    pub id: AuditId,
    pub workspace_id: WorkspaceId,
    /// The participant who performed the action (`None` for system actions).
    pub actor_id: Option<ParticipantId>,
    /// A stable dotted action token, e.g. `"member.add"`.
    pub action: String,
    /// The object acted upon (e.g. an affected participant id), if any.
    pub target: Option<String>,
    /// Free-form structured context (role, old/new values, …).
    pub detail: serde_json::Value,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// Largest page an audit listing will return, regardless of requested `limit`.
const MAX_PAGE: i64 = 200;

/// Clamp a requested page size into `1..=MAX_PAGE` (defaulting `None`/non-positive
/// to `MAX_PAGE`). Pure so it unit-tests without a DB.
fn clamp_limit(requested: Option<i64>) -> i64 {
    match requested {
        Some(n) if n >= 1 => n.min(MAX_PAGE),
        _ => MAX_PAGE,
    }
}

#[derive(Clone)]
pub struct AuditRepo {
    pool: PgPool,
}

impl AuditRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Append one audit event, returning its generated id.
    pub async fn append(
        &self,
        workspace: WorkspaceId,
        actor: Option<ParticipantId>,
        action: &str,
        target: Option<&str>,
        detail: serde_json::Value,
    ) -> Result<AuditId, sqlx::Error> {
        let id = AuditId::new();
        let created_at = time::OffsetDateTime::now_utc();
        sqlx::query(
            r"INSERT INTO audit_events (id, workspace_id, actor_id, action, target, detail, created_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id.to_uuid())
        .bind(workspace.to_uuid())
        .bind(actor.map(|p| p.to_uuid()))
        .bind(action)
        .bind(target)
        .bind(sqlx::types::Json(detail))
        .bind(created_at)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List a workspace's events, newest first, paginated by a keyset cursor.
    ///
    /// `before` is an exclusive upper bound (pass the oldest id from the previous
    /// page to fetch the next). Always filtered to `workspace` — tenant-scoped.
    pub async fn list_for_workspace(
        &self,
        workspace: WorkspaceId,
        before: Option<AuditId>,
        limit: Option<i64>,
    ) -> Result<Vec<AuditEvent>, sqlx::Error> {
        let limit = clamp_limit(limit);
        let rows = sqlx::query_as::<
            _,
            (
                uuid::Uuid,
                uuid::Uuid,
                Option<uuid::Uuid>,
                String,
                Option<String>,
                serde_json::Value,
                time::OffsetDateTime,
            ),
        >(
            r"SELECT id, workspace_id, actor_id, action, target, detail, created_at
               FROM audit_events
               WHERE workspace_id = $1
                 AND ($2::uuid IS NULL OR id < $2)
               ORDER BY id DESC
               LIMIT $3",
        )
        .bind(workspace.to_uuid())
        .bind(before.map(|a| a.to_uuid()))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(id, ws, actor, action, target, detail, created_at)| AuditEvent {
                    id: AuditId::from_uuid(id),
                    workspace_id: WorkspaceId::from_uuid(ws),
                    actor_id: actor.map(ParticipantId::from_uuid),
                    action,
                    target,
                    detail,
                    created_at,
                },
            )
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_limit_defaults_and_bounds() {
        assert_eq!(clamp_limit(None), MAX_PAGE);
        assert_eq!(clamp_limit(Some(0)), MAX_PAGE);
        assert_eq!(clamp_limit(Some(-5)), MAX_PAGE);
        assert_eq!(clamp_limit(Some(1)), 1);
        assert_eq!(clamp_limit(Some(50)), 50);
        assert_eq!(clamp_limit(Some(10_000)), MAX_PAGE);
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored audit_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{ParticipantId, WorkspaceId};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    // Create a throwaway workspace + actor so the test is self-contained.
    async fn fixture(repo_pool: &PgPool) -> (WorkspaceId, ParticipantId) {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(actor.to_uuid())
            .bind(format!("audit-actor-{actor}"))
            .execute(repo_pool)
            .await
            .expect("insert participant");
        let ws = WorkspaceId::new();
        sqlx::query("INSERT INTO workspaces (id, name, slug, created_by, created_at) VALUES ($1,$2,$3,$4, now())")
            .bind(ws.to_uuid())
            .bind("Audit Test WS")
            .bind(format!("audit-{ws}"))
            .bind(actor.to_uuid())
            .execute(repo_pool)
            .await
            .expect("insert workspace");
        (ws, actor)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn audit_append_then_list_is_newest_first() {
        let p = pool();
        let repo = AuditRepo::new(p.clone());
        let (ws, actor) = fixture(&p).await;

        repo.append(ws, Some(actor), "workspace.create", None, serde_json::json!({}))
            .await
            .unwrap();
        let target = ParticipantId::new();
        repo.append(
            ws,
            Some(actor),
            "member.add",
            Some(&target.to_string()),
            serde_json::json!({ "role": "member" }),
        )
        .await
        .unwrap();

        let events = repo.list_for_workspace(ws, None, Some(10)).await.unwrap();
        assert_eq!(events.len(), 2, "both events listed");
        assert_eq!(events[0].action, "member.add", "newest first");
        assert_eq!(events[1].action, "workspace.create");
        assert_eq!(events[0].detail["role"], "member");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn audit_list_is_tenant_scoped() {
        let p = pool();
        let repo = AuditRepo::new(p.clone());
        let (ws_a, actor) = fixture(&p).await;
        let (ws_b, _) = fixture(&p).await;

        repo.append(ws_a, Some(actor), "member.add", None, serde_json::json!({}))
            .await
            .unwrap();

        // Workspace B's trail must never include workspace A's event.
        let b_events = repo.list_for_workspace(ws_b, None, None).await.unwrap();
        assert!(
            b_events.iter().all(|e| e.workspace_id == ws_b),
            "listing is strictly scoped to the requested workspace"
        );
        assert!(
            !b_events.iter().any(|e| e.workspace_id == ws_a),
            "workspace A's events never leak into workspace B's trail"
        );
    }
}
