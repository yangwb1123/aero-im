//! `ai_jobs` repository — durable record of AI work (embed/summarize/moderate/answer).
//!
//! NATS `AI_QUEUE` is the live dispatch channel; this table is the source of truth
//! for retries, dead-letter, and operator visibility. Workers `claim()` a row to
//! flip status `queued → running`, then `complete()` or `fail()` it.

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use ulid::Ulid;

#[derive(Clone)]
pub struct AiJobRepo {
    pool: PgPool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiJobKind {
    Embed,
    Summarize,
    Moderate,
    Answer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiJobStatus {
    Queued,
    Running,
    Done,
    Failed,
    Dead,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiJob {
    pub id: Ulid,
    pub kind: AiJobKind,
    pub target_id: Option<uuid::Uuid>,
    /// Owning workspace (tenant), for per-tenant cost budgeting. `None` when it
    /// couldn't be resolved at enqueue time — such jobs are billed globally only.
    pub workspace_id: Option<uuid::Uuid>,
    pub status: AiJobStatus,
    pub attempts: i32,
    pub payload: serde_json::Value,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub scheduled_at: time::OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub started_at: Option<time::OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub finished_at: Option<time::OffsetDateTime>,
}

/// Queue priority for a job kind — **lower runs first**. Moderation gates message
/// visibility and `answer` has a user waiting, so they preempt the best-effort
/// `summarize`/`embed` backfill that floods the queue every tick. Pure, so the
/// lane ordering is unit-tested without a DB.
#[must_use]
pub fn priority_for(kind: AiJobKind) -> i16 {
    match kind {
        AiJobKind::Moderate => 10,
        AiJobKind::Answer => 20,
        AiJobKind::Summarize => 50,
        AiJobKind::Embed => 100,
    }
}

impl AiJobRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Enqueue a new job. Returns the row id so callers can publish a wakeup on NATS.
    pub async fn enqueue(
        &self,
        kind: AiJobKind,
        target_id: Option<uuid::Uuid>,
        workspace_id: Option<uuid::Uuid>,
        payload: serde_json::Value,
    ) -> Result<Ulid, sqlx::Error> {
        let id = Ulid::new();
        let kind_s = match kind {
            AiJobKind::Embed => "embed",
            AiJobKind::Summarize => "summarize",
            AiJobKind::Moderate => "moderate",
            AiJobKind::Answer => "answer",
        };
        sqlx::query(
            r#"INSERT INTO ai_jobs (id, kind, target_id, workspace_id, status, payload, priority)
               VALUES ($1, $2, $3, $4, 'queued', $5, $6)"#,
        )
        .bind(uuid::Uuid::from_u128(id.0))
        .bind(kind_s)
        .bind(target_id)
        .bind(workspace_id)
        .bind(&payload)
        .bind(priority_for(kind))
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Enqueue a job for `target_id` only if no `queued`/`running` job of the same
    /// `kind` already targets it (ROADMAP 第三版 方向三 — embedding backfill). The
    /// periodic backfill loop re-scans embedding-less messages every tick; without
    /// this guard a message would accrue a duplicate Embed job each tick until the
    /// worker caught up. Returns `Some(id)` when inserted, `None` when skipped.
    pub async fn enqueue_unique(
        &self,
        kind: AiJobKind,
        target_id: uuid::Uuid,
        workspace_id: Option<uuid::Uuid>,
        payload: serde_json::Value,
    ) -> Result<Option<Ulid>, sqlx::Error> {
        let id = Ulid::new();
        let kind_s = match kind {
            AiJobKind::Embed => "embed",
            AiJobKind::Summarize => "summarize",
            AiJobKind::Moderate => "moderate",
            AiJobKind::Answer => "answer",
        };
        let res = sqlx::query(
            r#"INSERT INTO ai_jobs (id, kind, target_id, workspace_id, status, payload, priority)
               SELECT $1, $2, $3, $4, 'queued', $5, $6
                WHERE NOT EXISTS (
                    SELECT 1 FROM ai_jobs
                     WHERE kind = $2 AND target_id = $3
                       AND status IN ('queued', 'running')
                )"#,
        )
        .bind(uuid::Uuid::from_u128(id.0))
        .bind(kind_s)
        .bind(target_id)
        .bind(workspace_id)
        .bind(&payload)
        .bind(priority_for(kind))
        .execute(&self.pool)
        .await?;
        Ok((res.rows_affected() > 0).then_some(id))
    }

    /// Atomically claim up to `n` ready jobs and flip them to `running`.
    /// Skips locked rows so multiple workers can run concurrently.
    pub async fn claim(&self, n: i32) -> Result<Vec<AiJob>, sqlx::Error> {
        let n = n.clamp(1, 64);
        let rows = sqlx::query_as::<_, AiJobRow>(
            r#"UPDATE ai_jobs SET status = 'running', started_at = NOW(), attempts = attempts + 1
                WHERE id IN (
                    SELECT id FROM ai_jobs
                     WHERE status = 'queued'
                       AND scheduled_at <= NOW()
                     ORDER BY priority ASC, scheduled_at ASC
                     FOR UPDATE SKIP LOCKED
                     LIMIT $1
                )
                RETURNING id, kind, target_id, workspace_id, status, attempts, payload, result,
                          error, scheduled_at, started_at, finished_at"#,
        )
        .bind(n)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(AiJob::from).collect())
    }

    pub async fn complete(
        &self,
        id: Ulid,
        result: serde_json::Value,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"UPDATE ai_jobs
                  SET status = 'done', result = $2, finished_at = NOW()
                WHERE id = $1"#,
        )
        .bind(uuid::Uuid::from_u128(id.0))
        .bind(&result)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn fail(
        &self,
        id: Ulid,
        error: &str,
        max_attempts: i32,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"UPDATE ai_jobs SET
                 status = CASE WHEN attempts >= $3 THEN 'dead' ELSE 'queued' END,
                 error = $2,
                 finished_at = CASE WHEN attempts >= $3 THEN NOW() ELSE NULL END,
                 scheduled_at = CASE WHEN attempts >= $3
                                     THEN scheduled_at
                                     ELSE NOW() + (LEAST(attempts, 5) * INTERVAL '5 seconds')
                                END
               WHERE id = $1"#,
        )
        .bind(uuid::Uuid::from_u128(id.0))
        .bind(error)
        .bind(max_attempts)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Return a claimed job to the queue, scheduled no earlier than `until`,
    /// WITHOUT consuming a retry attempt (a deferral is not a failure). Used by
    /// the worker to push back a job whose workspace has exhausted its per-tenant
    /// budget window, so the work is delayed rather than dropped.
    pub async fn defer(
        &self,
        id: Ulid,
        until: time::OffsetDateTime,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"UPDATE ai_jobs
                  SET status = 'queued',
                      scheduled_at = $2,
                      started_at = NULL,
                      attempts = GREATEST(attempts - 1, 0)
                WHERE id = $1",
        )
        .bind(uuid::Uuid::from_u128(id.0))
        .bind(until)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Count dead-letter jobs, optionally filtered by `kind` ("moderate",
    /// "summarize", etc.). Used for the `aero_ai_dlq_size` gauge.
    pub async fn count_dead(&self, kind: Option<&str>) -> Result<i64, sqlx::Error> {
        let row = if let Some(k) = kind {
            sqlx::query_as::<_, (i64,)>(
                "SELECT COUNT(*) FROM ai_jobs WHERE status = 'dead' AND kind = $1",
            )
            .bind(k)
            .fetch_one(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, (i64,)>(
                "SELECT COUNT(*) FROM ai_jobs WHERE status = 'dead'",
            )
            .fetch_one(&self.pool)
            .await?
        };
        Ok(row.0)
    }

    /// List the most-recently-failed dead-letter jobs, newest first. Capped at
    /// `limit` (caller should pass a reasonable ceiling like 50).
    pub async fn list_dead(&self, limit: i64) -> Result<Vec<AiJob>, sqlx::Error> {
        let rows = sqlx::query_as::<_, AiJobRow>(
            r"SELECT id, kind, target_id, workspace_id, status, attempts, payload,
                     result, error, scheduled_at, started_at, finished_at
               FROM ai_jobs
              WHERE status = 'dead'
              ORDER BY finished_at DESC NULLS LAST, id DESC
              LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(AiJob::from).collect())
    }

    /// Re-queue a dead-letter job: reset `status = 'queued'` and clear the
    /// retry counter so it gets a fresh set of attempts. Returns `true` when
    /// the row existed and was in `dead` state, `false` otherwise.
    pub async fn requeue(&self, id: Ulid) -> Result<bool, sqlx::Error> {
        let now = time::OffsetDateTime::now_utc();
        let rows = sqlx::query(
            r"UPDATE ai_jobs
                 SET status = 'queued', attempts = 0,
                     scheduled_at = $2, started_at = NULL, finished_at = NULL, error = NULL
               WHERE id = $1 AND status = 'dead'",
        )
        .bind(uuid::Uuid::from_u128(id.0))
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(rows.rows_affected() > 0)
    }
}

#[derive(sqlx::FromRow)]
struct AiJobRow {
    id: uuid::Uuid,
    kind: String,
    target_id: Option<uuid::Uuid>,
    workspace_id: Option<uuid::Uuid>,
    status: String,
    attempts: i32,
    payload: serde_json::Value,
    result: Option<serde_json::Value>,
    error: Option<String>,
    scheduled_at: time::OffsetDateTime,
    started_at: Option<time::OffsetDateTime>,
    finished_at: Option<time::OffsetDateTime>,
}

impl From<AiJobRow> for AiJob {
    fn from(r: AiJobRow) -> Self {
        let kind = match r.kind.as_str() {
            "summarize" => AiJobKind::Summarize,
            "moderate" => AiJobKind::Moderate,
            "answer" => AiJobKind::Answer,
            _ => AiJobKind::Embed,
        };
        let status = match r.status.as_str() {
            "running" => AiJobStatus::Running,
            "done" => AiJobStatus::Done,
            "failed" => AiJobStatus::Failed,
            "dead" => AiJobStatus::Dead,
            _ => AiJobStatus::Queued,
        };
        Self {
            id: Ulid(r.id.as_u128()),
            kind,
            target_id: r.target_id,
            workspace_id: r.workspace_id,
            status,
            attempts: r.attempts,
            payload: r.payload,
            result: r.result,
            error: r.error,
            scheduled_at: r.scheduled_at,
            started_at: r.started_at,
            finished_at: r.finished_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_lanes_order_user_facing_work_ahead_of_backfill() {
        // Lower = runs first. Moderation gates visibility, answer has a user
        // waiting; summarize then embed are best-effort backfill.
        assert!(priority_for(AiJobKind::Moderate) < priority_for(AiJobKind::Answer));
        assert!(priority_for(AiJobKind::Answer) < priority_for(AiJobKind::Summarize));
        assert!(priority_for(AiJobKind::Summarize) < priority_for(AiJobKind::Embed));
        // The schema default (100) matches the lowest lane, so an un-stamped row
        // sorts with embed backfill rather than ahead of moderation.
        assert_eq!(priority_for(AiJobKind::Embed), 100);
    }
}

/// PG-gated integration tests:
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored ai_job
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Priority preempts FIFO: an `embed` (backfill) enqueued FIRST — so with an
    /// earlier `scheduled_at` — is still claimed AFTER a `moderate` enqueued later,
    /// because moderation owns a higher-priority lane. A unique `workspace_id` tags
    /// our two rows so unrelated queued jobs in the shared DB don't perturb the
    /// assertion (claim(1) always takes the global min, and our moderate(10) <
    /// embed(100), so ours are claimed moderate-then-embed regardless of the rest).
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn claim_prefers_higher_priority_lane_over_fifo() {
        let p = pool();
        let repo = AiJobRepo::new(p.clone());
        let tag = uuid::Uuid::new_v4();

        let embed = repo
            .enqueue(AiJobKind::Embed, None, Some(tag), serde_json::json!({}))
            .await
            .expect("enqueue embed");
        let moderate = repo
            .enqueue(AiJobKind::Moderate, None, Some(tag), serde_json::json!({}))
            .await
            .expect("enqueue moderate");

        let mut mine = Vec::new();
        for _ in 0..1000 {
            let batch = repo.claim(1).await.expect("claim");
            if batch.is_empty() {
                break;
            }
            for j in batch {
                if j.workspace_id == Some(tag) {
                    mine.push(j.id);
                }
            }
            if mine.len() == 2 {
                break;
            }
        }
        assert_eq!(
            mine,
            vec![moderate, embed],
            "moderate (higher-priority lane) is claimed before the earlier-enqueued embed"
        );
    }
}
