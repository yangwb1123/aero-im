//! Per-tenant usage report — admin observability of one workspace's resource
//! consumption (operability plane).
//!
//! Read-only aggregate counts for a single tenant, scoped via `rooms.workspace_id`
//! / `ai_jobs.workspace_id` / `workspace_members`. There is **no** new table and
//! **no** new id — every figure is a parameterized aggregate SELECT over existing
//! tables, mirroring [`crate::analytics`] but oriented at *resource consumption*
//! (messages, AI spend, storage, membership) rather than activity dashboards.
//!
//! Counts that distinguish live vs. tombstoned content filter `deleted_at IS NULL`
//! (matching [`crate::MessageRepo`]). The per-kind AI breakdown and token totals
//! read `ai_jobs.result.usage.{input,output}_tokens` (the shape the AI worker
//! writes — see `aero-ai`'s worker), summing only what is present.
//!
//! Blobs have no `workspace_id` column; a blob is attributed to a workspace when a
//! (non-deleted) message in one of the workspace's rooms references it via a
//! `File`/`Voice` block (`blocks @> [{"blob_id": "<ulid>"}]`), the same linkage
//! [`crate::BlobRepo::is_accessible_by`] uses. Distinct blobs are counted (a blob
//! shared into N messages is one stored object).
//!
//! Purely additive: a NEW [`UsageReportRepo`]; no existing repo is touched.

use aero_common::WorkspaceId;
use serde::Serialize;
use sqlx::PgPool;

/// One per-kind AI-job line: the job kind, how many such jobs ran, and the total
/// input/output tokens summed from each job's `result.usage`.
#[derive(Debug, Clone, Serialize)]
pub struct AiKindUsage {
    /// The AI job kind (`embed` | `summarize` | `moderate` | `answer`).
    pub kind: String,
    /// Number of jobs of this kind for the workspace.
    pub jobs: i64,
    /// Sum of `result.usage.input_tokens` over this kind's jobs (0 when absent).
    pub input_tokens: i64,
    /// Sum of `result.usage.output_tokens` over this kind's jobs (0 when absent).
    pub output_tokens: i64,
}

/// A complete per-tenant usage report — the headline resource-consumption numbers
/// for one workspace.
#[derive(Debug, Clone, Serialize)]
pub struct UsageReport {
    /// All non-deleted messages in the workspace's rooms.
    pub total_messages: i64,
    /// Non-deleted messages created in the trailing 30 days.
    pub messages_last_30d: i64,
    /// AI jobs tagged to this workspace, broken down by kind (with token sums).
    pub ai_jobs_by_kind: Vec<AiKindUsage>,
    /// Total AI jobs across all kinds (the sum of `ai_jobs_by_kind[*].jobs`).
    pub ai_jobs_total: i64,
    /// Total input tokens across all of the workspace's AI jobs.
    pub ai_input_tokens: i64,
    /// Total output tokens across all of the workspace's AI jobs.
    pub ai_output_tokens: i64,
    /// Distinct blobs referenced by messages in the workspace's rooms.
    pub blob_count: i64,
    /// Total bytes of those distinct blobs.
    pub blob_bytes: i64,
    /// Distinct members enrolled in the workspace.
    pub member_count: i64,
    /// Distinct members who authored a message in the trailing 30 days.
    pub active_members_30d: i64,
}

/// Repository of read-only per-tenant usage aggregates. Cheap to clone (wraps a
/// [`PgPool`]), so feature modules build one inline via [`UsageReportRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct UsageReportRepo {
    pool: PgPool,
}

impl UsageReportRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Assemble the full usage report for `workspace`. A handful of scoped
    /// aggregate queries (messages, AI by kind, blobs, members); each is bounded
    /// to the tenant so one workspace's report can never include another's data.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the underlying queries.
    pub async fn report(&self, workspace: WorkspaceId) -> Result<UsageReport, sqlx::Error> {
        let ws = workspace.to_uuid();

        // ---- Messages (total + trailing 30d) ----
        let (total_messages, messages_last_30d) = sqlx::query_as::<_, (i64, i64)>(
            r"SELECT
                (SELECT COUNT(*) FROM messages m
                   JOIN rooms r ON r.id = m.room_id
                  WHERE r.workspace_id = $1 AND m.deleted_at IS NULL) AS total,
                (SELECT COUNT(*) FROM messages m
                   JOIN rooms r ON r.id = m.room_id
                  WHERE r.workspace_id = $1 AND m.deleted_at IS NULL
                    AND m.created_at >= now() - make_interval(days => 30)) AS last_30d",
        )
        .bind(ws)
        .fetch_one(&self.pool)
        .await?;

        // ---- AI jobs by kind, with token sums from result.usage ----
        // `result.usage.{input,output}_tokens` is the shape the AI worker writes.
        // A missing/null path coalesces to 0, so kinds without token accounting
        // (e.g. embed) contribute jobs but zero tokens.
        let ai_rows = sqlx::query_as::<_, (String, i64, i64, i64)>(
            r"SELECT kind,
                     COUNT(*) AS jobs,
                     COALESCE(SUM((result #>> '{usage,input_tokens}')::bigint), 0)::bigint AS input_tokens,
                     COALESCE(SUM((result #>> '{usage,output_tokens}')::bigint), 0)::bigint AS output_tokens
               FROM ai_jobs
              WHERE workspace_id = $1
              GROUP BY kind
              ORDER BY COUNT(*) DESC, kind ASC",
        )
        .bind(ws)
        .fetch_all(&self.pool)
        .await?;
        let ai_jobs_by_kind: Vec<AiKindUsage> = ai_rows
            .into_iter()
            .map(|(kind, jobs, input_tokens, output_tokens)| AiKindUsage {
                kind,
                jobs,
                input_tokens,
                output_tokens,
            })
            .collect();
        let ai_jobs_total = ai_jobs_by_kind.iter().map(|k| k.jobs).sum();
        let ai_input_tokens = ai_jobs_by_kind.iter().map(|k| k.input_tokens).sum();
        let ai_output_tokens = ai_jobs_by_kind.iter().map(|k| k.output_tokens).sum();

        // ---- Blobs referenced by messages in the workspace's rooms ----
        // A blob is attributed when a non-deleted message in a workspace room
        // references it via a File/Voice block. The `blob_id` in a block is the
        // BlobId's ULID string (NOT the `blobs.id` uuid), so collect the distinct
        // texts, parse them to typed ids in Rust (skipping malformed), then
        // aggregate the matching blobs once — mirroring file_index's text→BlobId
        // parse rather than a SQL `::uuid` cast (which would reject ULID strings).
        let blob_id_texts: Vec<String> = sqlx::query_scalar(
            r"SELECT DISTINCT (blk ->> 'blob_id')
                FROM messages m
                JOIN rooms r ON r.id = m.room_id
                CROSS JOIN LATERAL jsonb_array_elements(m.blocks) AS blk
               WHERE r.workspace_id = $1
                 AND m.deleted_at IS NULL
                 AND blk ? 'blob_id'",
        )
        .bind(ws)
        .fetch_all(&self.pool)
        .await?;
        let blob_uuids: Vec<uuid::Uuid> = blob_id_texts
            .iter()
            .filter_map(|s| {
                <aero_common::BlobId as std::str::FromStr>::from_str(s)
                    .ok()
                    .map(|b| b.to_uuid())
            })
            .collect();
        let (blob_count, blob_bytes) = if blob_uuids.is_empty() {
            (0_i64, 0_i64)
        } else {
            sqlx::query_as::<_, (i64, i64)>(
                r"SELECT COUNT(*) AS cnt, COALESCE(SUM(size), 0)::bigint AS bytes
                    FROM blobs WHERE id = ANY($1)",
            )
            .bind(&blob_uuids)
            .fetch_one(&self.pool)
            .await?
        };

        // ---- Members (total + active 30d) ----
        let (member_count, active_members_30d) = sqlx::query_as::<_, (i64, i64)>(
            r"SELECT
                (SELECT COUNT(DISTINCT participant_id) FROM workspace_members
                  WHERE workspace_id = $1) AS members,
                (SELECT COUNT(DISTINCT m.sender_id) FROM messages m
                   JOIN rooms r ON r.id = m.room_id
                  WHERE r.workspace_id = $1 AND m.deleted_at IS NULL
                    AND m.created_at >= now() - make_interval(days => 30)) AS active_30d",
        )
        .bind(ws)
        .fetch_one(&self.pool)
        .await?;

        Ok(UsageReport {
            total_messages,
            messages_last_30d,
            ai_jobs_by_kind,
            ai_jobs_total,
            ai_input_tokens,
            ai_output_tokens,
            blob_count,
            blob_bytes,
            member_count,
            active_members_30d,
        })
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored usage_report_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::{BlobId, ParticipantId, RoomId, WorkspaceRole};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("usage-actor-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    async fn workspace(p: &PgPool, owner: ParticipantId) -> WorkspaceId {
        let ws = WorkspaceId::new();
        sqlx::query("INSERT INTO workspaces (id, name, slug, created_by, created_at) VALUES ($1,$2,$3,$4, now())")
            .bind(ws.to_uuid())
            .bind("Usage Test WS")
            .bind(format!("usage-{ws}"))
            .bind(owner.to_uuid())
            .execute(p)
            .await
            .expect("insert workspace");
        sqlx::query("INSERT INTO workspace_members (workspace_id, participant_id, role, joined_at) VALUES ($1,$2,$3, now())")
            .bind(ws.to_uuid())
            .bind(owner.to_uuid())
            .bind(WorkspaceRole::Owner.as_str())
            .execute(p)
            .await
            .expect("insert workspace member");
        ws
    }

    async fn room_in(p: &PgPool, ws: WorkspaceId, creator: ParticipantId) -> RoomId {
        let id = RoomId::new();
        sqlx::query("INSERT INTO rooms (id, kind, name, created_by, created_at, workspace_id) VALUES ($1,'channel',$2,$3, now(), $4)")
            .bind(id.to_uuid())
            .bind(format!("usage-room-{id}"))
            .bind(creator.to_uuid())
            .bind(ws.to_uuid())
            .execute(p)
            .await
            .expect("insert room");
        id
    }

    /// Insert a message in `room`; if `blob` is given, attach a File block that
    /// references it (so the blob is attributed to the workspace).
    async fn insert_message(p: &PgPool, room: RoomId, sender: ParticipantId, blob: Option<BlobId>) {
        let id = aero_common::MessageId::new();
        let blocks = match blob {
            Some(b) => serde_json::json!([{ "type": "file", "blob_id": b.to_string(), "kind": "image", "name": "f.png", "size": 10 }]),
            None => serde_json::json!([{ "type": "text", "text": "usage body" }]),
        };
        sqlx::query("INSERT INTO messages (id, room_id, sender_id, blocks, searchable_text, created_at) VALUES ($1,$2,$3,$4,$5, now())")
            .bind(id.to_uuid())
            .bind(room.to_uuid())
            .bind(sender.to_uuid())
            .bind(blocks)
            .bind("usage body")
            .execute(p)
            .await
            .expect("insert message");
    }

    async fn insert_blob(p: &PgPool, owner: ParticipantId, size: i64) -> BlobId {
        let id = BlobId::new();
        sqlx::query("INSERT INTO blobs (id, owner_id, kind, name, mime, size, storage_key, created_at) VALUES ($1,$2,'image',$3,'image/png',$4,$5, now())")
            .bind(id.to_uuid())
            .bind(owner.to_uuid())
            .bind(format!("blob-{id}.png"))
            .bind(size)
            .bind(format!("key/{id}"))
            .execute(p)
            .await
            .expect("insert blob");
        id
    }

    async fn insert_ai_job(p: &PgPool, ws: WorkspaceId, kind: &str, input: i64, output: i64) {
        let id = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO ai_jobs (id, kind, workspace_id, status, payload, result) VALUES ($1,$2,$3,'done','{}'::jsonb,$4)")
            .bind(id)
            .bind(kind)
            .bind(ws.to_uuid())
            .bind(serde_json::json!({ "usage": { "input_tokens": input, "output_tokens": output } }))
            .execute(p)
            .await
            .expect("insert ai_job");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn usage_report_aggregates_match_seeded_data() {
        let p = pool();
        let repo = UsageReportRepo::new(p.clone());
        let owner = participant(&p).await;
        let ws = workspace(&p, owner).await;

        // Fresh workspace: empty except for the lone owner member.
        let before = repo.report(ws).await.unwrap();
        assert_eq!(before.total_messages, 0);
        assert_eq!(before.member_count, 1, "owner is the sole member");
        assert_eq!(before.ai_jobs_total, 0);
        assert_eq!(before.blob_count, 0);
        assert_eq!(before.active_members_30d, 0);

        // Seed: 2 plain messages + 1 message attaching a 1234-byte blob.
        let room = room_in(&p, ws, owner).await;
        insert_message(&p, room, owner, None).await;
        insert_message(&p, room, owner, None).await;
        let blob = insert_blob(&p, owner, 1234).await;
        insert_message(&p, room, owner, Some(blob)).await;

        // Seed: 2 answer jobs (token-accounted) + 1 embed job (no tokens).
        insert_ai_job(&p, ws, "answer", 100, 20).await;
        insert_ai_job(&p, ws, "answer", 50, 10).await;
        insert_ai_job(&p, ws, "embed", 0, 0).await;

        let after = repo.report(ws).await.unwrap();
        assert_eq!(after.total_messages, 3, "three messages");
        assert_eq!(after.messages_last_30d, 3, "all within 30d");
        assert_eq!(after.active_members_30d, 1, "owner is the lone author");

        // Blob attribution: one distinct blob, its byte size.
        assert_eq!(after.blob_count, 1, "one referenced blob");
        assert_eq!(after.blob_bytes, 1234, "blob byte total");

        // AI: 3 jobs total; answers carry tokens, embed does not.
        assert_eq!(after.ai_jobs_total, 3);
        assert_eq!(after.ai_input_tokens, 150, "100 + 50");
        assert_eq!(after.ai_output_tokens, 30, "20 + 10");
        let answer = after
            .ai_jobs_by_kind
            .iter()
            .find(|k| k.kind == "answer")
            .expect("answer kind present");
        assert_eq!(answer.jobs, 2);
        assert_eq!(answer.input_tokens, 150);
        let embed = after
            .ai_jobs_by_kind
            .iter()
            .find(|k| k.kind == "embed")
            .expect("embed kind present");
        assert_eq!(embed.jobs, 1);
        assert_eq!(embed.input_tokens, 0, "embed has no token accounting");

        // Cleanup so reruns stay self-contained.
        sqlx::query("DELETE FROM ai_jobs WHERE workspace_id = $1").bind(ws.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM rooms WHERE workspace_id = $1").bind(ws.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM blobs WHERE id = $1").bind(blob.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM workspaces WHERE id = $1").bind(ws.to_uuid()).execute(&p).await.ok();
    }

    /// A second workspace's data never leaks into the first's report (tenant scope).
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn usage_report_is_tenant_scoped() {
        let p = pool();
        let repo = UsageReportRepo::new(p.clone());
        let owner = participant(&p).await;
        let ws_a = workspace(&p, owner).await;
        let ws_b = workspace(&p, owner).await;

        // Seed B only.
        let room_b = room_in(&p, ws_b, owner).await;
        insert_message(&p, room_b, owner, None).await;
        insert_ai_job(&p, ws_b, "answer", 999, 999).await;

        // A is untouched.
        let a = repo.report(ws_a).await.unwrap();
        assert_eq!(a.total_messages, 0, "B's messages don't leak into A");
        assert_eq!(a.ai_jobs_total, 0, "B's AI jobs don't leak into A");
        assert_eq!(a.ai_input_tokens, 0);

        sqlx::query("DELETE FROM ai_jobs WHERE workspace_id = $1").bind(ws_b.to_uuid()).execute(&p).await.ok();
        sqlx::query("DELETE FROM rooms WHERE workspace_id = ANY($1)")
            .bind(vec![ws_a.to_uuid(), ws_b.to_uuid()])
            .execute(&p)
            .await
            .ok();
        sqlx::query("DELETE FROM workspaces WHERE id = ANY($1)")
            .bind(vec![ws_a.to_uuid(), ws_b.to_uuid()])
            .execute(&p)
            .await
            .ok();
    }
}
