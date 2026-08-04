//! Task / to-do repository (lightweight, durable, assignable work items).
//!
//! Backs `migrations/0055_tasks.sql`. A room member creates a task (optionally
//! anchored to a source message), assigns it, sets a due date, and marks it done.
//! This is deliberately distinct from the AI action-item *extraction* endpoint
//! (which only summarizes a channel): a task here is a durable, stateful row that
//! can be listed, reassigned, and re-statused.
//!
//! Mutations enforce the actor/assignee/source room boundary in the same
//! transaction as the task write, while the server layer also gates routes with
//! [`ImService::assert_room_access`](aero_im_core::ImService::assert_room_access).
//! The [`Task`] model lives here (and is re-exported from the crate root) rather
//! than in `aero-common`, since it is a storage-layer projection.

use aero_common::{MessageId, ParticipantId, RoomId, TaskId};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};

mod action_item_batch;
pub use action_item_batch::{
    ActionItemBatchError, MAX_ACTION_ITEM_BATCH_KEY_LEN, MAX_ACTION_ITEM_BATCH_SIZE,
};

/// Task status: a freshly created, not-yet-started task.
pub const STATUS_OPEN: &str = "open";
/// Task status: actively being worked on.
pub const STATUS_IN_PROGRESS: &str = "in_progress";
/// Task status: completed.
pub const STATUS_DONE: &str = "done";

/// Whether `status` is one of the recognized task states (`open` |
/// `in_progress` | `done`). Pure, so the domain check is unit-tested offline.
#[must_use]
pub fn validate_status(status: &str) -> bool {
    matches!(status, STATUS_OPEN | STATUS_IN_PROGRESS | STATUS_DONE)
}

/// One task / to-do item tracked in a room.
///
/// A storage-layer projection of a `tasks` row. `Serialize` so a handler can hand
/// the row straight back as JSON; `created_at` / `updated_at` render as RFC 3339,
/// and the optional `due_at` renders as RFC 3339 (omitted when unset).
#[derive(Debug, Clone, Serialize)]
pub struct Task {
    /// The task's unique id.
    pub id: TaskId,
    /// The room the task belongs to (the access-control boundary).
    pub room_id: RoomId,
    /// The participant who created the task.
    pub creator_id: ParticipantId,
    /// The participant the task is assigned to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee_id: Option<ParticipantId>,
    /// Human-readable summary of the work to do.
    pub title: String,
    /// The message the task was created from, if any (Slack/Lark "create task
    /// from message").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_message_id: Option<MessageId>,
    /// Lifecycle status: `open` | `in_progress` | `done`.
    pub status: String,
    /// Optional due date (RFC 3339 on the wire; omitted when unset).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub due_at: Option<time::OffsetDateTime>,
    /// When the task was created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    /// When the task was last modified (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: time::OffsetDateTime,
}

/// The columns a [`Task`] is built from, in select order. Shared by every query
/// so the row decoding stays in one place.
const COLUMNS: &str =
    "id, room_id, creator_id, assignee_id, title, source_message_id, status, due_at, created_at, updated_at";
const TASK_COLUMNS: &str = "task.id, task.room_id, task.creator_id, task.assignee_id, task.title, task.source_message_id, task.status, task.due_at, task.created_at, task.updated_at";

/// A task mutation failed before it could preserve the room access boundary.
#[derive(Debug, thiserror::Error)]
pub enum TaskWriteError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("task does not exist")]
    NotFound,
    #[error("actor is not an effective member of the task room")]
    ActorNotMember,
    #[error("assignee is not an effective member of the task room")]
    AssigneeNotMember,
    #[error("source message does not belong to the task room")]
    SourceMessageNotInRoom,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: uuid::Uuid,
    room_id: uuid::Uuid,
    creator_id: uuid::Uuid,
    assignee_id: Option<uuid::Uuid>,
    title: String,
    source_message_id: Option<uuid::Uuid>,
    status: String,
    due_at: Option<time::OffsetDateTime>,
    created_at: time::OffsetDateTime,
    updated_at: time::OffsetDateTime,
}

fn row_to_model(r: Row) -> Task {
    Task {
        id: TaskId::from_uuid(r.id),
        room_id: RoomId::from_uuid(r.room_id),
        creator_id: ParticipantId::from_uuid(r.creator_id),
        assignee_id: r.assignee_id.map(ParticipantId::from_uuid),
        title: r.title,
        source_message_id: r.source_message_id.map(MessageId::from_uuid),
        status: r.status,
        due_at: r.due_at,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }
}

/// Apply the same positive membership gates as
/// `ImService::assert_room_access` while holding the membership rows stable for
/// the surrounding task transaction.
async fn is_effective_room_member(
    tx: &mut Transaction<'_, Postgres>,
    room: RoomId,
    participant: ParticipantId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        r"SELECT true
            FROM room_members membership
            JOIN rooms room
              ON room.id = membership.room_id
            JOIN workspaces workspace
              ON workspace.id = room.workspace_id
            JOIN workspace_members workspace_membership
              ON workspace_membership.workspace_id = room.workspace_id
             AND workspace_membership.participant_id = membership.participant_id
            JOIN participants participant
              ON participant.id = membership.participant_id
             AND participant.deleted_at IS NULL
           WHERE membership.room_id = $1
             AND membership.participant_id = $2
             AND NOT EXISTS (
                 SELECT 1
                   FROM workspace_deactivations deactivated
                  WHERE deactivated.workspace_id = room.workspace_id
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
           FOR SHARE OF membership, room, workspace, workspace_membership, participant",
    )
    .bind(room.to_uuid())
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map(|row| row.is_some())
}

async fn source_message_is_visible_in_room(
    tx: &mut Transaction<'_, Postgres>,
    room: RoomId,
    message: MessageId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        r"SELECT true
            FROM messages
           WHERE id = $1
             AND room_id = $2
             AND deleted_at IS NULL
             AND (expires_at IS NULL OR expires_at > now())
           FOR SHARE",
    )
    .bind(message.to_uuid())
    .bind(room.to_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map(|row| row.is_some())
}

/// Repository over the `tasks` table (room to-do items).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`TaskRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct TaskRepo {
    pool: PgPool,
}

impl TaskRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persist a new task in `room`, created by `creator`, returning its generated
    /// id. The new row starts in status [`STATUS_OPEN`].
    ///
    /// The actor, optional assignee, and optional source message are validated
    /// inside the same transaction as the insert. This prevents a caller from
    /// assigning private-room work to an arbitrary global participant or
    /// attaching a message from another room.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn create(
        &self,
        room: RoomId,
        creator: ParticipantId,
        title: &str,
        assignee: Option<ParticipantId>,
        source_message: Option<MessageId>,
        due_at: Option<time::OffsetDateTime>,
    ) -> Result<TaskId, TaskWriteError> {
        let mut tx = self.pool.begin().await?;
        if !is_effective_room_member(&mut tx, room, creator).await? {
            return Err(TaskWriteError::ActorNotMember);
        }
        if let Some(assignee) = assignee {
            if !is_effective_room_member(&mut tx, room, assignee).await? {
                return Err(TaskWriteError::AssigneeNotMember);
            }
        }
        if let Some(source_message) = source_message {
            if !source_message_is_visible_in_room(&mut tx, room, source_message).await? {
                return Err(TaskWriteError::SourceMessageNotInRoom);
            }
        }

        let id = TaskId::new();
        sqlx::query(
            r"INSERT INTO tasks
                  (id, room_id, creator_id, assignee_id, title, source_message_id, due_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(creator.to_uuid())
        .bind(assignee.map(|a| a.to_uuid()))
        .bind(title)
        .bind(source_message.map(|m| m.to_uuid()))
        .bind(due_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Fetch one task by id, or `None` if no such row exists. Not scoped — the
    /// caller (server layer) asserts room access against the returned `room_id`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(&self, id: TaskId) -> Result<Option<Task>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM tasks WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// List `room`'s tasks, newest first. When `status` is `Some`, only tasks in
    /// that status are returned; when `None`, every status is included.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_room(
        &self,
        room: RoomId,
        status: Option<&str>,
    ) -> Result<Vec<Task>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM tasks
              WHERE room_id = $1
                AND ($2::text IS NULL OR status = $2)
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(room.to_uuid())
            .bind(status)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// List tasks assigned to `assignee` only in rooms they may currently
    /// access, unfinished first (`done` sinks to the bottom), then by soonest
    /// due date, then newest.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_assignee(
        &self,
        assignee: ParticipantId,
    ) -> Result<Vec<Task>, sqlx::Error> {
        let sql = format!(
            "SELECT {TASK_COLUMNS}
               FROM tasks task
               JOIN room_members membership
                 ON membership.room_id = task.room_id
                AND membership.participant_id = task.assignee_id
               JOIN rooms room
                 ON room.id = task.room_id
               JOIN workspaces workspace
                 ON workspace.id = room.workspace_id
               JOIN workspace_members workspace_membership
                 ON workspace_membership.workspace_id = room.workspace_id
                AND workspace_membership.participant_id = task.assignee_id
               JOIN participants participant
                 ON participant.id = task.assignee_id
                AND participant.deleted_at IS NULL
              WHERE task.assignee_id = $1
                AND NOT EXISTS (
                    SELECT 1
                      FROM workspace_deactivations deactivated
                     WHERE deactivated.workspace_id = room.workspace_id
                       AND deactivated.participant_id = task.assignee_id
                )
                AND (
                    participant.kind <> 'human'
                    OR NOT workspace.require_2fa
                    OR EXISTS (
                        SELECT 1
                          FROM totp_secrets totp
                         WHERE totp.participant_id = task.assignee_id
                           AND totp.activated
                    )
                )
              ORDER BY (task.status = 'done') ASC,
                       task.due_at ASC NULLS LAST,
                       task.created_at DESC,
                       task.id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(assignee.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Apply a partial edit to a task, bumping `updated_at`. The task row is
    /// locked first, then the actor and any replacement assignee are checked
    /// against the task's current room inside the same transaction.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn update(
        &self,
        id: TaskId,
        actor: ParticipantId,
        title: Option<&str>,
        assignee: Option<ParticipantId>,
        due_at: Option<time::OffsetDateTime>,
        status: Option<&str>,
    ) -> Result<bool, TaskWriteError> {
        let mut tx = self.pool.begin().await?;
        let room = sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT room_id FROM tasks WHERE id = $1 FOR UPDATE",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .map(RoomId::from_uuid)
        .ok_or(TaskWriteError::NotFound)?;
        if !is_effective_room_member(&mut tx, room, actor).await? {
            return Err(TaskWriteError::ActorNotMember);
        }
        if let Some(assignee) = assignee {
            if !is_effective_room_member(&mut tx, room, assignee).await? {
                return Err(TaskWriteError::AssigneeNotMember);
            }
        }

        let result = sqlx::query(
            r"UPDATE tasks
                 SET title = COALESCE($2, title),
                     assignee_id = COALESCE($3, assignee_id),
                     due_at = COALESCE($4, due_at),
                     status = COALESCE($5, status),
                     updated_at = now()
               WHERE id = $1",
        )
        .bind(id.to_uuid())
        .bind(title)
        .bind(assignee.map(|a| a.to_uuid()))
        .bind(due_at)
        .bind(status)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }

    /// Delete a task only while `actor` remains an effective member of its room.
    /// The row lock and access check share the delete transaction.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn delete(&self, id: TaskId, actor: ParticipantId) -> Result<bool, TaskWriteError> {
        let mut tx = self.pool.begin().await?;
        let room = sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT room_id FROM tasks WHERE id = $1 FOR UPDATE",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .map(RoomId::from_uuid)
        .ok_or(TaskWriteError::NotFound)?;
        if !is_effective_room_member(&mut tx, room, actor).await? {
            return Err(TaskWriteError::ActorNotMember);
        }
        let result = sqlx::query("DELETE FROM tasks WHERE id = $1")
            .bind(id.to_uuid())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_status_accepts_known_states() {
        assert!(validate_status("open"));
        assert!(validate_status("in_progress"));
        assert!(validate_status("done"));
    }

    #[test]
    fn validate_status_rejects_unknown() {
        assert!(!validate_status(""));
        assert!(!validate_status("Open"));
        assert!(!validate_status("closed"));
        assert!(!validate_status("pending"));
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored task
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway participant so the test is self-contained.
    async fn participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("task-actor-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    async fn room_with_members(
        p: &PgPool,
        creator: ParticipantId,
        members: &[ParticipantId],
    ) -> RoomId {
        let workspace = aero_common::WorkspaceId::new();
        let room = RoomId::new();
        let mut tx = p.begin().await.expect("begin workspace fixture");
        sqlx::query(
            "INSERT INTO workspaces (id, name, slug, created_by)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(workspace.to_uuid())
        .bind(format!("task-workspace-{workspace}"))
        .bind(format!("task-{workspace}"))
        .bind(creator.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("insert workspace");
        for member in members {
            sqlx::query(
                "INSERT INTO workspace_members
                     (workspace_id, participant_id, role, joined_at)
                 VALUES ($1, $2, $3, now())",
            )
            .bind(workspace.to_uuid())
            .bind(member.to_uuid())
            .bind(if *member == creator {
                "owner"
            } else {
                "member"
            })
            .execute(&mut *tx)
            .await
            .expect("insert workspace member");
        }
        tx.commit().await.expect("commit workspace fixture");
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id)
             VALUES ($1, 'group', $2, $3, $4)",
        )
        .bind(room.to_uuid())
        .bind(format!("task-room-{room}"))
        .bind(creator.to_uuid())
        .bind(workspace.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        for member in members {
            sqlx::query(
                "INSERT INTO room_members
                     (room_id, participant_id, role, joined_at)
                 VALUES ($1, $2, 'member', now())",
            )
            .bind(room.to_uuid())
            .bind(member.to_uuid())
            .execute(p)
            .await
            .expect("insert room member");
        }
        room
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn task_create_get_list_update_status_delete() {
        let p = pool();
        let repo = TaskRepo::new(p.clone());
        let creator = participant(&p).await;
        let assignee = participant(&p).await;
        let outsider = participant(&p).await;
        let room = room_with_members(&p, creator, &[creator, assignee]).await;
        let other_room = room_with_members(&p, outsider, &[outsider]).await;
        let other_message = MessageId::new();
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks)
             VALUES ($1, $2, $3, '[]'::jsonb)",
        )
        .bind(other_message.to_uuid())
        .bind(other_room.to_uuid())
        .bind(outsider.to_uuid())
        .execute(&p)
        .await
        .expect("insert other-room source message");
        let due = time::OffsetDateTime::now_utc() + time::Duration::days(2);

        assert!(matches!(
            repo.create(room, creator, "Do not leak", Some(outsider), None, None)
                .await,
            Err(TaskWriteError::AssigneeNotMember)
        ));
        assert!(matches!(
            repo.create(
                room,
                creator,
                "Do not cross-link",
                None,
                Some(other_message),
                None
            )
            .await,
            Err(TaskWriteError::SourceMessageNotInRoom)
        ));

        // create → get returns the row, defaulted to 'open'.
        let id = repo
            .create(
                room,
                creator,
                "Ship the thing",
                Some(assignee),
                None,
                Some(due),
            )
            .await
            .unwrap();
        let got = repo.get(id).await.unwrap().expect("created task exists");
        assert_eq!(got.id, id);
        assert_eq!(got.room_id, room);
        assert_eq!(got.creator_id, creator);
        assert_eq!(got.assignee_id, Some(assignee));
        assert_eq!(got.title, "Ship the thing");
        assert_eq!(got.status, STATUS_OPEN);
        assert!(got.due_at.is_some());

        // list_for_room (no filter + status filter); a different room does not show it.
        assert!(
            repo.list_for_room(room, None)
                .await
                .unwrap()
                .iter()
                .any(|t| t.id == id),
            "room list shows the task"
        );
        assert!(
            repo.list_for_room(room, Some("open"))
                .await
                .unwrap()
                .iter()
                .any(|t| t.id == id),
            "open filter shows the open task"
        );
        assert!(
            !repo
                .list_for_room(room, Some("done"))
                .await
                .unwrap()
                .iter()
                .any(|t| t.id == id),
            "done filter hides the open task"
        );
        assert!(
            !repo
                .list_for_room(other_room, None)
                .await
                .unwrap()
                .iter()
                .any(|t| t.id == id),
            "another room's list does not show it"
        );

        // list_for_assignee shows it.
        assert!(
            repo.list_for_assignee(assignee)
                .await
                .unwrap()
                .iter()
                .any(|t| t.id == id),
            "assignee list shows the task"
        );
        sqlx::query("DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
            .bind(room.to_uuid())
            .bind(assignee.to_uuid())
            .execute(&p)
            .await
            .unwrap();
        assert!(
            !repo
                .list_for_assignee(assignee)
                .await
                .unwrap()
                .iter()
                .any(|task| task.id == id),
            "revoked room membership hides historical assigned tasks"
        );
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role)
             VALUES ($1, $2, 'member')",
        )
        .bind(room.to_uuid())
        .bind(assignee.to_uuid())
        .execute(&p)
        .await
        .unwrap();

        // partial update: change title only; assignee/due unchanged (COALESCE).
        assert!(repo
            .update(id, creator, Some("Ship it now"), None, None, None)
            .await
            .unwrap());
        assert!(matches!(
            repo.update(id, creator, None, Some(outsider), None, None)
                .await,
            Err(TaskWriteError::AssigneeNotMember)
        ));
        let after = repo.get(id).await.unwrap().expect("still present");
        assert_eq!(after.title, "Ship it now");
        assert_eq!(after.assignee_id, Some(assignee), "assignee preserved");
        assert!(
            after.updated_at >= got.updated_at,
            "updated_at moved forward"
        );

        // Status update to done; the open filter no longer shows it.
        assert!(repo
            .update(id, creator, None, None, None, Some(STATUS_DONE))
            .await
            .unwrap());
        assert_eq!(
            repo.get(id).await.unwrap().expect("present").status,
            STATUS_DONE
        );
        assert!(
            !repo
                .list_for_room(room, Some("open"))
                .await
                .unwrap()
                .iter()
                .any(|t| t.id == id),
            "done task leaves the open filter"
        );

        // Delete is authorized in the same transaction; stale ids report NotFound.
        assert!(
            repo.delete(id, creator).await.unwrap(),
            "first delete removes"
        );
        assert!(matches!(
            repo.update(id, creator, Some("x"), None, None, None).await,
            Err(TaskWriteError::NotFound)
        ));
        assert!(matches!(
            repo.delete(id, creator).await,
            Err(TaskWriteError::NotFound)
        ));
        assert!(
            repo.get(id).await.unwrap().is_none(),
            "deleted task is gone"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM tasks WHERE room_id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = ANY($1)")
            .bind(vec![room.to_uuid(), other_room.to_uuid()])
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM workspaces WHERE created_by = ANY($1)")
            .bind(vec![
                creator.to_uuid(),
                assignee.to_uuid(),
                outsider.to_uuid(),
            ])
            .execute(&p)
            .await
            .ok();
        for who in [creator, assignee, outsider] {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(who.to_uuid())
                .execute(&p)
                .await
                .ok();
        }
    }
}
