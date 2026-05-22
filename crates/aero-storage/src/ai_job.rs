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

impl AiJobRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Enqueue a new job. Returns the row id so callers can publish a wakeup on NATS.
    pub async fn enqueue(
        &self,
        kind: AiJobKind,
        target_id: Option<uuid::Uuid>,
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
            r#"INSERT INTO ai_jobs (id, kind, target_id, status, payload)
               VALUES ($1, $2, $3, 'queued', $4)"#,
        )
        .bind(uuid::Uuid::from_u128(id.0))
        .bind(kind_s)
        .bind(target_id)
        .bind(&payload)
        .execute(&self.pool)
        .await?;
        Ok(id)
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
                     ORDER BY scheduled_at ASC
                     FOR UPDATE SKIP LOCKED
                     LIMIT $1
                )
                RETURNING id, kind, target_id, status, attempts, payload, result, error,
                          scheduled_at, started_at, finished_at"#,
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
}

#[derive(sqlx::FromRow)]
struct AiJobRow {
    id: uuid::Uuid,
    kind: String,
    target_id: Option<uuid::Uuid>,
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
