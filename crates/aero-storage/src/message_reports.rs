//! Message-report repository — user-initiated reports feeding a workspace
//! moderation review queue.
//!
//! Backs `migrations/0093_message_reports.sql`. A room member flags a message they
//! find abusive/spammy/etc.; the report lands here as `pending` and surfaces in a
//! workspace-admin review queue, where an administrator either KEEPs (dismisses the
//! report) or REMOVEs (the handler then soft-deletes the message via the existing
//! transactional moderate-delete path). This is the human-review counterpart to the
//! AI moderation pipeline, which auto-soft-deletes async with no human in the loop.
//!
//! A report is a HISTORICAL record: there is no FK to `messages`, so a report row
//! outlives the message it references (existence is validated by the handler at
//! report time, not by a foreign key — same lesson as ban appeals). The repo is
//! purely additive: a NEW table, no existing repo is touched. The [`MessageReport`]
//! model lives here (and is re-exported from the crate root), a storage-layer
//! projection.
//!
//! [`review`](MessageReportRepo::review) is RETURNING-idempotent: it stamps the
//! decision (`status` + reviewer + `reviewed_at` + note) on a STILL-`pending` row in
//! one statement, returning whether a row actually transitioned. A second review of
//! an already-decided (or unknown) report touches nothing and returns `false`, so a
//! double-submit can never re-trigger the moderate-delete side effect.

use aero_common::{MessageId, MessageReportId, ParticipantId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;

/// One user-filed message report (a row in the moderation review queue).
///
/// A storage-layer projection of a `message_reports` row. `Serialize` so a handler
/// can hand the row straight back as JSON; the timestamps render as RFC 3339.
#[derive(Debug, Clone, Serialize)]
pub struct MessageReport {
    /// The report's unique id.
    pub id: MessageReportId,
    /// The workspace the reported message's room belongs to (queue scope).
    pub workspace_id: WorkspaceId,
    /// The reported message (may since have been removed — no FK).
    pub message_id: MessageId,
    /// The participant who filed the report.
    pub reporter_id: ParticipantId,
    /// Free-text reason the reporter supplied.
    pub reason: String,
    /// Review lifecycle: `pending` -> `kept` | `removed`.
    pub status: String,
    /// Optional reviewer note recorded at decision time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The administrator who reviewed it; `None` while pending.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewed_by: Option<ParticipantId>,
    /// When the decision was stamped; `None` while pending.
    #[serde(with = "time::serde::rfc3339::option", skip_serializing_if = "Option::is_none")]
    pub reviewed_at: Option<time::OffsetDateTime>,
    /// When the report was filed (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// The columns a [`MessageReport`] is built from, in select order. Shared by every
/// query so the row decoding stays in one place.
const COLUMNS: &str = "id, workspace_id, message_id, reporter_id, reason, status, note, \
                       reviewed_by, reviewed_at, created_at";

type Row = (
    uuid::Uuid,
    uuid::Uuid,
    uuid::Uuid,
    uuid::Uuid,
    String,
    String,
    Option<String>,
    Option<uuid::Uuid>,
    Option<time::OffsetDateTime>,
    time::OffsetDateTime,
);

fn row_to_model(r: Row) -> MessageReport {
    let (id, workspace_id, message_id, reporter_id, reason, status, note, reviewed_by, reviewed_at, created_at) =
        r;
    MessageReport {
        id: MessageReportId::from_uuid(id),
        workspace_id: WorkspaceId::from_uuid(workspace_id),
        message_id: MessageId::from_uuid(message_id),
        reporter_id: ParticipantId::from_uuid(reporter_id),
        reason,
        status,
        note,
        reviewed_by: reviewed_by.map(ParticipantId::from_uuid),
        reviewed_at,
        created_at,
    }
}

/// Repository over the `message_reports` table (the moderation review queue).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`MessageReportRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct MessageReportRepo {
    pool: PgPool,
}

impl MessageReportRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// File a new report, returning the freshly inserted (`pending`) row. The caller
    /// is responsible for resolving the message's `workspace`, asserting the reporter
    /// may access the room, and validating the message exists (there is no FK).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert / read-back.
    pub async fn report(
        &self,
        workspace: WorkspaceId,
        message: MessageId,
        reporter: ParticipantId,
        reason: &str,
    ) -> Result<MessageReport, sqlx::Error> {
        let id = MessageReportId::new();
        let sql = format!(
            "INSERT INTO message_reports
                 (id, workspace_id, message_id, reporter_id, reason)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING {COLUMNS}"
        );
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .bind(workspace.to_uuid())
            .bind(message.to_uuid())
            .bind(reporter.to_uuid())
            .bind(reason)
            .fetch_one(&self.pool)
            .await?;
        Ok(row_to_model(row))
    }

    /// List the workspace's PENDING reports, oldest first (so an administrator works
    /// the backlog FIFO). Reviewed reports drop out of the queue.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_pending(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<MessageReport>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM message_reports
              WHERE workspace_id = $1 AND status = 'pending'
              ORDER BY created_at ASC, id ASC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(workspace.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Fetch a single report by id (any status), or `None` if unknown.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(
        &self,
        id: MessageReportId,
    ) -> Result<Option<MessageReport>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM message_reports WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// Record an administrator's decision on a report, RETURNING-idempotently.
    ///
    /// Stamps `status` (`removed` when `remove`, else `kept`), the `reviewer`,
    /// `reviewed_at = now()`, and the optional `note` on a STILL-`pending` row in a
    /// single statement. Returns `true` exactly when a row transitioned from
    /// `pending`; `false` if the report is unknown or already reviewed. The
    /// `WHERE status = 'pending'` guard makes a double-submit a no-op `false`, so the
    /// caller can safely fire the moderate-delete side effect only on a `true`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn review(
        &self,
        id: MessageReportId,
        reviewer: ParticipantId,
        remove: bool,
        note: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        let status = if remove { "removed" } else { "kept" };
        let row = sqlx::query_as::<_, (uuid::Uuid,)>(
            "UPDATE message_reports
                SET status = $2,
                    reviewed_by = $3,
                    reviewed_at = now(),
                    note = $4
              WHERE id = $1 AND status = 'pending'
            RETURNING id",
        )
        .bind(id.to_uuid())
        .bind(status)
        .bind(reviewer.to_uuid())
        .bind(note)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored message_report
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::message::{MessageRepo, NewMessage};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// The reserved all-zero default workspace, guaranteed to exist by migration
    /// 0006's backfill — reused so the room is well-scoped.
    const DEFAULT_WS: &str = "00000000-0000-0000-0000-000000000000";

    fn default_ws() -> WorkspaceId {
        WorkspaceId::from_uuid(uuid::Uuid::parse_str(DEFAULT_WS).unwrap())
    }

    /// Create a throwaway participant so the test is self-contained.
    async fn participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("message-report-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    /// Create a throwaway room in the default workspace, created by `by`.
    async fn room(p: &PgPool, by: ParticipantId) -> aero_common::RoomId {
        let id = aero_common::RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
             VALUES ($1, 'channel', $2, $3, now(), $4::uuid)",
        )
        .bind(id.to_uuid())
        .bind(format!("mr-room-{id}"))
        .bind(by.to_uuid())
        .bind(DEFAULT_WS)
        .execute(p)
        .await
        .expect("insert room");
        id
    }

    /// Insert a throwaway message so a report has something real to reference.
    async fn message(p: &PgPool, sender: ParticipantId, room: aero_common::RoomId) -> MessageId {
        let msgs = MessageRepo::new(p.clone());
        let m = msgs
            .insert(NewMessage {
                room_id: room,
                sender_id: sender,
                blocks: vec![aero_common::Block::text("reportable")],
                reply_to: None,
                metadata: serde_json::Value::Null,
                expires_at: None,
            })
            .await
            .expect("insert message");
        m.id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn report_then_list_pending_finds_it() {
        let p = pool();
        let repo = MessageReportRepo::new(p.clone());
        let sender = participant(&p).await;
        let reporter = participant(&p).await;
        let r = room(&p, sender).await;
        let msg = message(&p, sender, r).await;
        let ws = default_ws();

        let filed = repo
            .report(ws, msg, reporter, "spam")
            .await
            .expect("file report");
        assert_eq!(filed.status, "pending");
        assert_eq!(filed.reason, "spam");
        assert_eq!(filed.message_id, msg);
        assert!(filed.reviewed_by.is_none());
        assert!(filed.reviewed_at.is_none());

        let pending = repo.list_pending(ws).await.expect("list pending");
        assert!(
            pending.iter().any(|x| x.id == filed.id),
            "freshly filed report shows in the pending queue"
        );

        // get() round-trips the same row.
        let got = repo.get(filed.id).await.expect("get").expect("present");
        assert_eq!(got.id, filed.id);

        cleanup(&p, msg, r).await;
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn review_remove_transitions_and_stamps_then_is_idempotent() {
        let p = pool();
        let repo = MessageReportRepo::new(p.clone());
        let sender = participant(&p).await;
        let reporter = participant(&p).await;
        let reviewer = participant(&p).await;
        let r = room(&p, sender).await;
        let msg = message(&p, sender, r).await;
        let ws = default_ws();

        let filed = repo.report(ws, msg, reporter, "abuse").await.unwrap();

        // First review (remove) transitions the pending row -> true.
        let changed = repo
            .review(filed.id, reviewer, true, Some("clear violation"))
            .await
            .expect("review");
        assert!(changed, "pending -> removed returns true");

        let after = repo.get(filed.id).await.unwrap().expect("present");
        assert_eq!(after.status, "removed");
        assert_eq!(after.reviewed_by, Some(reviewer));
        assert!(after.reviewed_at.is_some());
        assert_eq!(after.note.as_deref(), Some("clear violation"));

        // It drops out of the pending queue.
        let pending = repo.list_pending(ws).await.unwrap();
        assert!(
            !pending.iter().any(|x| x.id == filed.id),
            "reviewed report leaves the pending queue"
        );

        // Re-reviewing an already-decided report is a no-op -> false (the
        // moderate-delete side effect can never re-fire).
        let again = repo
            .review(filed.id, reviewer, true, Some("dup"))
            .await
            .unwrap();
        assert!(!again, "idempotent re-review returns false");
        // And the original decision/note is untouched.
        let unchanged = repo.get(filed.id).await.unwrap().expect("present");
        assert_eq!(unchanged.note.as_deref(), Some("clear violation"));

        cleanup(&p, msg, r).await;
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn review_keep_transitions_to_kept() {
        let p = pool();
        let repo = MessageReportRepo::new(p.clone());
        let sender = participant(&p).await;
        let reporter = participant(&p).await;
        let reviewer = participant(&p).await;
        let r = room(&p, sender).await;
        let msg = message(&p, sender, r).await;
        let ws = default_ws();

        let filed = repo.report(ws, msg, reporter, "maybe").await.unwrap();
        let changed = repo
            .review(filed.id, reviewer, false, None)
            .await
            .expect("review keep");
        assert!(changed, "pending -> kept returns true");

        let after = repo.get(filed.id).await.unwrap().expect("present");
        assert_eq!(after.status, "kept");
        assert_eq!(after.reviewed_by, Some(reviewer));
        assert!(after.note.is_none());

        // An unknown report id is a no-op -> false.
        let missing = repo
            .review(MessageReportId::new(), reviewer, true, None)
            .await
            .unwrap();
        assert!(!missing, "review of unknown report returns false");

        cleanup(&p, msg, r).await;
    }

    /// Drop the throwaway message + room so reruns stay self-contained. The report
    /// rows have NO FK to messages, so they are deleted explicitly first.
    async fn cleanup(p: &PgPool, msg: MessageId, room: aero_common::RoomId) {
        sqlx::query("DELETE FROM message_reports WHERE message_id = $1")
            .bind(msg.to_uuid())
            .execute(p)
            .await
            .ok();
        sqlx::query("DELETE FROM messages WHERE id = $1")
            .bind(msg.to_uuid())
            .execute(p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .execute(p)
            .await
            .ok();
    }
}
