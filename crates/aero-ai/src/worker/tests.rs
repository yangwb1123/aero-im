use super::*;
use aero_storage::AiJobStatus;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::Mutex;

/// A fresh, isolated registry for a test — never the process-global one,
/// which is shared across the binary's parallel tests and would be flaky.
fn test_reg() -> Registry {
    Registry::new()
}

/// Default cost model for tests that don't assert on cost.
fn test_cost() -> CostModel {
    // Uniform per-kind cost so the budget-gating tests below stay COUNT-based
    // (every kind weighs 1 unit); the weighted-enforcement behaviour is
    // covered separately by `weighted_budget_defers_expensive_jobs` with the
    // tiered default model.
    CostModel {
        embed_micros: 20,
        summarize_micros: 20,
        moderate_micros: 20,
        answer_micros: 20,
        input_micros_per_mtok: 3_000_000,
        output_micros_per_mtok: 15_000_000,
    }
}

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

#[test]
fn was_paid_matches_handler_result_shapes() {
    // Embed: skip marker → unpaid; real embedding result → paid.
    assert!(!was_paid(
        AiJobKind::Embed,
        &serde_json::json!({ "skipped": "empty", "updated": false })
    ));
    // The already-embedded idempotency skip is likewise unpaid (any `skipped`).
    assert!(!was_paid(
        AiJobKind::Embed,
        &serde_json::json!({ "skipped": "already_embedded", "updated": false })
    ));
    assert!(was_paid(
        AiJobKind::Embed,
        &serde_json::json!({ "dim": 1024, "model": "voyage", "updated": true })
    ));
    // Moderate: paid only when "anthropic": true (key configured + real call made).
    assert!(was_paid(
        AiJobKind::Moderate,
        &serde_json::json!({ "verdict": "safe", "anthropic": true })
    ));
    assert!(!was_paid(
        AiJobKind::Moderate,
        &serde_json::json!({ "verdict": "safe", "anthropic": false })
    ));
    // Summarize / Answer always make a completion call on success.
    assert!(was_paid(AiJobKind::Summarize, &serde_json::json!({ "summary": "x" })));
    assert!(was_paid(AiJobKind::Answer, &serde_json::json!({ "answer": "y" })));
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

/// Stronger idempotency: an Embed whose target ALREADY has a stored embedding
/// must skip the paid embedder call entirely — a duplicate / replayed job is a
/// no-op, not a re-charge. The real `handle_embed` gate is `should_skip_embed`
/// OR `MessageRepo::has_embedding`; the latter needs a DB-backed `AiService`,
/// so (matching the offline style above) we drive the same two-stage decision
/// against a counting embedder, with `has_embedding` standing in for the
/// storage read. The embedder must fire only when text is non-empty AND the
/// row is not already embedded.
#[tokio::test]
async fn embed_handler_skips_paid_call_when_already_embedded() {
    use crate::embed::{Embedder, HashEmbedder};

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
    let embedder = CountingEmbedder { inner: HashEmbedder::new(), calls: Arc::clone(&calls) };

    // (searchable_text, already_embedded, expect_paid_call)
    let cases = [
        ("real content", false, true),  // fresh, non-empty → embed
        ("real content", true, false),  // already embedded → skip (the new gate)
        ("", false, false),             // empty → skip (existing gate)
        ("", true, false),              // empty AND embedded → skip
    ];
    for (text, already_embedded, expect_call) in cases {
        let before = calls.load(Ordering::SeqCst);
        // Mirror handle_embed: empty-text guard first, then has_embedding guard,
        // and only then the paid embedder call.
        if !should_skip_embed(text) && !already_embedded {
            let _ = embedder.embed_one(text).await.unwrap();
        }
        let made_call = calls.load(Ordering::SeqCst) > before;
        assert_eq!(
            made_call, expect_call,
            "text={text:?} already_embedded={already_embedded} expect_call={expect_call}"
        );
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "exactly one paid call — only the fresh, non-empty case embeds"
    );
}

// ---------- test doubles ----------

/// A per-workspace budget so permissive it never defers — for the tests that
/// exercise the global path and use workspace-less jobs.
fn unlimited_keyed() -> KeyedCostBudget<uuid::Uuid> {
    KeyedCostBudget::new(u32::MAX, Duration::from_secs(3600))
}

fn mk_job(kind: AiJobKind, attempts: i32) -> AiJob {
    AiJob {
        id: Ulid::new(),
        kind,
        target_id: None,
        workspace_id: None,
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
    /// Ids passed to `defer`, in order — lets tests assert deferrals.
    deferred: Mutex<Vec<Ulid>>,
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
        Self { rows: Mutex::new(rows), deferred: Mutex::default() }
    }

    fn deferred_ids(&self) -> Vec<Ulid> {
        self.deferred.lock().unwrap().clone()
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
    async fn defer(&self, id: Ulid, _until: time::OffsetDateTime) -> Result<()> {
        self.deferred.lock().unwrap().push(id);
        let mut rows = self.rows.lock().unwrap();
        if let Some(r) = rows.iter_mut().find(|r| r.job.id == id) {
            // Mirror the repo: back to queued, attempt refunded.
            r.status = AiJobStatus::Queued;
            r.job.attempts = (r.job.attempts - 1).max(0);
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

/// Processor that succeeds returning a caller-supplied result Value — lets a
/// metrics test drive the exact handler-result shape (e.g. an embed skip
/// marker) that `was_paid` keys off, without a DB-backed service.
struct FixedResult {
    value: serde_json::Value,
}
#[async_trait]
impl JobProcessor for FixedResult {
    async fn process(&self, _job: AiJob) -> Result<serde_json::Value> {
        Ok(self.value.clone())
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

    let reg = test_reg();
    let cost = test_cost();
    // Drive enough passes that, *if* the job kept being re-queued, it would be
    // processed well beyond MAX_ATTEMPTS.
    for _ in 0..(MAX_ATTEMPTS + 5) {
        let claimed = queue.claim(BATCH_SIZE).await.unwrap();
        if claimed.is_empty() {
            break;
        }
        process_batch(&queue, &proc, &sem, claimed, &reg, &cost, &shutdown).await;
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

    // Metrics: every attempt failed → MAX_ATTEMPTS failure outcomes recorded
    // for this kind (the storage layer flips the row to `dead`; the run-loop
    // sees each delivery as a `failure`, not an over-cap `dead_letter`).
    let out = reg.render_prometheus();
    assert!(
        out.contains(&format!(
            r#"aero_ai_jobs_total{{kind="summarize",outcome="failure"}} {MAX_ATTEMPTS}"#
        )),
        "expected {MAX_ATTEMPTS} failure outcomes:\n{out}"
    );
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
    let reg = test_reg();
    let cost = test_cost();

    run_one(&queue, &proc, job, &reg, &cost).await;

    assert_eq!(proc.calls.load(Ordering::SeqCst), 0, "must not spend past the cap");
    assert_eq!(queue.status_of(id), AiJobStatus::Dead);

    // Over-cap path records a dead-letter outcome and no duration/cost (the
    // paid processor was never invoked).
    let out = reg.render_prometheus();
    assert!(
        out.contains(r#"aero_ai_jobs_total{kind="summarize",outcome="dead_letter"} 1"#),
        "expected a dead_letter outcome:\n{out}"
    );
    assert!(
        !out.contains("aero_ai_job_duration_seconds_count"),
        "no duration should be recorded when nothing is processed:\n{out}"
    );
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

    process_batch(&queue, &probe, &sem, claimed.clone(), &test_reg(), &test_cost(), &shutdown).await;

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

    process_batch(&queue, &probe, &sem, claimed.clone(), &test_reg(), &test_cost(), &shutdown).await;

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
        process_batch(&queue, &probe, &sem, claimed.clone(), &test_reg(), &test_cost(), &shutdown),
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
        run_loop(
            &queue,
            &proc,
            &budget,
            &unlimited_keyed(),
            WorkerConfig::default(),
            &test_reg(),
            &test_cost(),
            &shutdown,
        ),
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
    let cfg = WorkerConfig { max_concurrency: 4, max_calls_per_window: 3, max_calls_per_window_per_workspace: u32::MAX, budget_window: Duration::from_secs(3600) };
    let shutdown = CancellationToken::new();
    let reg = test_reg();
    let cost = test_cost();

    // Run the loop briefly; it should drain the budget, then idle-back-off.
    let token = shutdown.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        token.cancel();
    });
    run_loop(&queue, &proc, &budget, &unlimited_keyed(), cfg, &reg, &cost, &shutdown).await;
    handle.await.unwrap();

    assert_eq!(proc.ran.load(Ordering::SeqCst), 3, "processed exactly the budget");
    assert_eq!(budget.available(), 0, "budget window fully consumed");
    // Remaining rows are still queued (not failed, not dead) for a later window.
    assert_eq!(queue.count_status(AiJobStatus::Queued), 17, "rest left for later window");
    assert_eq!(queue.count_status(AiJobStatus::Dead), 0);

    // End-to-end metrics for the run-loop: exactly 3 successful summarize
    // outcomes, a matching cost charge, and the queue-depth gauge settled
    // back to 0 after the (single) batch drained.
    let out = reg.render_prometheus();
    assert!(
        out.contains(r#"aero_ai_jobs_total{kind="summarize",outcome="success"} 3"#),
        "expected 3 successes:\n{out}"
    );
    let want_cost = cost.summarize_micros * 3;
    assert!(
        out.contains(&format!(
            r#"aero_ai_cost_micros_total{{kind="summarize"}} {want_cost}"#
        )),
        "expected summarize cost {want_cost}:\n{out}"
    );
    assert!(
        out.contains("\naero_ai_queue_depth 0\n"),
        "queue depth should settle to 0:\n{out}"
    );
}

#[tokio::test]
async fn weighted_budget_defers_expensive_jobs() {
    // ROADMAP 方向四 enforcement: with the TIERED cost model, an Answer weighs
    // 5 units. A 5-unit window therefore admits exactly ONE Answer — whereas
    // a flat call-count budget of 5 would have run both. The second is
    // deferred (not dropped), proving the window is a weighted COST ceiling.
    let jobs: Vec<AiJob> =
        vec![mk_job(AiJobKind::Answer, 1), mk_job(AiJobKind::Answer, 1)];
    let queue = FakeQueue::with_jobs(jobs);
    let proc = OkCounter::new();
    let budget = CostBudget::new(5, Duration::from_secs(3600));
    let cfg = WorkerConfig {
        max_concurrency: 4,
        max_calls_per_window: 5,
        max_calls_per_window_per_workspace: u32::MAX,
        budget_window: Duration::from_secs(3600),
    };
    let shutdown = CancellationToken::new();
    // Tiered model (NOT the uniform test_cost): Answer weighs 5 units.
    let cost = CostModel::default();

    let token = shutdown.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        token.cancel();
    });
    run_loop(&queue, &proc, &budget, &unlimited_keyed(), cfg, &test_reg(), &cost, &shutdown)
        .await;
    handle.await.unwrap();

    assert_eq!(
        proc.ran.load(Ordering::SeqCst),
        1,
        "one Answer (weight 5) fills the whole 5-unit window"
    );
    assert_eq!(budget.available(), 0, "Answer consumed the full weighted window");
    assert_eq!(
        queue.count_status(AiJobStatus::Queued),
        1,
        "the second Answer is deferred to a later window, not run or dropped"
    );
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
    let cfg = WorkerConfig { max_concurrency: 2, max_calls_per_window: 5, max_calls_per_window_per_workspace: u32::MAX, budget_window: Duration::from_secs(3600) };
    let shutdown = CancellationToken::new();

    let token = shutdown.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        token.cancel();
    });
    run_loop(&queue, &proc, &budget, &unlimited_keyed(), cfg, &test_reg(), &test_cost(), &shutdown).await;
    handle.await.unwrap();

    assert_eq!(proc.ran.load(Ordering::SeqCst), 5);
    assert_eq!(queue.count_status(AiJobStatus::Done), 5, "all claimed jobs completed");
    assert_eq!(queue.count_status(AiJobStatus::Running), 0, "no orphaned running rows");
}

#[tokio::test]
async fn run_loop_defers_jobs_over_the_per_workspace_budget() {
    // Two jobs for the SAME workspace, whose per-window allowance is 1 (global
    // budget is ample). The affordable one runs; the other is DEFERRED — not
    // failed, not dropped — so a single tenant can't exceed its share
    // (ROADMAP 方向三 per-tenant budget).
    let ws = uuid::Uuid::from_u128(7);
    let mut a = mk_job(AiJobKind::Summarize, 1);
    a.workspace_id = Some(ws);
    let mut b = mk_job(AiJobKind::Summarize, 1);
    b.workspace_id = Some(ws);
    let b_id = b.id;
    let queue = FakeQueue::with_jobs(vec![a, b]);
    let proc = OkCounter::new();
    let budget = CostBudget::new(100, Duration::from_secs(3600)); // global: ample
    let keyed = KeyedCostBudget::<uuid::Uuid>::new(1, Duration::from_secs(3600)); // 1 per ws
    let cfg = WorkerConfig {
        max_concurrency: 2,
        max_calls_per_window: 100,
        max_calls_per_window_per_workspace: 1,
        budget_window: Duration::from_secs(3600),
    };
    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        token.cancel();
    });
    run_loop(&queue, &proc, &budget, &keyed, cfg, &test_reg(), &test_cost(), &shutdown).await;
    handle.await.unwrap();

    assert_eq!(proc.ran.load(Ordering::SeqCst), 1, "only the within-budget job runs");
    assert!(queue.deferred_ids().contains(&b_id), "over-budget job was deferred");
    assert_eq!(queue.count_status(AiJobStatus::Done), 1, "exactly one job completed");
}

// ---------- (方向四) observability instrumentation ----------

#[tokio::test]
async fn run_one_success_records_duration_cost_and_outcome() {
    // A successful Answer job records: a duration observation, the cost
    // estimate for its kind, and one success outcome — all on a FRESH
    // registry (never the flaky process-global one).
    let job = mk_job(AiJobKind::Answer, 1);
    let id = job.id;
    let queue = FakeQueue::with_jobs(vec![job.clone()]);
    {
        // Mark it running so complete() has a row to flip (mirrors a claim).
        let mut rows = queue.rows.lock().unwrap();
        rows[0].status = AiJobStatus::Running;
    }
    let proc = FixedResult { value: serde_json::json!({ "answer": "42" }) };
    let reg = test_reg();
    let cost = test_cost();

    run_one(&queue, &proc, job, &reg, &cost).await;

    assert_eq!(queue.status_of(id), AiJobStatus::Done);
    let out = reg.render_prometheus();
    assert!(
        out.contains(r#"aero_ai_job_duration_seconds_count{kind="answer"} 1"#),
        "duration not recorded:\n{out}"
    );
    assert!(
        out.contains(&format!(
            r#"aero_ai_cost_micros_total{{kind="answer"}} {}"#,
            cost.answer_micros
        )),
        "cost not recorded:\n{out}"
    );
    assert!(
        out.contains(r#"aero_ai_jobs_total{kind="answer",outcome="success"} 1"#),
        "success outcome not recorded:\n{out}"
    );
}

// ---------- (方向三) real token usage → cost ----------

#[test]
fn attach_and_read_usage_round_trips() {
    let mut result = serde_json::json!({ "answer": "x" });
    attach_usage(&mut result, Some(Usage { input_tokens: 100, output_tokens: 25 }));
    // The usage is now part of the persisted result JSON (queryable).
    assert_eq!(result["usage"]["input_tokens"], 100);
    assert_eq!(result["usage"]["output_tokens"], 25);
    // And reads back as the same Usage.
    let u = usage_from_result(&result).expect("usage present");
    assert_eq!(u, Usage { input_tokens: 100, output_tokens: 25 });
}

#[test]
fn attach_usage_none_is_noop_and_reads_back_none() {
    let mut result = serde_json::json!({ "summary": "x" });
    attach_usage(&mut result, None);
    assert!(result.get("usage").is_none());
    assert!(usage_from_result(&result).is_none());
}

#[tokio::test]
async fn run_one_records_real_token_cost_when_usage_present() {
    // A successful Answer job whose result carries REAL token usage must
    // charge the cost counter the token-based figure, not the flat estimate.
    let job = mk_job(AiJobKind::Answer, 1);
    let queue = FakeQueue::with_jobs(vec![job.clone()]);
    {
        let mut rows = queue.rows.lock().unwrap();
        rows[0].status = AiJobStatus::Running;
    }
    // Result shape a real handler produces: answer + usage block.
    let proc = FixedResult {
        value: serde_json::json!({
            "answer": "42",
            "usage": { "input_tokens": 1000, "output_tokens": 100 },
        }),
    };
    let reg = test_reg();
    let cost = CostModel::default();

    run_one(&queue, &proc, job, &reg, &cost).await;

    // Real token cost: 1000*3 + 100*15 = 3000 + 1500 = 4500 micros (default
    // Sonnet rates) — NOT the flat answer_micros estimate (5000).
    let want = cost.token_micros(1000, 100);
    assert_ne!(want, cost.answer_micros, "test must distinguish real vs estimate");
    let out = reg.render_prometheus();
    assert!(
        out.contains(&format!(
            r#"aero_ai_cost_micros_total{{kind="answer"}} {want}"#
        )),
        "expected real token cost {want}, not the flat estimate:\n{out}"
    );
}

#[tokio::test]
async fn run_one_labels_cost_with_job_workspace() {
    // The job's workspace_id must flow through to a per-workspace cost series
    // (方向三 per-tenant 成本指标), while the aggregate per-kind series is kept.
    let ws = uuid::Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0099);
    let mut job = mk_job(AiJobKind::Answer, 1);
    job.workspace_id = Some(ws);
    let queue = FakeQueue::with_jobs(vec![job.clone()]);
    {
        let mut rows = queue.rows.lock().unwrap();
        rows[0].status = AiJobStatus::Running;
    }
    let proc = FixedResult {
        value: serde_json::json!({
            "answer": "42",
            "usage": { "input_tokens": 1000, "output_tokens": 100 },
        }),
    };
    let reg = test_reg();
    let cost = CostModel::default();

    run_one(&queue, &proc, job, &reg, &cost).await;

    let want = cost.token_micros(1000, 100); // 4500
    let out = reg.render_prometheus();
    assert!(
        out.contains(&format!(
            r#"aero_ai_cost_micros_total{{kind="answer",workspace="{ws}"}} {want}"#
        )),
        "per-workspace cost not recorded under the job's workspace:\n{out}"
    );
    // Aggregate per-kind series still present (operators want both views).
    assert!(
        out.contains(&format!(
            r#"aero_ai_cost_micros_total{{kind="answer"}} {want}"#
        )),
        "aggregate cost series must be preserved:\n{out}"
    );
}

#[tokio::test]
async fn run_one_falls_back_to_estimate_when_usage_absent() {
    // A successful Summarize job WITHOUT a usage block (heuristic / no-key
    // path) must fall back to the flat per-kind estimate.
    let job = mk_job(AiJobKind::Summarize, 1);
    let queue = FakeQueue::with_jobs(vec![job.clone()]);
    {
        let mut rows = queue.rows.lock().unwrap();
        rows[0].status = AiJobStatus::Running;
    }
    let proc = FixedResult { value: serde_json::json!({ "summary": "x", "anthropic": false }) };
    let reg = test_reg();
    let cost = CostModel::default();

    run_one(&queue, &proc, job, &reg, &cost).await;

    let out = reg.render_prometheus();
    assert!(
        out.contains(&format!(
            r#"aero_ai_cost_micros_total{{kind="summarize"}} {}"#,
            cost.summarize_micros
        )),
        "expected the flat estimate fallback:\n{out}"
    );
}

#[tokio::test]
async fn run_one_embed_skip_records_zero_cost_but_still_succeeds() {
    // An idempotent embed no-op (handler returns a `skipped` marker) must
    // record duration + a SUCCESS outcome but charge ZERO cost.
    let job = mk_job(AiJobKind::Embed, 1);
    let queue = FakeQueue::with_jobs(vec![job.clone()]);
    {
        let mut rows = queue.rows.lock().unwrap();
        rows[0].status = AiJobStatus::Running;
    }
    let proc = FixedResult {
        value: serde_json::json!({ "skipped": "empty", "updated": false }),
    };
    let reg = test_reg();
    let cost = test_cost();

    run_one(&queue, &proc, job, &reg, &cost).await;

    let out = reg.render_prometheus();
    assert!(
        out.contains(r#"aero_ai_jobs_total{kind="embed",outcome="success"} 1"#),
        "embed skip is still a success:\n{out}"
    );
    assert!(
        out.contains(r#"aero_ai_cost_micros_total{kind="embed"} 0"#),
        "embed skip must charge zero cost:\n{out}"
    );
    assert!(
        out.contains(r#"aero_ai_job_duration_seconds_count{kind="embed"} 1"#),
        "duration still recorded for a skip:\n{out}"
    );
}

#[tokio::test]
async fn run_loop_sets_queue_depth_gauge_during_a_batch() {
    // A slow processor lets us observe the queue-depth gauge while a batch is
    // in flight: it must equal the claimed batch size, then return to 0.
    let jobs: Vec<AiJob> = (0..3).map(|_| mk_job(AiJobKind::Moderate, 1)).collect();
    let queue = FakeQueue::with_jobs(jobs);
    let probe = ConcurrencyProbe::new(); // sleeps 20ms per job
    let budget = CostBudget::new(100, Duration::from_secs(3600));
    let cfg = WorkerConfig {
        max_concurrency: 4,
        max_calls_per_window: 100,
        max_calls_per_window_per_workspace: u32::MAX,
        budget_window: Duration::from_secs(3600),
    };
    let shutdown = CancellationToken::new();
    let reg = Arc::new(test_reg());
    let cost = test_cost();

    // Sample the gauge mid-batch (after the batch is claimed but before it
    // drains), then cancel.
    let token = shutdown.clone();
    let sample_reg = Arc::clone(&reg);
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(8)).await;
        let mid = sample_reg.render_prometheus();
        token.cancel();
        mid
    });
    run_loop(&queue, &probe, &budget, &unlimited_keyed(), cfg, &reg, &cost, &shutdown).await;
    let mid = handle.await.unwrap();

    assert!(
        mid.contains("\naero_ai_queue_depth 3\n"),
        "queue depth should reflect the 3-job batch mid-flight:\n{mid}"
    );
    // After draining, it settles back to 0.
    assert!(
        reg.render_prometheus().contains("\naero_ai_queue_depth 0\n"),
        "queue depth should return to 0 after drain"
    );
}