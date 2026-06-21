//! Rolling conversational context for the AI Q&A feature.
//!
//! Each `(participant, room)` pair gets its own Redis sorted set that keeps the
//! last N Q&A turns. The score is a microsecond-precision timestamp so turns
//! are ordered by insertion time. Trimming keeps only the most-recent
//! `MAX_TURNS` members; an `EXPIRE` on every write evicts idle sessions after
//! `SESSION_TTL`.
//!
//! Turn encoding: `"{role}\x1F{text}"` — the ASCII unit-separator (0x1F) is
//! safe as a delimiter because it never appears in normal UTF-8 prose.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aero_common::{ParticipantId, RoomId};
use fred::prelude::{Expiration, KeysInterface, RedisClient, SortedSetsInterface};

/// Maximum turns retained per session (5 Q&A pairs = 10 messages).
pub const MAX_TURNS: i64 = 10;

/// Sessions not touched within this window are evicted from Redis.
pub const SESSION_TTL: Duration = Duration::from_secs(7200); // 2 hours

const SEP: char = '\x1F';

#[derive(Clone)]
pub struct AiContextStore {
    client: RedisClient,
}

impl AiContextStore {
    pub fn new(client: RedisClient) -> Self {
        Self { client }
    }

    fn key(participant: ParticipantId, room: RoomId) -> String {
        format!("ai:ctx:{participant}:{room}")
    }

    /// Answer-cache key: ROOM-scoped (every member has the same room access, so a
    /// shared cached answer can never leak across tenants/permissions) and keyed by
    /// a sha-256 of the pre-normalized query (deterministic across nodes, bounded
    /// length, raw question text kept out of the Redis key).
    fn answer_cache_key(room: RoomId, query_norm: &str) -> String {
        format!("ai:ans:{room}:{}", crate::revoked_token::hash_token(query_norm))
    }

    /// Look up a cached answer (serialized `AnswerResult` JSON) for a room + a
    /// pre-normalized query. `None` ⇒ miss. ROADMAP 方向一·3.
    ///
    /// # Errors
    /// Propagates Redis errors; the caller logs + falls through to a live answer.
    pub async fn cache_answer_get(
        &self,
        room: RoomId,
        query_norm: &str,
    ) -> anyhow::Result<Option<String>> {
        let v: Option<String> = self.client.get(Self::answer_cache_key(room, query_norm)).await?;
        Ok(v)
    }

    /// Cache an answer for `ttl_secs`. Short TTL bounds staleness vs a referenced
    /// message edited/deleted after caching (a fuller design would invalidate on
    /// the room's `Edited`/`Deleted` events); the room scope guarantees isolation.
    ///
    /// # Errors
    /// Propagates Redis errors; the caller logs + continues (cache is best-effort).
    pub async fn cache_answer_put(
        &self,
        room: RoomId,
        query_norm: &str,
        value: &str,
        ttl_secs: i64,
    ) -> anyhow::Result<()> {
        self.client
            .set::<(), _, _>(
                Self::answer_cache_key(room, query_norm),
                value,
                Some(Expiration::EX(ttl_secs.max(1))),
                None,
                false,
            )
            .await?;
        Ok(())
    }

    /// Append one turn (role + text) to the session, then trim to the last
    /// [`MAX_TURNS`] and reset the idle TTL.
    ///
    /// Role should be `"user"` or `"assistant"`.
    ///
    /// # Errors
    /// Propagates Redis errors; the caller should log and continue without
    /// blocking the AI response path.
    pub async fn push_turn(
        &self,
        participant: ParticipantId,
        room: RoomId,
        role: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        let key = Self::key(participant, room);
        let score = microsecond_score();
        let member = format!("{role}{SEP}{text}");

        self.client
            .zadd::<(), _, _>(&key, None, None, false, false, (score, member))
            .await?;

        // Keep only the last MAX_TURNS; ZREMRANGEBYRANK removes [0, -(limit+1)]
        // so after trimming exactly MAX_TURNS members remain.
        self.client
            .zremrangebyrank::<(), _>(&key, 0, -(MAX_TURNS + 1))
            .await?;

        // SESSION_TTL is 7200s — well within i64 range; cast is safe.
        #[allow(clippy::cast_possible_wrap)]
        self.client
            .expire::<(), _>(&key, SESSION_TTL.as_secs() as i64)
            .await?;

        Ok(())
    }

    /// Load the last `n` turns in chronological order (oldest first).
    ///
    /// Returns `(role, text)` pairs. Silently skips any malformed members.
    ///
    /// # Errors
    /// Propagates Redis errors.
    pub async fn get_turns(
        &self,
        participant: ParticipantId,
        room: RoomId,
        n: usize,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let key = Self::key(participant, room);
        // ZRANGEBYRANK with REV would give newest-first; we want oldest-first
        // for chronological conversation order. Get the last n via tail indices.
        // n is caller-controlled (≤ MAX_TURNS = 10); always fits in i64.
        #[allow(clippy::cast_possible_wrap)]
        let start: i64 = -(n as i64);
        let raw: Vec<String> = self.client
            .zrange(&key, start, -1, None, false, None, false)
            .await?;

        Ok(raw
            .into_iter()
            .filter_map(|s| {
                let pos = s.find(SEP)?;
                let role = s[..pos].to_string();
                let text = s[pos + SEP.len_utf8()..].to_string();
                Some((role, text))
            })
            .collect())
    }

    /// Remove all turns for a session (explicit sign-out or test teardown).
    pub async fn clear(&self, participant: ParticipantId, room: RoomId) -> anyhow::Result<()> {
        let key = Self::key(participant, room);
        let _: () = self.client.del(&key).await?;
        Ok(())
    }
}

/// Microseconds since epoch as `f64` — sortable, fits all realistic timestamps
/// with sub-millisecond ordering precision. Saturates to 0 on pre-epoch clocks.
fn microsecond_score() -> f64 {
    // Microsecond timestamps fit in f64 with ~1µs resolution loss at the current
    // epoch (~2^51 µs). Sub-millisecond ordering is all we need here.
    #[allow(clippy::cast_precision_loss)]
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_micros() as f64)
}

// ---------- Pure helpers (unit-tested without Redis) ----------

/// Parse a raw member string `"{role}\x1F{text}"` into `(role, text)`.
/// Returns `None` if the separator is absent.
pub fn parse_turn(raw: &str) -> Option<(&str, &str)> {
    let pos = raw.find(SEP)?;
    Some((&raw[..pos], &raw[pos + SEP.len_utf8()..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_turn_splits_on_sep() {
        assert_eq!(parse_turn("user\x1FHello there"), Some(("user", "Hello there")));
        assert_eq!(parse_turn("assistant\x1FI can help!"), Some(("assistant", "I can help!")));
    }

    #[test]
    fn parse_turn_returns_none_without_sep() {
        assert_eq!(parse_turn("no separator here"), None);
        assert_eq!(parse_turn(""), None);
    }

    #[test]
    fn parse_turn_handles_text_with_sep() {
        // Text itself may contain \x1F — only the first occurrence is the delimiter.
        let raw = "user\x1FQuestion\x1Fwith\x1Fseps";
        let (role, text) = parse_turn(raw).unwrap();
        assert_eq!(role, "user");
        assert_eq!(text, "Question\x1Fwith\x1Fseps");
    }

    #[test]
    fn microsecond_score_is_positive() {
        assert!(microsecond_score() > 0.0);
    }

    #[test]
    fn microsecond_score_increases_monotonically() {
        let a = microsecond_score();
        // Brief busy-wait ensures different system-time samples.
        let mut b = microsecond_score();
        while b == a {
            b = microsecond_score();
        }
        assert!(b > a);
    }

    async fn redis() -> RedisClient {
        use fred::prelude::ClientLike;
        let url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
        let c = RedisClient::new(fred::types::RedisConfig::from_url(&url).unwrap(), None, None, None);
        c.connect();
        c.wait_for_connect().await.unwrap();
        c
    }

    #[tokio::test]
    #[ignore = "requires live Redis"]
    async fn answer_cache_roundtrip_and_room_isolation() {
        let store = AiContextStore::new(redis().await);
        let room_a = RoomId::new();
        let room_b = RoomId::new();
        let q = "部署在哪台机器";
        let val = r#"{"answer":"node-7","citations":[]}"#;

        // Miss before put.
        assert!(store.cache_answer_get(room_a, q).await.unwrap().is_none());

        // Put in room A → hit in room A, but a MISS in room B (room isolation = no
        // cross-tenant leak, the cache's safety guarantee).
        store.cache_answer_put(room_a, q, val, 60).await.unwrap();
        assert_eq!(store.cache_answer_get(room_a, q).await.unwrap().as_deref(), Some(val));
        assert!(
            store.cache_answer_get(room_b, q).await.unwrap().is_none(),
            "a cached answer must NOT leak across rooms"
        );
        // A different query in the same room is also a miss.
        assert!(store.cache_answer_get(room_a, "完全不同的问题").await.unwrap().is_none());
    }
}
