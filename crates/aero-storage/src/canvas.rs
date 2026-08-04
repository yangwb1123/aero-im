//! Channel-canvas repository (per-channel collaborative documents).
//!
//! Backs `migrations/0048_channel_canvas.sql`. A channel ("room") can own
//! several canvases — Slack Canvas / Lark Docs-in-channel. Each is a titled rich
//! document whose body is a JSON array of arbitrary blocks, stored verbatim as
//! JSONB and deliberately NOT coupled to [`aero_common::Block`]: the repo accepts
//! and returns an opaque [`serde_json::Value`].
//!
//! Every public operation owns its authorization transaction: the canonical
//! workspace → room → membership locks are held through the tenant-scoped
//! query/mutation. Only live `channel` rooms are eligible. A canvas id supplied
//! through another room is an opaque not-found, while a current room member
//! losing effective access is forbidden.

use aero_common::{CanvasId, Error, ParticipantId, RoomId};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};

const MAX_TITLE_CHARS: usize = 512;
const MAX_BLOCKS_BYTES: usize = 1024 * 1024;

/// One channel canvas — a per-channel collaborative document.
///
/// A storage-layer projection of a `channel_canvases` row. `Serialize` so a
/// handler can hand the row straight back as JSON; `created_at` / `updated_at`
/// render as RFC 3339, and `blocks` is the raw stored payload (a JSON array of
/// blocks).
#[derive(Debug, Clone, Serialize)]
pub struct Canvas {
    /// The canvas's unique id.
    pub id: CanvasId,
    /// The channel (room) the canvas belongs to.
    pub room_id: RoomId,
    /// The participant who originally created the canvas.
    pub author_id: ParticipantId,
    /// Human-readable title of the document.
    pub title: String,
    /// The document body (an arbitrary JSON array of blocks).
    pub blocks: serde_json::Value,
    /// When the canvas was created (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    /// When the canvas was last edited (RFC 3339 on the wire).
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: time::OffsetDateTime,
    /// Monotonic edit version for optimistic concurrency (bumped on every update).
    /// A client echoes this on a PUT to detect a concurrent edit (方向五).
    pub version: i64,
    /// Highest durable op already materialized into [`Self::blocks`].
    ///
    /// Clients rebuild from this snapshot and replay only ops with a greater
    /// per-canvas sequence.
    pub snapshot_op_seq: i64,
}

/// Optional fields for one authorized canvas update.
#[derive(Debug, Clone, Copy)]
pub struct CanvasPatch<'a> {
    /// Replacement title; omitted to retain the current title.
    pub title: Option<&'a str>,
    /// Replacement snapshot; omitted to retain the current blocks.
    pub blocks: Option<&'a serde_json::Value>,
    /// Optimistic version observed by the client.
    pub expected_version: Option<i64>,
    /// Highest durable op included in `blocks`.
    pub snapshot_op_seq: Option<i64>,
}

/// The columns a [`Canvas`] is built from, in select order. Shared by every query
/// so the row decoding stays in one place.
const COLUMNS: &str =
    "id, room_id, author_id, title, blocks, created_at, updated_at, version, snapshot_op_seq";

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    id: uuid::Uuid,
    room_id: uuid::Uuid,
    author_id: uuid::Uuid,
    title: String,
    blocks: serde_json::Value,
    created_at: time::OffsetDateTime,
    updated_at: time::OffsetDateTime,
    version: i64,
    snapshot_op_seq: i64,
}

fn row_to_model(r: Row) -> Canvas {
    Canvas {
        id: CanvasId::from_uuid(r.id),
        room_id: RoomId::from_uuid(r.room_id),
        author_id: ParticipantId::from_uuid(r.author_id),
        title: r.title,
        blocks: r.blocks,
        created_at: r.created_at,
        updated_at: r.updated_at,
        version: r.version,
        snapshot_op_seq: r.snapshot_op_seq,
    }
}

/// Repository over the `channel_canvases` table (per-channel documents).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`CanvasRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct CanvasRepo {
    pool: PgPool,
}

impl CanvasRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a canvas while `author` remains an effective member of the live
    /// channel, returning the exact committed row.
    ///
    /// # Errors
    /// Returns [`Error::Invalid`] for an invalid title/body,
    /// [`Error::NotFound`] for a missing/non-channel/archived room,
    /// [`Error::Forbidden`] when access was revoked, and propagates storage
    /// failures.
    pub async fn create_canvas_authorized(
        &self,
        room: RoomId,
        author: ParticipantId,
        title: &str,
        blocks: &serde_json::Value,
    ) -> Result<Canvas, Error> {
        let title = validate_title(title)?;
        validate_blocks(blocks)?;
        let mut tx = self.pool.begin().await?;
        lock_live_channel_access(&mut tx, room, author).await?;
        let id = CanvasId::new();
        let sql = format!(
            "INSERT INTO channel_canvases (id, room_id, author_id, title, blocks)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING {COLUMNS}"
        );
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .bind(room.to_uuid())
            .bind(author.to_uuid())
            .bind(title)
            .bind(blocks)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(row_to_model(row))
    }

    /// Fetch one canvas through its path room while the actor's effective live
    /// channel access remains locked.
    ///
    /// # Errors
    /// A missing id or id owned by another room is an opaque not-found.
    pub async fn get_canvas_authorized(
        &self,
        room: RoomId,
        id: CanvasId,
        actor: ParticipantId,
    ) -> Result<Canvas, Error> {
        let mut tx = self.pool.begin().await?;
        lock_live_channel_access(&mut tx, room, actor).await?;
        let sql = format!("SELECT {COLUMNS} FROM channel_canvases WHERE id = $1 AND room_id = $2");
        let row = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .bind(room.to_uuid())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| Error::NotFound(format!("canvas {id}")))?;
        tx.commit().await?;
        Ok(row_to_model(row))
    }

    /// List a live channel's canvases while the actor's effective access remains
    /// locked, newest edit first (`updated_at DESC`).
    ///
    /// # Errors
    pub async fn list_canvases_authorized(
        &self,
        room: RoomId,
        actor: ParticipantId,
    ) -> Result<Vec<Canvas>, Error> {
        let mut tx = self.pool.begin().await?;
        lock_live_channel_access(&mut tx, room, actor).await?;
        let sql = format!(
            "SELECT {COLUMNS}
               FROM channel_canvases
              WHERE room_id = $1
              ORDER BY updated_at DESC, id DESC"
        );
        let rows = sqlx::query_as::<_, Row>(&sql)
            .bind(room.to_uuid())
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(row_to_model).collect())
    }

    /// Edit a path-bound canvas while effective live-channel access remains
    /// locked, bumping its `version` and returning the committed snapshot.
    ///
    /// Omitted `title` / `blocks` values retain their latest database values,
    /// avoiding a stale handler pre-read overwriting a concurrent change. When
    /// `blocks` is present, `snapshot_op_seq` must equal the canvas's current
    /// `op_seq`: the single `UPDATE` takes the same row lock used by op append
    /// allocation, so either the snapshot anchors first and a concurrent op is
    /// strictly after it, or the op wins and the stale snapshot matches no row.
    /// `expected_version` is checked in that same atomic predicate.
    ///
    /// A stale `expected_version` or `snapshot_op_seq` is a conflict. A missing
    /// or cross-room id is an opaque not-found.
    pub async fn update_canvas_authorized(
        &self,
        room: RoomId,
        id: CanvasId,
        actor: ParticipantId,
        patch: CanvasPatch<'_>,
    ) -> Result<Canvas, Error> {
        let CanvasPatch {
            title,
            blocks,
            expected_version,
            snapshot_op_seq,
        } = patch;
        if title.is_none() && blocks.is_none() {
            return Err(Error::Invalid("title or blocks is required".into()));
        }
        let title = title.map(validate_title).transpose()?;
        if let Some(blocks) = blocks {
            validate_blocks(blocks)?;
            if expected_version.is_none() || snapshot_op_seq.is_none() {
                return Err(Error::Invalid(
                    "blocks requires expected_version and snapshot_op_seq".into(),
                ));
            }
        } else if snapshot_op_seq.is_some() {
            return Err(Error::Invalid(
                "snapshot_op_seq is only valid with blocks".into(),
            ));
        }
        if expected_version.is_some_and(|version| version < 0) {
            return Err(Error::Invalid(
                "expected_version must be non-negative".into(),
            ));
        }
        if snapshot_op_seq.is_some_and(|seq| seq < 0) {
            return Err(Error::Invalid(
                "snapshot_op_seq must be non-negative".into(),
            ));
        }

        let mut tx = self.pool.begin().await?;
        lock_live_channel_access(&mut tx, room, actor).await?;
        let current = sqlx::query_as::<_, (i64, i64)>(
            "SELECT version, op_seq
               FROM channel_canvases
              WHERE id = $1 AND room_id = $2
              FOR UPDATE",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::NotFound(format!("canvas {id}")))?;
        if expected_version.is_some_and(|expected| expected != current.0)
            || (blocks.is_some() && snapshot_op_seq != Some(current.1))
        {
            return Err(Error::Conflict(
                "canvas was modified concurrently; reload and retry".into(),
            ));
        }

        let sql = format!(
            r"UPDATE channel_canvases
                 SET title = COALESCE($3, title),
                     blocks = COALESCE($4, blocks),
                     snapshot_op_seq = CASE
                         WHEN $4::jsonb IS NULL THEN snapshot_op_seq
                         ELSE $5
                     END,
                     updated_at = now(),
                     version = version + 1
               WHERE id = $1 AND room_id = $2
               RETURNING {COLUMNS}"
        );
        let updated = sqlx::query_as::<_, Row>(&sql)
            .bind(id.to_uuid())
            .bind(room.to_uuid())
            .bind(title.as_deref())
            .bind(blocks)
            .bind(snapshot_op_seq)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(row_to_model(updated))
    }

    /// Delete a path-bound canvas while the actor remains an effective member of
    /// the live channel.
    ///
    /// # Errors
    /// A missing/already-deleted/cross-room id is one opaque not-found.
    pub async fn delete_canvas_authorized(
        &self,
        room: RoomId,
        id: CanvasId,
        actor: ParticipantId,
    ) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        lock_live_channel_access(&mut tx, room, actor).await?;
        let deleted = sqlx::query_scalar::<_, uuid::Uuid>(
            "DELETE FROM channel_canvases
              WHERE id = $1 AND room_id = $2
              RETURNING id",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        if deleted.is_none() {
            return Err(Error::NotFound(format!("canvas {id}")));
        }
        tx.commit().await?;
        Ok(())
    }
}

fn validate_title(raw: &str) -> Result<String, Error> {
    let title = raw.trim();
    if title.is_empty() {
        return Err(Error::Invalid("title must not be empty".into()));
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(Error::Invalid("title too long".into()));
    }
    Ok(title.to_owned())
}

fn validate_blocks(blocks: &serde_json::Value) -> Result<(), Error> {
    if !blocks.is_array() {
        return Err(Error::Invalid("blocks must be a JSON array".into()));
    }
    if serde_json::to_vec(blocks)?.len() > MAX_BLOCKS_BYTES {
        return Err(Error::Invalid("blocks too large".into()));
    }
    Ok(())
}

/// Lock and revalidate the canonical effective-access boundary for one live
/// channel. The helper resolves without a lock, lets the database function take
/// the governance-wide workspace → room → membership order, then rechecks the
/// channel kind/archive state under that room lock.
pub(crate) async fn lock_live_channel_access(
    tx: &mut Transaction<'_, Postgres>,
    room: RoomId,
    actor: ParticipantId,
) -> Result<(), Error> {
    let resolved = sqlx::query_as::<_, (uuid::Uuid, String, bool)>(
        "SELECT workspace_id, kind, is_archived
           FROM rooms
          WHERE id = $1",
    )
    .bind(room.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .filter(|(_, kind, archived)| kind == "channel" && !archived)
    .ok_or_else(|| Error::NotFound(format!("live channel {room}")))?;

    let allowed: bool = sqlx::query_scalar("SELECT aero_effective_room_access($1, $2, $3)")
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .bind(resolved.0)
        .fetch_one(&mut **tx)
        .await?;
    if !allowed {
        let current = sqlx::query_as::<_, (uuid::Uuid, String, bool)>(
            "SELECT workspace_id, kind, is_archived FROM rooms WHERE id = $1",
        )
        .bind(room.to_uuid())
        .fetch_optional(&mut **tx)
        .await?;
        return if current.as_ref().is_some_and(|(workspace, kind, archived)| {
            *workspace == resolved.0 && kind == "channel" && !archived
        }) {
            Err(Error::Forbidden("live channel membership required".into()))
        } else {
            Err(Error::NotFound(format!("live channel {room}")))
        };
    }

    let locked = sqlx::query_as::<_, (uuid::Uuid, String, bool)>(
        "SELECT workspace_id, kind, is_archived
           FROM rooms
          WHERE id = $1
          FOR SHARE",
    )
    .bind(room.to_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    if !locked.as_ref().is_some_and(|(workspace, kind, archived)| {
        *workspace == resolved.0 && kind == "channel" && !archived
    }) {
        return Err(Error::NotFound(format!("live channel {room}")));
    }
    Ok(())
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored canvas
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{RoomKind, WorkspaceRole};

    struct Fixture {
        owner: ParticipantId,
        member: ParticipantId,
        outsider: ParticipantId,
        room: RoomId,
        other_room: RoomId,
        group: RoomId,
    }

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(6)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn participant(p: &PgPool, label: &str) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("canvas-{label}-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    async fn fixture(p: &PgPool) -> Fixture {
        let owner = participant(p, "owner").await;
        let member = participant(p, "member").await;
        let outsider = participant(p, "outsider").await;
        let workspaces = crate::WorkspaceRepo::new(p.clone());
        let workspace = workspaces
            .create(format!("Canvas {owner}"), format!("canvas-{owner}"), owner)
            .await
            .unwrap()
            .id;
        for participant in [member, outsider] {
            workspaces
                .add_member(workspace, participant, WorkspaceRole::Member)
                .await
                .unwrap();
        }
        let rooms = crate::RoomRepo::new(p.clone());
        let room = rooms
            .create_in_workspace(
                workspace,
                RoomKind::Channel,
                Some("canvas-primary".into()),
                owner,
            )
            .await
            .unwrap()
            .id;
        rooms.add_member(room, member).await.unwrap();
        let other_room = rooms
            .create_in_workspace(
                workspace,
                RoomKind::Channel,
                Some("canvas-other".into()),
                owner,
            )
            .await
            .unwrap()
            .id;
        let group = rooms
            .create_in_workspace(
                workspace,
                RoomKind::Group,
                Some("not-a-channel".into()),
                owner,
            )
            .await
            .unwrap()
            .id;
        Fixture {
            owner,
            member,
            outsider,
            room,
            other_room,
            group,
        }
    }

    fn constraint(error: &sqlx::Error) -> Option<&str> {
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn authorized_canvas_crud_is_live_channel_scoped_and_conflict_safe() {
        let p = pool();
        let repo = CanvasRepo::new(p.clone());
        let f = fixture(&p).await;
        let blocks = serde_json::json!([{ "type": "heading", "text": "Plan" }]);

        let got = repo
            .create_canvas_authorized(f.room, f.member, "  Q3 Plan  ", &blocks)
            .await
            .unwrap();
        let id = got.id;
        assert_eq!(got.room_id, f.room);
        assert_eq!(got.author_id, f.member);
        assert_eq!(got.title, "Q3 Plan");
        assert_eq!(got.blocks, blocks);
        assert_eq!(
            repo.list_canvases_authorized(f.room, f.member)
                .await
                .unwrap()[0]
                .id,
            id
        );
        assert!(matches!(
            repo.get_canvas_authorized(f.other_room, id, f.owner).await,
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            repo.update_canvas_authorized(
                f.other_room,
                id,
                f.owner,
                CanvasPatch {
                    title: Some("wrong room"),
                    blocks: None,
                    expected_version: None,
                    snapshot_op_seq: None,
                },
            )
            .await,
            Err(Error::NotFound(_))
        ));

        let after = repo
            .update_canvas_authorized(
                f.room,
                id,
                f.member,
                CanvasPatch {
                    title: Some("Q3 Plan (final)"),
                    blocks: None,
                    expected_version: Some(0),
                    snapshot_op_seq: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(after.title, "Q3 Plan (final)");
        assert_eq!(after.version, 1);
        assert!(matches!(
            repo.update_canvas_authorized(
                f.room,
                id,
                f.member,
                CanvasPatch {
                    title: Some("stale"),
                    blocks: None,
                    expected_version: Some(0),
                    snapshot_op_seq: None,
                },
            )
            .await,
            Err(Error::Conflict(_))
        ));
        assert!(matches!(
            repo.create_canvas_authorized(f.group, f.owner, "no", &serde_json::json!([]))
                .await,
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            repo.create_canvas_authorized(f.room, f.outsider, "no", &serde_json::json!([]))
                .await,
            Err(Error::Forbidden(_))
        ));
        assert!(matches!(
            repo.delete_canvas_authorized(f.other_room, id, f.owner)
                .await,
            Err(Error::NotFound(_))
        ));
        repo.delete_canvas_authorized(f.room, id, f.member)
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn raw_canvas_guards_freeze_identity_preserve_departed_history_and_cascade() {
        let p = pool();
        let repo = CanvasRepo::new(p.clone());
        let f = fixture(&p).await;

        let raw_nonmember = CanvasId::new();
        let error = sqlx::query(
            "INSERT INTO channel_canvases (id, room_id, author_id, title, blocks)
             VALUES ($1, $2, $3, 'forged', '[]'::jsonb)",
        )
        .bind(raw_nonmember.to_uuid())
        .bind(f.room.to_uuid())
        .bind(f.outsider.to_uuid())
        .execute(&p)
        .await
        .unwrap_err();
        assert_eq!(
            constraint(&error),
            Some("channel_canvases_author_scope_chk")
        );

        let error = sqlx::query(
            "INSERT INTO channel_canvases (id, room_id, author_id, title, blocks)
             VALUES ($1, $2, $3, 'wrong kind', '[]'::jsonb)",
        )
        .bind(CanvasId::new().to_uuid())
        .bind(f.group.to_uuid())
        .bind(f.owner.to_uuid())
        .execute(&p)
        .await
        .unwrap_err();
        assert_eq!(constraint(&error), Some("channel_canvases_room_scope_chk"));

        let canvas = repo
            .create_canvas_authorized(f.room, f.member, "History", &serde_json::json!([]))
            .await
            .unwrap();
        let error = sqlx::query("UPDATE channel_canvases SET room_id = $2 WHERE id = $1")
            .bind(canvas.id.to_uuid())
            .bind(f.other_room.to_uuid())
            .execute(&p)
            .await
            .unwrap_err();
        assert_eq!(
            constraint(&error),
            Some("channel_canvases_identity_immutable_chk")
        );

        sqlx::query("DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
            .bind(f.room.to_uuid())
            .bind(f.member.to_uuid())
            .execute(&p)
            .await
            .unwrap();
        sqlx::query("UPDATE channel_canvases SET title = 'retained' WHERE id = $1")
            .bind(canvas.id.to_uuid())
            .execute(&p)
            .await
            .unwrap();
        assert!(matches!(
            repo.get_canvas_authorized(f.room, canvas.id, f.member)
                .await,
            Err(Error::Forbidden(_))
        ));
        assert_eq!(
            repo.get_canvas_authorized(f.room, canvas.id, f.owner)
                .await
                .unwrap()
                .title,
            "retained"
        );

        let cascade = repo
            .create_canvas_authorized(f.other_room, f.owner, "Cascade", &serde_json::json!([]))
            .await
            .unwrap();
        sqlx::query("DELETE FROM rooms WHERE id = $1")
            .bind(f.other_room.to_uuid())
            .execute(&p)
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM channel_canvases WHERE id = $1")
                .bind(cascade.id.to_uuid())
                .fetch_one(&p)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn concurrent_room_revocation_causes_zero_canvas_write_or_delete() {
        let p = pool();
        let repo = CanvasRepo::new(p.clone());
        let f = fixture(&p).await;
        let canvas = repo
            .create_canvas_authorized(f.room, f.member, "Before", &serde_json::json!([]))
            .await
            .unwrap();

        let mut revoke = p.begin().await.unwrap();
        sqlx::query("DELETE FROM room_members WHERE room_id = $1 AND participant_id = $2")
            .bind(f.room.to_uuid())
            .bind(f.member.to_uuid())
            .execute(&mut *revoke)
            .await
            .unwrap();

        let update_repo = repo.clone();
        let update = tokio::spawn(async move {
            update_repo
                .update_canvas_authorized(
                    f.room,
                    canvas.id,
                    f.member,
                    CanvasPatch {
                        title: Some("After"),
                        blocks: None,
                        expected_version: None,
                        snapshot_op_seq: None,
                    },
                )
                .await
        });
        let delete_repo = repo.clone();
        let delete = tokio::spawn(async move {
            delete_repo
                .delete_canvas_authorized(f.room, canvas.id, f.member)
                .await
        });
        tokio::task::yield_now().await;
        revoke.commit().await.unwrap();

        assert!(matches!(update.await.unwrap(), Err(Error::Forbidden(_))));
        assert!(matches!(delete.await.unwrap(), Err(Error::Forbidden(_))));
        let canonical = repo
            .get_canvas_authorized(f.room, canvas.id, f.owner)
            .await
            .unwrap();
        assert_eq!(canonical.title, "Before");
    }
}
