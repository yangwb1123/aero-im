//! Per-(participant, room) persistent **delivery** cursor (ROADMAP 第六版 ·
//! 方向三·A · 每用户持久投递台账).
//!
//! One row per (participant, room) holding the Last-Known-Good delivery point:
//! the newest message the client has durably `ACKed` receiving, its durable
//! per-room `last_delivery_ordinal`, plus `last_seq` (a diagnostic bus
//! de-duplication high-water mark). Reconnect is keyed only by the ordinal,
//! never by `last_seq` or `MAX(message_id)`: concurrent ULIDs are identifiers
//! rather than a contiguous delivery log.
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
    pub last_delivery_ordinal: i64,
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

    /// Advance the (participant, room) delivery cursor.
    ///
    /// The candidate is accepted only when `(message_id, delivery_ordinal)` names
    /// a real message in `room`, the ordinal is positive, and `seq` is
    /// non-negative. `seq = 0` is the explicit database-backfill sentinel.
    /// The ordinal is the sole cumulative replay floor; the message id follows
    /// the winning ordinal instead of being independently maximized. A higher
    /// `seq` attached to an older ordinal may update diagnostics/`updated_at`,
    /// but cannot change the message id or ordinal used for content replay.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn advance(
        &self,
        participant: ParticipantId,
        room: RoomId,
        message_id: MessageId,
        delivery_ordinal: i64,
        seq: i64,
    ) -> Result<bool, sqlx::Error> {
        let res = sqlx::query(
            r"INSERT INTO delivery_cursors AS current
                (participant_id, room_id, last_delivered_message_id,
                 last_delivery_ordinal, last_seq, updated_at)
              SELECT $1, $2, message.id, message.delivery_ordinal, $5, now()
                FROM messages AS message
               WHERE message.id = $3
                 AND message.room_id = $2
                 AND message.delivery_ordinal = $4
                 AND $4 > 0
                 AND $5 >= 0
              ON CONFLICT (participant_id, room_id) DO UPDATE
                SET last_delivered_message_id =
                        CASE
                          WHEN EXCLUDED.last_delivery_ordinal >
                               current.last_delivery_ordinal
                          THEN EXCLUDED.last_delivered_message_id
                          ELSE current.last_delivered_message_id
                        END,
                    last_delivery_ordinal =
                        GREATEST(current.last_delivery_ordinal,
                                 EXCLUDED.last_delivery_ordinal),
                    last_seq = GREATEST(current.last_seq, EXCLUDED.last_seq),
                    updated_at = EXCLUDED.updated_at
                WHERE EXCLUDED.last_delivery_ordinal >
                          current.last_delivery_ordinal
                   OR EXCLUDED.last_seq > current.last_seq",
        )
        .bind(participant.to_uuid())
        .bind(room.to_uuid())
        .bind(message_id.to_uuid())
        .bind(delivery_ordinal)
        .bind(seq)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// The (participant, room) cursor, or `None` when the client has never `ACKed`
    /// in that room.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn get(
        &self,
        participant: ParticipantId,
        room: RoomId,
    ) -> Result<Option<DeliveryCursor>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid, i64, i64, time::OffsetDateTime)>(
            r"SELECT last_delivered_message_id, last_delivery_ordinal,
                     last_seq, updated_at
              FROM delivery_cursors
              WHERE participant_id = $1 AND room_id = $2",
        )
        .bind(participant.to_uuid())
        .bind(room.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(mid, ordinal, seq, at)| DeliveryCursor {
            room_id: room,
            participant_id: participant,
            last_delivered_message_id: MessageId::from_uuid(mid),
            last_delivery_ordinal: ordinal,
            last_seq: seq,
            updated_at: at,
        }))
    }

    /// Every room cursor for `participant` — the reconnect seed set (one row per
    /// room the client has `ACKed` in). Rooms with no cursor are simply absent.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn cursors_for(
        &self,
        participant: ParticipantId,
    ) -> Result<Vec<DeliveryCursor>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (uuid::Uuid, uuid::Uuid, i64, i64, time::OffsetDateTime)>(
            r"SELECT room_id, last_delivered_message_id,
                         last_delivery_ordinal, last_seq, updated_at
                  FROM delivery_cursors
                  WHERE participant_id = $1",
        )
        .bind(participant.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(rid, mid, ordinal, seq, at)| DeliveryCursor {
                room_id: RoomId::from_uuid(rid),
                participant_id: participant,
                last_delivered_message_id: MessageId::from_uuid(mid),
                last_delivery_ordinal: ordinal,
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
    use crate::message::MessageRepo;

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
             VALUES ($1,'group',$2,$3, now(), '00000000-0000-0000-0000-000000000000')",
        )
        .bind(room.to_uuid())
        .bind("dc-room")
        .bind(actor.to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        (room, actor)
    }

    fn ordered_message_id(actor: ParticipantId, suffix: u128) -> MessageId {
        let prefix = actor.to_uuid().as_u128() & !0xffff;
        MessageId::from_uuid(uuid::Uuid::from_u128(prefix | suffix))
    }

    async fn insert_message(p: &PgPool, room: RoomId, sender: ParticipantId, id: MessageId) {
        sqlx::query(
            "INSERT INTO messages (id, room_id, sender_id, blocks)
             VALUES ($1, $2, $3, '[]'::jsonb)",
        )
        .bind(id.to_uuid())
        .bind(room.to_uuid())
        .bind(sender.to_uuid())
        .execute(p)
        .await
        .expect("insert delivery-cursor message");
    }

    async fn ordinal(p: &PgPool, id: MessageId) -> i64 {
        sqlx::query_scalar("SELECT delivery_ordinal FROM messages WHERE id = $1")
            .bind(id.to_uuid())
            .fetch_one(p)
            .await
            .expect("message delivery ordinal")
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn advance_validates_room_and_is_monotonic_on_both_floors() {
        let p = pool();
        let repo = DeliveryCursorRepo::new(p.clone());
        let (room, actor) = fixture(&p).await;
        let m3 = ordered_message_id(actor, 3);
        let m5 = ordered_message_id(actor, 5);
        let m7 = ordered_message_id(actor, 7);
        let m10 = ordered_message_id(actor, 10);
        for id in [m3, m5, m7, m10] {
            insert_message(&p, room, actor, id).await;
        }
        let o3 = ordinal(&p, m3).await;
        let o5 = ordinal(&p, m5).await;
        let o7 = ordinal(&p, m7).await;
        let o10 = ordinal(&p, m10).await;

        // First ACK inserts (advanced).
        assert!(
            repo.advance(actor, room, m5, o5, 5).await.unwrap(),
            "first ACK advances"
        );
        // Same seq redelivered → idempotent no-op.
        assert!(
            !repo.advance(actor, room, m5, o5, 5).await.unwrap(),
            "dup seq ignored"
        );
        // Both dimensions lower (racing slow device) → ignored.
        assert!(
            !repo.advance(actor, room, m3, o3, 3).await.unwrap(),
            "fully stale ACK ignored"
        );
        let cur = repo.get(actor, room).await.unwrap().expect("cursor");
        assert_eq!(cur.last_seq, 5);
        assert_eq!(cur.last_delivered_message_id, m5);
        assert_eq!(cur.last_delivery_ordinal, o5);

        // A newer ordinal with an older bus seq advances the replay prefix.
        assert!(
            repo.advance(actor, room, m10, o10, 3).await.unwrap(),
            "newer ordinal advances"
        );
        let cur = repo.get(actor, room).await.unwrap().expect("cursor");
        assert_eq!(cur.last_seq, 5, "seq floor never regressed");
        assert_eq!(cur.last_delivered_message_id, m10);
        assert_eq!(cur.last_delivery_ordinal, o10);

        // A higher bus seq attached to an older ordinal advances only diagnostics.
        assert!(
            repo.advance(actor, room, m7, o7, 10).await.unwrap(),
            "newer seq floor advances"
        );
        let cur = repo.get(actor, room).await.unwrap().expect("cursor");
        assert_eq!(cur.last_seq, 10);
        assert_eq!(
            cur.last_delivered_message_id, m10,
            "message floor never regressed"
        );
        assert_eq!(cur.last_delivery_ordinal, o10);

        let (other_room, _) = fixture(&p).await;
        assert!(
            !repo.advance(actor, other_room, m10, o10, 11).await.unwrap(),
            "a message from another room cannot seed this cursor"
        );
        assert!(
            !repo
                .advance(actor, room, MessageId::new(), o10 + 1, 11)
                .await
                .unwrap(),
            "an unknown message cannot seed this cursor"
        );
        let replayed = ordered_message_id(actor, 31);
        insert_message(&p, other_room, actor, replayed).await;
        let replayed_ordinal = ordinal(&p, replayed).await;
        assert!(
            repo.advance(actor, other_room, replayed, replayed_ordinal, 0)
                .await
                .unwrap(),
            "database backfill may advance only the message floor"
        );
        let replay_cursor = repo.get(actor, other_room).await.unwrap().unwrap();
        assert_eq!(replay_cursor.last_delivered_message_id, replayed);
        assert_eq!(replay_cursor.last_delivery_ordinal, replayed_ordinal);
        assert_eq!(replay_cursor.last_seq, 0);
        assert!(
            !repo.advance(actor, room, m10, o10, -1).await.unwrap(),
            "negative bus seq is invalid"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn cursors_for_is_per_room_and_participant_scoped() {
        let p = pool();
        let repo = DeliveryCursorRepo::new(p.clone());
        let (room_a, actor) = fixture(&p).await;
        let (room_b, other) = fixture(&p).await;
        let message_in_a = ordered_message_id(actor, 21);
        let message_in_b = ordered_message_id(actor, 22);
        let other_message_in_b = ordered_message_id(other, 23);
        insert_message(&p, room_a, actor, message_in_a).await;
        insert_message(&p, room_b, actor, message_in_b).await;
        insert_message(&p, room_b, other, other_message_in_b).await;
        let ordinal_in_a = ordinal(&p, message_in_a).await;
        let ordinal_in_b = ordinal(&p, message_in_b).await;
        let other_ordinal_in_b = ordinal(&p, other_message_in_b).await;
        // `actor` has cursors in BOTH rooms; `other` has one in room_b only.
        repo.advance(actor, room_a, message_in_a, ordinal_in_a, 7)
            .await
            .unwrap();
        repo.advance(actor, room_b, message_in_b, ordinal_in_b, 4)
            .await
            .unwrap();
        repo.advance(other, room_b, other_message_in_b, other_ordinal_in_b, 9)
            .await
            .unwrap();

        let mut mine = repo.cursors_for(actor).await.unwrap();
        mine.sort_by_key(|c| c.last_seq);
        assert_eq!(
            mine.len(),
            2,
            "actor sees only their own two rooms: {mine:?}"
        );
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
        assert!(
            repo.get(actor, room_a).await.unwrap().is_some(),
            "room_a untouched"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn delivery_replay_hides_soft_deleted_blocks_and_can_ack_across_the_gap() {
        let p = pool();
        let (room, actor) = fixture(&p).await;
        let first = ordered_message_id(actor, 41);
        let deleted = ordered_message_id(actor, 42);
        let third = ordered_message_id(actor, 43);
        for id in [first, deleted, third] {
            insert_message(&p, room, actor, id).await;
        }
        sqlx::query(
            "UPDATE messages
                SET blocks = '[{\"type\":\"text\",\"content\":\"deleted secret\"}]'::jsonb,
                    deleted_at = now()
              WHERE id = $1",
        )
        .bind(deleted.to_uuid())
        .execute(&p)
        .await
        .expect("soft delete middle message");

        let replay = MessageRepo::new(p.clone())
            .list_delivery_after(room, 0, 50)
            .await
            .expect("ordinal replay");
        assert_eq!(
            replay
                .iter()
                .map(|(message, _)| message.id)
                .collect::<Vec<_>>(),
            vec![first, third],
            "soft-deleted content must not be serialized into reconnect frames"
        );
        let third_ordinal = ordinal(&p, third).await;
        assert_eq!(
            replay
                .iter()
                .map(|(_, ordinal)| *ordinal)
                .collect::<Vec<_>>(),
            vec![ordinal(&p, first).await, third_ordinal],
            "a deleted ordinal is an intentional gap"
        );

        let repo = DeliveryCursorRepo::new(p);
        assert!(
            repo.advance(actor, room, third, third_ordinal, 0)
                .await
                .expect("ack visible message after deleted gap"),
            "the later visible message certifies the prefix across a deleted row"
        );
        assert_eq!(
            repo.get(actor, room)
                .await
                .expect("load cursor")
                .expect("cursor")
                .last_delivery_ordinal,
            third_ordinal
        );
    }
}
