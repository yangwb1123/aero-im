//! Thread operations: replies, summary, participants.
//!
//! Extracted from `message.rs` as part of REFACTOR_PLAN.md Step 2.

use aero_common::{Message, MessageId, ParticipantId};

use super::MessageRepo;
use crate::message::orig::MessageRow;

impl MessageRepo {
    /// Fetch replies to a thread `root`, OLDEST-first (chronological), excluding
    /// tombstones, with keyset pagination: `after` continues past a previous page's
    /// last reply id (`None` = from the start), `limit` bounds the page (clamped
    /// 1..=200). Oldest-first is load-bearing: the `/thread` endpoint pages forward,
    /// and the AI thread summary/title feed the transcript to the model in
    /// chronological order (`heuristic_summary` takes the last N as "most recent").
    ///
    /// REGRESSION GUARD: the `message.rs`→`message/` split dropped the `after` +
    /// `limit` params and flipped to `ORDER BY id DESC LIMIT 500`. That silently
    /// broke `GET /api/messages/:id/thread` pagination (the route computed
    /// `after`/`limit` but passed neither) and REVERSED the AI summary input, so the
    /// heuristic summarized the thread's OLDEST replies instead of its latest.
    pub async fn thread_replies(
        &self,
        root: MessageId,
        after: Option<MessageId>,
        limit: i64,
    ) -> Result<Vec<Message>, sqlx::Error> {
        let limit = limit.clamp(1, 200);
        let rows = sqlx::query_as::<_, MessageRow>(
            r"SELECT reply.id, reply.room_id, reply.sender_id, reply.blocks,
                      reply.reply_to, reply.metadata, reply.created_at,
                      reply.edited_at, reply.deleted_at, reply.expires_at,
                      reply.version
               FROM messages AS reply
               JOIN messages AS root
                 ON root.id = $1 AND root.room_id = reply.room_id
               WHERE reply.reply_to = $1
                 AND reply.deleted_at IS NULL
                 AND ($2::uuid IS NULL OR reply.id > $2)
               ORDER BY reply.id ASC
               LIMIT $3",
        )
        .bind(root.to_uuid())
        .bind(after.map(|m| m.to_uuid()))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Message::from).collect())
    }

    /// Count the number of replies to a thread root. Uses a fast index-only scan
    /// on `reply_to` (descending-indexed for the thread-replies query above).
    pub async fn thread_summary(
        &self,
        root: MessageId,
    ) -> Result<aero_common::ThreadSummary, sqlx::Error> {
        // Single round-trip (previously three: count + newest reply + distinct
        // repliers, each re-scanning the same reply set). A `MATERIALIZED` CTE
        // scans the replies ONCE; the scalar subqueries then read from it.
        // Postgres has no `max(uuid)` aggregate, so the newest reply is the
        // ordered single-row read (ids are time-sortable ULIDs stored as UUID, so
        // `ORDER BY id DESC LIMIT 1` is the most recent). `repliers` is capped at 8
        // distinct senders for avatar rendering; `array_agg` over the empty set
        // yields `NULL` → decoded as `None` → an empty replier list.
        #[allow(clippy::type_complexity)]
        let (count, last_id, last_at, repliers): (
            i64,
            Option<uuid::Uuid>,
            Option<time::OffsetDateTime>,
            Option<Vec<uuid::Uuid>>,
        ) = sqlx::query_as(
            r"WITH replies AS MATERIALIZED (
                  SELECT reply.id, reply.sender_id, reply.created_at
                    FROM messages AS reply
                    JOIN messages AS root
                      ON root.id = $1 AND root.room_id = reply.room_id
                   WHERE reply.reply_to = $1
                     AND reply.deleted_at IS NULL
              )
              SELECT
                  (SELECT COUNT(*) FROM replies),
                  (SELECT id FROM replies ORDER BY id DESC LIMIT 1),
                  (SELECT created_at FROM replies ORDER BY id DESC LIMIT 1),
                  (SELECT array_agg(sender_id)
                     FROM (SELECT DISTINCT sender_id FROM replies LIMIT 8) d)",
        )
        .bind(root.to_uuid())
        .fetch_one(&self.pool)
        .await?;

        Ok(aero_common::ThreadSummary {
            root_id: root,
            reply_count: u32::try_from(count).unwrap_or(u32::MAX),
            repliers: repliers
                .unwrap_or_default()
                .into_iter()
                .map(ParticipantId::from_uuid)
                .collect(),
            last_reply_id: last_id.map(MessageId::from_uuid),
            last_reply_at: last_at,
        })
    }

    /// Distinct participants who have posted at least one (non-deleted) reply in a
    /// thread. The thread is identified by `root` — every message whose `reply_to =
    /// root` and `deleted_at IS NULL`. Returns participant ids in arbitrary order;
    /// the caller join-fetches display names for the HTTP response. Empty when the
    /// thread has no replies (yet). Used by
    /// `GET /api/messages/:id/thread-participants`.
    pub async fn thread_participants(
        &self,
        root: MessageId,
    ) -> Result<Vec<ParticipantId>, sqlx::Error> {
        // LIMIT bounds both this result and the caller's per-id display-name
        // fan-out (one `participants.get` each): an unbounded roster would let a
        // thread with very many distinct repliers turn one request into that many
        // lookups. A roster is a UI affordance, so 500 distinct participants is far
        // beyond what any view renders.
        let rows = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT DISTINCT reply.sender_id
               FROM messages AS reply
               JOIN messages AS root
                 ON root.id = $1 AND root.room_id = reply.room_id
               WHERE reply.reply_to = $1 AND reply.deleted_at IS NULL
               LIMIT 500",
        )
        .bind(root.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(p,)| ParticipantId::from_uuid(p))
            .collect())
    }
}
