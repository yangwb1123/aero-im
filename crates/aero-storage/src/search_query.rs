//! Advanced cross-room search — Slack-style query operators.
//!
//! Parses a raw search string into a [`ParsedQuery`] of free-text `terms` plus
//! optional structured filters (`from:@<id>`, `in:<roomid>`, `before:<msgid>`,
//! `after:<msgid>`), then runs it through [`AdvancedSearchRepo::search`] — the
//! effective-access global search ([`MessageRepo::search_all_rooms_in_workspace`](crate::MessageRepo),
//! whose effective-access joins are the security boundary) with the structured
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
    /// `in:<roomid>` — restrict to a single room (still effective-access-scoped).
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
                format!("{v}T00:00:00Z")
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
                format!("{v}T23:59:59Z")
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
    recalled_at: Option<time::OffsetDateTime>,
    recalled_by: Option<uuid::Uuid>,
    expires_at: Option<time::OffsetDateTime>,
    version: i32,
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
                recalled_at: r.recalled_at,
                recalled_by: r.recalled_by.map(ParticipantId::from_uuid),
                expires_at: r.expires_at,
                version: r.version,
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
    /// Same effective-access boundary as
    /// [`MessageRepo::search_all_rooms_in_workspace`](crate::MessageRepo): the
    /// room/workspace/account/deactivation/2FA intersection is the security
    /// guard, so a stale room edge cannot leak content. The full-text/trigram match
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
        Ok(self
            .search_page(participant, workspace, q, limit, None)
            .await?
            .0)
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
                     created_at, edited_at, deleted_at, recalled_at, recalled_by, expires_at, version, score, headline
               FROM (
                 SELECT
                   m.id, m.room_id, m.sender_id, m.blocks, m.reply_to, m.metadata,
                   m.created_at, m.edited_at, m.deleted_at, m.recalled_at, m.recalled_by, m.expires_at, m.version,
                   GREATEST(
                     ts_rank(m.search_tsv, websearch_to_tsquery('english', f_unaccent($2))),
                     similarity(m.searchable_text, $2)
                   ) AS score,
                   ts_headline('english', m.searchable_text, websearch_to_tsquery('english', f_unaccent($2)),
                     'StartSel=<b>, StopSel=</b>, MaxWords=50, MinWords=15, ShortWord=3') AS headline
                 FROM messages m
                 JOIN rooms r ON r.id = m.room_id
                 JOIN workspaces w ON w.id = r.workspace_id
                 JOIN room_members rm
                   ON rm.room_id = m.room_id AND rm.participant_id = $1
                 JOIN workspace_members wm
                   ON wm.workspace_id = r.workspace_id AND wm.participant_id = $1
                 JOIN participants viewer
                   ON viewer.id = $1 AND viewer.deleted_at IS NULL
                 LEFT JOIN workspace_deactivations deactivated
                   ON deactivated.workspace_id = r.workspace_id
                  AND deactivated.participant_id = $1
                 LEFT JOIN totp_secrets totp ON totp.participant_id = $1
                 WHERE r.workspace_id = $4
                   AND deactivated.participant_id IS NULL
                   AND (
                       viewer.kind <> 'human'
                       OR NOT w.require_2fa
                       OR COALESCE(totp.activated, false)
                   )
                   AND m.deleted_at IS NULL
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
            rows.last().map(|r| SearchCursor {
                score: r.score,
                id: MessageId::from_uuid(r.id),
            })
        } else {
            None
        };
        Ok((rows.into_iter().map(SearchHit::from).collect(), next))
    }

    /// Total number of messages matching `q` for `participant` in `workspace`,
    /// independent of any page limit. Mirrors [`search_page`](Self::search_page)'s
    /// WHERE clause exactly (same effective-access boundary, same filters) so the count
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
               JOIN rooms r ON r.id = m.room_id
               JOIN workspaces w ON w.id = r.workspace_id
               JOIN room_members rm
                 ON rm.room_id = m.room_id AND rm.participant_id = $1
               JOIN workspace_members wm
                 ON wm.workspace_id = r.workspace_id AND wm.participant_id = $1
               JOIN participants viewer
                 ON viewer.id = $1 AND viewer.deleted_at IS NULL
               LEFT JOIN workspace_deactivations deactivated
                 ON deactivated.workspace_id = r.workspace_id
                AND deactivated.participant_id = $1
               LEFT JOIN totp_secrets totp ON totp.participant_id = $1
               WHERE r.workspace_id = $2
                 AND deactivated.participant_id IS NULL
                 AND (
                     viewer.kind <> 'human'
                     OR NOT w.require_2fa
                     OR COALESCE(totp.activated, false)
                 )
                 AND m.deleted_at IS NULL
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
    /// hit count, for the same effective-access-scoped result set as
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
        let rooms = self
            .facet_by(participant, workspace, q, FacetDim::Room, top)
            .await?;
        let senders = self
            .facet_by(participant, workspace, q, FacetDim::Sender, top)
            .await?;
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
               JOIN rooms r ON r.id = m.room_id
               JOIN workspaces w ON w.id = r.workspace_id
               JOIN room_members rm
                 ON rm.room_id = m.room_id AND rm.participant_id = $1
               JOIN workspace_members wm
                 ON wm.workspace_id = r.workspace_id AND wm.participant_id = $1
               JOIN participants viewer
                 ON viewer.id = $1 AND viewer.deleted_at IS NULL
               LEFT JOIN workspace_deactivations deactivated
                 ON deactivated.workspace_id = r.workspace_id
                AND deactivated.participant_id = $1
               LEFT JOIN totp_secrets totp ON totp.participant_id = $1
               WHERE r.workspace_id = $2
                 AND deactivated.participant_id IS NULL
                 AND (
                     viewer.kind <> 'human'
                     OR NOT w.require_2fa
                     OR COALESCE(totp.activated, false)
                 )
                 AND m.deleted_at IS NULL
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
        Ok(rows
            .into_iter()
            .map(|(value, count)| FacetCount { value, count })
            .collect())
    }

    /// "Did you mean…" suggestions for `term`: distinct words from the caller's
    /// recently-visible messages that are trigram-similar to `term`. Membership-
    /// scoped via the same effective-access boundary as search, so a suggestion
    /// never reveals a word from a room the caller can't currently see. Intended to be
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
                 JOIN rooms r ON r.id = m.room_id
                 JOIN workspaces w ON w.id = r.workspace_id
                 JOIN room_members rm
                   ON rm.room_id = m.room_id AND rm.participant_id = $1
                 JOIN workspace_members wm
                   ON wm.workspace_id = r.workspace_id AND wm.participant_id = $1
                 JOIN participants viewer
                   ON viewer.id = $1 AND viewer.deleted_at IS NULL
                 LEFT JOIN workspace_deactivations deactivated
                   ON deactivated.workspace_id = r.workspace_id
                  AND deactivated.participant_id = $1
                 LEFT JOIN totp_secrets totp ON totp.participant_id = $1
                 WHERE r.workspace_id = $2
                   AND deactivated.participant_id IS NULL
                   AND (
                       viewer.kind <> 'human'
                       OR NOT w.require_2fa
                       OR COALESCE(totp.activated, false)
                   )
                   AND m.deleted_at IS NULL
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
        Some(Self {
            score: f32::from_bits(bits),
            id,
        })
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
        assert!(
            pq.in_room.is_none()
                && pq.before.is_none()
                && pq.after.is_none()
                && pq.after_ts.is_none()
                && pq.before_ts.is_none()
        );
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
        assert!(
            pq.after_ts.is_some(),
            "since: should parse a YYYY-MM-DD date"
        );
        assert!(
            pq.before_ts.is_some(),
            "until: should parse a YYYY-MM-DD date"
        );
        assert_eq!(pq.terms, "crash");
        // RFC 3339 datetimes also work.
        let pq2 = parse_search_query("since:2024-01-15T00:00:00Z deploy");
        assert!(
            pq2.after_ts.is_some(),
            "since: should parse an RFC 3339 datetime"
        );
        assert_eq!(pq2.terms, "deploy");
        // Malformed date stays as free text.
        let pq3 = parse_search_query("since:notadate words");
        assert!(
            pq3.after_ts.is_none(),
            "malformed since: stays as free text"
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
#[path = "search_query/db_tests.rs"]
mod db_tests;
