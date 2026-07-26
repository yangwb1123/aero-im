//! Legal-hold repository (retention exemption for eDiscovery preservation).
//!
//! Backs `migrations/0054_legal_holds.sql`. An admin places a room — or, with a
//! `NULL` `room_id`, a whole workspace — under a legal hold; while an active hold
//! covers a message's room (directly, or via the room's workspace) the periodic
//! retention sweep
//! ([`WorkspaceRepo::sweep_expired_messages`](crate::WorkspaceRepo::sweep_expired_messages))
//! must NOT soft-delete that message. This repo owns the legal-hold CRUD plus the
//! [`is_held`](LegalHoldRepo::is_held) coverage check; the sweep's own SQL carries
//! the equivalent `NOT EXISTS` exclusion so a single set-based `UPDATE` stays
//! correct.
//!
//! Purely additive: a NEW [`LegalHoldRepo`]; the only existing code touched is the
//! sweep's `WHERE` clause. The [`LegalHold`] model lives here (and is re-exported
//! from the crate root) rather than in `aero-common`, since it is a storage-layer
//! projection.

use aero_common::{LegalHoldId, ParticipantId, RoomId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;

/// One legal hold — an admin-placed preservation order over a room or a whole
/// workspace.
///
/// A storage-layer projection of a `legal_holds` row. `Serialize` so a handler
/// can hand the row straight back as JSON; the timestamps render as RFC 3339. A
/// `None` [`room_id`](Self::room_id) means the whole [`workspace_id`](Self::workspace_id)
/// is held; a `None` [`released_at`](Self::released_at) means the hold is still in
/// force.
#[derive(Debug, Clone, Serialize)]
pub struct LegalHold {
    /// The legal hold's unique id.
    pub id: LegalHoldId,
    /// The tenant the hold belongs to (and, for a workspace-wide hold, covers).
    pub workspace_id: WorkspaceId,
    /// The single room the hold covers, or `None` for a workspace-wide hold.
    pub room_id: Option<RoomId>,
    /// Human-readable justification recorded when the hold was placed.
    pub reason: String,
    /// The admin who placed the hold.
    pub created_by: ParticipantId,
    /// Whether the hold is currently in force (released holds are `false`).
    pub active: bool,
    /// When the hold was placed (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    /// When the hold was released, or `None` while still active (RFC 3339).
    #[serde(with = "time::serde::rfc3339::option")]
    pub released_at: Option<time::OffsetDateTime>,
}

/// The columns a [`LegalHold`] is built from, in select order. Shared by every
/// query so the row decoding stays in one place.
const COLUMNS: &str =
    "id, workspace_id, room_id, reason, created_by, active, created_at, released_at";

type Row = (
    uuid::Uuid,
    uuid::Uuid,
    Option<uuid::Uuid>,
    String,
    uuid::Uuid,
    bool,
    time::OffsetDateTime,
    Option<time::OffsetDateTime>,
);

fn row_to_model(r: Row) -> LegalHold {
    let (id, workspace_id, room_id, reason, created_by, active, created_at, released_at) = r;
    LegalHold {
        id: LegalHoldId::from_uuid(id),
        workspace_id: WorkspaceId::from_uuid(workspace_id),
        room_id: room_id.map(RoomId::from_uuid),
        reason,
        created_by: ParticipantId::from_uuid(created_by),
        active,
        created_at,
        released_at,
    }
}

/// Repository over the `legal_holds` table (retention exemptions).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`LegalHoldRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct LegalHoldRepo {
    pool: PgPool,
}

impl LegalHoldRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Place a new active legal hold in `workspace`, returning its generated id.
    /// A `Some(room)` holds that single room; `None` holds the whole workspace.
    /// The caller is responsible for admin authorization and for validating that
    /// `room` (when given) belongs to `workspace`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn create(
        &self,
        workspace: WorkspaceId,
        room: Option<RoomId>,
        reason: &str,
        by: ParticipantId,
    ) -> Result<LegalHoldId, sqlx::Error> {
        let id = LegalHoldId::new();
        sqlx::query(
            r"INSERT INTO legal_holds (id, workspace_id, room_id, reason, created_by)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id.to_uuid())
        .bind(workspace.to_uuid())
        .bind(room.map(|r| r.to_uuid()))
        .bind(reason)
        .bind(by.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// List the active legal holds in `workspace`, newest first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn list_active(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<LegalHold>, sqlx::Error> {
        let sql = format!(
            "SELECT {COLUMNS}
               FROM legal_holds
              WHERE workspace_id = $1 AND active
              ORDER BY created_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(workspace.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Fetch one legal hold by id, or `None` if no such row exists. Used by the
    /// release endpoint to resolve the hold's workspace before re-checking admin.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get(&self, id: LegalHoldId) -> Result<Option<LegalHold>, sqlx::Error> {
        let sql = format!("SELECT {COLUMNS} FROM legal_holds WHERE id = $1");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(row_to_model))
    }

    /// Release an active legal hold — set `active = false` and stamp `released_at`.
    /// Workspace-scoped: the `id` must belong to `workspace`, so a caller can never
    /// release another tenant's hold. Returns `true` iff a still-active row was
    /// flipped; a second release (or a stranger's) is a no-op returning `false`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the update.
    pub async fn release(
        &self,
        id: LegalHoldId,
        workspace: WorkspaceId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE legal_holds
                 SET active = false, released_at = now()
               WHERE id = $1 AND workspace_id = $2 AND active",
        )
        .bind(id.to_uuid())
        .bind(workspace.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Whether `room` is currently under an active legal hold — either a
    /// room-scoped hold on it directly, or a workspace-wide hold (`room_id IS
    /// NULL`) on the room's workspace. Returns `false` for an unknown room (no
    /// matching `rooms` row ⇒ no coverage).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn is_held(&self, room: RoomId) -> Result<bool, sqlx::Error> {
        let held = sqlx::query_scalar::<_, bool>(
            r"SELECT EXISTS (
                SELECT 1
                  FROM rooms r
                  JOIN legal_holds lh
                    ON lh.active
                   AND (lh.room_id = r.id
                        OR (lh.room_id IS NULL AND lh.workspace_id = r.workspace_id))
                 WHERE r.id = $1
              )",
        )
        .bind(room.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(held)
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored legal_hold
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    /// The reserved all-zero default workspace, guaranteed to exist by migration
    /// 0006's backfill — reused so the legal-hold rows are well-scoped.
    const DEFAULT_WS: &str = "00000000-0000-0000-0000-000000000000";

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    fn default_ws() -> WorkspaceId {
        WorkspaceId::from_uuid(uuid::Uuid::parse_str(DEFAULT_WS).expect("valid uuid"))
    }

    /// Create a throwaway admin participant so the test is self-contained.
    async fn actor(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("legal-hold-actor-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    /// Create a throwaway workspace so a test that places a *workspace-wide* hold
    /// stays isolated from other tests running in parallel against the shared DB
    /// (a NULL-room hold on the default workspace would otherwise bleed into any
    /// concurrent test checking `is_held` on a default-workspace room).
    async fn fresh_ws(p: &PgPool, creator: ParticipantId) -> WorkspaceId {
        let id = WorkspaceId::new();
        sqlx::query(
            "INSERT INTO workspaces (id, name, slug, created_by, created_at)
             VALUES ($1, $2, $3, $4, now())",
        )
        .bind(id.to_uuid())
        .bind(format!("legal-hold-ws-{id}"))
        .bind(format!("lh-{id}"))
        .bind(creator.to_uuid())
        .execute(p)
        .await
        .expect("insert workspace");
        id
    }

    /// Create a throwaway room in `ws` so `is_held`'s `JOIN rooms` resolves.
    async fn room_in(p: &PgPool, ws: WorkspaceId, creator: ParticipantId) -> RoomId {
        let id = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id)
             VALUES ($1, 'channel', $2, $3, $4)",
        )
        .bind(id.to_uuid())
        .bind(format!("legal-hold-room-{id}"))
        .bind(creator.to_uuid())
        .bind(ws.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn create_list_release_room_scoped() {
        let p = pool();
        let repo = LegalHoldRepo::new(p.clone());
        let ws = default_ws();
        let actor = actor(&p).await;
        let room = room_in(&p, ws, actor).await;

        // Not held before any hold exists.
        assert!(!repo.is_held(room).await.unwrap(), "no hold yet");

        // Room-scoped hold → is_held true, list shows it.
        let id = repo
            .create(ws, Some(room), "litigation X", actor)
            .await
            .unwrap();
        assert!(repo.is_held(room).await.unwrap(), "room directly held");
        let listed = repo.list_active(ws).await.unwrap();
        let found = listed.iter().find(|h| h.id == id).expect("present");
        assert_eq!(found.room_id, Some(room));
        assert_eq!(found.reason, "litigation X");
        assert!(found.active && found.released_at.is_none());

        // get resolves the hold's workspace.
        let got = repo.get(id).await.unwrap().expect("present");
        assert_eq!(got.workspace_id, ws);

        // Release flips active; second release is a no-op; is_held back to false.
        assert!(repo.release(id, ws).await.unwrap(), "first release");
        assert!(!repo.release(id, ws).await.unwrap(), "second release no-op");
        assert!(!repo.is_held(room).await.unwrap(), "released → not held");
        assert!(
            !repo.list_active(ws).await.unwrap().iter().any(|h| h.id == id),
            "released hold leaves the active list"
        );

        // Cleanup.
        sqlx::query("DELETE FROM legal_holds WHERE created_by = $1")
            .bind(actor.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn workspace_wide_hold_covers_room() {
        let p = pool();
        let repo = LegalHoldRepo::new(p.clone());
        let actor = actor(&p).await;
        // Own throwaway workspace: a workspace-wide hold here must not leak into a
        // parallel test checking the shared default workspace.
        let ws = fresh_ws(&p, actor).await;
        let room = room_in(&p, ws, actor).await;

        // A workspace-wide hold (NULL room_id) covers the room via its workspace.
        let id = repo.create(ws, None, "ws-wide", actor).await.unwrap();
        assert!(
            repo.is_held(room).await.unwrap(),
            "workspace-wide hold covers the room"
        );
        assert!(repo.release(id, ws).await.unwrap());
        assert!(!repo.is_held(room).await.unwrap(), "released → not held");

        // Cleanup.
        sqlx::query("DELETE FROM legal_holds WHERE created_by = $1")
            .bind(actor.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(room.to_uuid())
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(ws.to_uuid())
            .execute(&p)
            .await
            .ok();
    }

    /// Insert a message into `room` with an explicit (back-dated) `created_at`,
    /// returning its id. Used to put a message past a retention window.
    async fn insert_msg(
        p: &PgPool,
        room: RoomId,
        sender: ParticipantId,
        created_at: time::OffsetDateTime,
    ) -> aero_common::MessageId {
        let id = aero_common::MessageId::new();
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks, searchable_text, created_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(sender.to_uuid())
        .bind(serde_json::json!([{"type": "text", "content": "preserve me"}]))
        .bind("preserve me")
        .bind(created_at)
        .execute(p)
        .await
        .expect("insert message");
        id
    }

    async fn is_deleted(p: &PgPool, msg: aero_common::MessageId) -> bool {
        let row: Option<(Option<time::OffsetDateTime>,)> =
            sqlx::query_as("SELECT deleted_at FROM messages WHERE id = $1")
                .bind(msg.to_uuid())
                .fetch_optional(p)
                .await
                .expect("select message");
        matches!(row, Some((Some(_),)))
    }

    /// The core value of a legal hold: the retention sweep must NOT soft-delete a
    /// held room's past-window messages, but must once the hold is released.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn sweep_preserves_legally_held_room() {
        let p = pool();
        let holds = LegalHoldRepo::new(p.clone());
        let workspaces = crate::WorkspaceRepo::new(p.clone());
        let actor = actor(&p).await;
        let ws = fresh_ws(&p, actor).await;
        let room = room_in(&p, ws, actor).await;

        // 1-day retention; a message 5 days old is well past the window.
        workspaces.set_retention(ws, Some(1)).await.unwrap();
        let msg = insert_msg(&p, room, actor, time::OffsetDateTime::now_utc() - time::Duration::days(5)).await;
        let now = time::OffsetDateTime::now_utc();

        // Held → sweep preserves the message.
        let hid = holds.create(ws, Some(room), "litigation hold", actor).await.unwrap();
        workspaces.sweep_expired_messages(now, Some(ws)).await.unwrap();
        assert!(
            !is_deleted(&p, msg).await,
            "a held room's past-window message must survive the sweep"
        );

        // Released → next sweep soft-deletes it.
        assert!(holds.release(hid, ws).await.unwrap());
        workspaces.sweep_expired_messages(now, Some(ws)).await.unwrap();
        assert!(
            is_deleted(&p, msg).await,
            "after release, the past-window message is swept"
        );

        // Cleanup.
        sqlx::query("DELETE FROM messages WHERE room_id = $1").bind(room.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM legal_holds WHERE created_by = $1").bind(actor.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1").bind(room.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM workspaces WHERE id = $1").bind(ws.to_uuid()).execute(&p).await.ok();
    }
}
