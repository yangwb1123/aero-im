//! Channel join-request repository (request-to-join with owner/admin approval).
//!
//! Backs `migrations/0043_channel_join_requests.sql`. A workspace member asks to
//! join a channel they are not yet in; the channel's creator (owner) or a
//! workspace admin then approves or denies. Each row starts `pending` and
//! transitions exactly once to `approved`/`denied`, recording who decided and
//! when.
//!
//! A partial unique index (`channel_join_requests_pending_uq`) keeps at most one
//! OUTSTANDING (`pending`) request per `(room, requester)`, so [`create`] is
//! idempotent for a repeated ask: on the unique violation it resolves to the
//! existing pending row's id rather than erroring. *Granting* membership on
//! approval is deliberately left to the existing
//! [`RoomRepo::add_member`](crate::RoomRepo) — this repo only owns the request
//! lifecycle and never touches `room_members`.
//!
//! Purely additive: a NEW [`JoinRequestRepo`] over a NEW table; no existing repo
//! is touched. The [`JoinRequest`] model lives here (and is re-exported from the
//! crate root) rather than in `aero-common`, since it is a storage-layer
//! projection.
//!
//! [`create`]: JoinRequestRepo::create

use aero_common::{JoinRequestId, ParticipantId, RoomId};
use serde::Serialize;
use sqlx::PgPool;

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
const COLUMNS: &str =
    "id, room_id, requester_id, status, created_at, decided_at, decided_by";

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

    /// Create a pending join request for `requester` to join `room`, returning its
    /// id. Idempotent for an outstanding ask: if a `pending` request already
    /// exists for this `(room, requester)` the partial unique index rejects the
    /// insert, and this resolves to the existing pending row's id instead of
    /// erroring. Caller is responsible for the owner/admin authorization and for
    /// rejecting an already-member requester.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] other than the expected unique-violation
    /// fast path (which is recovered into the existing id).
    pub async fn create(
        &self,
        room: RoomId,
        requester: ParticipantId,
    ) -> Result<JoinRequestId, sqlx::Error> {
        let id = JoinRequestId::new();
        let result = sqlx::query(
            r"INSERT INTO channel_join_requests (id, room_id, requester_id)
               VALUES ($1, $2, $3)",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(requester.to_uuid())
        .execute(&self.pool)
        .await;

        match result {
            Ok(_) => Ok(id),
            // A pending request already exists for this (room, requester): the
            // partial unique index fires. Resolve to the existing pending row so a
            // repeated ask is idempotent.
            Err(e) if is_unique_violation(&e) => {
                let row = sqlx::query_as::<_, (uuid::Uuid,)>(
                    r"SELECT id FROM channel_join_requests
                       WHERE room_id = $1 AND requester_id = $2 AND status = 'pending'
                       LIMIT 1",
                )
                .bind(room.to_uuid())
                .bind(requester.to_uuid())
                .fetch_one(&self.pool)
                .await?;
                Ok(JoinRequestId::from_uuid(row.0))
            }
            Err(e) => Err(e),
        }
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
    pub async fn get(
        &self,
        id: JoinRequestId,
    ) -> Result<Option<JoinRequest>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM channel_join_requests WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// Decide a `pending` request: set its `status` (to `approved` / `denied`),
    /// stamp `decided_at` + `decided_by`, and return `true` iff a pending row was
    /// changed. Only ever transitions a `pending` row (the `WHERE status =
    /// 'pending'` guard), so a second decide (or deciding an already-decided
    /// request) is a no-op returning `false` — making the decision idempotent and
    /// race-safe. Validating the target status string is the caller's concern.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn decide(
        &self,
        id: JoinRequestId,
        status: &str,
        decider: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE channel_join_requests
                 SET status = $2, decided_at = now(), decided_by = $3
               WHERE id = $1 AND status = 'pending'",
        )
        .bind(id.to_uuid())
        .bind(status)
        .bind(decider.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

/// Whether a [`sqlx::Error`] is a Postgres unique-violation (`SQLSTATE 23505`).
/// Used to recover [`JoinRequestRepo::create`]'s idempotent fast path when a
/// pending request already exists for the same `(room, requester)`.
fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(
        e,
        sqlx::Error::Database(db) if db.code().as_deref() == Some("23505")
    )
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

    /// Insert a channel room in the default workspace so the request is well-scoped.
    async fn mk_room(p: &PgPool, creator: ParticipantId) -> RoomId {
        let id = RoomId::new();
        sqlx::query(
            r"INSERT INTO rooms (id, kind, name, created_by, workspace_id)
               VALUES ($1, 'channel', $2, $3, '00000000-0000-0000-0000-000000000000')",
        )
        .bind(id.to_uuid())
        .bind(format!("join-req-room-{id}"))
        .bind(creator.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn create_is_idempotent_for_pending_then_decide_flips_once() {
        let p = pool();
        let repo = JoinRequestRepo::new(p.clone());

        let owner = mk_participant(&p).await;
        let requester = mk_participant(&p).await;
        let admin = mk_participant(&p).await;
        let room = mk_room(&p, owner).await;

        // create → a second create for the same (room, requester) while pending
        // resolves to the SAME id (idempotent for an outstanding ask).
        let id = repo.create(room, requester).await.unwrap();
        let again = repo.create(room, requester).await.unwrap();
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
        assert!(
            repo.decide(id, "approved", admin).await.unwrap(),
            "first decision flips the pending row"
        );
        assert!(
            !repo.decide(id, "approved", admin).await.unwrap(),
            "second decision is a no-op"
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
        for who in [owner, requester, admin] {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(who.to_uuid())
                .execute(&p)
                .await
                .ok();
        }
    }
}
