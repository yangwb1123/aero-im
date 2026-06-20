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

    /// Hard-delete audit events older than `cutoff` (data-lifecycle retention
    /// sweep, ROADMAP5 方向四). `audit_events` is append-only with no prior
    /// retention; it only ever shrank when a workspace was hard-deleted (FK
    /// cascade), so it grew without bound. Audit retention is a compliance
    /// window — once an event ages past it the row is no longer required and is
    /// purged to bound the table. Returns the number of rows deleted.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn sweep_before(
        &self,
        cutoff: time::OffsetDateTime,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(r"DELETE FROM audit_events WHERE created_at < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    /// Maintain the daily `RANGE` partitions of `audit_events` (migration 0146):
    /// pre-create the next few days' partitions and DROP daily partitions older
    /// than `keep_days`.
    ///
    /// This just invokes the idempotent server-side function
    /// `ensure_audit_event_partitions(keep_days, ahead)` defined in migration 0146,
    /// so the create/drop logic lives in one place (SQL) and the retention loop only
    /// has to call it each cycle. The catch-all `DEFAULT` partition guarantees
    /// inserts never fail even between maintenance runs, so a transiently-failing
    /// call is non-fatal (the next tick retries).
    ///
    /// `keep_days` SHOULD match the audit-retention window passed to
    /// [`sweep_before`](Self::sweep_before): a daily partition is only dropped once
    /// every row in it is past retention, so dropping the whole partition replaces
    /// the row-by-row DELETE with a metadata-only reclaim. `ahead` is how many days
    /// of future partitions to pre-create (3 is plenty for an hourly sweep).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the function call.
    pub async fn ensure_partitions(
        &self,
        keep_days: i32,
        ahead: i32,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT ensure_audit_event_partitions($1, $2)")
            .bind(keep_days)
            .bind(ahead)
            .execute(&self.pool)
            .await?;
        Ok(())
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

    /// Filtered, AND-composed audit listing for a workspace — search/filter/export
    /// (operability). Every supplied criterion narrows the result (logical AND);
    /// an omitted criterion (`None`) does not constrain. Always tenant-scoped to
    /// `workspace` and ordered newest-first, capped via [`clamp_limit`].
    ///
    /// * `action` — exact action token (`"member.add"`).
    /// * `actor` — exact actor participant id (`None`/system actions never match a
    ///   `Some(actor)` filter).
    /// * `target` — exact target string.
    /// * `since` / `until` — inclusive `created_at` lower / upper bounds (UTC).
    ///
    /// The predicate uses the SQL idiom `($n IS NULL OR col = $n)` per criterion so
    /// one query covers every combination without string-building (no injection
    /// surface).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    #[allow(clippy::too_many_arguments)]
    pub async fn list_for_workspace_filtered(
        &self,
        workspace: WorkspaceId,
        action: Option<&str>,
        actor: Option<ParticipantId>,
        target: Option<&str>,
        since: Option<time::OffsetDateTime>,
        until: Option<time::OffsetDateTime>,
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
                 AND ($2::text  IS NULL OR action = $2)
                 AND ($3::uuid  IS NULL OR actor_id = $3)
                 AND ($4::text  IS NULL OR target = $4)
                 AND ($5::timestamptz IS NULL OR created_at >= $5)
                 AND ($6::timestamptz IS NULL OR created_at <= $6)
               ORDER BY id DESC
               LIMIT $7",
        )
        .bind(workspace.to_uuid())
        .bind(action)
        .bind(actor.map(|a| a.to_uuid()))
        .bind(target)
        .bind(since)
        .bind(until)
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

/// RFC 4180-ish CSV-escape a single field: wrap in double quotes and double any
/// embedded quote whenever the value contains a comma, quote, CR or LF (always
/// safe to over-quote, but we keep simple values bare for readability). Pure, so
/// the escaping is unit-tested without a DB.
#[must_use]
pub fn csv_escape(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

/// The CSV header row for an audit export.
pub const AUDIT_CSV_HEADER: &str = "timestamp,actor,action,target,details";

/// Render audit `events` to a CSV string (header + one row each):
/// `timestamp,actor,action,target,details`. The timestamp is RFC 3339, the
/// `details` column is the compact JSON of the event's `detail`, every field
/// CSV-escaped via [`csv_escape`]. Pure (the events are an argument), so the
/// shape is unit-tested without a DB.
#[must_use]
pub fn events_to_csv(events: &[AuditEvent]) -> String {
    let mut out = String::from(AUDIT_CSV_HEADER);
    out.push('\n');
    for e in events {
        let ts = e
            .created_at
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let actor = e.actor_id.map(|a| a.to_string()).unwrap_or_default();
        let target = e.target.clone().unwrap_or_default();
        let details = serde_json::to_string(&e.detail).unwrap_or_default();
        out.push_str(&csv_escape(&ts));
        out.push(',');
        out.push_str(&csv_escape(&actor));
        out.push(',');
        out.push_str(&csv_escape(&e.action));
        out.push(',');
        out.push_str(&csv_escape(&target));
        out.push(',');
        out.push_str(&csv_escape(&details));
        out.push('\n');
    }
    out
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

    // ----- csv_escape: RFC 4180-ish quoting -----

    #[test]
    fn csv_escape_leaves_simple_values_bare() {
        assert_eq!(csv_escape("member.add"), "member.add");
        assert_eq!(csv_escape("01H..."), "01H...");
        assert_eq!(csv_escape(""), "");
    }

    #[test]
    fn csv_escape_quotes_and_doubles_special_chars() {
        // Comma ⇒ quoted.
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        // Embedded quote ⇒ quoted + doubled.
        assert_eq!(csv_escape("he said \"hi\""), "\"he said \"\"hi\"\"\"");
        // Newline ⇒ quoted (keeps the row intact).
        assert_eq!(csv_escape("line1\nline2"), "\"line1\nline2\"");
        assert_eq!(csv_escape("cr\rlf"), "\"cr\rlf\"");
    }

    // ----- events_to_csv: header + rows, JSON details, escaping -----

    fn sample_event(action: &str, actor: Option<ParticipantId>, detail: serde_json::Value) -> AuditEvent {
        AuditEvent {
            id: AuditId::new(),
            workspace_id: WorkspaceId::new(),
            actor_id: actor,
            action: action.to_string(),
            target: Some("tgt".to_string()),
            detail,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn events_to_csv_emits_header_then_rows() {
        let actor = ParticipantId::new();
        let csv = events_to_csv(&[
            sample_event("member.add", Some(actor), serde_json::json!({ "role": "member" })),
            sample_event("workspace.create", None, serde_json::json!({})),
        ]);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], AUDIT_CSV_HEADER, "first line is the header");
        assert_eq!(lines.len(), 3, "header + 2 rows");
        // The timestamp is RFC3339 (UNIX_EPOCH).
        assert!(lines[1].starts_with("1970-01-01T00:00:00Z,"));
        // The actor id appears for the first row, blank for the system row.
        assert!(lines[1].contains(&actor.to_string()));
        // The details JSON is escaped because it contains a comma-free object but
        // a quote (`{"role":"member"}` has quotes) ⇒ quoted+doubled.
        assert!(lines[1].contains("\"\"role\"\":\"\"member\"\""));
        // System actor row has an empty actor field (two consecutive commas).
        assert!(lines[2].contains(",,workspace.create,"));
    }

    #[test]
    fn events_to_csv_empty_is_header_only() {
        let csv = events_to_csv(&[]);
        assert_eq!(csv, format!("{AUDIT_CSV_HEADER}\n"));
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

    /// Filtered listing: by `action` returns only the matching subset; by `actor`
    /// excludes other actors and system rows; `since` bounds by time. CSV export
    /// emits a header plus one row per event.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn audit_filtered_list_and_csv_export() {
        let p = pool();
        let repo = AuditRepo::new(p.clone());
        let (ws, actor) = fixture(&p).await;
        let other = ParticipantId::new();
        // `other` must reference a real participant — audit_events.actor_id is FK-constrained.
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(other.to_uuid())
            .bind(format!("audit-other-{other}"))
            .execute(&p)
            .await
            .expect("insert other participant");

        repo.append(ws, Some(actor), "member.add", Some("t1"), serde_json::json!({ "role": "member" }))
            .await
            .unwrap();
        repo.append(ws, Some(actor), "member.remove", Some("t1"), serde_json::json!({}))
            .await
            .unwrap();
        repo.append(ws, Some(other), "member.add", Some("t2"), serde_json::json!({}))
            .await
            .unwrap();
        repo.append(ws, None, "workspace.create", None, serde_json::json!({}))
            .await
            .unwrap();

        // Filter by action=member.add ⇒ the two adds only.
        let adds = repo
            .list_for_workspace_filtered(ws, Some("member.add"), None, None, None, None, Some(50))
            .await
            .unwrap();
        assert_eq!(adds.len(), 2, "two member.add rows");
        assert!(adds.iter().all(|e| e.action == "member.add"));

        // Filter by action AND actor ⇒ only `actor`'s add.
        let actor_adds = repo
            .list_for_workspace_filtered(ws, Some("member.add"), Some(actor), None, None, None, None)
            .await
            .unwrap();
        assert_eq!(actor_adds.len(), 1, "only the actor's add");
        assert_eq!(actor_adds[0].actor_id, Some(actor));

        // Filter by target=t2 ⇒ only the third row.
        let t2 = repo
            .list_for_workspace_filtered(ws, None, None, Some("t2"), None, None, None)
            .await
            .unwrap();
        assert_eq!(t2.len(), 1);
        assert_eq!(t2[0].target.as_deref(), Some("t2"));

        // `since` in the future ⇒ nothing.
        let future = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
        let none = repo
            .list_for_workspace_filtered(ws, None, None, None, Some(future), None, None)
            .await
            .unwrap();
        assert!(none.is_empty(), "nothing newer than a future `since`");

        // CSV export of all four rows: header + 4 rows.
        let all = repo.list_for_workspace_filtered(ws, None, None, None, None, None, None).await.unwrap();
        let csv = events_to_csv(&all);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], AUDIT_CSV_HEADER);
        assert_eq!(lines.len(), 1 + all.len(), "header + one row per event");
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
            expires_at: None,
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

    /// `audit_events` is partitioned (migration 0146) and the maintenance call is
    /// idempotent: calling `ensure_partitions` repeatedly creates the future daily
    /// partitions once and is a clean no-op thereafter, and an append still routes
    /// to the right partition (so the parent stays writable). We also confirm the
    /// parent really is a partitioned table (relkind = 'p') so this test would fail
    /// if 0146 had not converted it.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn audit_partition_maintenance_is_idempotent_and_writable() {
        let p = pool();
        let repo = AuditRepo::new(p.clone());
        let (ws, actor) = fixture(&p).await;

        // The parent must be a partitioned table after migration 0146.
        let relkind: String = sqlx::query_scalar(
            "SELECT c.relkind::text FROM pg_class c
              JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relname = 'audit_events' AND n.nspname = 'public'",
        )
        .fetch_one(&p)
        .await
        .expect("audit_events exists");
        assert_eq!(relkind, "p", "audit_events is a partitioned (relkind=p) table");

        // First maintenance pass creates yesterday..today+3 daily partitions.
        repo.ensure_partitions(365, 3).await.expect("first maintenance pass");
        let count_parts = || async {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM pg_inherits i
                   JOIN pg_class c ON c.oid = i.inhrelid
                   JOIN pg_class pp ON pp.oid = i.inhparent
                  WHERE pp.relname = 'audit_events'
                    AND c.relname ~ '^audit_events_[0-9]{8}$'",
            )
            .fetch_one(&p)
            .await
            .unwrap()
        };
        let after_first = count_parts().await;
        assert!(after_first >= 4, "at least yesterday..today+3 daily partitions exist");

        // Re-running is a no-op: the count does not change.
        repo.ensure_partitions(365, 3).await.expect("second maintenance pass");
        assert_eq!(count_parts().await, after_first, "idempotent: no new partitions");

        // The parent is still writable and the row is retrievable (routed into a
        // daily partition by created_at).
        let id = repo
            .append(ws, Some(actor), "workspace.create", None, serde_json::json!({}))
            .await
            .expect("append into partitioned parent");
        let events = repo.list_for_workspace(ws, None, Some(10)).await.unwrap();
        assert!(events.iter().any(|e| e.id == id), "appended row is listed back");
    }
}
