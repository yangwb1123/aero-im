//! `ai_jobs` repository — durable record of AI work (embed/summarize/moderate/answer).
//!
//! NATS `AI_QUEUE` is the live dispatch channel; this table is the source of truth
//! for retries, dead-letter, and operator visibility. Workers `claim()` a row to
//! flip status `queued → running`, then `complete()` or `fail()` it.

use aero_common::{Error, ParticipantId, WorkspaceId};
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

    /// Hard-delete terminal jobs (`done`/`failed`/`dead`) that finished before
    /// `cutoff` (data-lifecycle retention sweep, ROADMAP5 方向四). `ai_jobs` had
    /// no completion cleanup, so finished rows accumulated forever. Queued and
    /// running jobs are never swept. `COALESCE(finished_at, scheduled_at)` ages
    /// out even a terminal row that predates the `finished_at` column. Returns
    /// the number of rows deleted.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn sweep_terminal_before(
        &self,
        cutoff: time::OffsetDateTime,
    ) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(
            r"DELETE FROM ai_jobs
               WHERE status IN ('done','failed','dead')
                 AND COALESCE(finished_at, scheduled_at) < $1",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
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
            r"INSERT INTO ai_jobs (id, kind, target_id, workspace_id, status, payload, priority)
               VALUES ($1, $2, $3, $4, 'queued', $5, $6)",
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
            r"INSERT INTO ai_jobs (id, kind, target_id, workspace_id, status, payload, priority)
               SELECT $1, $2, $3, $4, 'queued', $5, $6
                WHERE NOT EXISTS (
                    SELECT 1 FROM ai_jobs
                     WHERE kind = $2 AND target_id = $3
                       AND status IN ('queued', 'running')
                )",
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
            r"UPDATE ai_jobs SET status = 'running', started_at = NOW(), attempts = attempts + 1
                WHERE id IN (
                    SELECT id FROM ai_jobs
                     WHERE status = 'queued'
                       AND scheduled_at <= NOW()
                     ORDER BY priority ASC, scheduled_at ASC
                     FOR UPDATE SKIP LOCKED
                     LIMIT $1
                )
                RETURNING id, kind, target_id, workspace_id, status, attempts, payload, result,
                          error, scheduled_at, started_at, finished_at",
        )
        .bind(n)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(AiJob::from).collect())
    }

    pub async fn complete(&self, id: Ulid, result: serde_json::Value) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"UPDATE ai_jobs
                  SET status = 'done', result = $2, finished_at = NOW()
                WHERE id = $1",
        )
        .bind(uuid::Uuid::from_u128(id.0))
        .bind(&result)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn fail(&self, id: Ulid, error: &str, max_attempts: i32) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"UPDATE ai_jobs SET
                 status = CASE WHEN attempts >= $3 THEN 'dead' ELSE 'queued' END,
                 error = $2,
                 finished_at = CASE WHEN attempts >= $3 THEN NOW() ELSE NULL END,
                 scheduled_at = CASE WHEN attempts >= $3
                                     THEN scheduled_at
                                     ELSE NOW() + (LEAST(attempts, 5) * INTERVAL '5 seconds')
                                END
               WHERE id = $1",
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
    pub async fn defer(&self, id: Ulid, until: time::OffsetDateTime) -> Result<(), sqlx::Error> {
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
            sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM ai_jobs WHERE status = 'dead'")
                .fetch_one(&self.pool)
                .await?
        };
        Ok(row.0)
    }

    /// Count dead-letter jobs owned by one workspace, optionally filtered by
    /// `kind` ("moderate", "summarize", etc.).
    ///
    /// Unlike [`Self::count_dead`], this is tenant-scoped and is the only count
    /// suitable for a workspace-facing API. Jobs whose `workspace_id` is `NULL`
    /// are global/legacy work and are intentionally excluded.
    pub async fn count_dead_for_workspace(
        &self,
        workspace: WorkspaceId,
        kind: Option<&str>,
    ) -> Result<i64, sqlx::Error> {
        let row = if let Some(kind) = kind {
            sqlx::query_as::<_, (i64,)>(
                r"SELECT COUNT(*)
                    FROM ai_jobs
                   WHERE status = 'dead'
                     AND workspace_id = $1
                     AND kind = $2",
            )
            .bind(workspace.to_uuid())
            .bind(kind)
            .fetch_one(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, (i64,)>(
                r"SELECT COUNT(*)
                    FROM ai_jobs
                   WHERE status = 'dead'
                     AND workspace_id = $1",
            )
            .bind(workspace.to_uuid())
            .fetch_one(&self.pool)
            .await?
        };
        Ok(row.0)
    }

    /// List the most-recently-failed dead-letter jobs owned by one workspace.
    ///
    /// The mandatory `workspace_id` predicate is the tenant boundary. Global
    /// jobs (`workspace_id IS NULL`) and jobs owned by another workspace are
    /// never returned.
    pub async fn list_dead_for_workspace(
        &self,
        workspace: WorkspaceId,
        limit: i64,
    ) -> Result<Vec<AiJob>, sqlx::Error> {
        let rows = sqlx::query_as::<_, AiJobRow>(
            r"SELECT id, kind, target_id, workspace_id, status, attempts, payload,
                     result, error, scheduled_at, started_at, finished_at
               FROM ai_jobs
              WHERE status = 'dead'
                AND workspace_id = $1
              ORDER BY finished_at DESC NULLS LAST, id DESC
              LIMIT $2",
        )
        .bind(workspace.to_uuid())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(AiJob::from).collect())
    }

    /// Re-queue one workspace-owned dead-letter job while `actor` remains an
    /// effective workspace Owner/Admin.
    ///
    /// The workspace governance row and the actor's effective authorization are
    /// locked and checked in the same transaction as the update. A concurrent
    /// demotion therefore either waits for this requeue to commit or commits
    /// first and makes this call fail with [`Error::Forbidden`]. The update also
    /// binds both `id` and `workspace_id`; another tenant's job and a global
    /// (`NULL` workspace) job are indistinguishable from a missing/dead-state
    /// mismatch and return [`Error::NotFound`].
    pub async fn requeue_for_workspace_authorized(
        &self,
        id: Ulid,
        workspace: WorkspaceId,
        actor: ParticipantId,
    ) -> Result<(), Error> {
        let now = time::OffsetDateTime::now_utc();
        let mut tx = self.pool.begin().await?;
        crate::workspace::authz::assert_effective_admin_in_tx(&mut tx, workspace, actor).await?;
        let result = sqlx::query(
            r"UPDATE ai_jobs
                 SET status = 'queued', attempts = 0,
                     scheduled_at = $2, started_at = NULL, finished_at = NULL, error = NULL
               WHERE id = $1
                 AND workspace_id = $3
                 AND status = 'dead'",
        )
        .bind(uuid::Uuid::from_u128(id.0))
        .bind(now)
        .bind(workspace.to_uuid())
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Err(Error::NotFound(format!(
                "dead AI job {id} in workspace {workspace}"
            )));
        }
        tx.commit().await?;
        Ok(())
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
    use std::time::Duration;

    use aero_common::WorkspaceRole;
    use sqlx::postgres::PgPoolOptions;

    use crate::WorkspaceRepo;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        PgPoolOptions::new()
            .max_connections(4)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn participant(pool: &PgPool, label: &str) -> ParticipantId {
        let participant = ParticipantId::new();
        sqlx::query(
            "INSERT INTO participants (id, kind, display_name)
             VALUES ($1, 'human', $2)",
        )
        .bind(participant.to_uuid())
        .bind(format!("{label}-{participant}"))
        .execute(pool)
        .await
        .expect("insert participant");
        participant
    }

    async fn workspace_with_admin(
        pool: &PgPool,
        label: &str,
    ) -> (WorkspaceId, ParticipantId, ParticipantId) {
        let owner = participant(pool, &format!("{label}-owner")).await;
        let admin = participant(pool, &format!("{label}-admin")).await;
        let workspaces = WorkspaceRepo::new(pool.clone());
        let workspace = workspaces
            .create(
                format!("AI DLQ {label} {owner}"),
                format!("ai-dlq-{label}-{owner}"),
                owner,
            )
            .await
            .expect("create workspace")
            .id;
        workspaces
            .add_member(workspace, admin, WorkspaceRole::Admin)
            .await
            .expect("add workspace admin");
        (workspace, owner, admin)
    }

    async fn insert_dead_job(
        pool: &PgPool,
        workspace: Option<WorkspaceId>,
        kind: &str,
        error: &str,
    ) -> Ulid {
        let id = Ulid::new();
        sqlx::query(
            r"INSERT INTO ai_jobs
                  (id, kind, workspace_id, status, attempts, payload, error, finished_at)
               VALUES ($1, $2, $3, 'dead', 5, '{}'::jsonb, $4, now())",
        )
        .bind(uuid::Uuid::from_u128(id.0))
        .bind(kind)
        .bind(workspace.map(|id| id.to_uuid()))
        .bind(error)
        .execute(pool)
        .await
        .expect("insert dead AI job");
        id
    }

    async fn job_status(pool: &PgPool, id: Ulid) -> String {
        sqlx::query_scalar("SELECT status FROM ai_jobs WHERE id = $1")
            .bind(uuid::Uuid::from_u128(id.0))
            .fetch_one(pool)
            .await
            .expect("read AI job status")
    }

    async fn cleanup(
        pool: &PgPool,
        jobs: &[Ulid],
        workspaces: &[WorkspaceId],
        participants: &[ParticipantId],
    ) {
        let jobs = jobs
            .iter()
            .map(|id| uuid::Uuid::from_u128(id.0))
            .collect::<Vec<_>>();
        sqlx::query("DELETE FROM ai_jobs WHERE id = ANY($1)")
            .bind(&jobs)
            .execute(pool)
            .await
            .ok();
        let workspaces = workspaces
            .iter()
            .map(aero_common::WorkspaceId::to_uuid)
            .collect::<Vec<_>>();
        sqlx::query("DELETE FROM workspaces WHERE id = ANY($1)")
            .bind(&workspaces)
            .execute(pool)
            .await
            .ok();
        let participants = participants
            .iter()
            .map(aero_common::ParticipantId::to_uuid)
            .collect::<Vec<_>>();
        sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
            .bind(&participants)
            .execute(pool)
            .await
            .ok();
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

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn workspace_dlq_scope_excludes_other_tenants_and_global_jobs() {
        let p = pool();
        let repo = AiJobRepo::new(p.clone());
        let (workspace_a, owner_a, admin_a) = workspace_with_admin(&p, "scope-a").await;
        let (workspace_b, owner_b, admin_b) = workspace_with_admin(&p, "scope-b").await;
        let own = insert_dead_job(&p, Some(workspace_a), "moderate", "tenant-a-secret").await;
        let foreign = insert_dead_job(&p, Some(workspace_b), "answer", "tenant-b-secret").await;
        let global = insert_dead_job(&p, None, "summarize", "global-secret").await;

        let visible = repo
            .list_dead_for_workspace(workspace_a, 50)
            .await
            .expect("list tenant A DLQ");
        assert_eq!(
            visible.iter().map(|job| job.id).collect::<Vec<_>>(),
            vec![own],
            "tenant listing must exclude foreign and NULL-workspace jobs"
        );
        assert_eq!(
            repo.count_dead_for_workspace(workspace_a, None)
                .await
                .expect("count tenant A DLQ"),
            1
        );
        assert_eq!(
            repo.count_dead_for_workspace(workspace_a, Some("moderate"))
                .await
                .expect("count tenant A moderation DLQ"),
            1
        );
        assert_eq!(
            repo.count_dead_for_workspace(workspace_a, Some("answer"))
                .await
                .expect("count tenant A answer DLQ"),
            0
        );

        assert!(matches!(
            repo.requeue_for_workspace_authorized(foreign, workspace_a, admin_a)
                .await
                .expect_err("another tenant's job must be opaque"),
            Error::NotFound(_)
        ));
        assert!(matches!(
            repo.requeue_for_workspace_authorized(global, workspace_a, admin_a)
                .await
                .expect_err("a global job must be opaque to workspace admins"),
            Error::NotFound(_)
        ));
        assert_eq!(job_status(&p, foreign).await, "dead");
        assert_eq!(job_status(&p, global).await, "dead");

        repo.requeue_for_workspace_authorized(own, workspace_a, admin_a)
            .await
            .expect("current admin requeues own workspace job");
        assert_eq!(job_status(&p, own).await, "queued");

        cleanup(
            &p,
            &[own, foreign, global],
            &[workspace_a, workspace_b],
            &[owner_a, admin_a, owner_b, admin_b],
        )
        .await;
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn workspace_dlq_requeue_observes_concurrent_admin_demotion() {
        let p = pool();
        let repo = AiJobRepo::new(p.clone());
        let (workspace, owner, admin) = workspace_with_admin(&p, "demotion").await;
        let job = insert_dead_job(&p, Some(workspace), "answer", "must remain dead").await;

        let mut demotion = p.begin().await.expect("begin admin demotion");
        crate::ownership::lock_membership_governance(&mut demotion)
            .await
            .expect("lock membership governance");
        sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .fetch_one(&mut *demotion)
            .await
            .expect("lock workspace before demotion");
        sqlx::query(
            "UPDATE workspace_members
                SET role = 'member'
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(admin.to_uuid())
        .execute(&mut *demotion)
        .await
        .expect("stage admin demotion");

        let contender_repo = repo.clone();
        let mut contender = tokio::spawn(async move {
            contender_repo
                .requeue_for_workspace_authorized(job, workspace, admin)
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut contender)
                .await
                .is_err(),
            "requeue must wait behind the workspace governance lock"
        );

        demotion.commit().await.expect("commit admin demotion");
        assert!(matches!(
            contender
                .await
                .expect("join requeue contender")
                .expect_err("committed demotion must revoke requeue authority"),
            Error::Forbidden(_)
        ));
        assert_eq!(
            job_status(&p, job).await,
            "dead",
            "forbidden contender must not mutate the job"
        );

        cleanup(&p, &[job], &[workspace], &[owner, admin]).await;
    }
}
