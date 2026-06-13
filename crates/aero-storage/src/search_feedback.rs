//! Search click-feedback repository (ROADMAP5 方向三 P2).
//!
//! Records which search result a user actually opened — the data foundation for
//! learning-to-rank and relevance analytics. The advanced search ranks a result
//! list ([`AdvancedSearchRepo`](crate::AdvancedSearchRepo)); when a participant
//! clicks one of those results, the interactions surface records it here
//! ([`record_click`](SearchFeedbackRepo::record_click)) with the result's 0-based
//! rank in the list. Aggregates ([`ctr_stats`](SearchFeedbackRepo::ctr_stats))
//! turn the log into a click-through rate and mean-reciprocal-rank signal.
//!
//! Backs `migrations/0133_search_click_events.sql`. Append-only with no FK to
//! `messages` (the relevance signal must survive a later delete/erasure of the
//! clicked result), swept by the data-lifecycle retention loop. Purely additive —
//! a NEW repo; no existing repo is touched.

use aero_common::{MessageId, ParticipantId, WorkspaceId};
use serde::Serialize;
use sqlx::PgPool;

/// Aggregate relevance signal over a window of recorded search clicks.
///
/// `Serialize` so a handler can return it directly as JSON.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CtrStats {
    /// Number of recorded click-throughs in the window.
    pub clicks: i64,
    /// Number of *distinct* queries that produced at least one click.
    pub queries: i64,
    /// Mean reciprocal rank: average of `1 / (rank + 1)` over all clicks (rank is
    /// 0-based, so a top-hit click contributes 1.0, the 2nd result 0.5, …). A
    /// higher MRR means users click results the ranker already placed near the
    /// top — the headline learning-to-rank quality signal. `0.0` when no clicks.
    pub mean_reciprocal_rank: f64,
    /// Fraction of clicks that landed on the top result (`rank = 0`). `0.0` when
    /// no clicks. A complement to MRR that is easy to read as a percentage.
    pub top_result_ctr: f64,
}

/// Repository over `search_click_events`.
///
/// Cheap to clone — it just wraps a [`PgPool`] (an `Arc` internally), so feature
/// modules build one inline via [`SearchFeedbackRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct SearchFeedbackRepo {
    pool: PgPool,
}

impl SearchFeedbackRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record one search-result click-through. `query_text` is the free-text
    /// query whose result list was clicked; `result` is the opened message and
    /// `rank` its 0-based position in that list. A negative `rank` is clamped to
    /// 0 (defensive — the caller should pass the real list index).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the insert.
    pub async fn record_click(
        &self,
        participant: ParticipantId,
        workspace: WorkspaceId,
        query_text: &str,
        result: MessageId,
        rank: i32,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"INSERT INTO search_click_events
                 (participant_id, workspace_id, query_text, result_id, result_rank)
               VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(participant.to_uuid())
        .bind(workspace.to_uuid())
        .bind(query_text)
        .bind(result.to_uuid())
        .bind(rank.max(0))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Aggregate the recorded clicks for `workspace` over the last `window_days`
    /// into a [`CtrStats`] (click count, distinct queries, MRR, top-result CTR).
    ///
    /// MRR is computed in SQL as `AVG(1.0 / (result_rank + 1))` — a single pass,
    /// no per-row round-trip. Returns all-zero stats for a workspace with no
    /// clicks in the window (the `COALESCE`s hold).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the aggregate query.
    pub async fn ctr_stats(
        &self,
        workspace: WorkspaceId,
        window_days: i32,
    ) -> Result<CtrStats, sqlx::Error> {
        let (clicks, queries, mrr, top): (i64, i64, f64, f64) = sqlx::query_as(
            r"SELECT
                 COUNT(*)::bigint,
                 COUNT(DISTINCT query_text)::bigint,
                 COALESCE(AVG(1.0 / (result_rank + 1)), 0)::double precision,
                 COALESCE(AVG((result_rank = 0)::int), 0)::double precision
               FROM search_click_events
               WHERE workspace_id = $1
                 AND clicked_at > now() - make_interval(days => $2)",
        )
        .bind(workspace.to_uuid())
        .bind(window_days.max(1))
        .fetch_one(&self.pool)
        .await?;
        Ok(CtrStats { clicks, queries, mean_reciprocal_rank: mrr, top_result_ctr: top })
    }

    /// Hard-delete click events older than `cutoff` (data-lifecycle retention
    /// sweep, ROADMAP5 方向四). Append-only with no parent FK, so it grows without
    /// bound; the relevance signal is only useful while recent. Returns the number
    /// of rows deleted.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn sweep_before(
        &self,
        cutoff: time::OffsetDateTime,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(r"DELETE FROM search_click_events WHERE clicked_at < $1")
            .bind(cutoff)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }
}

/// PG-gated integration tests:
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored search_feedback
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{MessageId, ParticipantId, WorkspaceId};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn records_clicks_and_aggregates_ctr_and_mrr() {
        let p = pool();
        let repo = SearchFeedbackRepo::new(p.clone());
        // A throwaway workspace id isolates this test's rows from any other.
        let ws = WorkspaceId::new();
        let me = ParticipantId::new();

        // Empty window: all-zero stats (COALESCEs hold).
        let empty = repo.ctr_stats(ws, 30).await.expect("empty stats");
        assert_eq!(empty.clicks, 0);
        assert!(empty.mean_reciprocal_rank.abs() < f64::EPSILON);

        // Three clicks on query "alpha" at ranks 0, 1, 3 and one on "beta" at 0.
        // MRR = mean(1/1, 1/2, 1/4, 1/1) = (1 + 0.5 + 0.25 + 1) / 4 = 0.6875.
        // top_result_ctr = 2 of 4 clicks at rank 0 = 0.5.
        for (q, rank) in [("alpha", 0), ("alpha", 1), ("alpha", 3), ("beta", 0)] {
            repo.record_click(me, ws, q, MessageId::new(), rank)
                .await
                .expect("record click");
        }

        let s = repo.ctr_stats(ws, 30).await.expect("stats");
        assert_eq!(s.clicks, 4, "four clicks recorded");
        assert_eq!(s.queries, 2, "two distinct queries");
        assert!(
            (s.mean_reciprocal_rank - 0.6875).abs() < 1e-9,
            "MRR = (1 + 0.5 + 0.25 + 1)/4 = 0.6875, got {}",
            s.mean_reciprocal_rank
        );
        assert!(
            (s.top_result_ctr - 0.5).abs() < 1e-9,
            "2 of 4 clicks at rank 0 ⇒ 0.5, got {}",
            s.top_result_ctr
        );

        // A negative rank is clamped to 0 on insert (defensive).
        repo.record_click(me, ws, "gamma", MessageId::new(), -5)
            .await
            .expect("record clamped");
        let clamped: i32 = sqlx::query_scalar(
            "SELECT result_rank FROM search_click_events WHERE workspace_id = $1 AND query_text = 'gamma'",
        )
        .bind(ws.to_uuid())
        .fetch_one(&p)
        .await
        .expect("fetch clamped rank");
        assert_eq!(clamped, 0, "negative rank clamped to 0");

        // Retention sweep removes everything older than a far-future cutoff.
        let swept = repo
            .sweep_before(time::OffsetDateTime::now_utc() + time::Duration::days(1))
            .await
            .expect("sweep");
        assert!(swept >= 5, "all 5 rows swept, got {swept}");

        // Cleanup (sweep already removed them, but be explicit/idempotent).
        sqlx::query("DELETE FROM search_click_events WHERE workspace_id = $1")
            .bind(ws.to_uuid())
            .execute(&p)
            .await
            .ok();
    }
}
