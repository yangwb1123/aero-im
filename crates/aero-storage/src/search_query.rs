//! Advanced cross-room search — Slack-style query operators.
//!
//! Parses a raw search string into a [`ParsedQuery`] of free-text `terms` plus
//! optional structured filters (`from:@<id>`, `in:<roomid>`, `before:<msgid>`,
//! `after:<msgid>`), then runs it through [`AdvancedSearchRepo::search`] — the
//! membership-scoped global search ([`MessageRepo::search_all_rooms_in_workspace`](crate::MessageRepo),
//! whose `JOIN room_members` is the security boundary) with the structured
//! filters AND-ed on top.
//!
//! [`parse_search_query`] is pure (no I/O), so the operator-extraction rules are
//! unit-tested offline. Purely additive: a NEW [`AdvancedSearchRepo`]; no
//! existing repo is touched, and the [`SearchHit`](crate::SearchHit) rows are
//! reused verbatim from [`crate::MessageRepo`].

use std::str::FromStr;

use aero_common::{Block, Message, MessageId, ParticipantId, RoomId, WorkspaceId};
use sqlx::PgPool;

use crate::SearchHit;

/// A search string split into free-text `terms` and the structured operators a
/// caller may have included. Every operator is optional; an absent one is `None`
/// and is simply not AND-ed into the query.
///
/// Built by [`parse_search_query`]. Fields are public so a handler can echo the
/// recognized operators back to the client.
#[derive(Debug, Clone, Default)]
pub struct ParsedQuery {
    /// The free-text remainder (every token that was not a recognized operator),
    /// space-joined in original order. Fed to the full-text/trigram match.
    pub terms: String,
    /// `from:@<id>` — restrict to messages sent by this participant.
    pub from: Option<ParticipantId>,
    /// `in:<roomid>` — restrict to a single room (still membership-scoped).
    pub in_room: Option<RoomId>,
    /// `before:<msgid>` — only messages older than this id (`m.id < before`).
    pub before: Option<MessageId>,
    /// `after:<msgid>` — only messages newer than this id (`m.id > after`).
    pub after: Option<MessageId>,
}

/// Parse a raw search string into a [`ParsedQuery`].
///
/// Splits on whitespace; a token prefixed `from:`, `in:`, `before:`, or `after:`
/// sets the matching field by parsing the value (a leading `@` on `from:` is
/// tolerated, Slack-style). When the value fails to parse, the operator is
/// ignored and the whole token is kept as a free-text term. Every other token
/// joins back into [`ParsedQuery::terms`] in its original order.
#[must_use]
pub fn parse_search_query(raw: &str) -> ParsedQuery {
    let mut parsed = ParsedQuery::default();
    let mut terms: Vec<&str> = Vec::new();

    for token in raw.split_whitespace() {
        if let Some(v) = token.strip_prefix("from:") {
            // Slack writes `from:@<id>`; tolerate the leading sigil.
            if let Ok(id) = ParticipantId::from_str(v.trim_start_matches('@')) {
                parsed.from = Some(id);
                continue;
            }
        } else if let Some(v) = token.strip_prefix("in:") {
            if let Ok(id) = RoomId::from_str(v) {
                parsed.in_room = Some(id);
                continue;
            }
        } else if let Some(v) = token.strip_prefix("before:") {
            if let Ok(id) = MessageId::from_str(v) {
                parsed.before = Some(id);
                continue;
            }
        } else if let Some(v) = token.strip_prefix("after:") {
            if let Ok(id) = MessageId::from_str(v) {
                parsed.after = Some(id);
                continue;
            }
        }
        // Not an operator, or its value failed to parse: keep as a free term.
        terms.push(token);
    }

    parsed.terms = terms.join(" ");
    parsed
}

/// The row shape decoded from the advanced-search query: the same message
/// columns as the cross-room search plus a relevance `score`. Private to this
/// module (it mirrors `MessageRepo`'s internal row but cannot reuse it), mapped
/// into the shared [`SearchHit`] on the way out.
#[derive(sqlx::FromRow)]
struct ScoredRow {
    id: uuid::Uuid,
    room_id: uuid::Uuid,
    sender_id: uuid::Uuid,
    blocks: serde_json::Value,
    reply_to: Option<uuid::Uuid>,
    metadata: serde_json::Value,
    created_at: time::OffsetDateTime,
    edited_at: Option<time::OffsetDateTime>,
    deleted_at: Option<time::OffsetDateTime>,
    expires_at: Option<time::OffsetDateTime>,
    score: f32,
}

impl From<ScoredRow> for SearchHit {
    fn from(r: ScoredRow) -> Self {
        let blocks: Vec<Block> = serde_json::from_value(r.blocks).unwrap_or_default();
        SearchHit {
            message: Message {
                id: MessageId::from_uuid(r.id),
                room_id: RoomId::from_uuid(r.room_id),
                sender_id: ParticipantId::from_uuid(r.sender_id),
                blocks,
                reply_to: r.reply_to.map(MessageId::from_uuid),
                metadata: r.metadata,
                created_at: r.created_at,
                edited_at: r.edited_at,
                deleted_at: r.deleted_at,
                expires_at: r.expires_at,
            },
            score: r.score,
        }
    }
}

/// Repository for the advanced cross-room search.
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`AdvancedSearchRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct AdvancedSearchRepo {
    pool: PgPool,
}

impl AdvancedSearchRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Run a [`ParsedQuery`] across every room `participant` belongs to within
    /// `workspace`, AND-ing in each present structured operator.
    ///
    /// Same membership boundary as
    /// [`MessageRepo::search_all_rooms_in_workspace`](crate::MessageRepo): the
    /// `JOIN room_members` is the security guard, so a hit can never leak a room
    /// the caller isn't in — there is no post-filter. The full-text/trigram match
    /// runs on [`ParsedQuery::terms`]; when `terms` is empty (the query was only
    /// operators) the text predicate is skipped and results are filtered purely by
    /// the operators. `limit` is clamped to `[1, 100]`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn search(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        q: &ParsedQuery,
        limit: i64,
    ) -> Result<Vec<SearchHit>, sqlx::Error> {
        let limit = limit.clamp(1, 100);
        let rows = sqlx::query_as::<_, ScoredRow>(
            r"SELECT
                 m.id, m.room_id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                 m.created_at, m.edited_at, m.deleted_at, m.expires_at,
                 GREATEST(
                   ts_rank(m.search_tsv, websearch_to_tsquery('simple', $2)),
                   similarity(m.searchable_text, $2)
                 ) AS score
               FROM messages m
               JOIN room_members rm
                 ON rm.room_id = m.room_id AND rm.participant_id = $1
               WHERE m.deleted_at IS NULL
                 AND m.room_id IN (SELECT id FROM rooms WHERE workspace_id = $4)
                 AND ($5::uuid IS NULL OR m.sender_id = $5)
                 AND ($6::uuid IS NULL OR m.room_id = $6)
                 AND ($7::uuid IS NULL OR m.id < $7)
                 AND ($8::uuid IS NULL OR m.id > $8)
                 AND (
                   $2 = ''
                   OR m.search_tsv @@ websearch_to_tsquery('simple', $2)
                   OR m.searchable_text % $2
                 )
               ORDER BY score DESC, m.id DESC
               LIMIT $3",
        )
        .bind(participant.to_uuid())
        .bind(&q.terms)
        .bind(limit)
        .bind(workspace.to_uuid())
        .bind(q.from.map(|id| id.to_uuid()))
        .bind(q.in_room.map(|id| id.to_uuid()))
        .bind(q.before.map(|id| id.to_uuid()))
        .bind(q.after.map(|id| id.to_uuid()))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(SearchHit::from).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_search_query, MessageId, ParticipantId, RoomId};

    #[test]
    fn extracts_from_operator_with_at_sigil() {
        let who = ParticipantId::new();
        let pq = parse_search_query(&format!("hello from:@{who} world"));
        assert_eq!(pq.from, Some(who));
        assert_eq!(pq.terms, "hello world");
        assert!(pq.in_room.is_none() && pq.before.is_none() && pq.after.is_none());
    }

    #[test]
    fn extracts_from_operator_without_sigil() {
        let who = ParticipantId::new();
        let pq = parse_search_query(&format!("from:{who}"));
        assert_eq!(pq.from, Some(who));
        assert_eq!(pq.terms, "", "an operator-only query leaves no free terms");
    }

    #[test]
    fn extracts_in_room_operator() {
        let room = RoomId::new();
        let pq = parse_search_query(&format!("deploy in:{room} failed"));
        assert_eq!(pq.in_room, Some(room));
        assert_eq!(pq.terms, "deploy failed");
    }

    #[test]
    fn extracts_before_and_after_operators() {
        let lo = MessageId::new();
        let hi = MessageId::new();
        let pq = parse_search_query(&format!("after:{lo} crash before:{hi}"));
        assert_eq!(pq.after, Some(lo));
        assert_eq!(pq.before, Some(hi));
        assert_eq!(pq.terms, "crash");
    }

    #[test]
    fn free_text_passes_through_when_no_operators() {
        let pq = parse_search_query("just some words here");
        assert_eq!(pq.terms, "just some words here");
        assert!(
            pq.from.is_none()
                && pq.in_room.is_none()
                && pq.before.is_none()
                && pq.after.is_none()
        );
    }

    #[test]
    fn bad_operator_value_is_kept_as_a_free_term() {
        let pq = parse_search_query("from:not-a-ulid in:@@@ alpha");
        assert!(pq.from.is_none(), "an unparseable from: value is not set");
        assert!(pq.in_room.is_none(), "an unparseable in: value is not set");
        // The whole malformed tokens fall through into the free text.
        assert_eq!(pq.terms, "from:not-a-ulid in:@@@ alpha");
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored search_query
/// ```
#[cfg(test)]
mod db_tests {
    use super::{parse_search_query, AdvancedSearchRepo};
    use crate::{MessageRepo, NewMessage};
    use aero_common::{Block, ParticipantId, RoomId, WorkspaceId};
    use sqlx::PgPool;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    // A throwaway participant so message/membership FKs are satisfiable.
    async fn participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("adv-search-actor-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    // A throwaway room in the all-zero default workspace (guaranteed to exist by
    // migration 0006's backfill).
    async fn room(p: &PgPool, creator: ParticipantId) -> RoomId {
        let id = RoomId::new();
        sqlx::query(
            "INSERT INTO rooms (id, kind, name, created_by, workspace_id) VALUES ($1,'group',$2,$3,$4)",
        )
        .bind(id.to_uuid())
        .bind(format!("adv-search-room-{id}"))
        .bind(creator.to_uuid())
        .bind(WorkspaceId(ulid::Ulid(0)).to_uuid())
        .execute(p)
        .await
        .expect("insert room");
        id
    }

    async fn join(p: &PgPool, room: RoomId, who: ParticipantId) {
        sqlx::query(
            "INSERT INTO room_members (room_id, participant_id, role) VALUES ($1,$2,'member')",
        )
        .bind(room.to_uuid())
        .bind(who.to_uuid())
        .execute(p)
        .await
        .expect("insert membership");
    }

    fn default_ws() -> WorkspaceId {
        WorkspaceId(ulid::Ulid(0))
    }

    /// `from:` narrows to one sender; `in:` excludes hits from other rooms; bare
    /// free text matches across all the caller's rooms — all within the SQL
    /// membership boundary.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn advanced_search_filters_by_from_in_and_free_text() {
        let p = pool();
        let repo = AdvancedSearchRepo::new(p.clone());
        let msgs = MessageRepo::new(p.clone());
        let ws = default_ws();

        let me = participant(&p).await;
        let other = participant(&p).await;

        // Two rooms the caller belongs to.
        let mine = room(&p, me).await;
        let elsewhere = room(&p, me).await;
        join(&p, mine, me).await;
        join(&p, elsewhere, me).await;

        // A distinctive token shared by every seeded message.
        let needle = format!("zqxbladetoken{}", ParticipantId::new());
        let m_me = msgs
            .insert(NewMessage {
                room_id: mine,
                sender_id: me,
                blocks: vec![Block::text(format!("alpha {needle} from me"))],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("insert m_me");
        let m_other = msgs
            .insert(NewMessage {
                room_id: mine,
                sender_id: other,
                blocks: vec![Block::text(format!("beta {needle} from other"))],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("insert m_other");
        let m_elsewhere = msgs
            .insert(NewMessage {
                room_id: elsewhere,
                sender_id: me,
                blocks: vec![Block::text(format!("gamma {needle} elsewhere"))],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("insert m_elsewhere");

        // Free text alone: matches across both of the caller's rooms.
        let q = parse_search_query(&needle);
        let hits = repo.search(me, ws, &q, 50).await.expect("free-text search");
        assert!(hits.iter().any(|h| h.message.id == m_me.id));
        assert!(hits.iter().any(|h| h.message.id == m_other.id));
        assert!(hits.iter().any(|h| h.message.id == m_elsewhere.id));

        // from:<other> restricts to that sender only.
        let q = parse_search_query(&format!("{needle} from:{other}"));
        let hits = repo.search(me, ws, &q, 50).await.expect("from: search");
        assert!(
            hits.iter().all(|h| h.message.sender_id == other),
            "every hit is from the requested sender"
        );
        assert!(hits.iter().any(|h| h.message.id == m_other.id));
        assert!(!hits.iter().any(|h| h.message.id == m_me.id));

        // in:<mine> excludes the message that lives in the other room.
        let q = parse_search_query(&format!("{needle} in:{mine}"));
        let hits = repo.search(me, ws, &q, 50).await.expect("in: search");
        assert!(
            hits.iter().all(|h| h.message.room_id == mine),
            "every hit is from the requested room"
        );
        assert!(!hits.iter().any(|h| h.message.id == m_elsewhere.id));

        // Cleanup so reruns stay self-contained (children before parents).
        for id in [m_me.id, m_other.id, m_elsewhere.id] {
            sqlx::query("DELETE FROM messages WHERE id = $1")
                .bind(id.to_uuid())
                .execute(&p)
                .await
                .ok();
        }
        for r in [mine, elsewhere] {
            sqlx::query("DELETE FROM room_members WHERE room_id = $1")
                .bind(r.to_uuid())
                .execute(&p)
                .await
                .ok();
            sqlx::query("DELETE FROM rooms WHERE id = $1")
                .bind(r.to_uuid())
                .execute(&p)
                .await
                .ok();
        }
        for who in [me, other] {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(who.to_uuid())
                .execute(&p)
                .await
                .ok();
        }
    }
}
