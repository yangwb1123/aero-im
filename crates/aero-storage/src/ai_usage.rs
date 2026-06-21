//! Per-tenant AI usage ledger (ROADMAP 第六版 · 方向一·2).
//!
//! Durable, queryable record of paid AI spend per workspace — the billing /
//! usage-attribution foundation that the Prometheus `AI_COST_MICROS_TOTAL` counter
//! cannot be (counters are aggregate + scrape-window + non-historical). Rows are
//! batch-written by a boot drain task off the charge hot path (see
//! `aero_ai::metrics::charge_cost`, the single convergence point of BOTH the
//! ai_jobs worker queue AND the real-time moderation bot). Backed by
//! `migrations/0151_ai_usage_ledger.sql`.

use sqlx::PgPool;
use uuid::Uuid;

/// One paid-charge row to persist. `workspace_id` is the job's workspace (`None`
/// for legacy/system jobs); `kind` is the per-kind label (embed/summarize/
/// moderate/answer); `cost_micros` is the real (token-based) or estimated micros.
#[derive(Debug, Clone)]
pub struct UsageRow {
    pub workspace_id: Option<Uuid>,
    pub kind: String,
    pub cost_micros: i64,
}

/// A per-kind usage rollup for the summary endpoint.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UsageByKind {
    pub kind: String,
    pub calls: i64,
    pub cost_micros: i64,
}

/// Ledger repo over the shared pool.
#[derive(Clone)]
pub struct AiUsageRepo {
    pool: PgPool,
}

impl AiUsageRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Batch-insert paid-charge rows in one multi-row INSERT (parallel `UNNEST`
    /// arrays). An empty slice is a no-op. The drain task coalesces a burst of
    /// charges into one call so the hot path never blocks on a per-charge round
    /// trip.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn insert_batch(&self, rows: &[UsageRow]) -> Result<u64, sqlx::Error> {
        if rows.is_empty() {
            return Ok(0);
        }
        let ws: Vec<Option<Uuid>> = rows.iter().map(|r| r.workspace_id).collect();
        let kinds: Vec<String> = rows.iter().map(|r| r.kind.clone()).collect();
        let micros: Vec<i64> = rows.iter().map(|r| r.cost_micros).collect();
        let res = sqlx::query(
            r"INSERT INTO ai_usage_ledger (workspace_id, kind, cost_micros)
              SELECT * FROM UNNEST($1::uuid[], $2::text[], $3::bigint[])",
        )
        .bind(&ws)
        .bind(&kinds)
        .bind(&micros)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Per-kind usage rollup for a workspace since `since`, costliest kind first.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn summary_since(
        &self,
        workspace: Uuid,
        since: time::OffsetDateTime,
    ) -> Result<Vec<UsageByKind>, sqlx::Error> {
        let rows = sqlx::query_as::<_, (String, i64, i64)>(
            r"SELECT kind, COUNT(*)::bigint, COALESCE(SUM(cost_micros), 0)::bigint
              FROM ai_usage_ledger
              WHERE workspace_id = $1 AND created_at >= $2
              GROUP BY kind
              ORDER BY 3 DESC, kind ASC",
        )
        .bind(workspace)
        .bind(since)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(kind, calls, cost_micros)| UsageByKind { kind, calls, cost_micros })
            .collect())
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;

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
    async fn batch_insert_then_summary_rolls_up_per_kind() {
        let p = pool();
        let repo = AiUsageRepo::new(p.clone());
        let ws = Uuid::new_v4();
        let other = Uuid::new_v4();

        repo.insert_batch(&[
            UsageRow { workspace_id: Some(ws), kind: "answer".into(), cost_micros: 4500 },
            UsageRow { workspace_id: Some(ws), kind: "answer".into(), cost_micros: 1500 },
            UsageRow { workspace_id: Some(ws), kind: "embed".into(), cost_micros: 100 },
            // A different workspace's row must NOT leak into ws's summary.
            UsageRow { workspace_id: Some(other), kind: "answer".into(), cost_micros: 9000 },
            // A NULL-workspace (system) row is excluded from a per-workspace summary.
            UsageRow { workspace_id: None, kind: "moderate".into(), cost_micros: 200 },
        ])
        .await
        .expect("insert_batch");

        let epoch = time::OffsetDateTime::UNIX_EPOCH;
        let sum = repo.summary_since(ws, epoch).await.expect("summary");
        // answer (4500+1500=6000, 2 calls) then embed (100, 1 call); ordered by cost.
        assert_eq!(sum.len(), 2, "only ws's two kinds: {sum:?}");
        assert_eq!(sum[0].kind, "answer");
        assert_eq!(sum[0].calls, 2);
        assert_eq!(sum[0].cost_micros, 6000);
        assert_eq!(sum[1].kind, "embed");
        assert_eq!(sum[1].cost_micros, 100);
        assert!(!sum.iter().any(|u| u.kind == "moderate"), "NULL-ws row excluded");

        // An empty batch is a harmless no-op.
        assert_eq!(repo.insert_batch(&[]).await.expect("empty"), 0);
    }
}
