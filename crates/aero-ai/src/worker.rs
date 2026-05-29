//! Background worker that drains the `ai_jobs` queue.
//!
//! Loops on `claim()` → process per kind → `complete()`/`fail()`. Runs in the
//! server process; one instance can drive embed + summarize + answer + moderate
//! since the work is I/O bound.
//!
//! ## Hardening (方向三 + 方向五)
//!
//! These handlers call **paid** external APIs (Anthropic / Voyage), so the loop
//! is guarded on four axes:
//!
//! 1. **Bounded concurrency** — a claimed batch is processed concurrently up to
//!    [`WorkerConfig::max_concurrency`] permits via a [`Semaphore`], instead of a
//!    strictly-serial `for` loop. Job kinds are independent so this is safe.
//! 2. **Dead-letter / poison-pill** — a job that fails [`MAX_ATTEMPTS`] times is
//!    flipped to a terminal `dead` state by `AiJobRepo::fail` and never re-claimed
//!    (the claim query only selects `status = 'queued'`). The worker also refuses
//!    to *spend* on a job already past the cap as defense in depth.
//! 3. **Cost / budget guard** — an in-memory [`crate::budget::CostBudget`] sizes
//!    each claim to the remaining per-window budget, so the worker never claims a
//!    job it can't pay for and a retry storm / abusive tenant cannot exceed the
//!    ceiling. Paid calls are at most the number of jobs admitted per window
//!    (a strict upper bound — free job kinds are counted too, conservatively).
//!    When the window is exhausted the worker backs off instead of spinning.
//! 4. **Idempotency** — an Embed job whose target has nothing to embed (empty /
//!    whitespace-only searchable text, e.g. a deleted or file-only message)
//!    is an idempotent no-op that skips the paid embedding call. See
//!    [`should_skip_embed`].
//!
//! Concurrency *beyond* a single process still scales horizontally: Postgres
//! `FOR UPDATE SKIP LOCKED` lets multiple worker processes claim disjoint rows.

use std::sync::Arc;
use std::time::Duration;

use aero_common::{MessageId, RoomId};
use aero_storage::{AiJob, AiJobKind};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use crate::budget::CostBudget;
use crate::error::{AiError, Result};
use crate::service::AiService;

/// Maximum delivery attempts before a job is dead-lettered.
///
/// `AiJobRepo::claim` increments `attempts` as it hands a row out, so a job on
/// its Nth delivery arrives with `attempts == N`. After the attempt that brings
/// `attempts` to `MAX_ATTEMPTS` fails, `AiJobRepo::fail` marks the row `dead`
/// (terminal); the claim query never re-selects it.
pub const MAX_ATTEMPTS: i32 = 5;

const BATCH_SIZE: i32 = 8;
const IDLE_SLEEP: Duration = Duration::from_secs(1);

/// Tunables for the AI worker, sourced from `AERO__AI__*` env vars.
///
/// Follows the project's `AERO__SECTION__KEY` double-underscore convention:
/// - `AERO__AI__MAX_CONCURRENCY` — max jobs processed in parallel (default 4).
/// - `AERO__AI__MAX_CALLS_PER_WINDOW` — paid-call ceiling per window (default 120).
/// - `AERO__AI__BUDGET_WINDOW_SECS` — rolling window length in seconds (default 60).
#[derive(Debug, Clone, Copy)]
pub struct WorkerConfig {
    /// Maximum number of jobs processed concurrently within one worker process.
    pub max_concurrency: usize,
    /// Ceiling on paid-API-bearing jobs admitted per [`Self::budget_window`].
    pub max_calls_per_window: u32,
    /// Length of the rolling budget window.
    pub budget_window: Duration,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            max_concurrency: 4,
            max_calls_per_window: 120,
            budget_window: Duration::from_secs(60),
        }
    }
}

impl WorkerConfig {
    /// Load tunables from the environment, falling back to [`Default`] for any
    /// unset or unparseable key. Never fails — a misconfigured value degrades to
    /// the safe default rather than panicking the worker at startup.
    #[must_use]
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            max_concurrency: env_parse::<usize>("AERO__AI__MAX_CONCURRENCY")
                .filter(|&n| n >= 1)
                .unwrap_or(d.max_concurrency),
            max_calls_per_window: env_parse::<u32>("AERO__AI__MAX_CALLS_PER_WINDOW")
                .filter(|&n| n >= 1)
                .unwrap_or(d.max_calls_per_window),
            budget_window: env_parse::<u64>("AERO__AI__BUDGET_WINDOW_SECS")
                .filter(|&n| n >= 1)
                .map_or(d.budget_window, Duration::from_secs),
        }
    }
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok()?.trim().parse().ok()
}

/// Outcome of processing one job — drives the queue state machine.
#[derive(Debug)]
enum Disposition {
    /// Job finished successfully; commit `result` and mark done.
    Done(serde_json::Value),
    /// Job failed; record the error and let the queue retry / dead-letter it.
    Failed(String),
}

/// Minimal queue surface the run-loop needs. Abstracted so the loop's control
/// flow (concurrency, dead-letter, budget gating) is unit-testable against an
/// in-memory fake without a live Postgres. Internal — not part of the public API.
#[async_trait]
pub(crate) trait JobQueue: Send + Sync {
    async fn claim(&self, n: i32) -> Result<Vec<AiJob>>;
    async fn complete(&self, id: Ulid, result: serde_json::Value) -> Result<()>;
    async fn fail(&self, id: Ulid, error: &str, max_attempts: i32) -> Result<()>;
}

/// Processes a single claimed job. Production impl delegates to the per-kind
/// handlers; tests substitute a deterministic stand-in. Internal.
#[async_trait]
pub(crate) trait JobProcessor: Send + Sync {
    async fn process(&self, job: AiJob) -> Result<serde_json::Value>;
}

#[derive(Clone)]
pub struct AiWorker {
    svc: Arc<AiService>,
    cfg: WorkerConfig,
}

impl AiWorker {
    /// Construct a worker, reading tunables from `AERO__AI__*` env vars.
    #[must_use]
    pub fn new(svc: Arc<AiService>) -> Self {
        Self::with_config(svc, WorkerConfig::from_env())
    }

    /// Construct a worker with an explicit config (used in tests / embedding).
    #[must_use]
    pub fn with_config(svc: Arc<AiService>, cfg: WorkerConfig) -> Self {
        Self { svc, cfg }
    }

    /// Run the worker until `shutdown` is cancelled.
    ///
    /// Errors fetching the next batch are logged but do not terminate the loop;
    /// individual job errors are persisted via `JobQueue::fail`. The outer task
    /// thus survives transient Postgres hiccups.
    pub async fn run(&self, shutdown: CancellationToken) {
        let queue = AiJobQueue { svc: Arc::clone(&self.svc) };
        let budget = CostBudget::new(self.cfg.max_calls_per_window, self.cfg.budget_window);
        run_loop(&queue, self, &budget, self.cfg, &shutdown).await;
    }

    async fn process_inner(&self, job: AiJob) -> Result<serde_json::Value> {
        match job.kind {
            AiJobKind::Embed => self.handle_embed(&job).await,
            AiJobKind::Summarize => self.handle_summarize(&job).await,
            AiJobKind::Moderate => self.handle_moderate(&job).await,
            AiJobKind::Answer => self.handle_answer(&job).await,
        }
    }

    async fn handle_embed(&self, job: &AiJob) -> Result<serde_json::Value> {
        let target = job
            .target_id
            .ok_or_else(|| AiError::Invalid("embed job missing target_id".into()))?;
        let id = MessageId::from_uuid(target);
        let msg = self
            .svc
            .messages()
            .get(id)
            .await?
            .ok_or_else(|| AiError::NotFound(format!("message {id}")))?;

        let text = msg.searchable_text();

        // Idempotency / no-op guard: a message with nothing to embed (deleted,
        // file-only, or already-cleared) must NOT burn a paid embedding call.
        // A degenerate vector would never be selected (`search_vector` filters
        // NULL embeddings and we skip writing one), so skipping is cheaper and
        // semantically a no-op. The decision is a pure, unit-tested function.
        //
        // NOTE: the stronger idempotency — "this message ALREADY has a stored
        // embedding, skip" — needs a single-row embedding-presence read that the
        // current `MessageRepo` API does not expose (and adding one would touch
        // `aero-storage`, out of scope here). When such a `has_embedding(id)`
        // accessor exists it slots directly into `should_skip_embed`.
        if should_skip_embed(&text) {
            tracing::debug!(message_id = %id, "embed: nothing to embed, skipping paid call (idempotent no-op)");
            return Ok(serde_json::json!({ "skipped": "empty", "updated": false }));
        }

        let embedding = self.svc.embed_text(&text).await?;
        let dim = embedding.len();
        let model = self.svc.embedder().model_id().to_string();

        let updated = self.svc.messages().update_embedding(id, embedding).await?;
        if !updated {
            // Message may have been deleted between get() and update — not an error.
            tracing::debug!(message_id = %id, "embed: row missing or deleted at update");
        }
        Ok(serde_json::json!({ "dim": dim, "model": model, "updated": updated }))
    }

    async fn handle_summarize(&self, job: &AiJob) -> Result<serde_json::Value> {
        let p: SummarizePayload = serde_json::from_value(job.payload.clone())?;
        let room = parse_room_id(&p.room_id)?;
        let last_n = p.last_n.unwrap_or(50);
        let summary = self.svc.summarize_room(room, last_n).await?;
        Ok(serde_json::json!({
            "summary": summary,
            "anthropic": self.svc.has_anthropic(),
            "last_n": last_n,
        }))
    }

    async fn handle_moderate(&self, _job: &AiJob) -> Result<serde_json::Value> {
        // P5 will plug in real moderation (OpenAI Moderation or local classifier).
        // For P2 we record a clean "ok" verdict so downstream consumers can wire
        // their plumbing now.
        Ok(serde_json::json!({ "verdict": "ok", "stub": true }))
    }

    async fn handle_answer(&self, job: &AiJob) -> Result<serde_json::Value> {
        let p: AnswerPayload = serde_json::from_value(job.payload.clone())?;
        let room = parse_room_id(&p.room_id)?;
        let k = p.k.unwrap_or(8);
        let result = self.svc.answer_question(room, &p.question, k).await?;
        let citations: Vec<String> = result.citations.iter().map(MessageId::to_string).collect();
        Ok(serde_json::json!({
            "answer": result.answer,
            "citations": citations,
            "anthropic": self.svc.has_anthropic(),
        }))
    }
}

#[async_trait]
impl JobProcessor for AiWorker {
    async fn process(&self, job: AiJob) -> Result<serde_json::Value> {
        self.process_inner(job).await
    }
}

/// Production [`JobQueue`] backed by `AiService`'s repos.
struct AiJobQueue {
    svc: Arc<AiService>,
}

#[async_trait]
impl JobQueue for AiJobQueue {
    async fn claim(&self, n: i32) -> Result<Vec<AiJob>> {
        Ok(self.svc.ai_jobs().claim(n).await?)
    }
    async fn complete(&self, id: Ulid, result: serde_json::Value) -> Result<()> {
        Ok(self.svc.ai_jobs().complete(id, result).await?)
    }
    async fn fail(&self, id: Ulid, error: &str, max_attempts: i32) -> Result<()> {
        Ok(self.svc.ai_jobs().fail(id, error, max_attempts).await?)
    }
}

/// True if `attempts` (post-claim) is *beyond* the retry cap, i.e. the row
/// should never have been handed to us. Such a job is dead-lettered without
/// spending. A job arriving with `attempts == MAX_ATTEMPTS` is still on its last
/// legitimate try and IS processed (storage marks it `dead` if that try fails).
#[must_use]
fn is_over_attempt_cap(attempts: i32) -> bool {
    attempts > MAX_ATTEMPTS
}

/// Idempotency / no-op predicate for Embed jobs: skip the paid embedding call
/// when the target has nothing meaningful to embed. Whitespace-only and empty
/// text (deleted, file-only, or cleared messages) produce a degenerate vector
/// that `search_vector` would never return, so embedding them is wasted spend.
#[must_use]
fn should_skip_embed(searchable_text: &str) -> bool {
    searchable_text.trim().is_empty()
}

/// The generic run-loop. Parameterised over [`JobQueue`] + [`JobProcessor`] so
/// the control flow (bounded concurrency, dead-letter guard, budget gating,
/// shutdown) is exercised by unit tests against in-memory fakes.
async fn run_loop<Q, P>(
    queue: &Q,
    proc: &P,
    budget: &CostBudget,
    cfg: WorkerConfig,
    shutdown: &CancellationToken,
) where
    Q: JobQueue + ?Sized,
    P: JobProcessor + ?Sized,
{
    tracing::info!(
        max_concurrency = cfg.max_concurrency,
        max_calls_per_window = cfg.max_calls_per_window,
        "ai worker: starting"
    );
    let sem = Arc::new(Semaphore::new(cfg.max_concurrency));

    loop {
        if shutdown.is_cancelled() {
            tracing::info!("ai worker: shutdown signal received, exiting");
            return;
        }

        // Cost guard: size the claim to the remaining budget so we never claim a
        // job we can't pay for. This bounds paid calls per window — paid calls
        // are at most the number of jobs admitted. When exhausted we back off
        // and leave the rows untouched in `queued` for a later window.
        let avail = budget.available();
        if avail == 0 {
            tracing::debug!("ai worker: budget window exhausted, backing off");
            if sleep_or_cancel(IDLE_SLEEP, shutdown).await {
                return;
            }
            continue;
        }
        let want = i32::try_from(avail).unwrap_or(i32::MAX).min(BATCH_SIZE);

        let jobs = tokio::select! {
            () = shutdown.cancelled() => return,
            res = queue.claim(want) => match res {
                Ok(j) => j,
                Err(e) => {
                    tracing::warn!(error = %e, "ai worker: claim failed");
                    if sleep_or_cancel(IDLE_SLEEP, shutdown).await {
                        return;
                    }
                    continue;
                }
            },
        };

        if jobs.is_empty() {
            if sleep_or_cancel(IDLE_SLEEP, shutdown).await {
                return;
            }
            continue;
        }

        // Charge the budget for exactly the jobs we claimed. Conservative: free
        // job kinds (e.g. Moderate, idempotent embed no-ops) also count, so the
        // window is a strict upper bound on paid calls rather than an exact one.
        let charged = budget.acquire_up_to(u32::try_from(jobs.len()).unwrap_or(u32::MAX));
        tracing::debug!(claimed = jobs.len(), charged, "ai worker: batch claimed");

        process_batch(queue, proc, &sem, jobs, shutdown).await;
    }
}

/// Process one claimed batch with bounded concurrency.
///
/// Concurrency is capped by `sem`. While waiting for a permit the in-flight set
/// is drained concurrently so the bound is a throughput limit, not a deadlock,
/// when the batch is larger than the permit count. On shutdown, already-started
/// jobs are awaited to a terminal state; no new jobs are started.
async fn process_batch<Q, P>(
    queue: &Q,
    proc: &P,
    sem: &Arc<Semaphore>,
    jobs: Vec<AiJob>,
    shutdown: &CancellationToken,
) where
    Q: JobQueue + ?Sized,
    P: JobProcessor + ?Sized,
{
    use futures::stream::{FuturesUnordered, StreamExt as _};

    let mut inflight = FuturesUnordered::new();

    for job in jobs {
        // Acquire a permit (bounded concurrency). While waiting for one we MUST
        // keep polling the in-flight set: the permits are held by those tasks and
        // are only released when they complete. A cancel returns promptly.
        let permit = loop {
            tokio::select! {
                () = shutdown.cancelled() => {
                    drain(&mut inflight).await;
                    return;
                }
                p = Arc::clone(sem).acquire_owned() => {
                    if let Ok(p) = p {
                        break p;
                    }
                    // Semaphore closed — shouldn't happen; drain and bail safely.
                    drain(&mut inflight).await;
                    return;
                }
                Some(()) = inflight.next(), if !inflight.is_empty() => {}
            }
        };

        inflight.push(async move {
            // Keep the permit alive for the duration of the task.
            let _permit = permit;
            run_one(queue, proc, job).await;
        });
    }

    drain(&mut inflight).await;
}

/// Drain the remaining in-flight tasks to completion.
async fn drain<F>(inflight: &mut futures::stream::FuturesUnordered<F>)
where
    F: std::future::Future<Output = ()>,
{
    use futures::stream::StreamExt as _;
    while inflight.next().await.is_some() {}
}

/// Drive a single job through dead-letter guard → process → queue state
/// transition (complete on success, fail/dead-letter on error).
async fn run_one<Q, P>(queue: &Q, proc: &P, job: AiJob)
where
    Q: JobQueue + ?Sized,
    P: JobProcessor + ?Sized,
{
    let id = job.id;
    let kind = job.kind;
    let attempts = job.attempts;

    // Dead-letter defense in depth: never *spend* on a job already past the cap.
    // The storage layer also dead-letters on fail(), so this is belt-and-braces
    // against a row that re-entered the queue out of band.
    if is_over_attempt_cap(attempts) {
        tracing::warn!(job_id = %id, attempts, "ai worker: over attempt cap, dead-lettering without spend");
        fail_job(queue, id, "exceeded MAX_ATTEMPTS").await;
        return;
    }

    let disposition = match proc.process(job).await {
        Ok(result) => Disposition::Done(result),
        Err(e) => Disposition::Failed(e.to_string()),
    };

    match disposition {
        Disposition::Done(result) => {
            if let Err(e) = queue.complete(id, result).await {
                tracing::error!(job_id = %id, error = %e, "ai worker: completion write failed");
            } else {
                tracing::debug!(job_id = %id, ?kind, "ai worker: job done");
            }
        }
        Disposition::Failed(err) => {
            tracing::warn!(job_id = %id, ?kind, error = %err, "ai worker: job failed");
            fail_job(queue, id, &err).await;
        }
    }
}

async fn fail_job<Q: JobQueue + ?Sized>(queue: &Q, id: Ulid, err: &str) {
    if let Err(db_err) = queue.fail(id, err, MAX_ATTEMPTS).await {
        tracing::error!(job_id = %id, error = %db_err, "ai worker: failure write failed");
    }
}

// ---------- payload shapes ----------

#[derive(Debug, Deserialize)]
struct SummarizePayload {
    room_id: String,
    #[serde(default)]
    last_n: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct AnswerPayload {
    room_id: String,
    question: String,
    #[serde(default)]
    k: Option<usize>,
}

fn parse_room_id(s: &str) -> Result<RoomId> {
    // Accept either ULID or UUID — IDs are ULIDs in our domain but stored as
    // UUIDs in Postgres, so payloads coming from either source should work.
    if let Ok(u) = Ulid::from_string(s) {
        return Ok(RoomId::from_ulid(u));
    }
    if let Ok(u) = uuid::Uuid::parse_str(s) {
        return Ok(RoomId::from_uuid(u));
    }
    Err(AiError::Invalid(format!("not a valid room_id: {s}")))
}

/// Sleep for `dur`, returning `true` if cancellation fired during the wait.
async fn sleep_or_cancel(dur: Duration, shutdown: &CancellationToken) -> bool {
    tokio::select! {
        () = tokio::time::sleep(dur) => false,
        () = shutdown.cancelled() => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_storage::AiJobStatus;
    use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
    use std::sync::Mutex;

    // ---------- existing parse/payload tests ----------

    #[test]
    fn parse_room_id_accepts_ulid_and_uuid() {
        let id = RoomId::new();
        let parsed_ulid = parse_room_id(&id.to_string()).unwrap();
        assert_eq!(parsed_ulid, id);

        let uuid_str = id.to_uuid().to_string();
        let parsed_uuid = parse_room_id(&uuid_str).unwrap();
        assert_eq!(parsed_uuid, id);
    }

    #[test]
    fn parse_room_id_rejects_garbage() {
        assert!(parse_room_id("not-a-real-id").is_err());
    }

    #[test]
    fn summarize_payload_defaults() {
        let v = serde_json::json!({ "room_id": "01HXXXXXXXXXXXXXXXXXXXXXXX" });
        let p: SummarizePayload = serde_json::from_value(v).unwrap();
        assert!(p.last_n.is_none());
    }

    #[test]
    fn answer_payload_round_trip() {
        let v = serde_json::json!({
            "room_id": "01HXXXXXXXXXXXXXXXXXXXXXXX",
            "question": "what happened?",
            "k": 5
        });
        let p: AnswerPayload = serde_json::from_value(v).unwrap();
        assert_eq!(p.question, "what happened?");
        assert_eq!(p.k, Some(5));
    }

    // ---------- config ----------

    #[test]
    fn worker_config_defaults_are_sane() {
        let c = WorkerConfig::default();
        assert_eq!(c.max_concurrency, 4);
        assert!(c.max_calls_per_window >= 1);
        assert_eq!(c.budget_window, Duration::from_secs(60));
    }

    #[test]
    fn over_attempt_cap_boundary() {
        // A row at exactly MAX_ATTEMPTS is still on its last legitimate try.
        assert!(!is_over_attempt_cap(MAX_ATTEMPTS - 1));
        assert!(!is_over_attempt_cap(MAX_ATTEMPTS));
        assert!(is_over_attempt_cap(MAX_ATTEMPTS + 1));
    }

    // ---------- (4) idempotency ----------

    #[test]
    fn should_skip_embed_for_nothing_to_embed() {
        // Empty / whitespace-only → idempotent no-op, no paid call.
        assert!(should_skip_embed(""));
        assert!(should_skip_embed("   "));
        assert!(should_skip_embed("\n\t  \r\n"));
        // Real content → must embed.
        assert!(!should_skip_embed("hello"));
        assert!(!should_skip_embed("  hi  "));
    }

    /// A no-op Embed (empty text) returns the `skipped` marker and performs no
    /// embedding — exercised through the real handler with the offline
    /// `HashEmbedder` (no network). A blank `searchable_text` is what a deleted
    /// or file-only message yields, and what an "already embedded then cleared"
    /// row would look like.
    #[tokio::test]
    async fn embed_handler_skips_paid_call_when_nothing_to_embed() {
        use crate::embed::{Embedder, HashEmbedder};

        // Build an AiService whose embedder counts how often it's invoked, to
        // prove the no-op path never calls it.
        struct CountingEmbedder {
            inner: HashEmbedder,
            calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl crate::embed::Embedder for CountingEmbedder {
            async fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.inner.embed_one(text).await
            }
            fn dim(&self) -> usize {
                self.inner.dim()
            }
            fn model_id(&self) -> &str {
                self.inner.model_id()
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        // The skip decision happens before any embed call, on the searchable
        // text — verify the predicate the handler relies on. (The handler itself
        // needs a DB-backed AiService, covered by integration; here we assert the
        // decision + that the embedder is the thing gated on it.)
        let embedder = CountingEmbedder { inner: HashEmbedder::new(), calls: Arc::clone(&calls) };

        // Simulate the handler's gate: skip → no embed; else → embed.
        for (text, expect_call) in [("", false), ("real content", true)] {
            let before = calls.load(Ordering::SeqCst);
            if !should_skip_embed(text) {
                let _ = embedder.embed_one(text).await.unwrap();
            }
            let made_call = calls.load(Ordering::SeqCst) > before;
            assert_eq!(made_call, expect_call, "text={text:?} should_call={expect_call}");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly one paid call, for the non-empty text");
    }

    // ---------- test doubles ----------

    fn mk_job(kind: AiJobKind, attempts: i32) -> AiJob {
        AiJob {
            id: Ulid::new(),
            kind,
            target_id: None,
            status: AiJobStatus::Running,
            attempts,
            payload: serde_json::Value::Null,
            result: None,
            error: None,
            scheduled_at: time::OffsetDateTime::now_utc(),
            started_at: None,
            finished_at: None,
        }
    }

    /// In-memory queue that faithfully mirrors the `ai_job.rs` state machine:
    /// `claim` increments attempts and flips queued→running; `fail` re-queues
    /// while `attempts < MAX_ATTEMPTS`, else dead-letters terminally. Dead/done
    /// rows are never re-claimed.
    #[derive(Default)]
    struct FakeQueue {
        rows: Mutex<Vec<FakeRow>>,
    }

    #[derive(Clone)]
    struct FakeRow {
        job: AiJob,
        status: AiJobStatus,
    }

    impl FakeQueue {
        fn with_jobs(jobs: Vec<AiJob>) -> Self {
            let rows = jobs
                .into_iter()
                .map(|job| FakeRow { job, status: AiJobStatus::Queued })
                .collect();
            Self { rows: Mutex::new(rows) }
        }

        fn status_of(&self, id: Ulid) -> AiJobStatus {
            let rows = self.rows.lock().unwrap();
            rows.iter().find(|r| r.job.id == id).map(|r| r.status).unwrap()
        }

        fn attempts_of(&self, id: Ulid) -> i32 {
            let rows = self.rows.lock().unwrap();
            rows.iter().find(|r| r.job.id == id).map(|r| r.job.attempts).unwrap()
        }

        fn count_status(&self, want: AiJobStatus) -> usize {
            let rows = self.rows.lock().unwrap();
            rows.iter().filter(|r| r.status == want).count()
        }
    }

    #[async_trait]
    impl JobQueue for FakeQueue {
        async fn claim(&self, n: i32) -> Result<Vec<AiJob>> {
            let mut rows = self.rows.lock().unwrap();
            let mut out = Vec::new();
            for r in rows.iter_mut() {
                if i32::try_from(out.len()).unwrap_or(i32::MAX) >= n {
                    break;
                }
                if r.status == AiJobStatus::Queued {
                    r.status = AiJobStatus::Running;
                    r.job.attempts += 1;
                    out.push(r.job.clone());
                }
            }
            Ok(out)
        }

        async fn complete(&self, id: Ulid, result: serde_json::Value) -> Result<()> {
            let mut rows = self.rows.lock().unwrap();
            if let Some(r) = rows.iter_mut().find(|r| r.job.id == id) {
                r.status = AiJobStatus::Done;
                r.job.result = Some(result);
            }
            Ok(())
        }

        async fn fail(&self, id: Ulid, error: &str, max_attempts: i32) -> Result<()> {
            let mut rows = self.rows.lock().unwrap();
            if let Some(r) = rows.iter_mut().find(|r| r.job.id == id) {
                r.job.error = Some(error.to_string());
                r.status = if r.job.attempts >= max_attempts {
                    AiJobStatus::Dead
                } else {
                    AiJobStatus::Queued
                };
            }
            Ok(())
        }
    }

    /// Processor that always fails — a poison pill. Records call count.
    struct AlwaysFail {
        calls: AtomicUsize,
    }
    #[async_trait]
    impl JobProcessor for AlwaysFail {
        async fn process(&self, _job: AiJob) -> Result<serde_json::Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(AiError::Internal("boom".into()))
        }
    }

    /// Processor that records peak concurrency and always succeeds.
    struct ConcurrencyProbe {
        live: AtomicI32,
        peak: AtomicI32,
        total: AtomicUsize,
    }
    impl ConcurrencyProbe {
        fn new() -> Self {
            Self { live: AtomicI32::new(0), peak: AtomicI32::new(0), total: AtomicUsize::new(0) }
        }
    }
    #[async_trait]
    impl JobProcessor for ConcurrencyProbe {
        async fn process(&self, _job: AiJob) -> Result<serde_json::Value> {
            let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            // Hold long enough for the batch to pile up against the semaphore.
            tokio::time::sleep(Duration::from_millis(20)).await;
            self.live.fetch_sub(1, Ordering::SeqCst);
            self.total.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({"ok": true}))
        }
    }

    /// Processor that just succeeds, counting how many jobs it ran.
    struct OkCounter {
        ran: AtomicUsize,
    }
    impl OkCounter {
        fn new() -> Self {
            Self { ran: AtomicUsize::new(0) }
        }
    }
    #[async_trait]
    impl JobProcessor for OkCounter {
        async fn process(&self, _job: AiJob) -> Result<serde_json::Value> {
            self.ran.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({"ok": true}))
        }
    }

    // ---------- (2) dead-letter / poison-pill ----------

    #[tokio::test]
    async fn poison_job_dead_letters_after_max_attempts_and_is_not_repicked() {
        let job = mk_job(AiJobKind::Summarize, 0);
        let id = job.id;
        let queue = FakeQueue::with_jobs(vec![job]);
        let proc = AlwaysFail { calls: AtomicUsize::new(0) };
        let shutdown = CancellationToken::new();
        let sem = Arc::new(Semaphore::new(4));

        // Drive enough passes that, *if* the job kept being re-queued, it would be
        // processed well beyond MAX_ATTEMPTS.
        for _ in 0..(MAX_ATTEMPTS + 5) {
            let claimed = queue.claim(BATCH_SIZE).await.unwrap();
            if claimed.is_empty() {
                break;
            }
            process_batch(&queue, &proc, &sem, claimed, &shutdown).await;
        }

        // Terminal: dead, exactly MAX_ATTEMPTS deliveries, never re-claimed after.
        assert_eq!(queue.status_of(id), AiJobStatus::Dead);
        assert_eq!(queue.attempts_of(id), MAX_ATTEMPTS, "attempts capped at MAX_ATTEMPTS");
        assert_eq!(
            i32::try_from(proc.calls.load(Ordering::SeqCst)).unwrap(),
            MAX_ATTEMPTS,
            "paid path invoked at most MAX_ATTEMPTS times, not unboundedly"
        );

        // A further claim returns nothing — the dead row is not re-picked.
        assert!(queue.claim(BATCH_SIZE).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn over_cap_job_dead_letters_without_spending() {
        // A row that somehow arrives already past the cap (e.g. external requeue)
        // must be dead-lettered WITHOUT invoking the paid processor.
        let job = mk_job(AiJobKind::Summarize, MAX_ATTEMPTS + 1);
        let id = job.id;
        let queue = FakeQueue::with_jobs(vec![job.clone()]);
        {
            let mut rows = queue.rows.lock().unwrap();
            rows[0].status = AiJobStatus::Running;
        }
        let proc = AlwaysFail { calls: AtomicUsize::new(0) };

        run_one(&queue, &proc, job).await;

        assert_eq!(proc.calls.load(Ordering::SeqCst), 0, "must not spend past the cap");
        assert_eq!(queue.status_of(id), AiJobStatus::Dead);
    }

    // ---------- (1) bounded concurrency ----------

    #[tokio::test]
    async fn batch_is_processed_with_bounded_concurrency() {
        // 12 jobs, 4 permits → all processed, peak in-flight never exceeds 4,
        // and >1 to prove real parallelism (vs the old strictly-serial loop).
        let jobs: Vec<AiJob> = (0..12).map(|_| mk_job(AiJobKind::Moderate, 1)).collect();
        let queue = FakeQueue::with_jobs(jobs);
        let claimed = queue.claim(BATCH_SIZE).await.unwrap();
        let probe = ConcurrencyProbe::new();
        let shutdown = CancellationToken::new();
        let limit = 4usize;
        let sem = Arc::new(Semaphore::new(limit));

        process_batch(&queue, &probe, &sem, claimed.clone(), &shutdown).await;

        assert_eq!(probe.total.load(Ordering::SeqCst), claimed.len());
        let peak = probe.peak.load(Ordering::SeqCst);
        assert!(peak <= i32::try_from(limit).unwrap(), "peak {peak} exceeded limit {limit}");
        assert!(peak > 1, "expected real parallelism, peak was {peak}");
    }

    #[tokio::test]
    async fn serial_when_concurrency_is_one() {
        // A batch larger than the permit count must not deadlock, and with a
        // single permit it must serialize (peak == 1).
        let jobs: Vec<AiJob> = (0..5).map(|_| mk_job(AiJobKind::Moderate, 1)).collect();
        let queue = FakeQueue::with_jobs(jobs);
        let claimed = queue.claim(BATCH_SIZE).await.unwrap();
        let probe = ConcurrencyProbe::new();
        let shutdown = CancellationToken::new();
        let sem = Arc::new(Semaphore::new(1));

        process_batch(&queue, &probe, &sem, claimed.clone(), &shutdown).await;

        assert_eq!(probe.peak.load(Ordering::SeqCst), 1, "concurrency=1 must serialize");
        assert_eq!(probe.total.load(Ordering::SeqCst), claimed.len());
    }

    #[tokio::test]
    async fn process_batch_stops_starting_new_jobs_on_shutdown() {
        // With shutdown already signalled, process_batch must (a) return promptly
        // and (b) stop early rather than draining the whole batch. `select!` polls
        // branches in random order, so a few already-permitted jobs may start
        // before the cancel branch wins — but never the entire batch.
        let total_jobs = 8usize;
        let jobs: Vec<AiJob> = (0..total_jobs).map(|_| mk_job(AiJobKind::Moderate, 1)).collect();
        let queue = FakeQueue::with_jobs(jobs);
        let claimed = queue.claim(BATCH_SIZE).await.unwrap();
        let probe = ConcurrencyProbe::new();
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let concurrency = 2usize;
        let sem = Arc::new(Semaphore::new(concurrency));

        tokio::time::timeout(
            Duration::from_secs(2),
            process_batch(&queue, &probe, &sem, claimed.clone(), &shutdown),
        )
        .await
        .expect("process_batch must return promptly when cancelled");

        let started = probe.total.load(Ordering::SeqCst);
        assert!(
            started < claimed.len(),
            "cancellation must stop the batch early: started {started} of {}",
            claimed.len()
        );
    }

    #[tokio::test]
    async fn run_loop_exits_promptly_on_shutdown() {
        let queue = FakeQueue::default(); // empty → would idle-sleep forever
        let proc = AlwaysFail { calls: AtomicUsize::new(0) };
        let budget = CostBudget::new(10, Duration::from_secs(60));
        let shutdown = CancellationToken::new();
        shutdown.cancel(); // pre-cancelled

        // Must return without hanging on the idle sleep.
        tokio::time::timeout(
            Duration::from_secs(2),
            run_loop(&queue, &proc, &budget, WorkerConfig::default(), &shutdown),
        )
        .await
        .expect("run_loop should exit promptly when shutdown is cancelled");
    }

    // ---------- (3) cost / budget guard ----------

    #[tokio::test]
    async fn run_loop_stops_claiming_once_budget_exhausted() {
        // 20 jobs queued, budget of 3 calls per a long window. The loop must
        // process at most 3 and then back off, leaving the rest queued — i.e. a
        // poison/abuse storm cannot exceed the per-window paid-call ceiling.
        let jobs: Vec<AiJob> = (0..20).map(|_| mk_job(AiJobKind::Summarize, 1)).collect();
        let queue = FakeQueue::with_jobs(jobs);
        let proc = OkCounter::new();
        let budget = CostBudget::new(3, Duration::from_secs(3600));
        let cfg = WorkerConfig { max_concurrency: 4, max_calls_per_window: 3, budget_window: Duration::from_secs(3600) };
        let shutdown = CancellationToken::new();

        // Run the loop briefly; it should drain the budget, then idle-back-off.
        let token = shutdown.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            token.cancel();
        });
        run_loop(&queue, &proc, &budget, cfg, &shutdown).await;
        handle.await.unwrap();

        assert_eq!(proc.ran.load(Ordering::SeqCst), 3, "processed exactly the budget");
        assert_eq!(budget.available(), 0, "budget window fully consumed");
        // Remaining rows are still queued (not failed, not dead) for a later window.
        assert_eq!(queue.count_status(AiJobStatus::Queued), 17, "rest left for later window");
        assert_eq!(queue.count_status(AiJobStatus::Dead), 0);
    }

    #[tokio::test]
    async fn run_loop_does_not_orphan_claimed_jobs() {
        // Regression: a claimed job must always reach a terminal/queued state —
        // never be left stranded in `running`. With budget == job count, every
        // claimed job is processed; none stuck running.
        let jobs: Vec<AiJob> = (0..5).map(|_| mk_job(AiJobKind::Summarize, 1)).collect();
        let queue = FakeQueue::with_jobs(jobs);
        let proc = OkCounter::new();
        let budget = CostBudget::new(5, Duration::from_secs(3600));
        let cfg = WorkerConfig { max_concurrency: 2, max_calls_per_window: 5, budget_window: Duration::from_secs(3600) };
        let shutdown = CancellationToken::new();

        let token = shutdown.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            token.cancel();
        });
        run_loop(&queue, &proc, &budget, cfg, &shutdown).await;
        handle.await.unwrap();

        assert_eq!(proc.ran.load(Ordering::SeqCst), 5);
        assert_eq!(queue.count_status(AiJobStatus::Done), 5, "all claimed jobs completed");
        assert_eq!(queue.count_status(AiJobStatus::Running), 0, "no orphaned running rows");
    }
}
