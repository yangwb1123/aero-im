//! Channel join-request repository (request-to-join with owner/admin approval).
//!
//! Backs `migrations/0043_channel_join_requests.sql`. A workspace member asks to
//! join a channel they are not yet in; the channel's creator (owner) or a
//! workspace admin then approves or denies. Each row starts `pending` and
//! transitions exactly once to `approved`/`denied`, recording who decided and
//! when.
//!
//! A partial unique index (`channel_join_requests_pending_uq`) keeps at most one
//! outstanding request per `(room, requester)`. Creation and decision both
//! repeat tenant authorization inside their transaction; approval grants room
//! membership in that same transaction, so a crash cannot leave a pending
//! request whose requester was already admitted.
//!
//! Purely additive: a NEW [`JoinRequestRepo`] over a NEW table; no existing repo
//! is touched. The [`JoinRequest`] model lives here (and is re-exported from the
//! crate root) rather than in `aero-common`, since it is a storage-layer
//! projection.
//!
//! [`create_authorized`]: JoinRequestRepo::create_authorized

use aero_common::{JoinRequestId, ParticipantId, RoomId, WorkspaceRole};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};

/// One channel join request — a member's request to join a channel, pending an
/// owner's / admin's decision.
///
/// A storage-layer projection of a `channel_join_requests` row. `Serialize` so a
/// handler can hand the row straight back as JSON; `created_at` renders as
/// RFC 3339, and `decided_at` as RFC 3339 or `null` while still pending.
#[derive(Debug, Clone, Serialize)]
pub struct JoinRequest {
    /// The join request's unique id.
    pub id: JoinRequestId,
    /// The channel (room) the requester wants to join.
    pub room_id: RoomId,
    /// The participant asking to join.
    pub requester_id: ParticipantId,
    /// Lifecycle status: `pending`, `approved`, or `denied`.
    pub status: String,
    /// When the request was created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    /// When the request was decided, or `None` while still pending (RFC 3339 or
    /// `null` on the wire).
    #[serde(with = "time::serde::rfc3339::option")]
    pub decided_at: Option<time::OffsetDateTime>,
    /// Who decided the request (owner / admin), or `None` while still pending.
    pub decided_by: Option<ParticipantId>,
}

/// The columns a [`JoinRequest`] is built from, in select order. Shared by every
/// query so the row decoding stays in one place.
const COLUMNS: &str = "id, room_id, requester_id, status, created_at, decided_at, decided_by";

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: uuid::Uuid,
    room_id: uuid::Uuid,
    requester_id: uuid::Uuid,
    status: String,
    created_at: time::OffsetDateTime,
    decided_at: Option<time::OffsetDateTime>,
    decided_by: Option<uuid::Uuid>,
}

fn row_to_model(r: Row) -> JoinRequest {
    JoinRequest {
        id: JoinRequestId::from_uuid(r.id),
        room_id: RoomId::from_uuid(r.room_id),
        requester_id: ParticipantId::from_uuid(r.requester_id),
        status: r.status,
        created_at: r.created_at,
        decided_at: r.decided_at,
        decided_by: r.decided_by.map(ParticipantId::from_uuid),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum JoinRequestWriteError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("channel or request not found")]
    NotFound,
    #[error("requester is not an effective member of the channel workspace")]
    RequesterNotMember,
    #[error("requester is already a room member")]
    AlreadyMember,
    #[error("decider is not allowed to manage this channel")]
    Forbidden,
    #[error("decision status must be approved or denied")]
    InvalidStatus,
}

async fn effective_role(
    tx: &mut Transaction<'_, Postgres>,
    workspace: uuid::Uuid,
    participant: ParticipantId,
) -> Result<Option<WorkspaceRole>, sqlx::Error> {
    let role = sqlx::query_scalar::<_, String>(
        r"SELECT membership.role
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
    .bind(workspace)
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    Ok(role.and_then(|value| WorkspaceRole::from_db_str(&value)))
}

/// Repository over the `channel_join_requests` table (request-to-join lifecycle).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`JoinRequestRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct JoinRequestRepo {
    pool: PgPool,
}

impl JoinRequestRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a pending request after transactionally proving that `requester`
    /// remains an effective member of the room's workspace and is not already in
    /// the room. Repeated pending asks return the existing id.
    ///
    /// # Errors
    /// Propagates storage errors and returns a typed authorization conflict.
    pub async fn create_authorized(
        &self,
        room: RoomId,
        requester: ParticipantId,
    ) -> Result<JoinRequestId, JoinRequestWriteError> {
        let mut tx = self.pool.begin().await?;
        let workspace = sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT workspace_id
               FROM rooms
              WHERE id = $1 AND kind = 'channel'
              FOR SHARE",
        )
        .bind(room.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(JoinRequestWriteError::NotFound)?;
        effective_role(&mut tx, workspace, requester)
            .await?
            .ok_or(JoinRequestWriteError::RequesterNotMember)?;
        let already_member = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
                 SELECT 1 FROM room_members
                  WHERE room_id = $1 AND participant_id = $2
             )",
        )
        .bind(room.to_uuid())
        .bind(requester.to_uuid())
        .fetch_one(&mut *tx)
        .await?;
        if already_member {
            return Err(JoinRequestWriteError::AlreadyMember);
        }

        let id = JoinRequestId::new();
        let stored = sqlx::query_scalar::<_, uuid::Uuid>(
            r"INSERT INTO channel_join_requests (id, room_id, requester_id)
               VALUES ($1, $2, $3)
               ON CONFLICT (room_id, requester_id) WHERE status = 'pending'
               DO UPDATE SET requester_id = EXCLUDED.requester_id
               RETURNING id",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(requester.to_uuid())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(JoinRequestId::from_uuid(stored))
    }

    /// List join requests for `room`, newest first. When `status` is `Some`, only
    /// rows in that status are returned (e.g. `Some("pending")` for the approval
    /// queue); `None` returns every request regardless of status.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_for_room(
        &self,
        room: RoomId,
        status: Option<&str>,
    ) -> Result<Vec<JoinRequest>, sqlx::Error> {
        let rows = if let Some(status) = status {
            let sql = format!(
                "SELECT {COLUMNS} FROM channel_join_requests
                  WHERE room_id = $1 AND status = $2
                  ORDER BY created_at DESC, id DESC"
            );
            sqlx::query_as::<_, Row>(&sql)
                .bind(room.to_uuid())
                .bind(status)
                .fetch_all(&self.pool)
                .await?
        } else {
            let sql = format!(
                "SELECT {COLUMNS} FROM channel_join_requests
                  WHERE room_id = $1
                  ORDER BY created_at DESC, id DESC"
            );
            sqlx::query_as::<_, Row>(&sql)
                .bind(room.to_uuid())
                .fetch_all(&self.pool)
                .await?
        };
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Fetch one join request by id, or `None` if no such row exists.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(&self, id: JoinRequestId) -> Result<Option<JoinRequest>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM channel_join_requests WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// Decide a pending request under one row lock. The decider must still be the
    /// room creator or an effective workspace administrator; the requester must
    /// still be an effective workspace member. Approval inserts `room_members`
    /// before the request transition in the same transaction.
    ///
    /// # Errors
    /// Returns typed authorization/lifecycle errors.
    pub async fn decide_authorized(
        &self,
        id: JoinRequestId,
        status: &str,
        decider: ParticipantId,
    ) -> Result<JoinRequest, JoinRequestWriteError> {
        if !matches!(status, "approved" | "denied") {
            return Err(JoinRequestWriteError::InvalidStatus);
        }
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, String, uuid::Uuid, uuid::Uuid)>(
            r"SELECT request.room_id, request.requester_id, request.status,
                     room.workspace_id, room.created_by
                FROM channel_join_requests request
                JOIN rooms room ON room.id = request.room_id
               WHERE request.id = $1
               FOR UPDATE OF request, room",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(JoinRequestWriteError::NotFound)?;
        let (room_id, requester_id, current_status, workspace, creator) = row;
        if current_status != "pending" {
            return Err(JoinRequestWriteError::NotFound);
        }
        let role = effective_role(&mut tx, workspace, decider)
            .await?
            .ok_or(JoinRequestWriteError::Forbidden)?;
        if decider.to_uuid() != creator && !role.can_administer() {
            return Err(JoinRequestWriteError::Forbidden);
        }
        let requester = ParticipantId::from_uuid(requester_id);
        effective_role(&mut tx, workspace, requester)
            .await?
            .ok_or(JoinRequestWriteError::RequesterNotMember)?;

        if status == "approved" {
            sqlx::query(
                "INSERT INTO room_members (room_id, participant_id, role)
                 VALUES ($1, $2, 'member')
                 ON CONFLICT (room_id, participant_id) DO NOTHING",
            )
            .bind(room_id)
            .bind(requester_id)
            .execute(&mut *tx)
            .await?;
        }
        let sql = format!(
            "UPDATE channel_join_requests
                SET status = $2, decided_at = now(), decided_by = $3
              WHERE id = $1 AND status = 'pending'
          RETURNING {COLUMNS}"
        );
        let updated = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .bind(status)
            .bind(decider.to_uuid())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(JoinRequestWriteError::NotFound)?;
        tx.commit().await?;
        Ok(row_to_model(updated))
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored join_request
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::{RoomRepo, WorkspaceRepo};

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
            .bind(format!("join-req-participant-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    async fn mk_workspace(p: &PgPool, owner: ParticipantId) -> aero_common::WorkspaceId {
        WorkspaceRepo::new(p.clone())
            .create(
                format!("Join request {owner}"),
                format!("join-request-{owner}"),
                owner,
            )
            .await
            .expect("create workspace")
            .id
    }

    /// Insert a channel room in a test-owned workspace.
    async fn mk_room(
        p: &PgPool,
        workspace: aero_common::WorkspaceId,
        creator: ParticipantId,
    ) -> RoomId {
        RoomRepo::new(p.clone())
            .create_in_workspace(
                workspace,
                aero_common::RoomKind::Channel,
                Some(format!("join-req-room-{}", RoomId::new())),
                creator,
            )
            .await
            .expect("insert room")
            .id
    }

    async fn enroll(
        p: &PgPool,
        workspace: aero_common::WorkspaceId,
        participant: ParticipantId,
        role: &str,
    ) {
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1, $2, $3)
             ON CONFLICT (workspace_id, participant_id)
             DO UPDATE SET role = EXCLUDED.role",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(role)
        .execute(p)
        .await
        .expect("enroll participant");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn create_is_idempotent_for_pending_then_decide_flips_once() {
        let p = pool();
        let repo = JoinRequestRepo::new(p.clone());

        let owner = mk_participant(&p).await;
        let requester = mk_participant(&p).await;
        let revoked_requester = mk_participant(&p).await;
        let admin = mk_participant(&p).await;
        let outsider = mk_participant(&p).await;
        let workspace = mk_workspace(&p, owner).await;
        let room = mk_room(&p, workspace, owner).await;
        enroll(&p, workspace, requester, "member").await;
        enroll(&p, workspace, revoked_requester, "member").await;
        enroll(&p, workspace, admin, "admin").await;

        assert!(matches!(
            repo.create_authorized(room, outsider).await,
            Err(JoinRequestWriteError::RequesterNotMember)
        ));

        // create → a second create for the same (room, requester) while pending
        // resolves to the SAME id (idempotent for an outstanding ask).
        let id = repo.create_authorized(room, requester).await.unwrap();
        let again = repo.create_authorized(room, requester).await.unwrap();
        assert_eq!(id, again, "repeated pending ask is idempotent");

        // list_for_room(pending) surfaces it.
        let pending = repo.list_for_room(room, Some("pending")).await.unwrap();
        assert!(
            pending.iter().any(|r| r.id == id),
            "pending request shows in the approval queue"
        );

        // get reflects the pending status (and no decision yet).
        let got = repo.get(id).await.unwrap().expect("present");
        assert_eq!(got.status, "pending");
        assert!(got.decided_at.is_none());
        assert!(got.decided_by.is_none());

        // decide('approved', admin) flips the pending row; a second decide is a
        // no-op (false).
        assert!(matches!(
            repo.decide_authorized(id, "approved", outsider).await,
            Err(JoinRequestWriteError::Forbidden)
        ));
        let approved = repo.decide_authorized(id, "approved", admin).await.unwrap();
        assert_eq!(approved.status, "approved");
        assert!(matches!(
            repo.decide_authorized(id, "approved", admin).await,
            Err(JoinRequestWriteError::NotFound)
        ));
        let admitted = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
                 SELECT 1 FROM room_members
                  WHERE room_id = $1 AND participant_id = $2
             )",
        )
        .bind(room.to_uuid())
        .bind(requester.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert!(admitted, "approval grants membership atomically");
        assert!(matches!(
            repo.create_authorized(room, requester).await,
            Err(JoinRequestWriteError::AlreadyMember)
        ));

        let revoked_id = repo
            .create_authorized(room, revoked_requester)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO workspace_deactivations
                 (workspace_id, participant_id, deactivated_by)
             VALUES ($1, $2, $3)",
        )
        .bind(workspace.to_uuid())
        .bind(revoked_requester.to_uuid())
        .bind(admin.to_uuid())
        .execute(&p)
        .await
        .unwrap();
        assert!(matches!(
            repo.decide_authorized(revoked_id, "approved", admin).await,
            Err(JoinRequestWriteError::RequesterNotMember)
        ));
        let not_admitted = sqlx::query_scalar::<_, bool>(
            "SELECT NOT EXISTS (
                 SELECT 1 FROM room_members
                  WHERE room_id = $1 AND participant_id = $2
             )",
        )
        .bind(room.to_uuid())
        .bind(revoked_requester.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert!(
            not_admitted,
            "revoked requester is never partially admitted"
        );

        // get reflects the new status + decision stamp.
        let decided = repo.get(id).await.unwrap().expect("present");
        assert_eq!(decided.status, "approved");
        assert!(decided.decided_at.is_some());
        assert_eq!(decided.decided_by, Some(admin));

        // The pending queue no longer lists it; "all" still does.
        assert!(
            !repo
                .list_for_room(room, Some("pending"))
                .await
                .unwrap()
                .iter()
                .any(|r| r.id == id),
            "decided request leaves the pending queue"
        );
        assert!(
            repo.list_for_room(room, None)
                .await
                .unwrap()
                .iter()
                .any(|r| r.id == id),
            "list_for_room(None) still shows the decided request"
        );

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM channel_join_requests WHERE room_id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(workspace.to_uuid())
            .execute(&p)
            .await
            .expect("delete workspace");
        for who in [owner, requester, revoked_requester, admin, outsider] {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(who.to_uuid())
                .execute(&p)
                .await
                .ok();
        }
    }
}
