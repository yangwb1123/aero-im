//! Channel-canvas append-only op log (ROADMAP 第六版 · 方向五·③).
//!
//! Backs `migrations/0155_canvas_ops.sql`. A durable, totally-ordered-per-canvas
//! stream of immutable incremental edit ops (insert / delete / format / …). The
//! repo is content-agnostic — it stores each op's JSON verbatim and never
//! interprets it; clients reduce the stream locally (OT/CRDT). Like
//! [`CanvasRepo`](crate::CanvasRepo) it owns no access control — the server gates
//! every route on `assert_room_access`.
//!
//! Ordering is the contract: [`append`](CanvasOpRepo::append) bumps the canvas's
//! `op_seq` under a row lock, so every op gets a distinct, gap-free `seq` even
//! under concurrent appends; [`ops_since`](CanvasOpRepo::ops_since) returns the
//! tail after a client's last-seen `seq`, in order.

use aero_common::{CanvasId, ParticipantId};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// One immutable op in a canvas's edit log.
#[derive(Debug, Clone, Serialize)]
pub struct CanvasOp {
    pub id: Uuid,
    pub canvas_id: CanvasId,
    /// Per-canvas monotonic, gap-free position.
    pub seq: i64,
    pub author_id: ParticipantId,
    /// The client-defined incremental edit, stored verbatim.
    pub op: serde_json::Value,
    pub created_at: time::OffsetDateTime,
}

/// Op-log repo over the shared pool.
#[derive(Clone)]
pub struct CanvasOpRepo {
    pool: PgPool,
}

impl CanvasOpRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Append `op` to `canvas`'s log, returning the persisted op (with its
    /// assigned `seq`). The canvas row is locked while its `op_seq` is bumped, so
    /// concurrent appends serialize and each gets a distinct, gap-free seq — no
    /// `UNIQUE (canvas_id, seq)` collision is possible. Returns `Ok(None)` when the
    /// canvas does not exist (the `UPDATE … RETURNING` matched no row, so the
    /// `INSERT … SELECT` inserts nothing).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn append(
        &self,
        canvas: CanvasId,
        author: ParticipantId,
        op: &serde_json::Value,
    ) -> Result<Option<CanvasOp>, sqlx::Error> {
        let id = Uuid::new_v4();
        let row = sqlx::query_as::<_, (i64, time::OffsetDateTime)>(
            r"WITH bumped AS (
                  UPDATE channel_canvases
                     SET op_seq = op_seq + 1, updated_at = now()
                   WHERE id = $1
                  RETURNING op_seq
              )
              INSERT INTO canvas_ops (id, canvas_id, seq, author_id, op)
              SELECT $2, $1, bumped.op_seq, $3, $4 FROM bumped
              RETURNING seq, created_at",
        )
        .bind(canvas.to_uuid())
        .bind(id)
        .bind(author.to_uuid())
        .bind(op)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(seq, created_at)| CanvasOp {
            id,
            canvas_id: canvas,
            seq,
            author_id: author,
            op: op.clone(),
            created_at,
        }))
    }

    /// Ops for `canvas` strictly after `after_seq`, oldest first, capped at
    /// `limit` (the delta a reconnecting/late client needs to catch up). Pass
    /// `after_seq = 0` for the whole log.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn ops_since(
        &self,
        canvas: CanvasId,
        after_seq: i64,
        limit: i64,
    ) -> Result<Vec<CanvasOp>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (Uuid, i64, Uuid, serde_json::Value, time::OffsetDateTime)>(
            r"SELECT id, seq, author_id, op, created_at
              FROM canvas_ops
              WHERE canvas_id = $1 AND seq > $2
              ORDER BY seq
              LIMIT $3",
        )
        .bind(canvas.to_uuid())
        .bind(after_seq)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, seq, author, op, created_at)| CanvasOp {
                id,
                canvas_id: canvas,
                seq,
                author_id: ParticipantId::from_uuid(author),
                op,
                created_at,
            })
            .collect())
    }
}

/// PG-gated integration tests (live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored canvas_op
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// A throwaway participant + room + canvas so the test is self-contained.
    async fn fixture(p: &PgPool) -> (CanvasId, ParticipantId) {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(actor.to_uuid())
            .bind(format!("co-actor-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");
        let room = aero_common::RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
             VALUES ($1,'channel',$2,$3, now(), '00000000-0000-0000-0000-000000000000')",
        )
        .bind(room.to_uuid())
        .bind("co-room")
        .bind(actor.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        let canvas = CanvasId::new();
        sqlx::query(
            "INSERT INTO channel_canvases (id, room_id, author_id, title, blocks)
             VALUES ($1,$2,$3,'co-canvas','[]'::jsonb)",
        )
        .bind(canvas.to_uuid())
        .bind(room.to_uuid())
        .bind(actor.to_uuid())
        .execute(p)
        .await
        .expect("insert canvas");
        (canvas, actor)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn append_assigns_gapfree_seq_and_ops_since_returns_ordered_delta() {
        let p = pool();
        let repo = CanvasOpRepo::new(p.clone());
        let (canvas, a1) = fixture(&p).await;
        let a2 = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human','co-a2')")
            .bind(a2.to_uuid())
            .execute(&p)
            .await
            .unwrap();

        // Three ops from two authors → seq 1,2,3 (gap-free, per canvas).
        let o1 = repo.append(canvas, a1, &serde_json::json!({"t":"insert","at":0,"s":"a"})).await.unwrap().unwrap();
        let o2 = repo.append(canvas, a2, &serde_json::json!({"t":"insert","at":1,"s":"b"})).await.unwrap().unwrap();
        let o3 = repo.append(canvas, a1, &serde_json::json!({"t":"delete","at":0})).await.unwrap().unwrap();
        assert_eq!((o1.seq, o2.seq, o3.seq), (1, 2, 3), "gap-free per-canvas seq");

        // Whole log (after 0), in order.
        let all = repo.ops_since(canvas, 0, 100).await.unwrap();
        assert_eq!(all.iter().map(|o| o.seq).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(all[1].author_id, a2, "author round-trips");
        assert_eq!(all[0].op["s"], "a", "op payload round-trips verbatim");

        // Delta after seq 1 → only ops 2,3 (the reconnect catch-up).
        let delta = repo.ops_since(canvas, 1, 100).await.unwrap();
        assert_eq!(delta.iter().map(|o| o.seq).collect::<Vec<_>>(), vec![2, 3]);

        // A different canvas's log is isolated.
        let (other, _) = fixture(&p).await;
        repo.append(other, a1, &serde_json::json!({"t":"insert","at":0,"s":"x"})).await.unwrap();
        let other_log = repo.ops_since(other, 0, 100).await.unwrap();
        assert_eq!(other_log.len(), 1, "other canvas sees only its own op");
        assert_eq!(other_log[0].seq, 1, "its seq starts fresh at 1");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn append_to_missing_canvas_is_none() {
        let p = pool();
        let repo = CanvasOpRepo::new(p.clone());
        let ghost = CanvasId::new();
        let author = ParticipantId::new();
        let r = repo.append(ghost, author, &serde_json::json!({"t":"noop"})).await.unwrap();
        assert!(r.is_none(), "appending to a non-existent canvas inserts nothing");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn concurrent_appends_get_distinct_gapfree_seqs() {
        // The canvas row lock must serialize concurrent appends — no two ops share
        // a seq, and the seqs are exactly 1..=N with no gap.
        let p = pool();
        let repo = CanvasOpRepo::new(p.clone());
        let (canvas, author) = fixture(&p).await;

        let mut handles = Vec::new();
        for i in 0..12 {
            let repo = repo.clone();
            handles.push(tokio::spawn(async move {
                repo.append(canvas, author, &serde_json::json!({"i": i})).await.unwrap().unwrap().seq
            }));
        }
        let mut seqs = Vec::new();
        for h in handles {
            seqs.push(h.await.unwrap());
        }
        seqs.sort_unstable();
        assert_eq!(seqs, (1..=12).collect::<Vec<_>>(), "12 concurrent appends → seqs 1..=12, no dup/gap");
    }
}
