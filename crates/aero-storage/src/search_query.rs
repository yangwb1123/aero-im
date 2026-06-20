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
use serde::Serialize;
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
    /// `since:<YYYY-MM-DD>` — only messages created on or after this date (UTC).
    pub after_ts: Option<time::OffsetDateTime>,
    /// `until:<YYYY-MM-DD>` — only messages created on or before this date (UTC).
    pub before_ts: Option<time::OffsetDateTime>,
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
        } else if let Some(v) = token.strip_prefix("since:") {
            let dt_str = if v.len() == 10 {
                format!("{}T00:00:00Z", v)
            } else {
                v.to_owned()
            };
            if let Ok(dt) =
                time::OffsetDateTime::parse(&dt_str, &time::format_description::well_known::Rfc3339)
            {
                parsed.after_ts = Some(dt);
                continue;
            }
        } else if let Some(v) = token.strip_prefix("until:") {
            let dt_str = if v.len() == 10 {
                format!("{}T23:59:59Z", v)
            } else {
                v.to_owned()
            };
            if let Ok(dt) =
                time::OffsetDateTime::parse(&dt_str, &time::format_description::well_known::Rfc3339)
            {
                parsed.before_ts = Some(dt);
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
    headline: Option<String>,
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
            headline: r.headline,
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
        Ok(self.search_page(participant, workspace, q, limit, None).await?.0)
    }

    /// Keyset-paginated variant of [`search`](Self::search): returns one page plus
    /// an opaque [`SearchCursor`] for the next page (`None` once the result set is
    /// exhausted).
    ///
    /// Pagination is keyset, not `OFFSET`: pass the previous page's returned
    /// cursor as `after` to fetch the next page in O(log n + limit) regardless of
    /// depth. The cursor is the composite `(score, id)` of the last row — because
    /// `score` (a `ts_rank`/`similarity` float) is **not** unique, a score-only
    /// cursor would drop or repeat rows at ties, so the `m.id` tiebreaker is part
    /// of the cursor and the predicate is the lexicographic tuple compare
    /// `(score, id) < (cursor.score, cursor.id)` — exactly the rows that sort
    /// after the cursor under `ORDER BY score DESC, id DESC`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn search_page(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        q: &ParsedQuery,
        limit: i64,
        after: Option<SearchCursor>,
    ) -> Result<(Vec<SearchHit>, Option<SearchCursor>), sqlx::Error> {
        let limit = limit.clamp(1, 100);
        // The keyset predicate lives in an outer query so it can reference the
        // computed `score` alias (a WHERE clause can't see a SELECT alias, and
        // repeating the GREATEST(...) expression would have to stay byte-identical
        // forever). `$11` NULL ⇒ first page (no cursor).
        let rows = sqlx::query_as::<_, ScoredRow>(
            r"SELECT id, room_id, sender_id, blocks, reply_to, metadata,
                     created_at, edited_at, deleted_at, expires_at, score, headline
               FROM (
                 SELECT
                   m.id, m.room_id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                   m.created_at, m.edited_at, m.deleted_at, m.expires_at,
                   GREATEST(
                     ts_rank(m.search_tsv, websearch_to_tsquery('english', f_unaccent($2))),
                     similarity(m.searchable_text, $2)
                   ) AS score,
                   ts_headline('english', m.searchable_text, websearch_to_tsquery('english', f_unaccent($2)),
                     'StartSel=<b>, StopSel=</b>, MaxWords=50, MinWords=15, ShortWord=3') AS headline
                 FROM messages m
                 JOIN room_members rm
                   ON rm.room_id = m.room_id AND rm.participant_id = $1
                 WHERE m.deleted_at IS NULL
                   AND m.room_id IN (SELECT id FROM rooms WHERE workspace_id = $4)
                   AND ($5::uuid IS NULL OR m.sender_id = $5)
                   AND ($6::uuid IS NULL OR m.room_id = $6)
                   AND ($7::uuid IS NULL OR m.id < $7)
                   AND ($8::uuid IS NULL OR m.id > $8)
                   AND ($9::timestamptz IS NULL OR m.created_at >= $9)
                   AND ($10::timestamptz IS NULL OR m.created_at <= $10)
                   AND (
                     $2 = ''
                     OR m.search_tsv @@ websearch_to_tsquery('english', f_unaccent($2))
                     OR m.searchable_text % $2
                   )
               ) sub
               WHERE ($11::real IS NULL OR (sub.score, sub.id) < ($11, $12::uuid))
               ORDER BY sub.score DESC, sub.id DESC
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
        .bind(q.after_ts)
        .bind(q.before_ts)
        .bind(after.map(|c| c.score))
        .bind(after.map(|c| c.id.to_uuid()))
        .fetch_all(&self.pool)
        .await?;
        // A next cursor only when the page came back full — a short page is the
        // last one. (A full final page yields a cursor whose next fetch is empty,
        // which is the standard, harmless keyset terminal condition.)
        let next = if usize::try_from(limit).is_ok_and(|l| rows.len() == l) {
            rows.last().map(|r| SearchCursor { score: r.score, id: MessageId::from_uuid(r.id) })
        } else {
            None
        };
        Ok((rows.into_iter().map(SearchHit::from).collect(), next))
    }

    /// Total number of messages matching `q` for `participant` in `workspace`,
    /// independent of any page limit. Mirrors [`search_page`](Self::search_page)'s
    /// WHERE clause exactly (same membership boundary, same filters) so the count
    /// is consistent with the paged results.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn count(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        q: &ParsedQuery,
    ) -> Result<i64, sqlx::Error> {
        let (total,): (i64,) = sqlx::query_as(
            r"SELECT COUNT(*)
               FROM messages m
               JOIN room_members rm
                 ON rm.room_id = m.room_id AND rm.participant_id = $1
               WHERE m.deleted_at IS NULL
                 AND m.room_id IN (SELECT id FROM rooms WHERE workspace_id = $2)
                 AND ($3::uuid IS NULL OR m.sender_id = $3)
                 AND ($4::uuid IS NULL OR m.room_id = $4)
                 AND ($5::uuid IS NULL OR m.id < $5)
                 AND ($6::uuid IS NULL OR m.id > $6)
                 AND ($7::timestamptz IS NULL OR m.created_at >= $7)
                 AND ($8::timestamptz IS NULL OR m.created_at <= $8)
                 AND (
                   $9 = ''
                   OR m.search_tsv @@ websearch_to_tsquery('english', f_unaccent($9))
                   OR m.searchable_text % $9
                 )",
        )
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .bind(q.from.map(|id| id.to_uuid()))
        .bind(q.in_room.map(|id| id.to_uuid()))
        .bind(q.before.map(|id| id.to_uuid()))
        .bind(q.after.map(|id| id.to_uuid()))
        .bind(q.after_ts)
        .bind(q.before_ts)
        .bind(&q.terms)
        .fetch_one(&self.pool)
        .await?;
        Ok(total)
    }

    /// Faceted breakdown of the matches for `q`: the top rooms and top senders by
    /// hit count, for the same membership-scoped result set as
    /// [`count`](Self::count)/[`search_page`](Self::search_page) (identical WHERE
    /// clause). Lets a client offer "narrow to room X / sender Y" drill-down with
    /// live counts. `top` caps each facet list (clamped to `[1, 50]`).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn facets(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        q: &ParsedQuery,
        top: i64,
    ) -> Result<SearchFacets, sqlx::Error> {
        let top = top.clamp(1, 50);
        let rooms = self.facet_by(participant, workspace, q, FacetDim::Room, top).await?;
        let senders = self.facet_by(participant, workspace, q, FacetDim::Sender, top).await?;
        Ok(SearchFacets { rooms, senders })
    }

    /// One facet dimension's grouped counts. The grouped column is chosen by
    /// `dim` (interpolated from a fixed allowlist — never user input — so there is
    /// no injection surface), while every value predicate stays parameter-bound.
    async fn facet_by(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        q: &ParsedQuery,
        dim: FacetDim,
        top: i64,
    ) -> Result<Vec<FacetCount>, sqlx::Error> {
        let col = dim.column(); // "m.room_id" | "m.sender_id" — fixed, not user input
        let sql = format!(
            r"SELECT {col}::text AS value, COUNT(*) AS n
               FROM messages m
               JOIN room_members rm
                 ON rm.room_id = m.room_id AND rm.participant_id = $1
               WHERE m.deleted_at IS NULL
                 AND m.room_id IN (SELECT id FROM rooms WHERE workspace_id = $2)
                 AND ($3::uuid IS NULL OR m.sender_id = $3)
                 AND ($4::uuid IS NULL OR m.room_id = $4)
                 AND ($5::uuid IS NULL OR m.id < $5)
                 AND ($6::uuid IS NULL OR m.id > $6)
                 AND ($7::timestamptz IS NULL OR m.created_at >= $7)
                 AND ($8::timestamptz IS NULL OR m.created_at <= $8)
                 AND (
                   $9 = ''
                   OR m.search_tsv @@ websearch_to_tsquery('english', f_unaccent($9))
                   OR m.searchable_text % $9
                 )
               GROUP BY {col}
               ORDER BY n DESC, value
               LIMIT $10"
        );
        let rows = sqlx::query_as::<_, (String, i64)>(&sql)
            .bind(participant.to_uuid())
            .bind(workspace.to_uuid())
            .bind(q.from.map(|id| id.to_uuid()))
            .bind(q.in_room.map(|id| id.to_uuid()))
            .bind(q.before.map(|id| id.to_uuid()))
            .bind(q.after.map(|id| id.to_uuid()))
            .bind(q.after_ts)
            .bind(q.before_ts)
            .bind(&q.terms)
            .bind(top)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|(value, count)| FacetCount { value, count }).collect())
    }

    /// "Did you mean…" suggestions for `term`: distinct words from the caller's
    /// recently-visible messages that are trigram-similar to `term`. Membership-
    /// scoped via the same `JOIN room_members` boundary as search, so a suggestion
    /// never reveals a word from a room the caller can't see. Intended to be
    /// called only when a query returned few/zero hits (it scans a bounded recent
    /// window, not the whole corpus).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn suggest_terms(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        term: &str,
        limit: i64,
    ) -> Result<Vec<String>, sqlx::Error> {
        let term = term.trim();
        if term.len() < 3 {
            return Ok(Vec::new()); // too short to suggest meaningfully
        }
        let limit = limit.clamp(1, 20);
        let rows = sqlx::query_as::<_, (String,)>(
            r"WITH recent AS (
                 SELECT lower(m.searchable_text) AS t
                 FROM messages m
                 JOIN room_members rm
                   ON rm.room_id = m.room_id AND rm.participant_id = $1
                 WHERE m.deleted_at IS NULL
                   AND m.room_id IN (SELECT id FROM rooms WHERE workspace_id = $2)
                   AND m.searchable_text <> ''
                   AND m.created_at > now() - interval '30 days'
                 ORDER BY m.id DESC
                 LIMIT 2000
              ),
              words AS (
                 SELECT DISTINCT w AS word
                 FROM recent, LATERAL regexp_split_to_table(recent.t, '[^a-z0-9]+') AS w
                 WHERE length(w) BETWEEN 3 AND 30
              )
              SELECT word
              FROM words
              WHERE word <> lower($3)
                AND word_similarity($3, word) > 0.4
              ORDER BY word_similarity($3, word) DESC, word
              LIMIT $4",
        )
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .bind(term)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(w,)| w).collect())
    }
}

/// A facet dimension — which column [`AdvancedSearchRepo::facets`] groups by.
/// The mapped column name comes from this fixed enum (never from request data),
/// so interpolating it into the GROUP BY carries no injection risk.
#[derive(Debug, Clone, Copy)]
enum FacetDim {
    Room,
    Sender,
}

impl FacetDim {
    fn column(self) -> &'static str {
        match self {
            FacetDim::Room => "m.room_id",
            FacetDim::Sender => "m.sender_id",
        }
    }
}

/// One bucket of a faceted breakdown: a dimension value (a room or sender id,
/// rendered as a string) and how many matches fell into it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FacetCount {
    /// The grouped value — a room id (`facets.rooms`) or sender id
    /// (`facets.senders`) as its text UUID.
    pub value: String,
    /// Number of matching messages in this bucket.
    pub count: i64,
}

/// Faceted breakdown of a search result set: the top rooms and top senders by
/// match count, for drill-down ("narrow to…") UX. `Serialize` so a handler hands
/// it straight back as JSON.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SearchFacets {
    /// Top rooms by match count (descending).
    pub rooms: Vec<FacetCount>,
    /// Top senders by match count (descending).
    pub senders: Vec<FacetCount>,
}

/// An opaque keyset cursor for [`AdvancedSearchRepo::search_page`]: the
/// `(score, id)` of the last row on a page. Round-trips through the HTTP layer as
/// an encoded string so a client can request the next page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SearchCursor {
    /// Relevance score of the last returned row (the primary sort key).
    pub score: f32,
    /// Id of the last returned row (the tiebreaker that makes the cursor unique).
    pub id: MessageId,
}

impl SearchCursor {
    /// Encode as a compact, URL-safe `"<score-bits-hex>.<message-id>"` string.
    /// The score is encoded by its raw IEEE-754 bits so it round-trips exactly
    /// (no decimal-parse drift that could shift the keyset boundary).
    #[must_use]
    pub fn encode(&self) -> String {
        format!("{:08x}.{}", self.score.to_bits(), self.id)
    }

    /// Decode a cursor produced by [`encode`](Self::encode). Returns `None` for a
    /// malformed string (treated as "no cursor" / first page by the caller).
    #[must_use]
    pub fn decode(s: &str) -> Option<Self> {
        let (bits_hex, id_str) = s.split_once('.')?;
        let bits = u32::from_str_radix(bits_hex, 16).ok()?;
        let id = MessageId::from_str(id_str).ok()?;
        Some(Self { score: f32::from_bits(bits), id })
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
        assert!(pq.in_room.is_none() && pq.before.is_none() && pq.after.is_none() && pq.after_ts.is_none() && pq.before_ts.is_none());
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
                && pq.after_ts.is_none()
                && pq.before_ts.is_none()
        );
    }

    #[test]
    fn extracts_since_and_until_date_operators() {
        let pq = parse_search_query("since:2024-01-15 crash until:2024-02-28");
        assert!(pq.after_ts.is_some(), "since: should parse a YYYY-MM-DD date");
        assert!(pq.before_ts.is_some(), "until: should parse a YYYY-MM-DD date");
        assert_eq!(pq.terms, "crash");
        // RFC 3339 datetimes also work.
        let pq2 = parse_search_query("since:2024-01-15T00:00:00Z deploy");
        assert!(pq2.after_ts.is_some(), "since: should parse an RFC 3339 datetime");
        assert_eq!(pq2.terms, "deploy");
        // Malformed date stays as free text.
        let pq3 = parse_search_query("since:notadate words");
        assert!(pq3.after_ts.is_none(), "malformed since: stays as free text");
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
    use aero_common::{Block, MessageId, ParticipantId, RoomId, WorkspaceId};
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

    /// The advanced-search path stems and folds accents in LOCK-STEP with the
    /// stored `search_tsv` (migrations 0128 + 0131): a query for the root/unaccented
    /// form matches an inflected/diacritic'd message. Guards the regression where
    /// this repo still used `websearch_to_tsquery('simple', …)` against the
    /// `english`+`f_unaccent` column — which silently dropped stem/accent hits.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn advanced_search_stems_and_unaccents() {
        let p = pool();
        let repo = AdvancedSearchRepo::new(p.clone());
        let msgs = MessageRepo::new(p.clone());
        let ws = default_ws();

        let me = participant(&p).await;
        let r = room(&p, me).await;
        join(&p, r, me).await;

        // Unique marker so concurrent rows can't satisfy the assertions for us.
        let marker = format!("zqftsmark{}", ParticipantId::new());
        let m = msgs
            .insert(NewMessage {
                room_id: r,
                sender_id: me,
                // 'deploying' (inflected) + 'café' (accented), neither appearing
                // literally in the queries below.
                blocks: vec![Block::text(format!("{marker} deploying to the café"))],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("insert m");

        // Root form 'deploy' (stem) — must match 'deploying'.
        let q = parse_search_query(&format!("{marker} deploy"));
        let hits = repo.search(me, ws, &q, 50).await.expect("stem search");
        assert!(
            hits.iter().any(|h| h.message.id == m.id),
            "advanced search stems 'deploy' → 'deploying'"
        );

        // Unaccented 'cafe' — must match 'café'.
        let q = parse_search_query(&format!("{marker} cafe"));
        let hits = repo.search(me, ws, &q, 50).await.expect("accent search");
        assert!(
            hits.iter().any(|h| h.message.id == m.id),
            "advanced search folds 'cafe' → 'café'"
        );

        // Cleanup.
        sqlx::query("DELETE FROM messages WHERE id = $1").bind(m.id.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM room_members WHERE room_id = $1").bind(r.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1").bind(r.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM participants WHERE id = $1").bind(me.to_uuid()).execute(&p).await.ok();
    }

    /// `count` returns the full match total regardless of page size, and
    /// `search_page` walks the whole result set in keyset pages with no overlaps
    /// and no gaps — including across score ties (every seeded message shares the
    /// same single needle token, so they rank near-identically, exercising the
    /// `(score, id)` tiebreaker).
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn count_and_keyset_pagination_cover_every_hit_once() {
        use super::SearchCursor;
        let p = pool();
        let repo = AdvancedSearchRepo::new(p.clone());
        let msgs = MessageRepo::new(p.clone());
        let ws = default_ws();

        let me = participant(&p).await;
        let r = room(&p, me).await;
        join(&p, r, me).await;

        // Seed 25 messages all carrying one distinctive token.
        let needle = format!("zpagetoken{}", ParticipantId::new());
        let mut seeded = Vec::new();
        for i in 0..25 {
            let m = msgs
                .insert(NewMessage {
                    room_id: r,
                    sender_id: me,
                    blocks: vec![Block::text(format!("{needle} item {i}"))],
                    reply_to: None,
                    metadata: serde_json::json!({}),
                    expires_at: None,
                })
                .await
                .expect("insert");
            seeded.push(m.id);
        }

        let q = parse_search_query(&needle);

        // count() is page-independent.
        let total = repo.count(me, ws, &q).await.expect("count");
        assert_eq!(total, 25, "count reflects all seeded matches");

        // Walk every page of size 10 via the returned cursor, collecting ids.
        let mut seen: Vec<MessageId> = Vec::new();
        let mut cursor: Option<SearchCursor> = None;
        let mut pages = 0;
        loop {
            let (hits, next) = repo.search_page(me, ws, &q, 10, cursor).await.expect("page");
            pages += 1;
            assert!(pages <= 10, "pagination must terminate");
            for h in &hits {
                seen.push(h.message.id);
            }
            match next {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }

        // Every seeded id appears exactly once across all pages (no gaps/overlaps).
        assert_eq!(seen.len(), 25, "every hit returned exactly once across pages");
        let mut sorted = seen.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 25, "no duplicate ids across pages (keyset tiebreak holds)");
        for id in &seeded {
            assert!(seen.contains(id), "seeded id {id} appears in some page");
        }

        // A round-tripped cursor decodes back to the same boundary.
        let (_first, next) = repo.search_page(me, ws, &q, 10, None).await.expect("first page");
        let c = next.expect("first page has a next cursor");
        assert_eq!(SearchCursor::decode(&c.encode()), Some(c), "cursor encode/decode round-trips");

        // Cleanup.
        for id in &seeded {
            sqlx::query("DELETE FROM messages WHERE id = $1").bind(id.to_uuid()).execute(&p).await.ok();
        }
        sqlx::query("DELETE FROM room_members WHERE room_id = $1").bind(r.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM rooms WHERE id = $1").bind(r.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM participants WHERE id = $1").bind(me.to_uuid()).execute(&p).await.ok();
    }

    /// `suggest_terms` returns trigram-near words from the caller's own recent
    /// messages — so a typo'd query can surface a "did you mean" hint — and is
    /// membership-scoped (never a word from a room the caller can't see).
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn suggest_terms_offers_trigram_near_words_membership_scoped() {
        let p = pool();
        let repo = AdvancedSearchRepo::new(p.clone());
        let msgs = MessageRepo::new(p.clone());
        let ws = default_ws();

        let me = participant(&p).await;
        let stranger = participant(&p).await;
        let mine = room(&p, me).await;
        let theirs = room(&p, stranger).await;
        join(&p, mine, me).await;
        join(&p, theirs, stranger).await; // I am NOT a member of `theirs`.

        // A distinctive, unusual word in MY room.
        let marker = "zqdeploymentpipeline";
        let m = msgs
            .insert(NewMessage {
                room_id: mine,
                sender_id: me,
                blocks: vec![Block::text(format!("the {marker} is green"))],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("insert mine");
        // A different distinctive word in a room I can't see.
        let secret = "zqforbiddenkeyword";
        let m_secret = msgs
            .insert(NewMessage {
                room_id: theirs,
                sender_id: stranger,
                blocks: vec![Block::text(format!("a {secret} here"))],
                reply_to: None,
                metadata: serde_json::json!({}),
                expires_at: None,
            })
            .await
            .expect("insert theirs");

        // A near-miss of my word suggests it.
        let sugg = repo
            .suggest_terms(me, ws, "zqdeploymentpipelin", 5)
            .await
            .expect("suggest");
        assert!(
            sugg.iter().any(|w| w == marker),
            "a trigram-near typo surfaces my word, got {sugg:?}"
        );

        // A near-miss of the forbidden word suggests NOTHING — I'm not in that room.
        let sugg = repo
            .suggest_terms(me, ws, "zqforbiddenkeywor", 5)
            .await
            .expect("suggest secret");
        assert!(
            !sugg.iter().any(|w| w == secret),
            "membership boundary: a word from a room I can't see is never suggested, got {sugg:?}"
        );

        // Cleanup.
        sqlx::query("DELETE FROM messages WHERE id = $1").bind(m.id.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM messages WHERE id = $1").bind(m_secret.id.to_uuid()).execute(&p).await.ok();
        for rm in [mine, theirs] {
            sqlx::query("DELETE FROM room_members WHERE room_id = $1").bind(rm.to_uuid()).execute(&p).await.ok();
            sqlx::query("DELETE FROM rooms WHERE id = $1").bind(rm.to_uuid()).execute(&p).await.ok();
        }
        for who in [me, stranger] {
            sqlx::query("DELETE FROM participants WHERE id = $1").bind(who.to_uuid()).execute(&p).await.ok();
        }
    }

    /// `facets` groups the SAME membership-scoped match set by room and by sender,
    /// with counts that sum to the total and lists ordered by count — the
    /// drill-down breakdown. Membership-scoped like search/count.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn facets_break_down_matches_by_room_and_sender() {
        let p = pool();
        let repo = AdvancedSearchRepo::new(p.clone());
        let msgs = MessageRepo::new(p.clone());
        let ws = default_ws();

        let me = participant(&p).await;
        let other = participant(&p).await;
        let room_a = room(&p, me).await;
        let room_b = room(&p, me).await;
        join(&p, room_a, me).await;
        join(&p, room_b, me).await;

        // needle distribution: room_a gets 3 (2 from me, 1 from other), room_b 1 (me).
        let needle = format!("zqfacettoken{}", ParticipantId::new());
        let plan = [(room_a, me), (room_a, me), (room_a, other), (room_b, me)];
        let mut ids = Vec::new();
        for (i, (rm, sender)) in plan.iter().enumerate() {
            let m = msgs
                .insert(NewMessage {
                    room_id: *rm,
                    sender_id: *sender,
                    blocks: vec![Block::text(format!("{needle} n{i}"))],
                    reply_to: None,
                    metadata: serde_json::json!({}),
                    expires_at: None,
                })
                .await
                .expect("insert");
            ids.push(m.id);
        }

        let q = parse_search_query(&needle);
        let total = repo.count(me, ws, &q).await.expect("count");
        let facets = repo.facets(me, ws, &q, 10).await.expect("facets");

        // Room facet: room_a=3, room_b=1, ordered by count desc, summing to total.
        let room_sum: i64 = facets.rooms.iter().map(|f| f.count).sum();
        assert_eq!(room_sum, total, "room facet counts sum to total");
        assert_eq!(facets.rooms.first().map(|f| f.count), Some(3), "top room has 3 hits");
        assert!(
            facets.rooms.iter().any(|f| f.value == room_a.to_uuid().to_string() && f.count == 3),
            "room_a has 3 hits, got {:?}",
            facets.rooms
        );
        assert!(facets.rooms.windows(2).all(|w| w[0].count >= w[1].count), "rooms ordered desc");

        // Sender facet: me=3, other=1.
        let sender_sum: i64 = facets.senders.iter().map(|f| f.count).sum();
        assert_eq!(sender_sum, total, "sender facet counts sum to total");
        assert!(
            facets.senders.iter().any(|f| f.value == me.to_uuid().to_string() && f.count == 3),
            "sender me has 3 hits, got {:?}",
            facets.senders
        );

        // Cleanup.
        for id in &ids {
            sqlx::query("DELETE FROM messages WHERE id = $1").bind(id.to_uuid()).execute(&p).await.ok();
        }
        for rm in [room_a, room_b] {
            sqlx::query("DELETE FROM room_members WHERE room_id = $1").bind(rm.to_uuid()).execute(&p).await.ok();
            sqlx::query("DELETE FROM rooms WHERE id = $1").bind(rm.to_uuid()).execute(&p).await.ok();
        }
        for who in [me, other] {
            sqlx::query("DELETE FROM participants WHERE id = $1").bind(who.to_uuid()).execute(&p).await.ok();
        }
    }
}
