//! Per-stream creator analytics — read-only aggregates over already-persisted data.
//!
//! Backs the creator dashboard with one method, [`StreamStatsRepo::analytics`],
//! that rolls up a single stream's gift ledger (`stream_gifts`, migration 0005),
//! danmaku chat (`stream_chat`), and lifecycle timestamps (`streams.started_at` /
//! `streams.ended_at`) into a [`StreamAnalytics`] snapshot. Purely additive: a
//! NEW [`StreamStatsRepo`]; no existing repo, table, or migration is touched, and
//! it never writes.
//!
//! NOTE (future seam): peak / average *concurrent* viewers are intentionally
//! omitted — there is no viewer-count sampling table to aggregate. Surfacing them
//! would first require a periodic sampling hook (write a `(stream_id, ts,
//! viewers)` row on an interval); when that lands, extend this struct and query
//! rather than changing call sites.

use serde::Serialize;
use sqlx::PgPool;
use ulid::Ulid;
use uuid::Uuid;

/// A creator-dashboard snapshot for one stream — pure aggregates over the gift
/// ledger, chat log, and the stream's own start/end timestamps.
///
/// `Serialize` so a handler can hand it straight back as JSON. Every count is a
/// non-negative `i64` (`0` for a stream with no gifts/chat); `duration_secs` is
/// `None` until the stream has both a `started_at` and an `ended_at`.
#[derive(Debug, Clone, Serialize)]
pub struct StreamAnalytics {
    /// Number of gift *events* sent to the stream (rows in `stream_gifts`).
    pub gift_count: i64,
    /// Total gift *units* sent (sum of each event's quantity).
    pub gift_units: i64,
    /// Total gift revenue in coins (sum of each event's denormalized `coins`,
    /// i.e. quantity × unit price — the stored revenue column).
    pub gift_coins: i64,
    /// Number of chat (danmaku) lines posted to the stream.
    pub chat_count: i64,
    /// Distinct participants who posted at least one chat line.
    pub unique_chatters: i64,
    /// Stream wall-clock duration in whole seconds, or `None` until the stream
    /// has both started and ended (`ended_at - started_at`).
    pub duration_secs: Option<i64>,
}

/// Repository for read-only per-stream creator analytics.
///
/// Cheap to clone — it just wraps a [`PgPool`] (an `Arc` internally), so feature
/// modules build one inline via [`StreamStatsRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct StreamStatsRepo {
    pool: PgPool,
}

impl StreamStatsRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Aggregate one stream's already-persisted gift, chat, and lifecycle data
    /// into a [`StreamAnalytics`] snapshot. Stream ids are [`Ulid`]s stored as
    /// UUID (matching [`StreamRepo`](crate::StreamRepo)), bound via
    /// `uuid::Uuid::from_u128(ulid.0)`.
    ///
    /// Counts default to `0` for an unknown or empty stream, and `duration_secs`
    /// is `None` unless the stream has both `started_at` and `ended_at` — callers
    /// that need a 404 for an unknown stream should check existence/ownership
    /// first (the server handler does, via `StreamRepo::get`).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the aggregate query.
    pub async fn analytics(&self, stream: Ulid) -> Result<StreamAnalytics, sqlx::Error> {
        let sid = Uuid::from_u128(stream.0);
        let (gift_count, gift_units, gift_coins, chat_count, unique_chatters, duration_secs) =
            sqlx::query_as::<_, (i64, i64, i64, i64, i64, Option<i64>)>(
                r"SELECT
                    (SELECT COUNT(*) FROM stream_gifts WHERE stream_id = $1),
                    (SELECT COALESCE(SUM(qty), 0)::bigint FROM stream_gifts WHERE stream_id = $1),
                    (SELECT COALESCE(SUM(coins), 0)::bigint FROM stream_gifts WHERE stream_id = $1),
                    (SELECT COUNT(*) FROM stream_chat WHERE stream_id = $1),
                    (SELECT COUNT(DISTINCT sender_id) FROM stream_chat WHERE stream_id = $1),
                    (SELECT EXTRACT(EPOCH FROM (ended_at - started_at))::bigint
                       FROM streams WHERE id = $1)",
            )
            .bind(sid)
            .fetch_one(&self.pool)
            .await?;
        Ok(StreamAnalytics {
            gift_count,
            gift_units,
            gift_coins,
            chat_count,
            unique_chatters,
            duration_secs,
        })
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored stream_stats
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::ParticipantId;
    use time::OffsetDateTime;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway participant so the test is self-contained.
    async fn participant(p: &PgPool, label: &str) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("stream-stats-{label}-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn stream_stats_counts_gifts_chat_and_duration() {
        let p = pool();
        let repo = StreamStatsRepo::new(p.clone());
        let owner = participant(&p, "owner").await;
        let viewer = participant(&p, "viewer").await;
        let stream = Ulid::new();
        let sid = Uuid::from_u128(stream.0);

        // A started + ended stream so duration is computable (120s window).
        let started = OffsetDateTime::now_utc();
        let ended = started + time::Duration::seconds(120);
        sqlx::query(
            r"INSERT INTO streams (id, owner_id, title, stream_key, status, protocol, started_at, ended_at)
               VALUES ($1, $2, 'analytics test', $3, 'ended', 'rtmp', $4, $5)",
        )
        .bind(sid)
        .bind(owner.to_uuid())
        .bind(format!("key-{stream}"))
        .bind(started)
        .bind(ended)
        .execute(&p)
        .await
        .expect("insert stream");

        // Two chat lines from the SAME viewer ⇒ chat_count 2, unique_chatters 1.
        for body in ["hello", "world"] {
            sqlx::query(
                "INSERT INTO stream_chat (id, stream_id, sender_id, body) VALUES ($1, $2, $3, $4)",
            )
            .bind(Uuid::new_v4())
            .bind(sid)
            .bind(viewer.to_uuid())
            .bind(body)
            .execute(&p)
            .await
            .expect("insert chat");
        }

        // One gift event: qty 3, coins 300 ⇒ gift_count 1, gift_units 3, gift_coins 300.
        sqlx::query(
            "INSERT INTO stream_gifts (id, stream_id, sender_id, gift_id, qty, coins)
               VALUES ($1, $2, $3, 'rose', 3, 300)",
        )
        .bind(Uuid::new_v4())
        .bind(sid)
        .bind(viewer.to_uuid())
        .execute(&p)
        .await
        .expect("insert gift");

        let a = repo.analytics(stream).await.expect("analytics");
        assert_eq!(a.gift_count, 1, "one gift event");
        assert_eq!(a.gift_units, 3, "three gift units");
        assert_eq!(a.gift_coins, 300, "300 coins of revenue");
        assert_eq!(a.chat_count, 2, "two chat lines");
        assert_eq!(a.unique_chatters, 1, "one distinct chatter");
        assert_eq!(a.duration_secs, Some(120), "120-second duration");

        // Cleanup: chat/gift rows cascade with the stream; then the participants.
        sqlx::query("DELETE FROM streams WHERE id = $1")
            .bind(sid)
            .execute(&p)
            .await
            .ok();
        for pid in [owner, viewer] {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(pid.to_uuid())
                .execute(&p)
                .await
                .ok();
        }
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn stream_stats_empty_stream_is_zeroed() {
        let p = pool();
        let repo = StreamStatsRepo::new(p.clone());
        let owner = participant(&p, "owner-empty").await;
        let stream = Ulid::new();
        let sid = Uuid::from_u128(stream.0);

        // A never-started stream: zero counts, no duration.
        sqlx::query(
            r"INSERT INTO streams (id, owner_id, title, stream_key, status, protocol)
               VALUES ($1, $2, 'empty', $3, 'idle', 'rtmp')",
        )
        .bind(sid)
        .bind(owner.to_uuid())
        .bind(format!("key-{stream}"))
        .execute(&p)
        .await
        .expect("insert stream");

        let a = repo.analytics(stream).await.expect("analytics");
        assert_eq!(a.gift_count, 0);
        assert_eq!(a.gift_units, 0);
        assert_eq!(a.gift_coins, 0);
        assert_eq!(a.chat_count, 0);
        assert_eq!(a.unique_chatters, 0);
        assert_eq!(a.duration_secs, None, "no duration without start/end");

        sqlx::query("DELETE FROM streams WHERE id = $1")
            .bind(sid)
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
