//! Message-report repository — user-initiated reports feeding a workspace
//! moderation review queue.
//!
//! Backs migrations 0093 and 0205. A room member flags a message they find
//! abusive/spammy/etc.; the report lands here as `pending` and surfaces in a
//! workspace-admin review queue, where an administrator either KEEPS (dismisses
//! the report) or REMOVEs it.
//!
//! A report is a HISTORICAL record: there is no FK to `messages`, so a report row
//! outlives the message it references (existence is validated by the handler at
//! report time, not by a foreign key — same lesson as ban appeals). The repo is
//! purely additive: a NEW table, no existing repo is touched. The [`MessageReport`]
//! model lives here (and is re-exported from the crate root), a storage-layer
//! projection.
//!
//! Filing owns room/message containment and effective-access checks through
//! commit. Review owns workspace-admin authorization, the report transition,
//! optional message tombstone, audit row, blob cleanup, and durable room-event
//! outbox in one transaction. A delete/audit/outbox failure therefore leaves the
//! report pending instead of committing a false `"removed"` decision.

use aero_common::{Error, MessageId, MessageReportId, ParticipantId, RoomId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;

use crate::message::authorization::{lock_effective_message_write_access, PostPolicy};
use crate::message::MessageRepo;

const MAX_REASON_BYTES: usize = 2_000;
const MAX_NOTE_BYTES: usize = 2_000;
const DIGEST_CHARS: usize = 120;

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
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub reviewed_at: Option<time::OffsetDateTime>,
    /// When the report was filed (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
}

/// One committed review decision and its optional deletion outbox row.
#[derive(Debug, Clone)]
pub struct MessageReportReview {
    pub report: MessageReport,
    /// Present only when this review newly tombstoned the message.
    pub delete_outbox_id: Option<uuid::Uuid>,
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
    let (
        id,
        workspace_id,
        message_id,
        reporter_id,
        reason,
        status,
        note,
        reviewed_by,
        reviewed_at,
        created_at,
    ) = r;
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

    /// File a new report after locking the reporter's complete effective room
    /// access and the live message in the same transaction.
    ///
    /// # Errors
    /// Returns [`Error::Invalid`] for an empty/overlong reason,
    /// [`Error::Forbidden`] when room access was revoked, [`Error::NotFound`]
    /// when the message is missing/deleted/not in `room`, and propagates
    /// storage errors.
    pub async fn report_authorized(
        &self,
        room: RoomId,
        message: MessageId,
        reporter: ParticipantId,
        reason: &str,
    ) -> Result<MessageReport, Error> {
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(Error::Invalid("reason is empty".into()));
        }
        if reason.len() > MAX_REASON_BYTES {
            return Err(Error::Invalid("reason too long".into()));
        }

        let mut tx = self.pool.begin().await?;
        let Some(access) =
            lock_effective_message_write_access(&mut tx, room, reporter, PostPolicy::Ignore)
                .await?
        else {
            return Err(Error::Forbidden(
                "message-report room access was revoked before commit".into(),
            ));
        };
        let Some(existing) = MessageRepo::lock_message_in_tx(&mut tx, message).await? else {
            return Err(Error::NotFound(format!(
                "live message {message} in room {room}"
            )));
        };
        if existing.room_id != room || existing.deleted_at.is_some() {
            return Err(Error::NotFound(format!(
                "live message {message} in room {room}"
            )));
        }

        let id = MessageReportId::new();
        let sql = format!(
            "INSERT INTO message_reports
                 (id, workspace_id, message_id, reporter_id, reason)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING {COLUMNS}"
        );
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .bind(access.workspace.to_uuid())
            .bind(message.to_uuid())
            .bind(reporter.to_uuid())
            .bind(reason)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(row_to_model(row))
    }

    /// List a workspace's pending queue while `actor` remains an effective
    /// Owner/Admin. Oldest reports come first.
    ///
    /// # Errors
    /// Returns [`Error::Forbidden`] unless `actor` is a current effective
    /// workspace admin and propagates storage errors.
    pub async fn list_pending_authorized(
        &self,
        workspace: WorkspaceId,
        actor: ParticipantId,
    ) -> Result<Vec<MessageReport>, Error> {
        let mut tx = self.pool.begin().await?;
        crate::workspace::authz::assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        let sql = format!(
            "SELECT {COLUMNS}
               FROM message_reports
              WHERE workspace_id = $1 AND status = 'pending'
              ORDER BY created_at ASC, id ASC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(workspace.to_uuid())
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Decide one pending workspace report while `reviewer` remains an
    /// effective Owner/Admin.
    ///
    /// A `remove` decision and any live message tombstone, audit row, attachment
    /// cleanup, and durable deleted-event outbox all commit with the report
    /// transition. A message that was already removed does not prevent recording
    /// the terminal review decision.
    ///
    /// # Errors
    /// Returns [`Error::Invalid`] for an overlong note,
    /// [`Error::Forbidden`] unless `reviewer` is a current effective admin,
    /// [`Error::NotFound`] for a missing/cross-tenant/non-pending report,
    /// [`Error::Conflict`] if a retained historical report points at a message
    /// that now belongs to another workspace, and propagates storage errors.
    pub async fn review_authorized(
        &self,
        id: MessageReportId,
        workspace: WorkspaceId,
        reviewer: ParticipantId,
        remove: bool,
        note: Option<&str>,
        traceparent: Option<&str>,
    ) -> Result<MessageReportReview, Error> {
        if note.is_some_and(|note| note.len() > MAX_NOTE_BYTES) {
            return Err(Error::Invalid("review note too long".into()));
        }

        let mut tx = self.pool.begin().await?;
        crate::workspace::authz::assert_effective_admin_in_tx(&mut tx, workspace, reviewer).await?;

        let select = format!(
            "SELECT {COLUMNS}
               FROM message_reports
              WHERE id = $1
                AND workspace_id = $2
                AND status = 'pending'
              FOR UPDATE"
        );
        let pending = sqlx::query_as::<_, Row>(&select)
            .bind(id.to_uuid())
            .bind(workspace.to_uuid())
            .fetch_optional(&mut *tx)
            .await?
            .map(row_to_model)
            .ok_or_else(|| Error::NotFound(format!("pending message report {id}")))?;

        let delete_outbox_id = if remove {
            if let Some(message) =
                MessageRepo::lock_message_in_tx(&mut tx, pending.message_id).await?
            {
                let actual_workspace = sqlx::query_scalar::<_, uuid::Uuid>(
                    "SELECT workspace_id FROM rooms WHERE id = $1 FOR SHARE",
                )
                .bind(message.room_id.to_uuid())
                .fetch_optional(&mut *tx)
                .await?;
                if actual_workspace != Some(workspace.to_uuid()) {
                    return Err(Error::Conflict(
                        "reported message no longer belongs to the report workspace".into(),
                    ));
                }

                let digest: String = message
                    .searchable_text()
                    .chars()
                    .take(DIGEST_CHARS)
                    .collect();
                let reason = format!("workspace report {id} removed by {reviewer}");
                let detail = serde_json::json!({
                    "room_id": message.room_id,
                    "report_id": id,
                    "reason": reason,
                    "digest": digest,
                });
                MessageRepo::soft_delete_locked_outboxed_in_tx(
                    &mut tx,
                    message,
                    Some(workspace),
                    Some(reviewer),
                    Some("message.moderated"),
                    detail,
                    reviewer,
                    traceparent,
                )
                .await?
                .map(|deleted| deleted.outbox_id)
            } else {
                None
            }
        } else {
            None
        };

        let status = if remove { "removed" } else { "kept" };
        let update = format!(
            "UPDATE message_reports
                SET status = $2,
                    reviewed_by = $3,
                    reviewed_at = now(),
                    note = $4
              WHERE id = $1
                AND workspace_id = $5
                AND status = 'pending'
            RETURNING {COLUMNS}"
        );
        let report = sqlx::query_as::<_, Row>(&update)
            .bind(id.to_uuid())
            .bind(status)
            .bind(reviewer.to_uuid())
            .bind(note)
            .bind(workspace.to_uuid())
            .fetch_optional(&mut *tx)
            .await?
            .map(row_to_model)
            .ok_or_else(|| Error::NotFound(format!("pending message report {id}")))?;
        tx.commit().await?;
        Ok(MessageReportReview {
            report,
            delete_outbox_id,
        })
    }

    #[cfg(test)]
    async fn get_unscoped(
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
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored message_report
/// ```
#[cfg(test)]
mod db_tests {
    use std::time::Duration;

    use aero_common::{Block, RoomKind, WorkspaceRole};

    use super::*;
    use crate::message::{MessageRepo, NewMessage};
    use crate::{RoomRepo, WorkspaceRepo};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_lazy(&url)
            .expect("valid DATABASE_URL")
    }

    async fn participant(p: &PgPool, label: &str) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("message-report-{label}-{id}"))
            .execute(p)
            .await
            .unwrap();
        id
    }

    async fn message(p: &PgPool, sender: ParticipantId, room: RoomId) -> MessageId {
        let msgs = MessageRepo::new(p.clone());
        let m = msgs
            .insert(NewMessage {
                room_id: room,
                sender_id: sender,
                blocks: vec![Block::text("reportable content")],
                reply_to: None,
                metadata: serde_json::Value::Null,
                expires_at: None,
            })
            .await
            .unwrap();
        m.id
    }

    struct Fixture {
        pool: PgPool,
        repo: MessageReportRepo,
        workspace: WorkspaceId,
        other_workspace: WorkspaceId,
        room: RoomId,
        message: MessageId,
        other_message: MessageId,
        reviewer: ParticipantId,
        reporter: ParticipantId,
        outsider: ParticipantId,
    }

    async fn fixture() -> Fixture {
        let pool = pool();
        let workspaces = WorkspaceRepo::new(pool.clone());
        let rooms = RoomRepo::new(pool.clone());
        let owner = participant(&pool, "owner").await;
        let reviewer = participant(&pool, "reviewer").await;
        let reporter = participant(&pool, "reporter").await;
        let other_owner = participant(&pool, "other-owner").await;
        let outsider = participant(&pool, "outsider").await;
        let workspace = workspaces
            .create(
                format!("Message reports {owner}"),
                format!("message-reports-{owner}"),
                owner,
            )
            .await
            .unwrap()
            .id;
        let other_workspace = workspaces
            .create(
                format!("Other reports {other_owner}"),
                format!("other-message-reports-{other_owner}"),
                other_owner,
            )
            .await
            .unwrap()
            .id;
        workspaces
            .add_member(workspace, reviewer, WorkspaceRole::Admin)
            .await
            .unwrap();
        workspaces
            .add_member(workspace, reporter, WorkspaceRole::Member)
            .await
            .unwrap();
        workspaces
            .add_member(other_workspace, outsider, WorkspaceRole::Member)
            .await
            .unwrap();
        let room = rooms
            .create_in_workspace(
                workspace,
                RoomKind::Channel,
                Some(format!("reports-{}", RoomId::new())),
                owner,
            )
            .await
            .unwrap()
            .id;
        rooms.add_member(room, reporter).await.unwrap();
        let other_room = rooms
            .create_in_workspace(
                other_workspace,
                RoomKind::Channel,
                Some(format!("other-reports-{}", RoomId::new())),
                other_owner,
            )
            .await
            .unwrap()
            .id;
        rooms.add_member(other_room, outsider).await.unwrap();
        let primary_message = message(&pool, owner, room).await;
        let other_message = message(&pool, other_owner, other_room).await;
        Fixture {
            repo: MessageReportRepo::new(pool.clone()),
            pool,
            workspace,
            other_workspace,
            room,
            message: primary_message,
            other_message,
            reviewer,
            reporter,
            outsider,
        }
    }

    fn constraint(error: &sqlx::Error) -> Option<&str> {
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint)
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL with migrations applied"]
    async fn report_authorization_binds_room_message_and_workspace() {
        let fixture = fixture().await;
        let filed = fixture
            .repo
            .report_authorized(fixture.room, fixture.message, fixture.reporter, "spam")
            .await
            .unwrap();
        assert_eq!(filed.status, "pending");
        assert_eq!(filed.reason, "spam");
        assert_eq!(filed.workspace_id, fixture.workspace);
        assert!(matches!(
            fixture
                .repo
                .report_authorized(
                    fixture.room,
                    fixture.other_message,
                    fixture.reporter,
                    "cross-room",
                )
                .await,
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            fixture
                .repo
                .report_authorized(
                    fixture.room,
                    fixture.message,
                    fixture.outsider,
                    "no room access",
                )
                .await,
            Err(Error::Forbidden(_))
        ));

        let raw = sqlx::query(
            "INSERT INTO message_reports
                 (id, workspace_id, message_id, reporter_id, reason)
             VALUES ($1, $2, $3, $4, 'raw cross-tenant report')",
        )
        .bind(MessageReportId::new().to_uuid())
        .bind(fixture.other_workspace.to_uuid())
        .bind(fixture.message.to_uuid())
        .bind(fixture.reporter.to_uuid())
        .execute(&fixture.pool)
        .await
        .expect_err("migration trigger rejects raw workspace spoofing");
        assert_eq!(
            constraint(&raw),
            Some("message_reports_scope_containment_chk")
        );

        let mutate = sqlx::query(
            "UPDATE message_reports
                SET workspace_id = $2
              WHERE id = $1",
        )
        .bind(filed.id.to_uuid())
        .bind(fixture.other_workspace.to_uuid())
        .execute(&fixture.pool)
        .await
        .expect_err("report identity is immutable");
        assert_eq!(
            constraint(&mutate),
            Some("message_reports_identity_immutable_chk")
        );

        let queue = fixture
            .repo
            .list_pending_authorized(fixture.workspace, fixture.reviewer)
            .await
            .unwrap();
        assert!(queue.iter().any(|report| report.id == filed.id));
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL with migrations applied"]
    async fn remove_review_commits_decision_delete_audit_and_outbox_once() {
        let fixture = fixture().await;
        let filed = fixture
            .repo
            .report_authorized(fixture.room, fixture.message, fixture.reporter, "abuse")
            .await
            .unwrap();
        let outcome = fixture
            .repo
            .review_authorized(
                filed.id,
                fixture.workspace,
                fixture.reviewer,
                true,
                Some("clear violation"),
                Some("00-message-report-test"),
            )
            .await
            .unwrap();
        assert_eq!(outcome.report.status, "removed");
        assert_eq!(outcome.report.reviewed_by, Some(fixture.reviewer));
        assert_eq!(outcome.report.note.as_deref(), Some("clear violation"));
        let outbox = outcome
            .delete_outbox_id
            .expect("a live message produces a deleted-event outbox");
        let deleted: Option<time::OffsetDateTime> =
            sqlx::query_scalar("SELECT deleted_at FROM messages WHERE id = $1")
                .bind(fixture.message.to_uuid())
                .fetch_one(&fixture.pool)
                .await
                .unwrap();
        assert!(deleted.is_some());
        let audit_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
               FROM audit_events
              WHERE workspace_id = $1
                AND actor_id = $2
                AND action = 'message.moderated'
                AND target = $3",
        )
        .bind(fixture.workspace.to_uuid())
        .bind(fixture.reviewer.to_uuid())
        .bind(fixture.message.to_string())
        .fetch_one(&fixture.pool)
        .await
        .unwrap();
        assert_eq!(audit_count, 1);
        let outbox_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM event_outbox WHERE id = $1")
                .bind(outbox)
                .fetch_one(&fixture.pool)
                .await
                .unwrap();
        assert_eq!(outbox_count, 1);

        assert!(matches!(
            fixture
                .repo
                .review_authorized(
                    filed.id,
                    fixture.workspace,
                    fixture.reviewer,
                    true,
                    None,
                    None,
                )
                .await,
            Err(Error::NotFound(_))
        ));
        let audit_after: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
               FROM audit_events
              WHERE workspace_id = $1
                AND action = 'message.moderated'
                AND target = $2",
        )
        .bind(fixture.workspace.to_uuid())
        .bind(fixture.message.to_string())
        .fetch_one(&fixture.pool)
        .await
        .unwrap();
        assert_eq!(audit_after, 1);
    }

    #[tokio::test]
    #[ignore = "requires DATABASE_URL with migrations applied"]
    async fn admin_revocation_race_leaves_report_pending_and_message_live() {
        let fixture = fixture().await;
        let filed = fixture
            .repo
            .report_authorized(fixture.room, fixture.message, fixture.reporter, "race")
            .await
            .unwrap();

        let mut revocation = fixture.pool.begin().await.unwrap();
        crate::ownership::lock_membership_governance(&mut revocation)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(fixture.workspace.to_uuid())
            .execute(&mut *revocation)
            .await
            .unwrap();
        sqlx::query(
            "DELETE FROM workspace_members
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(fixture.workspace.to_uuid())
        .bind(fixture.reviewer.to_uuid())
        .execute(&mut *revocation)
        .await
        .unwrap();

        let raced_repo = fixture.repo.clone();
        let mut raced = tokio::spawn(async move {
            raced_repo
                .review_authorized(
                    filed.id,
                    fixture.workspace,
                    fixture.reviewer,
                    true,
                    None,
                    None,
                )
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut raced)
                .await
                .is_err(),
            "review waits behind the workspace revocation lock"
        );
        revocation.commit().await.unwrap();
        assert!(matches!(raced.await.unwrap(), Err(Error::Forbidden(_))));

        let retained = fixture.repo.get_unscoped(filed.id).await.unwrap().unwrap();
        assert_eq!(retained.status, "pending");
        let deleted: Option<time::OffsetDateTime> =
            sqlx::query_scalar("SELECT deleted_at FROM messages WHERE id = $1")
                .bind(fixture.message.to_uuid())
                .fetch_one(&fixture.pool)
                .await
                .unwrap();
        assert!(deleted.is_none());
    }
}
