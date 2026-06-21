//! Per-(participant, room) persistent **delivery** cursor (ROADMAP 第六版 ·
//! 方向三·A · 每用户持久投递台账).
//!
//! One row per (participant, room) holding the Last-Known-Good delivery point:
//! the highest-seq message the client has durably ACKed receiving, plus that
//! `last_seq` (the per-subject bus seq the client dedupes on). Advanced by an
//! explicit `delivery_ack` from the client, **monotonically** — only a strictly
//! higher seq moves the cursor, so at-least-once redelivery and racing
//! multi-device ACKs converge to `max(seq)` without rolling backward.
//!
//! Distinct from [`ReceiptRepo`](crate::ReceiptRepo) (the *seen* cursor, advanced
//! on visual mark-read, powering unread badges): a message can be delivered yet
//! unread, which only this table can express. The reconnect backfill reads each
//! room's cursor here to resume from "what this client already HAS" per room,
//! instead of replaying one global `?since=` id across every room. Backed by
//! `migrations/0153_delivery_cursors.sql`.

use aero_common::{MessageId, ParticipantId, RoomId};
use sqlx::PgPool;

/// A participant's delivery LKG in one room.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DeliveryCursor {
    pub room_id: RoomId,
    pub participant_id: ParticipantId,
    pub last_delivered_message_id: MessageId,
    pub last_seq: i64,
    pub updated_at: time::OffsetDateTime,
}

/// Delivery-cursor repo over the shared pool.
#[derive(Clone)]
pub struct DeliveryCursorRepo {
    pool: PgPool,
}

impl DeliveryCursorRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Advance the (participant, room) delivery cursor to `(message_id, seq)`.
    ///
    /// **Monotonic on `seq`**: the upsert only writes when the new seq strictly
    /// exceeds the stored one (a fresh insert always writes). Returns `true` when
    /// the cursor actually moved, `false` when a stale/duplicate ACK was ignored —
    /// so an at-least-once redelivery (same seq) and a racing second device (lower
    /// seq) are both harmless no-ops, and concurrent ACKs converge to `max(seq)`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn advance(
        &self,
        participant: ParticipantId,
        room: RoomId,
        message_id: MessageId,
        seq: i64,
    ) -> Result<bool, sqlx::Error> {
        let res = sqlx::query(
            r"INSERT INTO delivery_cursors
                (participant_id, room_id, last_delivered_message_id, last_seq, updated_at)
              VALUES ($1, $2, $3, $4, now())
              ON CONFLICT (participant_id, room_id) DO UPDATE
                SET last_delivered_message_id = EXCLUDED.last_delivered_message_id,
                    last_seq = EXCLUDED.last_seq,
                    updated_at = EXCLUDED.updated_at
                WHERE EXCLUDED.last_seq > delivery_cursors.last_seq",
        )
        .bind(participant.to_uuid())
        .bind(room.to_uuid())
        .bind(message_id.to_uuid())
        .bind(seq)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// The (participant, room) cursor, or `None` when the client has never ACKed
    /// in that room.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn get(
        &self,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<Option<DeliveryCursor>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid, i64, time::OffsetDateTime)>(
            r"SELECT last_delivered_message_id, last_seq, updated_at
              FROM delivery_cursors
              WHERE participant_id = $1 AND room_id = $2",
        )
        .bind(participant.to_uuid())
        .bind(room.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(mid, seq, at)| DeliveryCursor {
            room_id: room,
            participant_id: participant,
            last_delivered_message_id: MessageId::from_uuid(mid),
            last_seq: seq,
            updated_at: at,
        }))
    }

    /// Every room cursor for `participant` — the reconnect seed set (one row per
    /// room the client has ACKed in). Rooms with no cursor are simply absent.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn cursors_for(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<DeliveryCursor>, sqlx::Error> {
        let rows =
            sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, i64, time::OffsetDateTime)>(
                r"SELECT room_id, last_delivered_message_id, last_seq, updated_at
                  FROM delivery_cursors
                  WHERE participant_id = $1",
            )
            .bind(participant.to_uuid())
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|(rid, mid, seq, at)| DeliveryCursor {
                room_id: RoomId::from_uuid(rid),
                participant_id: participant,
                last_delivered_message_id: MessageId::from_uuid(mid),
                last_seq: seq,
                updated_at: at,
            })
            .collect())
    }

    /// Drop every cursor for `room` (legal-hold / retention edge case): clearing
    /// the LKG forces the next reconnect to re-evaluate the room from scratch, so a
    /// preserved-then-restored or swept message is never silently skipped past.
    /// Returns rows removed.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn clear_room(&self, room: RoomId) -> Result<u64, sqlx::Error> {
        let res = sqlx::query("DELETE FROM delivery_cursors WHERE room_id = $1")
            .bind(room.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }
}

/// PG-gated integration tests (live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored delivery_cursor
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

    /// A throwaway participant + room so each test is self-contained.
    async fn fixture(p: &PgPool) -> (RoomId, ParticipantId) {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(actor.to_uuid())
            .bind(format!("dc-actor-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");
        let room = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id)
             VALUES ($1,'channel',$2,$3, now(), '00000000-0000-0000-0000-000000000000')",
        )
        .bind(room.to_uuid())
        .bind("dc-room")
        .bind(actor.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        (room, actor)
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn advance_is_monotonic_on_seq_and_idempotent() {
        let p = pool();
        let repo = DeliveryCursorRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;
        let m5 = MessageId::new();
        let m10 = MessageId::new();

        // First ACK inserts (advanced).
        assert!(repo.advance(actor, room, m5, 5).await.unwrap(), "first ACK advances");
        // Same seq redelivered → idempotent no-op.
        assert!(!repo.advance(actor, room, m5, 5).await.unwrap(), "dup seq ignored");
        // Lower seq (racing slow device) → ignored; cursor stays at 5.
        assert!(!repo.advance(actor, room, MessageId::new(), 3).await.unwrap(), "stale seq ignored");
        let cur = repo.get(actor, room).await.unwrap().expect("cursor");
        assert_eq!(cur.last_seq, 5);
        assert_eq!(cur.last_delivered_message_id, m5, "stale ACK did not overwrite the message id");
        // Strictly higher seq advances (and rewrites the message id).
        assert!(repo.advance(actor, room, m10, 10).await.unwrap(), "higher seq advances");
        let cur = repo.get(actor, room).await.unwrap().expect("cursor");
        assert_eq!(cur.last_seq, 10);
        assert_eq!(cur.last_delivered_message_id, m10);
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn cursors_for_is_per_room_and_participant_scoped() {
        let p = pool();
        let repo = DeliveryCursorRepo::new(p.clone());
        let (room_a, actor) = fixture(&p).await;
        let (room_b, other) = fixture(&p).await;
        // `actor` has cursors in BOTH rooms; `other` has one in room_b only.
        repo.advance(actor, room_a, MessageId::new(), 7).await.unwrap();
        repo.advance(actor, room_b, MessageId::new(), 4).await.unwrap();
        repo.advance(other, room_b, MessageId::new(), 9).await.unwrap();

        let mut mine = repo.cursors_for(actor).await.unwrap();
        mine.sort_by_key(|c| c.last_seq);
        assert_eq!(mine.len(), 2, "actor sees only their own two rooms: {mine:?}");
        assert_eq!(mine[0].last_seq, 4);
        assert_eq!(mine[1].last_seq, 7);
        assert!(mine.iter().all(|c| c.participant_id == actor));
        // `other`'s room_b cursor is independent and not leaked into actor's set.
        let theirs = repo.cursors_for(other).await.unwrap();
        assert_eq!(theirs.len(), 1);
        assert_eq!(theirs[0].last_seq, 9);
        assert_eq!(theirs[0].room_id, room_b);

        // Clearing room_b drops BOTH participants' cursors there; room_a survives.
        let removed = repo.clear_room(room_b).await.unwrap();
        assert_eq!(removed, 2, "both room_b cursors cleared");
        assert!(repo.get(actor, room_b).await.unwrap().is_none());
        assert!(repo.get(other, room_b).await.unwrap().is_none());
        assert!(repo.get(actor, room_a).await.unwrap().is_some(), "room_a untouched");
    }
}
