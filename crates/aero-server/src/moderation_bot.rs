//! AI content moderation (P5 AI 内容审核, hardened per ROADMAP3 方向三).
//!
//! Subscribes to `im.room.*`; for each new text message it asks the AI backend
//! to classify the content. Flagged messages are soft-deleted via
//! [`ImService::moderate_delete`](aero_im_core::ImService::moderate_delete),
//! which broadcasts a `Deleted` event so every client removes the message. The
//! delete and a `message.moderated` audit row (room, message id, model reason,
//! content digest) commit in ONE transaction (方向五 审计事务化) so a removal can
//! never succeed unaudited — it is always independently reviewable.
//!
//! Opt-in: only runs when an AI backend is configured **and**
//! `AERO_AI_MODERATION` is set — it can spend one LLM call per message, so it is
//! off by default. The synchronous `AERO_BLOCKED_WORDS` keyword pre-filter in
//! `ImService::send_message` is independent and always active.
//!
//! ## Cost governance (方向三)
//!
//! The naive form of this bot made one **paid** LLM call per message with no
//! ceiling — a message flood meant unbounded spend. The pipeline is now:
//!
//! ```text
//! bus consumer ──try_send──▶ bounded mpsc queue ──▶ N workers ──▶ ai.moderate()
//!   (never blocks)             (AERO_AI_MODERATION_QUEUE)   ▲ per-ws + global budget
//! ```
//!
//! - **Bounded queue** — the bus consumer only ever `try_send`s; a full queue
//!   skips the message instead of back-pressuring the shared NATS consumer.
//! - **Bounded concurrency** — [`ModerationConfig::concurrency`] worker tasks,
//!   each processing one message at a time.
//! - **Budget gates** — every call must pass BOTH a global
//!   [`CostBudget`] window and a per-workspace [`KeyedCostBudget`] window
//!   (the room's tenant, resolved via `RoomRepo::room_workspace`), mirroring
//!   the `aero-ai` worker's per-ws gating. Either window exhausted → skip.
//!
//! **Trade-off (deliberate):** moderation here is best-effort *screening*, not a
//! delivery gate — messages are already delivered when this bot sees them, so a
//! skipped message is simply not screened (counted in
//! [`AI_MODERATION_SKIPPED_TOTAL`] with a `reason` label and debug-logged), and
//! delivery latency is never coupled to LLM latency or spend ceilings.

use std::sync::Arc;
use std::time::Duration;

use aero_ai::{CostBudget, KeyedCostBudget};
use aero_common::{metrics, MessageId, RoomEvent, RoomId, WorkspaceId};
use aero_im_core::moderation_text;
use aero_storage::ConsumerEventReceiptRepo;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    state::{AiBackend, AppState},
    task_shutdown::{self, NextOrCancelled},
};

/// Counter: moderation LLM calls attempted (admitted past every gate). The
/// hard per-window spend signal for this pipeline.
pub const AI_MODERATION_CALLS_TOTAL: &str = "aero_ai_moderation_calls_total";
/// Counter: flagged verdicts (calls that returned a block reason).
pub const AI_MODERATION_FLAGGED_TOTAL: &str = "aero_ai_moderation_flagged_total";
/// Counter: messages NOT screened, labeled by `reason`
/// (`queue_full` / `queue_closed` / `global_budget` / `workspace_budget` /
/// `workspace_unresolvable`).
pub const AI_MODERATION_SKIPPED_TOTAL: &str = "aero_ai_moderation_skipped_total";

/// Tunables for the moderation pipeline, sourced from `AERO_AI_MODERATION_*`
/// env vars (same flat prefix as the `AERO_AI_MODERATION` opt-in switch).
#[derive(Debug, Clone, Copy)]
pub struct ModerationConfig {
    /// Bounded work-queue capacity (`AERO_AI_MODERATION_QUEUE`, default 512).
    /// When full, new messages are skipped — the bus consumer never blocks.
    pub queue_capacity: usize,
    /// Number of worker tasks draining the queue
    /// (`AERO_AI_MODERATION_CONCURRENCY`, default 2). Each processes one
    /// message at a time, so this is also the LLM-call concurrency bound.
    pub concurrency: usize,
    /// Global ceiling on moderation calls per window
    /// (`AERO_AI_MODERATION_MAX_PER_WINDOW`, default 300).
    pub max_calls_per_window: u32,
    /// Per-workspace ceiling per window (`AERO_AI_MODERATION_PER_WS_WINDOW`,
    /// default 60) — one flooding tenant cannot drain the global window.
    pub per_ws_calls_per_window: u32,
    /// Length of both budget windows (`AERO_AI_MODERATION_WINDOW_SECS`,
    /// default 60).
    pub window: Duration,
}

impl Default for ModerationConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 512,
            concurrency: 2,
            max_calls_per_window: 300,
            per_ws_calls_per_window: 60,
            window: Duration::from_secs(60),
        }
    }
}

impl ModerationConfig {
    /// Load tunables from the environment, falling back to [`Default`] for any
    /// unset or unparseable key. Never fails — a misconfigured value degrades to
    /// the safe default rather than panicking the listener at startup.
    #[must_use]
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            queue_capacity: env_parse::<usize>("AERO_AI_MODERATION_QUEUE")
                .filter(|&n| n >= 1)
                .unwrap_or(d.queue_capacity),
            concurrency: env_parse::<usize>("AERO_AI_MODERATION_CONCURRENCY")
                .filter(|&n| n >= 1)
                .unwrap_or(d.concurrency),
            max_calls_per_window: env_parse::<u32>("AERO_AI_MODERATION_MAX_PER_WINDOW")
                .filter(|&n| n >= 1)
                .unwrap_or(d.max_calls_per_window),
            per_ws_calls_per_window: env_parse::<u32>("AERO_AI_MODERATION_PER_WS_WINDOW")
                .filter(|&n| n >= 1)
                .unwrap_or(d.per_ws_calls_per_window),
            window: env_parse::<u64>("AERO_AI_MODERATION_WINDOW_SECS")
                .filter(|&n| n >= 1)
                .map_or(d.window, Duration::from_secs),
        }
    }
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok()?.trim().parse().ok()
}

/// One unit of queued moderation work. The text is captured at enqueue time —
/// the message body may be edited or cleared before a worker gets to it, and
/// screening the content as delivered is the point.
#[derive(Debug)]
struct ModerationJob {
    message_id: MessageId,
    room_id: RoomId,
    text: String,
}

/// Why a message was skipped (not screened). Drives the `reason` label on
/// [`AI_MODERATION_SKIPPED_TOTAL`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkipReason {
    /// The bounded work queue was full at enqueue time.
    QueueFull,
    /// The queue receiver is gone (listener shutting down).
    QueueClosed,
    /// The global per-window call budget is exhausted.
    GlobalBudget,
    /// The message's workspace exhausted its per-tenant window.
    WorkspaceBudget,
    /// The room's workspace could not be resolved, so a flagged message must
    /// NOT be deleted un-audited (R-D1 parity — `moderate_delete(None)` would
    /// commit the soft delete + `Deleted` broadcast with ZERO `audit_events`
    /// rows). The message stays visible; fail-closed content safety.
    WorkspaceUnresolvable,
}

impl SkipReason {
    fn label(self) -> &'static str {
        match self {
            Self::QueueFull => "queue_full",
            Self::QueueClosed => "queue_closed",
            Self::GlobalBudget => "global_budget",
            Self::WorkspaceBudget => "workspace_budget",
            Self::WorkspaceUnresolvable => "workspace_unresolvable",
        }
    }
}

/// Queue admission: non-blocking enqueue so the bus consumer can never be
/// stalled by a slow LLM. A full (or closed) queue is a skip, not back-pressure.
fn try_enqueue(tx: &mpsc::Sender<ModerationJob>, job: ModerationJob) -> Result<(), SkipReason> {
    tx.try_send(job).map_err(|e| match e {
        mpsc::error::TrySendError::Full(_) => SkipReason::QueueFull,
        mpsc::error::TrySendError::Closed(_) => SkipReason::QueueClosed,
    })
}

/// Budget-gate decision: a call is admitted only when BOTH the message's
/// workspace window (if the workspace is known) and the global window grant a
/// unit. The per-ws gate is checked first so a throttled tenant does not burn
/// global budget it can't use; a workspace-less message (room gone, lookup
/// failed) is governed by the global window alone — same policy as the
/// `aero-ai` worker's per-ws job gating.
fn admit_call(
    global: &CostBudget,
    per_ws: &KeyedCostBudget<WorkspaceId>,
    workspace: Option<WorkspaceId>,
) -> Result<(), SkipReason> {
    if let Some(ws) = workspace {
        if !per_ws.try_acquire(ws) {
            return Err(SkipReason::WorkspaceBudget);
        }
    }
    if !global.try_acquire() {
        return Err(SkipReason::GlobalBudget);
    }
    Ok(())
}

/// Char-boundary-safe 120-char content summary recorded in the audit trail —
/// mirrors the `message.deleted` audit digest in `routes.rs` so both deletion
/// audit shapes review the same way.
fn content_digest(text: &str) -> String {
    text.chars().take(120).collect()
}

fn record_skip(reason: SkipReason, message_id: MessageId) {
    metrics::inc_counter_labeled(
        AI_MODERATION_SKIPPED_TOTAL,
        1,
        &[("reason", reason.label())],
    );
    debug!(%message_id, reason = reason.label(), "moderation: skipped (best-effort screening)");
}

/// Run the moderation listener with env-derived tunables until the bus stream
/// ends. No-op (returns `Ok`) when no AI backend is configured.
pub async fn run(state: AppState) -> anyhow::Result<()> {
    run_until_cancelled(state, CancellationToken::new()).await
}

/// Run with env-derived tunables until `cancel` is triggered.
pub async fn run_until_cancelled(state: AppState, cancel: CancellationToken) -> anyhow::Result<()> {
    run_with_config_until_cancelled(state, ModerationConfig::from_env(), cancel).await
}

/// Run the moderation listener with an explicit [`ModerationConfig`].
pub async fn run_with_config(state: AppState, cfg: ModerationConfig) -> anyhow::Result<()> {
    run_with_config_until_cancelled(state, cfg, CancellationToken::new()).await
}

/// Run with an explicit config until `cancel` is triggered. Workers stop
/// accepting queued work on cancellation, finish at most their current job,
/// and are joined before this function returns.
pub async fn run_with_config_until_cancelled(
    state: AppState,
    cfg: ModerationConfig,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    // In-process idempotency for JetStream at-least-once REDELIVERY: the same message
    // re-delivered (e.g. an ack timeout while this process is alive) would be
    // re-screened — a SECOND paid `ai.moderate()` call + a second cost-counter
    // increment + a duplicate `message.moderated` audit row. A bounded recently-seen
    // set skips the re-screen. (It does not survive a process restart; the per-call
    // budget admission gate bounds that residual double-charge.)
    const DEDUP_CAP: usize = 8192;
    let Some(ai) = state.ai.clone() else {
        info!("moderation_bot: no AI backend; not started");
        return Ok(());
    };

    let reg = metrics::global();
    reg.register_help(
        AI_MODERATION_CALLS_TOTAL,
        metrics::MetricKind::Counter,
        "AI moderation LLM calls attempted",
    );
    reg.register_help(
        AI_MODERATION_FLAGGED_TOTAL,
        metrics::MetricKind::Counter,
        "AI moderation flagged (blocked) verdicts",
    );
    reg.register_help(
        AI_MODERATION_SKIPPED_TOTAL,
        metrics::MetricKind::Counter,
        "Messages not screened by AI moderation, by reason",
    );

    let global_budget = Arc::new(CostBudget::new(cfg.max_calls_per_window, cfg.window));
    let ws_budget = Arc::new(KeyedCostBudget::<WorkspaceId>::new(
        cfg.per_ws_calls_per_window,
        cfg.window,
    ));

    let (tx, rx) = mpsc::channel::<ModerationJob>(cfg.queue_capacity.max(1));
    // Workers share one receiver behind a Mutex: each takes the lock only for
    // the duration of one `recv`, so items are handed out one-at-a-time while
    // processing runs concurrently across workers.
    let rx = Arc::new(Mutex::new(rx));
    let mut workers = Vec::with_capacity(cfg.concurrency.max(1));
    for _ in 0..cfg.concurrency.max(1) {
        workers.push(tokio::spawn(worker_loop(
            state.clone(),
            Arc::clone(&ai),
            Arc::clone(&global_budget),
            Arc::clone(&ws_budget),
            Arc::clone(&rx),
            cancel.clone(),
        )));
    }

    let bus = state.bus.clone();
    let receipts = ConsumerEventReceiptRepo::new(state.pg.clone());
    info!(
        queue = cfg.queue_capacity,
        concurrency = cfg.concurrency,
        max_calls_per_window = cfg.max_calls_per_window,
        per_ws_calls_per_window = cfg.per_ws_calls_per_window,
        window_secs = cfg.window.as_secs(),
        "moderation_bot listener started"
    );
    // Resubscribe across NATS reconnects. The worker pool + queue (`tx`) persist
    // (never dropped) so a dropped subscription stream never permanently stops
    // moderation. Durable consumer "aero-moderation" resumes from its cursor; every
    // message is acked (skips are deliberately not redelivered).
    //
    // (The DEDUP_CAP rationale lives at the fn top with the constant.)
    let mut seen: std::collections::HashSet<aero_common::MessageId> =
        std::collections::HashSet::new();
    let mut seen_order: std::collections::VecDeque<aero_common::MessageId> =
        std::collections::VecDeque::new();
    'listener: loop {
        let subscribed = task_shutdown::subscribe_or_cancelled(
            &bus,
            "im.room.*",
            Some("aero-moderation"),
            &cancel,
        )
        .await;
        let mut stream = match subscribed {
            None => break 'listener,
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                warn!(error = %e, "moderation_bot subscribe failed; retrying");
                if task_shutdown::delay_or_cancelled(std::time::Duration::from_secs(1), &cancel)
                    .await
                {
                    break 'listener;
                }
                continue;
            }
        };
        loop {
            let sub = match task_shutdown::next_or_cancelled(&mut stream, &cancel).await {
                NextOrCancelled::Item(sub) => sub,
                NextOrCancelled::Ended => break,
                NextOrCancelled::Cancelled => break 'listener,
            };
            let event = serde_json::from_slice::<RoomEvent>(sub.payload());
            let handler_tx = &tx;
            let handler_seen = &mut seen;
            let handler_seen_order = &mut seen_order;
            // For this explicitly best-effort pipeline, queue admission (including
            // a budget/capacity skip) is the consumer action protected by the
            // receipt. Workers remain detached from the broker ACK exactly as
            // before, but a completed event can no longer enqueue a second paid
            // call after a late producer replay.
            let _ = crate::consumer_event_receipt::process(
                &receipts,
                "aero-moderation",
                sub,
                || async move {
                    if let Ok(RoomEvent::Message(env)) = event {
                        let text = moderation_text(&env.message.blocks);
                        if !text.trim().is_empty() {
                            let job = ModerationJob {
                                message_id: env.message.id,
                                room_id: env.message.room_id,
                                text,
                            };
                            let id = job.message_id;
                            if !handler_seen.contains(&id) {
                                if handler_seen_order.len() >= DEDUP_CAP {
                                    if let Some(old) = handler_seen_order.pop_front() {
                                        handler_seen.remove(&old);
                                    }
                                }
                                handler_seen.insert(id);
                                handler_seen_order.push_back(id);
                                if let Err(reason) = try_enqueue(handler_tx, job) {
                                    record_skip(reason, id);
                                }
                            }
                        }
                    }
                    Ok(())
                },
            )
            .await;
        }
        if cancel.is_cancelled() {
            break;
        }
        warn!("moderation_bot subscription stream ended; resubscribing");
        if task_shutdown::delay_or_cancelled(std::time::Duration::from_secs(1), &cancel).await {
            break;
        }
    }

    // No detached workers survive the tracked listener. Cancellation makes
    // idle workers leave immediately and lets in-flight jobs finish; queued
    // best-effort screening work is intentionally discarded.
    drop(tx);
    for worker in workers {
        if let Err(e) = worker.await {
            warn!(error = ?e, "moderation worker task failed during shutdown");
        }
    }
    Ok(())
}

/// One worker: pull jobs off the shared queue until it closes.
async fn worker_loop(
    state: AppState,
    ai: Arc<dyn AiBackend>,
    global: Arc<CostBudget>,
    per_ws: Arc<KeyedCostBudget<WorkspaceId>>,
    rx: Arc<Mutex<mpsc::Receiver<ModerationJob>>>,
    cancel: CancellationToken,
) {
    loop {
        let job = recv_job_or_cancelled(&rx, &cancel).await;
        let Some(job) = job else { return };
        process(&state, ai.as_ref(), &global, &per_ws, job).await;
    }
}

async fn recv_job_or_cancelled(
    rx: &Arc<Mutex<mpsc::Receiver<ModerationJob>>>,
    cancel: &CancellationToken,
) -> Option<ModerationJob> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        job = async { rx.lock().await.recv().await } => job,
    }
}

/// Gate → moderate → (on a flagged verdict) delete + audit, for one message.
async fn process(
    state: &AppState,
    ai: &dyn AiBackend,
    global: &CostBudget,
    per_ws: &KeyedCostBudget<WorkspaceId>,
    job: ModerationJob,
) {
    // Resolve the room's tenant for the per-workspace budget key. A lookup
    // failure degrades to global-only gating (the message still gets screened
    // if the global window allows) rather than skipping outright.
    let workspace = match state.rooms.room_workspace(job.room_id).await {
        Ok(ws) => ws,
        Err(e) => {
            debug!(error = ?e, room_id = %job.room_id, "moderation: workspace lookup failed; global gate only");
            None
        }
    };

    if let Err(reason) = admit_call(global, per_ws, workspace) {
        record_skip(reason, job.message_id);
        return;
    }

    metrics::inc_counter(AI_MODERATION_CALLS_TOTAL, 1);
    let verdict = ai
        .moderate_with_context(
            &job.text,
            aero_ai::usage::UsageContext::for_message(
                job.message_id.to_uuid(),
                workspace.map(|value| value.to_uuid()),
            ),
        )
        .await;
    match verdict {
        Ok(Some(reason)) => {
            metrics::inc_counter(AI_MODERATION_FLAGGED_TOTAL, 1);
            let digest = content_digest(&job.text);
            // R-D1 parity (`aero-ai` worker `moderation_delete_workspace`):
            // refuse the delete when the room's workspace is unresolvable. The
            // audit action is derived from the workspace, so `None` would commit
            // the removal with zero `audit_events` + zero governance rows (0239
            // never fires) — an invisible, un-audited removal. The message stays
            // visible, same fail-closed terminal as the 0239 binding RAISE.
            let Some(workspace) = workspace else {
                record_skip(SkipReason::WorkspaceUnresolvable, job.message_id);
                return;
            };
            if let Err(e) = state
                .im
                .moderate_delete(job.message_id, Some(workspace), &reason, &digest)
                .await
            {
                warn!(error = ?e, message_id = %job.message_id, "moderate_delete failed");
            }
        }
        Ok(None) => {}
        Err(e) => warn!(error = %e, message_id = %job.message_id, "ai.moderate failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The bus/DB/LLM edges are integration-tested elsewhere; these lock in the
    // pure decision logic the pipeline's spend ceiling depends on.

    fn cfg_budgets(global_max: u32, ws_max: u32) -> (CostBudget, KeyedCostBudget<WorkspaceId>) {
        (
            CostBudget::new(global_max, Duration::from_secs(3600)),
            KeyedCostBudget::new(ws_max, Duration::from_secs(3600)),
        )
    }

    #[test]
    fn config_defaults_are_sane() {
        let c = ModerationConfig::default();
        assert_eq!(c.queue_capacity, 512);
        assert_eq!(c.concurrency, 2);
        assert_eq!(c.per_ws_calls_per_window, 60);
        assert!(c.max_calls_per_window >= c.per_ws_calls_per_window);
        assert_eq!(c.window, Duration::from_secs(60));
    }

    // ---------- budget-gate decision ----------

    #[test]
    fn admit_within_both_budgets() {
        let (global, per_ws) = cfg_budgets(10, 10);
        let ws = WorkspaceId::new();
        assert_eq!(admit_call(&global, &per_ws, Some(ws)), Ok(()));
        assert_eq!(global.used(), 1, "admitted call charges the global window");
        assert_eq!(
            per_ws.used(&ws),
            1,
            "admitted call charges the tenant window"
        );
    }

    #[test]
    fn workspace_budget_exhaustion_skips_without_burning_global() {
        let (global, per_ws) = cfg_budgets(10, 2);
        let ws = WorkspaceId::new();
        assert_eq!(admit_call(&global, &per_ws, Some(ws)), Ok(()));
        assert_eq!(admit_call(&global, &per_ws, Some(ws)), Ok(()));
        // Tenant window exhausted: denied, and the global window must NOT be
        // charged for a call that never happens.
        assert_eq!(
            admit_call(&global, &per_ws, Some(ws)),
            Err(SkipReason::WorkspaceBudget)
        );
        assert_eq!(global.used(), 2, "denied tenant burned no global budget");
    }

    #[test]
    fn one_flooding_workspace_does_not_starve_another() {
        let (global, per_ws) = cfg_budgets(10, 2);
        let noisy = WorkspaceId::new();
        let quiet = WorkspaceId::new();
        assert_eq!(admit_call(&global, &per_ws, Some(noisy)), Ok(()));
        assert_eq!(admit_call(&global, &per_ws, Some(noisy)), Ok(()));
        assert_eq!(
            admit_call(&global, &per_ws, Some(noisy)),
            Err(SkipReason::WorkspaceBudget)
        );
        // The quiet tenant still has its full independent window.
        assert_eq!(admit_call(&global, &per_ws, Some(quiet)), Ok(()));
    }

    #[test]
    fn global_budget_exhaustion_skips() {
        let (global, per_ws) = cfg_budgets(2, 10);
        let a = WorkspaceId::new();
        let b = WorkspaceId::new();
        assert_eq!(admit_call(&global, &per_ws, Some(a)), Ok(()));
        assert_eq!(admit_call(&global, &per_ws, Some(b)), Ok(()));
        // Global window exhausted: every tenant is denied, regardless of its
        // own remaining per-ws budget.
        assert_eq!(
            admit_call(&global, &per_ws, Some(a)),
            Err(SkipReason::GlobalBudget)
        );
        assert_eq!(
            admit_call(&global, &per_ws, None),
            Err(SkipReason::GlobalBudget)
        );
    }

    #[test]
    fn workspace_less_message_is_global_gated_only() {
        let (global, per_ws) = cfg_budgets(3, 1);
        // No workspace key → per-ws windows untouched, only global charged.
        assert_eq!(admit_call(&global, &per_ws, None), Ok(()));
        assert_eq!(admit_call(&global, &per_ws, None), Ok(()));
        assert_eq!(global.used(), 2);
        // A real workspace still has its own full (independent) window.
        assert_eq!(
            admit_call(&global, &per_ws, Some(WorkspaceId::new())),
            Ok(())
        );
        assert_eq!(
            admit_call(&global, &per_ws, None),
            Err(SkipReason::GlobalBudget)
        );
    }

    // ---------- queue admission ----------

    fn mk_job() -> ModerationJob {
        ModerationJob {
            message_id: MessageId::new(),
            room_id: RoomId::new(),
            text: "hi".into(),
        }
    }

    #[test]
    fn enqueue_skips_with_queue_full_at_capacity() {
        // try_send is synchronous — no runtime needed.
        let (tx, mut rx) = mpsc::channel::<ModerationJob>(2);
        assert_eq!(try_enqueue(&tx, mk_job()), Ok(()));
        assert_eq!(try_enqueue(&tx, mk_job()), Ok(()));
        assert_eq!(
            try_enqueue(&tx, mk_job()),
            Err(SkipReason::QueueFull),
            "third enqueue over capacity 2 must skip, never block"
        );
        // Draining one slot re-admits.
        let _ = rx.try_recv().unwrap();
        assert_eq!(try_enqueue(&tx, mk_job()), Ok(()));
    }

    #[test]
    fn enqueue_skips_with_queue_closed_after_receiver_drop() {
        let (tx, rx) = mpsc::channel::<ModerationJob>(2);
        drop(rx);
        assert_eq!(try_enqueue(&tx, mk_job()), Err(SkipReason::QueueClosed));
    }

    #[tokio::test]
    async fn idle_worker_receive_stops_promptly_on_cancellation() {
        let (_tx, rx) = mpsc::channel::<ModerationJob>(1);
        let rx = Arc::new(Mutex::new(rx));
        let cancel = CancellationToken::new();
        let task_rx = Arc::clone(&rx);
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move { recv_job_or_cancelled(&task_rx, &task_cancel).await });

        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("idle worker should stop promptly")
            .expect("worker wait should not panic");
        assert!(result.is_none());
    }

    // ---------- labels / digest ----------

    #[test]
    fn skip_reason_labels_are_stable_and_distinct() {
        let all = [
            SkipReason::QueueFull,
            SkipReason::QueueClosed,
            SkipReason::GlobalBudget,
            SkipReason::WorkspaceBudget,
        ];
        let labels: Vec<_> = all.iter().map(|r| r.label()).collect();
        assert_eq!(
            labels,
            [
                "queue_full",
                "queue_closed",
                "global_budget",
                "workspace_budget"
            ]
        );
        let mut dedup = labels.clone();
        dedup.sort_unstable();
        dedup.dedup();
        assert_eq!(dedup.len(), labels.len(), "labels must be distinct");
    }

    #[test]
    fn content_digest_truncates_on_char_boundaries() {
        assert_eq!(content_digest("hello"), "hello");
        // 200 multibyte chars → exactly 120 chars kept, no boundary panic.
        let long: String = "审".repeat(200);
        let d = content_digest(&long);
        assert_eq!(d.chars().count(), 120);
        assert!(d.chars().all(|c| c == '审'));
    }
}
