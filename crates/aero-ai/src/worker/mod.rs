//! Background worker that drains the `ai_jobs` queue.
//!
//! Loops on `claim()` → process per kind → `complete()`/`fail()`. Runs in the
//! server process; one instance can drive embed + summarize + answer + moderate
//! since the work is I/O bound.
//!
//! ## Hardening (方向三 + 方向五)
//!
//! These handlers call **paid** external APIs (Anthropic / Voyage), so the loop
//! is guarded on four axes (plus observability, below):
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
//!
//! ## Observability (方向四)
//!
//! Each job emits metrics into [`aero_common::metrics`] (see [`crate::metrics`]):
//! per-kind **job duration** (histogram), **estimated cost** (counter, coarse —
//! see [`CostModel`]), and **outcome** counters (success / failure / dead-letter).
//! The run-loop also keeps the **queue-depth** gauge in step as batches are
//! claimed and drained. The recording layer takes an injectable `&Registry` so it
//! unit-tests against a fresh registry; production wires it to the process-global
//! one via [`aero_common::metrics::global`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use aero_common::metrics::Registry;
use aero_common::{MessageId, ParticipantId, RoomId, WorkspaceId};
use aero_storage::{AiJob, AiJobKind};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use crate::anthropic::Usage;
use crate::budget::{CostBudget, KeyedCostBudget};
use crate::error::{AiError, Result};
use crate::governance::LOCAL_ACTION_MODERATED;
use crate::metrics::{self as ai_metrics, CostModel};
use crate::service::AiService;
use crate::usage::{self as ai_usage, UsageSink};

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
/// - `AERO__AI__MAX_CALLS_PER_WINDOW` — global paid-call ceiling per window (default 120).
/// - `AERO__AI__MAX_CALLS_PER_WINDOW_PER_WS` — per-workspace paid-call ceiling per
///   window (default 120 — same as global, i.e. permissive until tightened).
/// - `AERO__AI__BUDGET_WINDOW_SECS` — rolling window length in seconds (default 60).
#[derive(Debug, Clone, Copy)]
pub struct WorkerConfig {
    /// Maximum number of jobs processed concurrently within one worker process.
    pub max_concurrency: usize,
    /// Global ceiling on paid-API-bearing jobs admitted per [`Self::budget_window`].
    pub max_calls_per_window: u32,
    /// Per-workspace ceiling per window. Bounds any single tenant's share so one
    /// busy/abusive workspace cannot consume the whole global window. A job whose
    /// workspace has hit this is *deferred* to the next window, not dropped.
    pub max_calls_per_window_per_workspace: u32,
    /// Length of the rolling budget window.
    pub budget_window: Duration,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            max_concurrency: 4,
            max_calls_per_window: 120,
            max_calls_per_window_per_workspace: 120,
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
            max_calls_per_window_per_workspace: env_parse::<u32>(
                "AERO__AI__MAX_CALLS_PER_WINDOW_PER_WS",
            )
            .filter(|&n| n >= 1)
            .unwrap_or(d.max_calls_per_window_per_workspace),
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
    /// Return a claimed job to `queued` no earlier than `until`, without spending
    /// a retry attempt — used to defer a job whose workspace is over budget.
    async fn defer(&self, id: Ulid, until: time::OffsetDateTime) -> Result<()>;
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
    cost_model: CostModel,
}

/// Resolve the workspace a moderation finalize must delete under — FAIL-CLOSED
/// (B5-1 R-D1). The audit action is derived from the workspace, so deleting
/// with `None` would commit the soft delete + `Deleted` broadcast with ZERO
/// `audit_events` rows (the 0236 governance trigger never fires) — an
/// invisible, un-audited removal and the only route around the otherwise
/// fail-closed binding RAISE. Refuse instead: the caller propagates `Err` to
/// the existing retry path → bounded DLQ, and the provider verdict is already
/// durably finalized under the job's stable usage context, so the retry
/// replays it without a second paid call. Pure, so unit-testable without a DB
/// (mirrors the `should_skip_embed` decision pattern).
fn moderation_delete_workspace(job: &AiJob) -> Result<WorkspaceId> {
    job.workspace_id.map(WorkspaceId::from_uuid).ok_or_else(|| {
        AiError::Invalid(format!(
            "moderation job {} has no workspace_id; refusing un-audited delete (R-D1)",
            job.id
        ))
    })
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
        Self {
            svc,
            cfg,
            cost_model: CostModel::default(),
        }
    }

    /// Override the per-kind cost estimate used for the `AI_COST_MICROS_TOTAL`
    /// metric (defaults to [`CostModel::default`]). The estimate is coarse — see
    /// [`CostModel`] — and exists for budget alerting, not billing.
    #[must_use]
    pub fn with_cost_model(mut self, cost_model: CostModel) -> Self {
        self.cost_model = cost_model;
        self
    }

    /// Run the worker until `shutdown` is cancelled.
    ///
    /// Errors fetching the next batch are logged but do not terminate the loop;
    /// individual job errors are persisted via `JobQueue::fail`. The outer task
    /// thus survives transient Postgres hiccups.
    ///
    /// Metrics are emitted to the process-global registry
    /// ([`aero_common::metrics::global`]); the `/metrics` route lives in
    /// `aero-server`.
    pub async fn run(&self, shutdown: CancellationToken) {
        let queue = AiJobQueue {
            svc: Arc::clone(&self.svc),
        };
        let budget = CostBudget::new(self.cfg.max_calls_per_window, self.cfg.budget_window);
        let keyed = KeyedCostBudget::<uuid::Uuid>::new(
            self.cfg.max_calls_per_window_per_workspace,
            self.cfg.budget_window,
        );
        run_loop(
            &queue,
            self,
            &budget,
            &keyed,
            self.cfg,
            aero_common::metrics::global(),
            &self.cost_model,
            self.svc.usage_sink().map(Arc::as_ref),
            &shutdown,
        )
        .await;
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
        if should_skip_embed(&text) {
            tracing::debug!(message_id = %id, "embed: nothing to embed, skipping paid call (idempotent no-op)");
            return Ok(serde_json::json!({ "skipped": "empty", "updated": false }));
        }

        // Stronger idempotency: a message that ALREADY has a stored embedding
        // must NOT be re-embedded — that would re-pay the embedder for a vector
        // we already have. A duplicate Embed job (re-enqueue, retry, replay) is
        // therefore a paid-call-free no-op. `edit` / `update_voice_transcript`
        // null the column when content changes, so a present embedding is by
        // construction still current. Checked AFTER the empty-text guard so an
        // (impossible) empty-but-embedded row still short-circuits cheaply.
        if self.svc.messages().has_embedding(id).await? {
            tracing::debug!(message_id = %id, "embed: already embedded, skipping paid call (idempotent no-op)");
            return Ok(serde_json::json!({ "skipped": "already_embedded", "updated": false }));
        }

        // 方向三-2 file content search: if this message carries an extractable
        // document (PDF / docx / xlsx / pptx, or plain-text), fold its *body
        // text* into what we index + embed, so the document's CONTENTS — not just
        // its file name — are full-text- and semantically searchable. Reuses the
        // existing doc_extract + blob read (same path as `read_attachment`); no
        // new extractor. Strictly FAIL-OPEN: any failure leaves `text` unchanged
        // so the indexing job never crashes on a bad/oversized/binary attachment.
        let text = match self.svc.extract_attachment_text(&msg).await {
            Some(doc) => {
                let folded = crate::service::fold_searchable_with_document(&text, &doc);
                if folded != text {
                    // Persist the augmented searchable_text so FTS (the STORED
                    // `search_tsv` generated column over searchable_text) also
                    // matches the document body. Fail-open: a write failure is
                    // logged but we still embed the folded text below.
                    match self
                        .svc
                        .messages()
                        .update_searchable_text(id, &folded)
                        .await
                    {
                        Ok(updated) => tracing::debug!(
                            message_id = %id, updated, doc_len = doc.len(),
                            "embed: folded attachment text into searchable_text"
                        ),
                        Err(e) => tracing::warn!(
                            error = %e, message_id = %id,
                            "embed: persisting folded searchable_text failed (fail-open, still embedding)"
                        ),
                    }
                }
                folded
            }
            None => text,
        };

        let usage_context = crate::usage::UsageContext::for_job(job.id, job.workspace_id);
        let embedding = self
            .svc
            .embed_text_with_context(&text, usage_context, "voyage_embed_document")
            .await?;
        let dim = embedding.len();
        let model = self.svc.embedder().model_id().to_string();

        let updated = self.svc.messages().update_embedding(id, embedding).await?;
        if !updated {
            // Message may have been deleted between get() and update — not an error.
            tracing::debug!(message_id = %id, "embed: row missing or deleted at update");
        }
        Ok(serde_json::json!({
            "dim": dim,
            "model": model,
            "updated": updated,
            "paid_provider": self.svc.embedder().is_paid_provider(),
            "usage_accounted": true,
        }))
    }

    async fn handle_summarize(&self, job: &AiJob) -> Result<serde_json::Value> {
        let p: SummarizePayload = serde_json::from_value(job.payload.clone())?;
        let room = parse_room_id(&p.room_id)?;
        let last_n = p.last_n.unwrap_or(50);
        let usage_context = crate::usage::UsageContext::for_job(job.id, job.workspace_id);
        let (summary, usage) = self
            .svc
            .summarize_room_with_usage_context(room, last_n, usage_context)
            .await?;
        let mut result = serde_json::json!({
            "summary": summary,
            "anthropic": self.svc.has_anthropic(),
            "last_n": last_n,
            "usage_accounted": true,
        });
        attach_usage(&mut result, usage);
        Ok(result)
    }

    async fn handle_moderate(&self, job: &AiJob) -> Result<serde_json::Value> {
        let p: ModeratePayload = serde_json::from_value(job.payload.clone())?;
        let usage_context = crate::usage::UsageContext::for_job(job.id, job.workspace_id);
        let (verdict, usage) = self
            .svc
            .moderate_with_usage_context(&p.text, usage_context)
            .await?;
        let anthropic = self.svc.has_anthropic();

        let mut result = if let Some(reason) = verdict {
            // BLOCK: soft-delete the offending message so it stops being visible.
            if let Some(target) = job.target_id {
                let id = MessageId::from_uuid(target);
                // R-D1 (B5-1): refuse to delete when the workspace cannot be
                // resolved — `audit_action` is derived from the workspace, so
                // `None` would commit the removal with zero audit + zero
                // governance rows. Propagate Err → retry → bounded DLQ.
                let workspace = moderation_delete_workspace(job)?;
                let detail = serde_json::json!({
                    "reason": reason,
                    "source": "ai_worker",
                });
                match self
                    .svc
                    .messages()
                    .soft_delete_outboxed_system(
                        id,
                        Some(workspace),
                        None,
                        Some(LOCAL_ACTION_MODERATED),
                        detail,
                        ParticipantId::nil(),
                        None,
                    )
                    .await
                {
                    Ok(Some(_)) => {
                        tracing::info!(message_id = %id, %reason, "moderation: blocked and removed");
                    }
                    Ok(None) => {
                        // Already deleted by sender or a concurrent job — no-op.
                        tracing::debug!(message_id = %id, "moderation: message already deleted");
                    }
                    Err(e) => {
                        // Removing blocked content is safety-critical: do NOT report
                        // the job a success with the message still visible. Propagate
                        // so the queue retries (bounded → DLQ). The provider verdict
                        // is already durably finalized under this job's stable usage
                        // context, so a retry replays it without a second paid call.
                        tracing::warn!(error = %e, message_id = %id, "moderation: soft_delete failed; retrying job");
                        return Err(e.into());
                    }
                }
            } else {
                tracing::warn!(job_id = %job.id, "moderation: BLOCK verdict but no target_id to remove");
            }
            serde_json::json!({
                "verdict": "block",
                "reason": reason,
                "anthropic": anthropic,
                "usage_accounted": true,
            })
        } else {
            serde_json::json!({
                "verdict": "safe",
                "anthropic": anthropic,
                "usage_accounted": true,
            })
        };
        // Attach the real token usage so the success path charges BILLED cost
        // (record_token_cost) rather than the flat moderate_micros estimate
        // whenever Anthropic was actually called (ROADMAP 方向四).
        attach_usage(&mut result, usage);
        Ok(result)
    }

    async fn handle_answer(&self, job: &AiJob) -> Result<serde_json::Value> {
        let p: AnswerPayload = serde_json::from_value(job.payload.clone())?;
        let room = parse_room_id(&p.room_id)?;
        let k = p.k.unwrap_or(8);
        // Opt-in agentic mode (方向三): the model drives its own room-scoped retrieval
        // (search → refine → answer) instead of one fixed top-k. Off by default so
        // the cheaper single-shot path stays the norm; agentic costs extra model turns.
        let agentic = std::env::var("AERO_AGENTIC_ANSWERS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let (answer, usage) = if agentic {
            self.svc
                .answer_question_agentic_with_context(
                    room,
                    &p.question,
                    4,
                    crate::usage::UsageContext::for_job(job.id, job.workspace_id),
                )
                .await?
        } else {
            self.svc
                .answer_question_with_usage_context(
                    room,
                    &p.question,
                    k,
                    crate::usage::UsageContext::for_job(job.id, job.workspace_id),
                )
                .await?
        };
        let citations: Vec<String> = answer.citations.iter().map(MessageId::to_string).collect();
        let mut result = serde_json::json!({
            "answer": answer.answer,
            "citations": citations,
            "anthropic": self.svc.has_anthropic(),
            "agentic": agentic,
            "usage_accounted": true,
        });
        attach_usage(&mut result, usage);
        Ok(result)
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
    async fn defer(&self, id: Ulid, until: time::OffsetDateTime) -> Result<()> {
        Ok(self.svc.ai_jobs().defer(id, until).await?)
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

/// Whether a *successful* job of `kind` actually hit a paid upstream, used to
/// decide if it should charge the cost metric. Derived from the handler's result
/// shape so it stays in lockstep with the handlers:
/// - `Moderate` is a local stub → never paid.
/// - `Embed` skips the paid call on the idempotent no-op path, which the handler
///   marks with a `skipped` field → unpaid when that field is present.
/// - `Summarize` / `Answer` always make a completion call on success → paid.
///
/// Embed real Anthropic token [`Usage`] into a job's result JSON under a `usage`
/// key, so it is durably recorded by `AiJobRepo::complete` and queryable later
/// (usage is part of the persisted result). A no-op when `usage` is
/// `None` (the heuristic / no-Anthropic path made no paid call).
fn attach_usage(result: &mut serde_json::Value, usage: Option<Usage>) {
    if let (Some(obj), Some(u)) = (result.as_object_mut(), usage) {
        obj.insert(
            "usage".to_string(),
            serde_json::json!({
                "input_tokens": u.input_tokens,
                "output_tokens": u.output_tokens,
            }),
        );
    }
}

/// Read the real token usage back out of a completed job's result, if a handler
/// recorded it via [`attach_usage`]. Returns `None` when the result has no
/// `usage` object (heuristic path, stub kinds, idempotent skips), in which case
/// the worker falls back to the flat per-kind cost estimate.
#[must_use]
fn usage_from_result(result: &serde_json::Value) -> Option<Usage> {
    let u = result.get("usage")?;
    Some(Usage {
        input_tokens: u32::try_from(u.get("input_tokens")?.as_u64()?).ok()?,
        output_tokens: u32::try_from(u.get("output_tokens")?.as_u64()?).ok()?,
    })
}

#[must_use]
fn was_paid(kind: AiJobKind, result: &serde_json::Value) -> bool {
    match kind {
        // This is the compatibility finalizer for custom/legacy processors whose
        // result does not set `usage_accounted`. Provider-aware handlers reserve
        // and finalize durable usage themselves before returning their result.
        // Moderate is paid here only when such a processor reports Anthropic use.
        AiJobKind::Moderate | AiJobKind::Summarize | AiJobKind::Answer => result
            .get("anthropic")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        AiJobKind::Embed => result
            .get("paid_provider")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or_else(|| {
                result.get("skipped").is_none()
                    && result.get("model").and_then(serde_json::Value::as_str) != Some("hash-1024")
            }),
    }
}

/// The generic run-loop. Parameterised over [`JobQueue`] + [`JobProcessor`] so
/// the control flow (bounded concurrency, dead-letter guard, budget gating,
/// shutdown) is exercised by unit tests against in-memory fakes.
///
/// `reg` is the metrics registry (injectable for tests; the process-global one in
/// production) and `cost_model` the per-kind cost estimate.
#[allow(clippy::too_many_arguments)]
async fn run_loop<Q, P>(
    queue: &Q,
    proc: &P,
    budget: &CostBudget,
    keyed: &KeyedCostBudget<uuid::Uuid>,
    cfg: WorkerConfig,
    reg: &Registry,
    cost_model: &CostModel,
    usage_sink: Option<&dyn UsageSink>,
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

        // Per-workspace budget gate (ROADMAP 方向三): a job whose workspace has
        // exhausted its per-tenant window is *deferred* to the next window (not
        // dropped, not failed), so one busy/abusive tenant can't monopolise the
        // global window. Jobs without a workspace are governed by the global
        // budget only. `defer` returns the row to `queued` without burning a retry.
        let defer_until = time::OffsetDateTime::now_utc() + cfg.budget_window;
        let mut runnable = Vec::with_capacity(jobs.len());
        for job in jobs {
            match job.workspace_id {
                Some(ws) if !keyed.try_acquire(ws) => {
                    if let Err(e) = queue.defer(job.id, defer_until).await {
                        tracing::warn!(error = %e, job = %job.id, "defer (ws budget) failed; running");
                        runnable.push(job);
                    } else {
                        tracing::debug!(job = %job.id, "deferred: workspace budget window exhausted");
                    }
                }
                _ => runnable.push(job),
            }
        }
        if runnable.is_empty() {
            // Everything claimed this tick was deferred; back off briefly.
            if sleep_or_cancel(IDLE_SLEEP, shutdown).await {
                return;
            }
            continue;
        }

        // Charge the GLOBAL budget per job by its cost WEIGHT (ROADMAP 方向四):
        // an Answer consumes more of the window than a cheap Embed, so a tenant
        // can't run a window full of maxed-out completions for the price of
        // trivial embeds. A job the window can't currently afford is DEFERRED
        // (not dropped, not failed) to the next window, making the window a true
        // weighted cost ceiling rather than a flat call count. `try_acquire_n` is
        // all-or-nothing, so a partially-affordable job is never half-charged.
        let mut to_run = Vec::with_capacity(runnable.len());
        for job in runnable {
            let weight = cost_model.weight_for(job.kind);
            if budget.try_acquire_n(weight) {
                to_run.push(job);
            } else if let Err(e) = queue.defer(job.id, defer_until).await {
                tracing::warn!(error = %e, job = %job.id, "defer (global budget) failed; running");
                to_run.push(job);
            } else {
                tracing::debug!(job = %job.id, weight, "deferred: global budget window exhausted");
            }
        }
        if to_run.is_empty() {
            if sleep_or_cancel(IDLE_SLEEP, shutdown).await {
                return;
            }
            continue;
        }
        tracing::debug!(running = to_run.len(), "ai worker: batch claimed");

        // Queue-depth gauge: reflect the outstanding (claimed, not-yet-terminal)
        // work for this process. It rises with the claimed batch and returns to 0
        // once the batch has drained.
        ai_metrics::set_queue_depth(reg, to_run.len());
        process_batch(
            queue, proc, &sem, to_run, reg, cost_model, usage_sink, shutdown,
        )
        .await;
        ai_metrics::set_queue_depth(reg, 0);
    }
}

/// Process one claimed batch with bounded concurrency.
///
/// Concurrency is capped by `sem`. While waiting for a permit the in-flight set
/// is drained concurrently so the bound is a throughput limit, not a deadlock,
/// when the batch is larger than the permit count. On shutdown, already-started
/// jobs are awaited to a terminal state; no new jobs are started.
#[allow(clippy::too_many_arguments)]
async fn process_batch<Q, P>(
    queue: &Q,
    proc: &P,
    sem: &Arc<Semaphore>,
    jobs: Vec<AiJob>,
    reg: &Registry,
    cost_model: &CostModel,
    usage_sink: Option<&dyn UsageSink>,
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
            run_one(queue, proc, job, reg, cost_model, usage_sink).await;
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
/// transition (complete on success, fail/dead-letter on error), recording
/// duration / cost / outcome metrics into `reg` along the way.
async fn run_one<Q, P>(
    queue: &Q,
    proc: &P,
    job: AiJob,
    reg: &Registry,
    cost_model: &CostModel,
    usage_sink: Option<&dyn UsageSink>,
) where
    Q: JobQueue + ?Sized,
    P: JobProcessor + ?Sized,
{
    // Process-level timeout (9th analysis 方向五): a stuck Anthropic request
    // must not occupy a semaphore permit forever. When the timeout fires the
    // job is marked `failed` (not `dead`) so a retry can claim it next tick.
    const JOB_TIMEOUT: Duration = Duration::from_secs(120);
    let id = job.id;
    let kind = job.kind;
    let attempts = job.attempts;
    // Capture before `job` is moved into `process`; drives the per-workspace cost
    // label (方向三 per-tenant 成本指标 + 看板). `None` for legacy/system jobs.
    let workspace_id = job.workspace_id;

    // Dead-letter defense in depth: never *spend* on a job already past the cap.
    // The storage layer also dead-letters on fail(), so this is belt-and-braces
    // against a row that re-entered the queue out of band. No paid call happens,
    // so no duration/cost is recorded — only the dead-letter outcome.
    if is_over_attempt_cap(attempts) {
        tracing::warn!(job_id = %id, attempts, "ai worker: over attempt cap, dead-lettering without spend");
        ai_metrics::record_outcome(reg, kind, ai_metrics::OUTCOME_DEAD_LETTER);
        fail_job(queue, id, "exceeded MAX_ATTEMPTS").await;
        return;
    }
    // Time the actual processing — the histogram covers the work whether it
    // succeeds or fails (a slow failure is still a latency signal).
    let started = Instant::now();
    let disposition = match tokio::time::timeout(JOB_TIMEOUT, proc.process(job)).await {
        Ok(result) => {
            ai_metrics::record_duration(reg, kind, started.elapsed().as_secs_f64());
            match result {
                Ok(result) => Disposition::Done(result),
                Err(e) => Disposition::Failed(e.to_string()),
            }
        }
        Err(_elapsed) => {
            ai_metrics::record_duration(reg, kind, JOB_TIMEOUT.as_secs_f64());
            tracing::warn!(job_id = %id, kind = ?kind, "ai worker: job timed out after 120s");
            ai_metrics::record_outcome(reg, kind, ai_metrics::OUTCOME_FAILURE);
            fail_job(queue, id, "timed out after 120s").await;
            return;
        }
    };

    match disposition {
        Disposition::Done(result) => {
            // Provider-aware handlers reserve before the external call, finalize
            // its durable outcome, and return `usage_accounted=true`. This branch
            // remains a compatibility finalizer for legacy/custom processors:
            // prefer reported token usage, otherwise apply the coarse estimate,
            // with `was_paid` keeping stub/no-op paths at zero.
            let usage_id = ai_usage::usage_id_for_job(id);
            let already_accounted = result
                .get("usage_accounted")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let accounting = if already_accounted {
                Ok(ai_usage::UsagePersistOutcome::Duplicate)
            } else if let Some(usage) = usage_from_result(&result) {
                ai_usage::record_durable_token_cost(
                    usage_sink,
                    usage_id,
                    reg,
                    cost_model,
                    kind,
                    workspace_id,
                    usage.input_tokens,
                    usage.output_tokens,
                )
                .await
            } else {
                ai_usage::record_durable_cost(
                    usage_sink,
                    usage_id,
                    reg,
                    cost_model,
                    kind,
                    workspace_id,
                    was_paid(kind, &result),
                )
                .await
            };
            if let Err(error) = accounting {
                ai_metrics::record_outcome(reg, kind, ai_metrics::OUTCOME_FAILURE);
                tracing::error!(
                    job_id = %id,
                    usage_id = %usage_id,
                    error = %error,
                    "ai worker: compatibility cost finalization failed"
                );
                fail_job(queue, id, &format!("usage accounting failed: {error}")).await;
                return;
            }
            ai_metrics::record_outcome(reg, kind, ai_metrics::OUTCOME_SUCCESS);
            if let Err(e) = queue.complete(id, result).await {
                tracing::error!(job_id = %id, error = %e, "ai worker: completion write failed");
            } else {
                tracing::debug!(job_id = %id, ?kind, "ai worker: job done");
            }
        }
        Disposition::Failed(err) => {
            ai_metrics::record_outcome(reg, kind, ai_metrics::OUTCOME_FAILURE);
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
struct ModeratePayload {
    text: String,
}

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
pub mod tests;
