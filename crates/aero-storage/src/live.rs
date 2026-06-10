//! Live-stream interactivity repository (P4): danmaku chat + gift ledger.
//!
//! Cosmetic gift fields (`gift_name`/`gift_icon`) are resolved from the in-code
//! [`gift_catalog`](aero_common::gift_catalog) at read time, so the store only
//! persists `gift_id` and the denormalized coin total.

use aero_common::{gift_by_id, GiftLeaderRow, ParticipantId, StreamChatLine, StreamGiftLine};
use sqlx::PgPool;
use time::OffsetDateTime;
use ulid::Ulid;
use uuid::Uuid;

#[derive(Clone)]
pub struct LiveRepo {
    pool: PgPool,
}

impl LiveRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    // -------------------------------------------------------------- chat

    /// Persist a danmaku line. Returns the generated id + timestamp so the
    /// caller can build the broadcast envelope without a re-read. `is_subscriber`
    /// records whether the sender had an active creator subscription to the stream
    /// owner at post time (Twitch-style subscriber badge, migration 0082); the
    /// caller computes it via [`SubscriptionRepo::is_subscribed`](crate::SubscriptionRepo).
    pub async fn insert_chat(
        &self,
        stream_id: Ulid,
        sender_id: ParticipantId,
        body: &str,
        is_subscriber: bool,
    ) -> Result<(Ulid, OffsetDateTime), sqlx::Error> {
        let id = Ulid::new();
        let created_at = OffsetDateTime::now_utc();
        sqlx::query(
            r#"INSERT INTO stream_chat (id, stream_id, sender_id, body, is_subscriber, created_at)
               VALUES ($1, $2, $3, $4, $5, $6)"#,
        )
        .bind(Uuid::from_u128(id.0))
        .bind(Uuid::from_u128(stream_id.0))
        .bind(sender_id.to_uuid())
        .bind(body)
        .bind(is_subscriber)
        .bind(created_at)
        .execute(&self.pool)
        .await?;
        Ok((id, created_at))
    }

    /// Most-recent chat lines, returned oldest-first for natural display.
    pub async fn recent_chat(
        &self,
        stream_id: Ulid,
        limit: i64,
    ) -> Result<Vec<StreamChatLine>, sqlx::Error> {
        // Defense in depth: the server wrapper clamps to 200, but bound it here too
        // so the store never streams an unbounded danmaku backlog.
        let limit = limit.clamp(1, 200);
        let rows = sqlx::query_as::<_, ChatRow>(
            r#"SELECT c.id, c.stream_id, c.sender_id, p.display_name AS sender_name,
                      c.body, c.is_subscriber, c.created_at
               FROM stream_chat c
               JOIN participants p ON p.id = c.sender_id
               WHERE c.stream_id = $1
               ORDER BY c.created_at DESC
               LIMIT $2"#,
        )
        .bind(Uuid::from_u128(stream_id.0))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().rev().map(StreamChatLine::from).collect())
    }

    /// Chat lines strictly newer than `since` (by id), oldest-first — late-joiner
    /// catch-up so a viewer who joins mid-stream isn't capped at the last
    /// [`recent_chat`](Self::recent_chat) tail and silently misses the lines in
    /// between (ROADMAP 第三版 方向一). When `since` is `None` this is exactly
    /// `recent_chat` (the newest bounded tail). Chat ids are monotonic ULIDs, so
    /// `id > since` is a correct forward keyset window; pass the last id the client
    /// has rendered to page forward.
    pub async fn recent_chat_since(
        &self,
        stream_id: Ulid,
        since: Option<Ulid>,
        limit: i64,
    ) -> Result<Vec<StreamChatLine>, sqlx::Error> {
        let Some(cursor) = since else {
            return self.recent_chat(stream_id, limit).await;
        };
        let limit = limit.clamp(1, 200);
        let rows = sqlx::query_as::<_, ChatRow>(
            r#"SELECT c.id, c.stream_id, c.sender_id, p.display_name AS sender_name,
                      c.body, c.is_subscriber, c.created_at
               FROM stream_chat c
               JOIN participants p ON p.id = c.sender_id
               WHERE c.stream_id = $1 AND c.id > $2
               ORDER BY c.id ASC
               LIMIT $3"#,
        )
        .bind(Uuid::from_u128(stream_id.0))
        .bind(Uuid::from_u128(cursor.0))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(StreamChatLine::from).collect())
    }

    // -------------------------------------------------------------- gifts

    /// Append a gift to the ledger. `coins` is the already-computed total
    /// (`qty * unit price`).
    pub async fn insert_gift(
        &self,
        stream_id: Ulid,
        sender_id: ParticipantId,
        gift_id: &str,
        qty: u32,
        coins: u64,
    ) -> Result<(Ulid, OffsetDateTime), sqlx::Error> {
        let id = Ulid::new();
        let created_at = OffsetDateTime::now_utc();
        sqlx::query(
            r#"INSERT INTO stream_gifts (id, stream_id, sender_id, gift_id, qty, coins, created_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7)"#,
        )
        .bind(Uuid::from_u128(id.0))
        .bind(Uuid::from_u128(stream_id.0))
        .bind(sender_id.to_uuid())
        .bind(gift_id)
        .bind(i32::try_from(qty).unwrap_or(i32::MAX))
        .bind(i64::try_from(coins).unwrap_or(i64::MAX))
        .bind(created_at)
        .execute(&self.pool)
        .await?;
        Ok((id, created_at))
    }

    /// Most-recent gifts, oldest-first.
    pub async fn recent_gifts(
        &self,
        stream_id: Ulid,
        limit: i64,
    ) -> Result<Vec<StreamGiftLine>, sqlx::Error> {
        // Defense in depth: bound the gift backlog the store will stream.
        let limit = limit.clamp(1, 200);
        let rows = sqlx::query_as::<_, GiftRow>(
            r#"SELECT g.id, g.stream_id, g.sender_id, p.display_name AS sender_name,
                      g.gift_id, g.qty, g.coins, g.created_at
               FROM stream_gifts g
               JOIN participants p ON p.id = g.sender_id
               WHERE g.stream_id = $1
               ORDER BY g.created_at DESC
               LIMIT $2"#,
        )
        .bind(Uuid::from_u128(stream_id.0))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().rev().map(gift_line).collect())
    }

    /// Late-joiner catch-up for gifts: ledger entries strictly newer than `since`
    /// (forward cursor by id), oldest-first, or the bounded recent tail when
    /// `since` is `None`. Symmetric to [`Self::recent_chat_since`] (ROADMAP 第三版
    /// 方向一); gift ids are monotonic ULIDs, so `id > since` is a correct window.
    pub async fn recent_gifts_since(
        &self,
        stream_id: Ulid,
        since: Option<Ulid>,
        limit: i64,
    ) -> Result<Vec<StreamGiftLine>, sqlx::Error> {
        let Some(cursor) = since else {
            return self.recent_gifts(stream_id, limit).await;
        };
        let limit = limit.clamp(1, 200);
        let rows = sqlx::query_as::<_, GiftRow>(
            r#"SELECT g.id, g.stream_id, g.sender_id, p.display_name AS sender_name,
                      g.gift_id, g.qty, g.coins, g.created_at
               FROM stream_gifts g
               JOIN participants p ON p.id = g.sender_id
               WHERE g.stream_id = $1 AND g.id > $2
               ORDER BY g.id ASC
               LIMIT $3"#,
        )
        .bind(Uuid::from_u128(stream_id.0))
        .bind(Uuid::from_u128(cursor.0))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(gift_line).collect())
    }

    /// Top spenders on a stream, by total coins.
    pub async fn leaderboard(
        &self,
        stream_id: Ulid,
        limit: i64,
    ) -> Result<Vec<GiftLeaderRow>, sqlx::Error> {
        let rows = sqlx::query_as::<_, LeaderRow>(
            r#"SELECT g.sender_id,
                      p.display_name AS sender_name,
                      SUM(g.coins)::BIGINT AS total_coins,
                      SUM(g.qty)::BIGINT   AS total_qty
               FROM stream_gifts g
               JOIN participants p ON p.id = g.sender_id
               WHERE g.stream_id = $1
               GROUP BY g.sender_id, p.display_name
               ORDER BY total_coins DESC
               LIMIT $2"#,
        )
        .bind(Uuid::from_u128(stream_id.0))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| GiftLeaderRow {
                sender_id: ParticipantId::from_uuid(r.sender_id),
                sender_name: r.sender_name,
                total_coins: r.total_coins.max(0) as u64,
                total_qty: r.total_qty.max(0) as u64,
            })
            .collect())
    }
}

#[derive(sqlx::FromRow)]
struct ChatRow {
    id: Uuid,
    stream_id: Uuid,
    sender_id: Uuid,
    sender_name: String,
    body: String,
    is_subscriber: bool,
    created_at: OffsetDateTime,
}

impl From<ChatRow> for StreamChatLine {
    fn from(r: ChatRow) -> Self {
        Self {
            id: Ulid(r.id.as_u128()),
            stream_id: Ulid(r.stream_id.as_u128()),
            sender_id: ParticipantId::from_uuid(r.sender_id),
            sender_name: r.sender_name,
            body: r.body,
            is_subscriber: r.is_subscriber,
            created_at: r.created_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct GiftRow {
    id: Uuid,
    stream_id: Uuid,
    sender_id: Uuid,
    sender_name: String,
    gift_id: String,
    qty: i32,
    coins: i64,
    created_at: OffsetDateTime,
}

/// Map a stored gift row to the wire type, enriching name/icon from the catalog
/// (falling back gracefully if the catalog changed since the gift was sent).
fn gift_line(r: GiftRow) -> StreamGiftLine {
    let (name, icon) = gift_by_id(&r.gift_id)
        .map(|g| (g.name, g.icon))
        .unwrap_or_else(|| (r.gift_id.clone(), "🎁".to_owned()));
    StreamGiftLine {
        id: Ulid(r.id.as_u128()),
        stream_id: Ulid(r.stream_id.as_u128()),
        sender_id: ParticipantId::from_uuid(r.sender_id),
        sender_name: r.sender_name,
        gift_id: r.gift_id,
        gift_name: name,
        gift_icon: icon,
        qty: r.qty.max(0) as u32,
        coins: r.coins.max(0) as u64,
        created_at: r.created_at,
    }
}

#[derive(sqlx::FromRow)]
struct LeaderRow {
    sender_id: Uuid,
    sender_name: String,
    total_coins: i64,
    total_qty: i64,
}

#[cfg(test)]
mod db_tests {
    use super::{LiveRepo, ParticipantId};
    use sqlx::PgPool;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// `recent_chat_since` returns only lines strictly newer than the cursor,
    /// oldest-first (late-joiner catch-up); `since = None` falls back to the tail.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn recent_chat_since_returns_only_newer_lines() {
        let p = pool();
        let repo = LiveRepo::new(p.clone());

        let owner = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(owner)
            .bind(format!("live-owner-{owner}"))
            .execute(&p)
            .await
            .expect("owner");
        let stream = ulid::Ulid::new();
        sqlx::query("INSERT INTO streams (id, owner_id, title, stream_key) VALUES ($1,$2,$3,$4)")
            .bind(uuid::Uuid::from_u128(stream.0))
            .bind(owner)
            .bind("catchup-stream")
            .bind(format!("key-{stream}"))
            .execute(&p)
            .await
            .expect("stream");

        let sender = ParticipantId::from_uuid(owner);
        let (l1, _) = repo.insert_chat(stream, sender, "line 1", false).await.expect("l1");
        let (l2, _) = repo.insert_chat(stream, sender, "line 2", false).await.expect("l2");
        let (l3, _) = repo.insert_chat(stream, sender, "line 3", true).await.expect("l3");

        // since = l1 → only l2, l3, oldest-first.
        let after = repo.recent_chat_since(stream, Some(l1), 50).await.expect("since l1");
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].id, l2);
        assert_eq!(after[1].id, l3);

        // since = newest → nothing newer.
        let none_newer = repo.recent_chat_since(stream, Some(l3), 50).await.expect("since l3");
        assert!(none_newer.is_empty(), "no lines after the newest cursor");

        // since = None → the bounded tail (all three, oldest-first).
        let tail = repo.recent_chat_since(stream, None, 50).await.expect("tail");
        assert_eq!(tail.len(), 3);
        assert_eq!(tail.first().map(|l| l.id), Some(l1));

        // Cleanup (stream cascade removes its chat rows).
        sqlx::query("DELETE FROM streams WHERE id = $1")
            .bind(uuid::Uuid::from_u128(stream.0))
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1").bind(owner).execute(&p).await.ok();
    }

    /// `recent_gifts_since` mirrors the chat catch-up: only ledger entries newer
    /// than the cursor, oldest-first; `None` falls back to the recent tail.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn recent_gifts_since_returns_only_newer_lines() {
        let p = pool();
        let repo = LiveRepo::new(p.clone());

        let owner = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(owner)
            .bind(format!("gift-owner-{owner}"))
            .execute(&p)
            .await
            .expect("owner");
        let stream = ulid::Ulid::new();
        sqlx::query("INSERT INTO streams (id, owner_id, title, stream_key) VALUES ($1,$2,$3,$4)")
            .bind(uuid::Uuid::from_u128(stream.0))
            .bind(owner)
            .bind("gift-stream")
            .bind(format!("gkey-{stream}"))
            .execute(&p)
            .await
            .expect("stream");

        let sender = ParticipantId::from_uuid(owner);
        let (g1, _) = repo.insert_gift(stream, sender, "rose", 1, 10).await.expect("g1");
        let (g2, _) = repo.insert_gift(stream, sender, "rose", 2, 20).await.expect("g2");

        let after = repo.recent_gifts_since(stream, Some(g1), 50).await.expect("since g1");
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, g2);

        let none_newer = repo.recent_gifts_since(stream, Some(g2), 50).await.expect("since g2");
        assert!(none_newer.is_empty(), "no gifts after the newest cursor");

        let tail = repo.recent_gifts_since(stream, None, 50).await.expect("tail");
        assert_eq!(tail.len(), 2);

        sqlx::query("DELETE FROM streams WHERE id = $1")
            .bind(uuid::Uuid::from_u128(stream.0))
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1").bind(owner).execute(&p).await.ok();
    }

    /// A subscriber's chat line carries `is_subscriber = true`; a non-subscriber's
    /// carries `false` (migration 0082, the Twitch-style subscriber badge).
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn insert_chat_records_subscriber_flag() {
        let p = pool();
        let repo = LiveRepo::new(p.clone());

        let owner = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1,'human',$2)")
            .bind(owner)
            .bind(format!("sub-owner-{owner}"))
            .execute(&p)
            .await
            .expect("owner");
        let stream = ulid::Ulid::new();
        sqlx::query("INSERT INTO streams (id, owner_id, title, stream_key) VALUES ($1,$2,$3,$4)")
            .bind(uuid::Uuid::from_u128(stream.0))
            .bind(owner)
            .bind("sub-flag-stream")
            .bind(format!("skey-{stream}"))
            .execute(&p)
            .await
            .expect("stream");

        let sender = ParticipantId::from_uuid(owner);
        // Non-subscriber then subscriber line.
        let (l_no, _) = repo.insert_chat(stream, sender, "non-sub", false).await.expect("l_no");
        let (l_yes, _) = repo.insert_chat(stream, sender, "sub", true).await.expect("l_yes");

        let lines = repo.recent_chat(stream, 50).await.expect("recent");
        let no = lines.iter().find(|l| l.id == l_no).expect("non-sub line present");
        let yes = lines.iter().find(|l| l.id == l_yes).expect("sub line present");
        assert!(!no.is_subscriber, "non-subscriber line flag is false");
        assert!(yes.is_subscriber, "subscriber line flag is true");

        sqlx::query("DELETE FROM streams WHERE id = $1")
            .bind(uuid::Uuid::from_u128(stream.0))
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1").bind(owner).execute(&p).await.ok();
    }
}
