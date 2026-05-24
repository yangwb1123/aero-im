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
    /// caller can build the broadcast envelope without a re-read.
    pub async fn insert_chat(
        &self,
        stream_id: Ulid,
        sender_id: ParticipantId,
        body: &str,
    ) -> Result<(Ulid, OffsetDateTime), sqlx::Error> {
        let id = Ulid::new();
        let created_at = OffsetDateTime::now_utc();
        sqlx::query(
            r#"INSERT INTO stream_chat (id, stream_id, sender_id, body, created_at)
               VALUES ($1, $2, $3, $4, $5)"#,
        )
        .bind(Uuid::from_u128(id.0))
        .bind(Uuid::from_u128(stream_id.0))
        .bind(sender_id.to_uuid())
        .bind(body)
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
        let rows = sqlx::query_as::<_, ChatRow>(
            r#"SELECT c.id, c.stream_id, c.sender_id, p.display_name AS sender_name,
                      c.body, c.created_at
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
