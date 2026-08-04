//! Channel-canvas append-only op log (ROADMAP 第六版 · 方向五·③).
//!
//! Backs `migrations/0155_canvas_ops.sql`. A durable, totally-ordered-per-canvas
//! stream of immutable incremental edit ops (insert / delete / format / …). The
//! repo is content-agnostic — it stores each op's JSON verbatim and never
//! interprets it; clients reduce the stream locally (OT/CRDT). Append and delta
//! reads own the canonical effective live-channel authorization transaction.
//!
//! Ordering is the contract: append bumps the canvas's `op_seq` under a row
//! lock, so every op gets a distinct, gap-free `seq` even under concurrent
//! appends. The same transaction dedupes `(canvas, author, client_op_id)` and
//! writes the durable room-event outbox, so an uncertain retry returns the
//! canonical op/outbox without allocating or publishing twice.

use aero_common::{CanvasId, Error, MessageId, ParticipantId, RoomEvent, RoomId};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::event_outbox::{EventOutboxKind, EventOutboxRepo};

const MAX_OP_BYTES: usize = 64 * 1024;
const MAX_OPS_LIMIT: i64 = 1_000;
const MAX_TRACEPARENT_BYTES: usize = 512;

/// One immutable op in a canvas's edit log.
#[derive(Debug, Clone, Serialize)]
pub struct CanvasOp {
    pub id: Uuid,
    pub canvas_id: CanvasId,
    /// Caller-generated retry key, scoped by canvas and author.
    pub client_op_id: Uuid,
    /// Per-canvas monotonic, gap-free position.
    pub seq: i64,
    pub author_id: ParticipantId,
    /// The client-defined incremental edit, stored verbatim.
    pub op: serde_json::Value,
    pub created_at: time::OffsetDateTime,
}

/// Result of an idempotent append.
#[derive(Debug, Clone)]
pub struct CanvasOpAppend {
    /// The canonical durable op, whether newly inserted or previously committed.
    pub op: CanvasOp,
    /// `true` only for the transaction that allocated and inserted this op.
    ///
    /// Retry responses return the existing op and durable event row without
    /// allocating another sequence or aggregate version.
    pub inserted: bool,
    /// Durable queue row committed atomically with the op. Retrying the same
    /// `client_op_id` returns this same row id.
    pub outbox_id: Uuid,
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

    /// Append `op` to a path-bound canvas while `author` remains an effective
    /// member of the live channel.
    ///
    /// The canvas row is locked before checking `(canvas, author,
    /// client_op_id)`. A retry returns the existing row with `inserted = false`;
    /// a new key bumps `op_seq` and inserts in that same transaction. Concurrent
    /// appends therefore serialize into distinct, gap-free sequences, while
    /// concurrent retries allocate exactly once. Reusing a client id with a
    /// different JSON operation is a conflict. The op and its `canvas_op`
    /// room-event outbox row commit atomically.
    ///
    /// # Errors
    /// Returns [`Error::NotFound`] for a missing/cross-room canvas or non-live
    /// channel, [`Error::Forbidden`] when access was revoked,
    /// [`Error::Conflict`] for a mismatched retry, and [`Error::Invalid`] for an
    /// invalid/oversized operation.
    pub async fn append_canvas_op_authorized(
        &self,
        room: RoomId,
        canvas: CanvasId,
        author: ParticipantId,
        client_op_id: Uuid,
        op: &serde_json::Value,
        traceparent: Option<&str>,
    ) -> Result<CanvasOpAppend, Error> {
        validate_op(op)?;
        if traceparent.is_some_and(|value| value.len() > MAX_TRACEPARENT_BYTES) {
            return Err(Error::Invalid("traceparent too long".into()));
        }
        let mut tx = self.pool.begin().await?;
        crate::canvas::lock_live_channel_access(&mut tx, room, author).await?;
        let current_seq = sqlx::query_scalar::<_, i64>(
            r"SELECT op_seq
                FROM channel_canvases
               WHERE id = $1 AND room_id = $2
               FOR UPDATE",
        )
        .bind(canvas.to_uuid())
        .bind(room.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| Error::NotFound(format!("canvas {canvas}")))?;

        let existing =
            sqlx::query_as::<_, (Uuid, i64, Uuid, serde_json::Value, time::OffsetDateTime)>(
                r"SELECT id, seq, client_op_id, op, created_at
                FROM canvas_ops
               WHERE canvas_id = $1
                 AND author_id = $2
                 AND client_op_id = $3",
            )
            .bind(canvas.to_uuid())
            .bind(author.to_uuid())
            .bind(client_op_id)
            .fetch_optional(&mut *tx)
            .await?;
        if let Some((id, seq, stored_client_op_id, stored_op, created_at)) = existing {
            if stored_op != *op {
                return Err(Error::Conflict(
                    "client_op_id was already used for a different canvas operation".into(),
                ));
            }
            let canonical = CanvasOp {
                id,
                canvas_id: canvas,
                client_op_id: stored_client_op_id,
                seq,
                author_id: author,
                op: stored_op,
                created_at,
            };
            let outbox_id = insert_canvas_op_outbox(&mut tx, room, &canonical, traceparent).await?;
            tx.commit().await?;
            return Ok(CanvasOpAppend {
                op: canonical,
                inserted: false,
                outbox_id,
            });
        }

        let seq: i64 = sqlx::query_scalar(
            r"UPDATE channel_canvases
                 SET op_seq = op_seq + 1, updated_at = now()
               WHERE id = $1 AND room_id = $2
               RETURNING op_seq",
        )
        .bind(canvas.to_uuid())
        .bind(room.to_uuid())
        .fetch_one(&mut *tx)
        .await?;
        debug_assert_eq!(seq, current_seq + 1);
        let id = Uuid::new_v4();
        let (created_at, stored_op) =
            sqlx::query_as::<_, (time::OffsetDateTime, serde_json::Value)>(
                r"INSERT INTO canvas_ops
                 (id, canvas_id, seq, author_id, client_op_id, op)
               VALUES ($1, $2, $3, $4, $5, $6)
               RETURNING created_at, op",
            )
            .bind(id)
            .bind(canvas.to_uuid())
            .bind(seq)
            .bind(author.to_uuid())
            .bind(client_op_id)
            .bind(op)
            .fetch_one(&mut *tx)
            .await?;
        let canonical = CanvasOp {
            id,
            canvas_id: canvas,
            client_op_id,
            seq,
            author_id: author,
            op: stored_op,
            created_at,
        };
        let outbox_id = insert_canvas_op_outbox(&mut tx, room, &canonical, traceparent).await?;
        tx.commit().await?;

        Ok(CanvasOpAppend {
            op: canonical,
            inserted: true,
            outbox_id,
        })
    }

    /// Ops for `canvas` strictly after `after_seq`, oldest first, capped at
    /// `limit` (the delta a reconnecting/late client needs to catch up). Pass
    /// `after_seq = 0` for the whole log.
    ///
    /// # Errors
    /// The canvas id is bound to `room` before any op is returned. `after_seq`
    /// must be non-negative and `limit` is constrained to 1..=1000 here as well
    /// as at the HTTP edge.
    pub async fn list_canvas_ops_authorized(
        &self,
        room: RoomId,
        canvas: CanvasId,
        actor: ParticipantId,
        after_seq: i64,
        limit: i64,
    ) -> Result<Vec<CanvasOp>, Error> {
        if after_seq < 0 {
            return Err(Error::Invalid("since must be non-negative".into()));
        }
        if !(1..=MAX_OPS_LIMIT).contains(&limit) {
            return Err(Error::Invalid("limit must be between 1 and 1000".into()));
        }
        let mut tx = self.pool.begin().await?;
        crate::canvas::lock_live_channel_access(&mut tx, room, actor).await?;
        let canvas_exists = sqlx::query_scalar::<_, bool>(
            "SELECT true
               FROM channel_canvases
              WHERE id = $1 AND room_id = $2
              FOR SHARE",
        )
        .bind(canvas.to_uuid())
        .bind(room.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !canvas_exists {
            return Err(Error::NotFound(format!("canvas {canvas}")));
        }
        let rows = sqlx::query_as::<
            _,
            (
                Uuid,
                Uuid,
                i64,
                Uuid,
                serde_json::Value,
                time::OffsetDateTime,
            ),
        >(
            r"SELECT id, client_op_id, seq, author_id, op, created_at
              FROM canvas_ops
              WHERE canvas_id = $1 AND seq > $2
              ORDER BY seq
              LIMIT $3",
        )
        .bind(canvas.to_uuid())
        .bind(after_seq)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows
            .into_iter()
            .map(|(id, client_op_id, seq, author, op, created_at)| CanvasOp {
                id,
                canvas_id: canvas,
                client_op_id,
                seq,
                author_id: ParticipantId::from_uuid(author),
                op,
                created_at,
            })
            .collect())
    }
}

fn validate_op(op: &serde_json::Value) -> Result<(), Error> {
    if !op.is_object() {
        return Err(Error::Invalid("op must be a JSON object".into()));
    }
    if serde_json::to_vec(op)?.len() > MAX_OP_BYTES {
        return Err(Error::Invalid("op too large".into()));
    }
    Ok(())
}

async fn insert_canvas_op_outbox(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    room: RoomId,
    op: &CanvasOp,
    traceparent: Option<&str>,
) -> Result<Uuid, Error> {
    // Appends and retries hold the canvas row lock, so a committed canonical
    // outbox row cannot race this lookup. Returning it before attempting the
    // generic `MAX(aggregate_version) + 1` insert is important: PostgreSQL
    // checks the proposed row before resolving `ON CONFLICT`, while canvas-op
    // events deliberately have one stable aggregate version.
    if let Some(id) = sqlx::query_scalar::<_, Uuid>(
        "SELECT id
           FROM event_outbox
          WHERE event_id = $1
            AND message_id = $1
            AND event_kind = 'canvas_op'",
    )
    .bind(op.id)
    .fetch_optional(&mut **tx)
    .await?
    {
        return Ok(id);
    }

    let event = RoomEvent::CanvasOp {
        room_id: room,
        canvas_id: op.canvas_id,
        op_id: op.id,
        op_seq: op.seq,
        author_id: op.author_id,
        op: op.op.clone(),
    };
    Ok(EventOutboxRepo::insert_room_event_in_tx(
        tx,
        MessageId::from_uuid(op.id),
        room,
        EventOutboxKind::CanvasOp,
        &event,
        traceparent.map(str::to_owned),
        Some(op.id),
    )
    .await?)
}

/// PG-gated integration tests (live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored canvas_op
/// ```
#[cfg(test)]
#[path = "canvas_op/security_tests.rs"]
mod security_tests;

#[cfg(any())] // Replaced by transaction-fence PG tests in this change.
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
    async fn fixture(p: &PgPool) -> (RoomId, CanvasId, ParticipantId) {
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
             VALUES ($1,'group',$2,$3, now(), '00000000-0000-0000-0000-000000000000')",
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
        (room, canvas, actor)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn append_assigns_gapfree_seq_and_ops_since_returns_ordered_delta() {
        let p = pool();
        let repo = CanvasOpRepo::new(p.clone());
        let (room, canvas, a1) = fixture(&p).await;
        let a2 = ParticipantId::new();
        sqlx::query(
            "INSERT INTO participants (id, kind, display_name) VALUES ($1,'human','co-a2')",
        )
        .bind(a2.to_uuid())
        .execute(&p)
        .await
        .unwrap();

        // Three ops from two authors → seq 1,2,3 (gap-free, per canvas).
        let o1 = repo
            .append(
                room,
                canvas,
                a1,
                Uuid::new_v4(),
                &serde_json::json!({"t":"insert","at":0,"s":"a"}),
            )
            .await
            .unwrap()
            .unwrap()
            .op;
        let o2 = repo
            .append(
                room,
                canvas,
                a2,
                Uuid::new_v4(),
                &serde_json::json!({"t":"insert","at":1,"s":"b"}),
            )
            .await
            .unwrap()
            .unwrap()
            .op;
        let o3 = repo
            .append(
                room,
                canvas,
                a1,
                Uuid::new_v4(),
                &serde_json::json!({"t":"delete","at":0}),
            )
            .await
            .unwrap()
            .unwrap()
            .op;
        assert_eq!(
            (o1.seq, o2.seq, o3.seq),
            (1, 2, 3),
            "gap-free per-canvas seq"
        );

        // Whole log (after 0), in order.
        let all = repo.ops_since(canvas, 0, 100).await.unwrap();
        assert_eq!(all.iter().map(|o| o.seq).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(all[1].author_id, a2, "author round-trips");
        assert_eq!(all[0].op["s"], "a", "op payload round-trips verbatim");

        // Delta after seq 1 → only ops 2,3 (the reconnect catch-up).
        let delta = repo.ops_since(canvas, 1, 100).await.unwrap();
        assert_eq!(delta.iter().map(|o| o.seq).collect::<Vec<_>>(), vec![2, 3]);

        // A different canvas's log is isolated.
        let (other_room, other, _) = fixture(&p).await;
        repo.append(
            other_room,
            other,
            a1,
            Uuid::new_v4(),
            &serde_json::json!({"t":"insert","at":0,"s":"x"}),
        )
        .await
        .unwrap();
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
        let room = RoomId::new();
        let author = ParticipantId::new();
        let r = repo
            .append(
                room,
                ghost,
                author,
                Uuid::new_v4(),
                &serde_json::json!({"t":"noop"}),
            )
            .await
            .unwrap();
        assert!(
            r.is_none(),
            "appending to a non-existent canvas inserts nothing"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn legacy_append_without_client_id_remains_rolling_upgrade_compatible() {
        let p = pool();
        let (room, canvas, author) = fixture(&p).await;
        let id = Uuid::new_v4();

        let (stored_id, client_op_id): (Uuid, Uuid) = sqlx::query_as(
            r"WITH bumped AS (
                  UPDATE channel_canvases
                     SET op_seq = op_seq + 1, updated_at = now()
                   WHERE id = $1
                  RETURNING op_seq
              )
              INSERT INTO canvas_ops (id, canvas_id, seq, author_id, op)
              SELECT $2, $1, bumped.op_seq, $3, $4 FROM bumped
              RETURNING id, client_op_id",
        )
        .bind(canvas.to_uuid())
        .bind(id)
        .bind(author.to_uuid())
        .bind(serde_json::json!({"type":"legacy","room":room.to_string()}))
        .fetch_one(&p)
        .await
        .expect("0175 trigger fills a previous binary's omitted client_op_id");

        assert_eq!(stored_id, id);
        assert_eq!(
            client_op_id, id,
            "the immutable server op id is the legacy retry identity"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn append_is_scoped_to_the_authorized_room() {
        let p = pool();
        let repo = CanvasOpRepo::new(p.clone());
        let (room, canvas, author) = fixture(&p).await;

        let rejected = repo
            .append(
                RoomId::new(),
                canvas,
                author,
                Uuid::new_v4(),
                &serde_json::json!({"t":"insert","s":"wrong room"}),
            )
            .await
            .unwrap();
        assert!(
            rejected.is_none(),
            "a canvas id cannot be written through a different room scope"
        );
        assert!(
            repo.ops_since(canvas, 0, 10).await.unwrap().is_empty(),
            "rejected ownership scope leaves the durable log unchanged"
        );

        let accepted = repo
            .append(
                room,
                canvas,
                author,
                Uuid::new_v4(),
                &serde_json::json!({"t":"insert","s":"ok"}),
            )
            .await
            .unwrap()
            .expect("the owning room may append");
        assert_eq!(accepted.op.seq, 1);
        assert!(accepted.inserted);
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn concurrent_appends_get_distinct_gapfree_seqs() {
        // The canvas row lock must serialize concurrent appends — no two ops share
        // a seq, and the seqs are exactly 1..=N with no gap.
        let p = pool();
        let repo = CanvasOpRepo::new(p.clone());
        let (room, canvas, author) = fixture(&p).await;

        let mut handles = Vec::new();
        for i in 0..12 {
            let repo = repo.clone();
            handles.push(tokio::spawn(async move {
                repo.append(
                    room,
                    canvas,
                    author,
                    Uuid::new_v4(),
                    &serde_json::json!({"i": i}),
                )
                .await
                .unwrap()
                .unwrap()
                .op
                .seq
            }));
        }
        let mut seqs = Vec::new();
        for h in handles {
            seqs.push(h.await.unwrap());
        }
        seqs.sort_unstable();
        assert_eq!(
            seqs,
            (1..=12).collect::<Vec<_>>(),
            "12 concurrent appends → seqs 1..=12, no dup/gap"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn client_op_id_is_idempotent_within_canvas_and_participant_scope() {
        let p = pool();
        let repo = CanvasOpRepo::new(p.clone());
        let (room, canvas, author) = fixture(&p).await;
        let client_op_id = Uuid::new_v4();
        let payload = serde_json::json!({"type":"set_text","text":"once"});

        let first = repo
            .append(room, canvas, author, client_op_id, &payload)
            .await
            .unwrap()
            .unwrap();
        let retry = repo
            .append(room, canvas, author, client_op_id, &payload)
            .await
            .unwrap()
            .unwrap();

        assert!(first.inserted);
        assert!(first.request_matches);
        assert!(!retry.inserted);
        assert!(retry.request_matches);
        assert_eq!(retry.op.id, first.op.id);
        assert_eq!(retry.op.seq, first.op.seq);
        assert_eq!(retry.op.client_op_id, client_op_id);
        assert_eq!(
            repo.ops_since(canvas, 0, 10).await.unwrap().len(),
            1,
            "a retry neither allocates another sequence nor inserts another row"
        );
        let op_seq: i64 = sqlx::query_scalar("SELECT op_seq FROM channel_canvases WHERE id = $1")
            .bind(canvas.to_uuid())
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(op_seq, 1);

        let mismatched_retry = repo
            .append(
                room,
                canvas,
                author,
                client_op_id,
                &serde_json::json!({"type":"set_text","text":"different"}),
            )
            .await
            .unwrap()
            .unwrap();
        assert!(!mismatched_retry.inserted);
        assert!(!mismatched_retry.request_matches);
        assert_eq!(mismatched_retry.op.id, first.op.id);

        let concurrent_id = Uuid::new_v4();
        let left_repo = repo.clone();
        let right_repo = repo.clone();
        let left_payload = serde_json::json!({"type":"add_note","note_id":"race","text":"once"});
        let right_payload = left_payload.clone();
        let (left, right) = tokio::join!(
            left_repo.append(room, canvas, author, concurrent_id, &left_payload),
            right_repo.append(room, canvas, author, concurrent_id, &right_payload),
        );
        let left = left.unwrap().unwrap();
        let right = right.unwrap().unwrap();
        assert_ne!(
            left.inserted, right.inserted,
            "exactly one concurrent claimant inserts"
        );
        assert_eq!(left.op.id, right.op.id);
        assert_eq!(left.op.seq, right.op.seq);
        assert_eq!(left.op.seq, 2);
        assert!(left.request_matches && right.request_matches);

        let other_author = ParticipantId::new();
        sqlx::query(
            "INSERT INTO participants (id, kind, display_name) VALUES ($1,'human','co-idem-a2')",
        )
        .bind(other_author.to_uuid())
        .execute(&p)
        .await
        .unwrap();
        let other_author_op = repo
            .append(room, canvas, other_author, client_op_id, &payload)
            .await
            .unwrap()
            .unwrap();
        assert!(other_author_op.inserted);
        assert_eq!(other_author_op.op.seq, 3);

        let (other_room, other_canvas, _) = fixture(&p).await;
        let other_canvas_op = repo
            .append(other_room, other_canvas, author, client_op_id, &payload)
            .await
            .unwrap()
            .unwrap();
        assert!(other_canvas_op.inserted);
        assert_eq!(other_canvas_op.op.seq, 1);
    }
}
