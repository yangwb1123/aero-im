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
use sqlx::{PgPool, Postgres, Transaction};

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
        Self::append_on(&self.pool, workspace, actor, action, target, detail).await
    }

    /// Transaction-scoped [`append`](Self::append) (ROADMAP 第三版 方向五
    /// 审计事务化): the audit INSERT rides the caller's transaction so it
    /// commits or rolls back atomically with the action being audited.
    pub async fn append_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        workspace: WorkspaceId,
        actor: Option<ParticipantId>,
        action: &str,
        target: Option<&str>,
        detail: serde_json::Value,
    ) -> Result<AuditId, sqlx::Error> {
        Self::append_on(&mut **tx, workspace, actor, action, target, detail).await
    }

    /// Shared INSERT for [`append`](Self::append) / [`append_in_tx`](Self::append_in_tx),
    /// generic over the executor (pool vs. open transaction).
    async fn append_on<'e, E>(
        executor: E,
        workspace: WorkspaceId,
        actor: Option<ParticipantId>,
        action: &str,
        target: Option<&str>,
        detail: serde_json::Value,
    ) -> Result<AuditId, sqlx::Error>
    where
        E: sqlx::Executor<'e, Database = Postgres>,
    {
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
        .execute(executor)
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

    // Room + message fixture in `ws`, so the transactional delete+audit tests
    // have something real to soft-delete.
    async fn message_in_workspace(
        p: &PgPool,
        ws: WorkspaceId,
        sender: ParticipantId,
    ) -> aero_common::MessageId {
        let room = aero_common::RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id) VALUES ($1,'group',$2,$3,$4)",
        )
        .bind(room.to_uuid())
        .bind(format!("audit-tx-room-{room}"))
        .bind(sender.to_uuid())
        .bind(ws.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        let repo = crate::message::MessageRepo::new(p.clone());
        repo.insert(crate::message::NewMessage {
            room_id: room,
            sender_id: sender,
            blocks: vec![aero_common::Block::text("to be deleted, audited atomically")],
            reply_to: None,
            metadata: serde_json::json!({}),
        })
        .await
        .expect("insert message")
        .id
    }

    /// ROADMAP 第三版 方向五 审计事务化: `soft_delete_audited` commits the
    /// soft-delete AND the `message.deleted` audit row together; a repeat
    /// delete is a no-op that appends NO second audit row.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn audit_tx_delete_and_audit_commit_together() {
        let p = pool();
        let (ws, actor) = fixture(&p).await;
        let id = message_in_workspace(&p, ws, actor).await;

        let msg_repo = crate::message::MessageRepo::new(p.clone());
        let deleted = msg_repo
            .soft_delete_audited(id, ws, Some(actor), serde_json::json!({ "digest": "x" }))
            .await
            .unwrap();
        assert!(deleted, "first delete deletes");

        let after = msg_repo.get(id).await.unwrap().expect("row still exists");
        assert!(after.deleted_at.is_some(), "message is soft-deleted");

        let trail = AuditRepo::new(p.clone()).list_for_workspace(ws, None, None).await.unwrap();
        let audit_rows: Vec<_> = trail
            .iter()
            .filter(|e| e.action == "message.deleted" && e.target.as_deref() == Some(&id.to_string()))
            .collect();
        assert_eq!(audit_rows.len(), 1, "exactly one audit row committed with the delete");
        assert_eq!(audit_rows[0].actor_id, Some(actor));
        assert_eq!(audit_rows[0].detail["digest"], "x");

        // Idempotent repeat: nothing deleted, so nothing audited.
        let again = msg_repo
            .soft_delete_audited(id, ws, Some(actor), serde_json::json!({ "digest": "x" }))
            .await
            .unwrap();
        assert!(!again, "second delete is a no-op");
        let trail = AuditRepo::new(p.clone()).list_for_workspace(ws, None, None).await.unwrap();
        let repeats = trail
            .iter()
            .filter(|e| e.action == "message.deleted" && e.target.as_deref() == Some(&id.to_string()))
            .count();
        assert_eq!(repeats, 1, "the no-op repeat appended no second audit row");
    }

    /// The rollback half of 审计事务化: when the audit INSERT fails (here via
    /// the `workspace_id` FK — the workspace does not exist), the soft-delete
    /// must roll back with it. The message stays live and unmodified; no
    /// "deleted but unaudited" state can be observed.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn audit_tx_failed_audit_rolls_back_delete() {
        let p = pool();
        let (ws, actor) = fixture(&p).await;
        let id = message_in_workspace(&p, ws, actor).await;

        let msg_repo = crate::message::MessageRepo::new(p.clone());
        let missing_ws = WorkspaceId::new(); // never inserted -> FK violation
        let err = msg_repo
            .soft_delete_audited(id, missing_ws, Some(actor), serde_json::json!({}))
            .await;
        assert!(err.is_err(), "audit insert into a missing workspace must fail");

        let after = msg_repo.get(id).await.unwrap().expect("row still exists");
        assert!(after.deleted_at.is_none(), "soft-delete rolled back with the audit");
        assert!(!after.blocks.is_empty(), "blocks were not cleared");

        let orphaned: i64 = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM audit_events WHERE target = $1",
        )
        .bind(id.to_string())
        .fetch_one(&p)
        .await
        .unwrap()
        .0;
        assert_eq!(orphaned, 0, "no audit row escaped the rolled-back transaction");
    }

    /// Moderation path of 审计事务化 (方向三 closeout): `soft_delete_moderated`
    /// commits the soft-delete AND a `message.moderated` audit row together. The
    /// audit row carries the model reason + content digest and a `None` (system)
    /// actor; a repeat delete is a no-op that appends NO second row.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn moderation_delete_and_audit_commit_together() {
        let p = pool();
        let (ws, actor) = fixture(&p).await;
        let id = message_in_workspace(&p, ws, actor).await;

        let msg_repo = crate::message::MessageRepo::new(p.clone());
        let detail = serde_json::json!({ "reason": "spam", "digest": "buy now" });
        let deleted = msg_repo
            .soft_delete_moderated(id, ws, detail)
            .await
            .unwrap();
        assert!(deleted, "first moderation delete deletes");

        let after = msg_repo.get(id).await.unwrap().expect("row still exists");
        assert!(after.deleted_at.is_some(), "message is soft-deleted");

        let trail = AuditRepo::new(p.clone()).list_for_workspace(ws, None, None).await.unwrap();
        let rows: Vec<_> = trail
            .iter()
            .filter(|e| {
                e.action == "message.moderated" && e.target.as_deref() == Some(&id.to_string())
            })
            .collect();
        assert_eq!(rows.len(), 1, "exactly one message.moderated row committed with the delete");
        assert_eq!(rows[0].actor_id, None, "system-initiated → no actor");
        assert_eq!(rows[0].detail["reason"], "spam");
        assert_eq!(rows[0].detail["digest"], "buy now");

        // Idempotent repeat: nothing deleted, so nothing audited.
        let again = msg_repo
            .soft_delete_moderated(id, ws, serde_json::json!({ "reason": "spam" }))
            .await
            .unwrap();
        assert!(!again, "second moderation delete is a no-op");
        let trail = AuditRepo::new(p.clone()).list_for_workspace(ws, None, None).await.unwrap();
        let repeats = trail
            .iter()
            .filter(|e| {
                e.action == "message.moderated" && e.target.as_deref() == Some(&id.to_string())
            })
            .count();
        assert_eq!(repeats, 1, "the no-op repeat appended no second audit row");
    }

    /// Rollback half of the moderation path: a failing audit INSERT (missing
    /// workspace → FK violation) must roll the moderation soft-delete back with
    /// it, so no "moderated but unaudited" / "deleted but unreviewable" state can
    /// be observed.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn moderation_failed_audit_rolls_back_delete() {
        let p = pool();
        let (ws, actor) = fixture(&p).await;
        let id = message_in_workspace(&p, ws, actor).await;

        let msg_repo = crate::message::MessageRepo::new(p.clone());
        let missing_ws = WorkspaceId::new(); // never inserted -> FK violation
        let err = msg_repo
            .soft_delete_moderated(id, missing_ws, serde_json::json!({ "reason": "x" }))
            .await;
        assert!(err.is_err(), "moderation audit into a missing workspace must fail");

        let after = msg_repo.get(id).await.unwrap().expect("row still exists");
        assert!(after.deleted_at.is_none(), "moderation soft-delete rolled back with the audit");
        assert!(!after.blocks.is_empty(), "blocks were not cleared");

        let orphaned: i64 = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM audit_events WHERE target = $1 AND action = 'message.moderated'",
        )
        .bind(id.to_string())
        .fetch_one(&p)
        .await
        .unwrap()
        .0;
        assert_eq!(orphaned, 0, "no moderation audit row escaped the rolled-back transaction");
    }
}
