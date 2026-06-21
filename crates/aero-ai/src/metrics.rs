//! AI-worker observability: cost model + per-job metric recording.
//!
//! ROADMAP 方向四 (observability) calls out AI **latency / cost / queue depth**
//! as key signals for a realtime `SaaS` that "cannot fly blind". The metrics
//! *foundation* (registry, Prometheus exposition, well-known name constants)
//! already lives in [`aero_common::metrics`]; this module is the AI-worker's
//! **emit layer** on top of it.
//!
//! ## What this records
//!
//! - **Job duration** → [`names::AI_JOB_DURATION_SECONDS`] histogram, labeled by
//!   `kind`. Timed around each `process(job)`.
//! - **Cost** → [`names::AI_COST_MICROS_TOTAL`] counter, labeled by `kind`. A
//!   *coarse, configurable* per-kind micro-USD estimate (see [`CostModel`]) — it
//!   is an order-of-magnitude budget signal for dashboards / alerts, **not** a
//!   billing-grade figure (real spend depends on token counts the worker does
//!   not surface). Skipped / unpaid work records zero.
//! - **Outcomes** → [`AI_JOBS_TOTAL`] counter, labeled by `kind` + `outcome`
//!   (`success` / `failure` / `dead_letter`).
//!
//! [`names::AI_QUEUE_DEPTH`] (a gauge) is set by the worker run-loop directly via
//! [`set_queue_depth`] as batches are claimed / drained; it needs no per-job
//! recorder.
//!
//! ## Testability
//!
//! Every recorder takes an injectable `&Registry`, so unit tests target a
//! **fresh** [`aero_common::metrics::Registry::new()`] (never the flaky
//! process-global one shared across parallel tests) and assert on counter values
//! / `render_prometheus()` output. Production wires the recorders to
//! [`aero_common::metrics::global`].

use aero_common::metrics::{names, Registry};
use aero_storage::AiJobKind;

/// Local metric name for AI job outcomes (success / failure / dead-letter).
///
/// Deliberately **not** added to `aero_common` (that crate is a shared leaf and
/// out of scope to modify here): it is AI-worker-specific. It still follows the
/// same conventions — `aero_` namespace, `snake_case`, `_total` counter suffix —
/// and carries `kind` + `outcome` labels.
pub const AI_JOBS_TOTAL: &str = "aero_ai_jobs_total";

/// Stable `outcome` label for a job that completed successfully.
pub const OUTCOME_SUCCESS: &str = "success";
/// Stable `outcome` label for a job that failed but may be retried.
pub const OUTCOME_FAILURE: &str = "failure";
/// Stable `outcome` label for a job dead-lettered (terminal, over the cap).
pub const OUTCOME_DEAD_LETTER: &str = "dead_letter";

/// The `kind` label value for a job, mirroring the `snake_case` strings the
/// storage layer persists (`ai_job.rs`). Returns a `'static str` so it can be
/// used directly as a label without allocating.
#[must_use]
pub fn kind_label(kind: AiJobKind) -> &'static str {
    match kind {
        AiJobKind::Embed => "embed",
        AiJobKind::Summarize => "summarize",
        AiJobKind::Moderate => "moderate",
        AiJobKind::Answer => "answer",
    }
}

/// Coarse per-kind cost estimate in **micro-USD** (1 USD = `1_000_000` micros).
///
/// These are intentionally rough, order-of-magnitude figures so the
/// [`names::AI_COST_MICROS_TOTAL`] counter gives operators a *budget* signal
/// (a runaway retry storm or abusive tenant shows up as a cost spike) without
/// pretending to be invoice-accurate. Real spend depends on token counts the
/// worker does not currently surface; when it does, this can be refined to a
/// token-based estimate. Values are configurable so deployments can tune them to
/// their actual provider pricing.
///
/// Defaults reflect the relative shape of the upstreams the worker calls:
/// - `Embed` — one Voyage embedding request (cheap).
/// - `Summarize` / `Answer` — an Anthropic Messages completion (the expensive
///   ones; `Answer` also runs retrieval but the LLM call dominates).
/// - `Moderate` — a real paid classification call when an Anthropic key is
///   configured (`was_paid` is true), so it carries a non-zero estimate (方向四).
#[derive(Debug, Clone, Copy)]
pub struct CostModel {
    /// Estimated micro-USD for an `Embed` job.
    pub embed_micros: u64,
    /// Estimated micro-USD for a `Summarize` job.
    pub summarize_micros: u64,
    /// Estimated micro-USD for a `Moderate` job.
    pub moderate_micros: u64,
    /// Estimated micro-USD for an `Answer` job.
    pub answer_micros: u64,
    /// Micro-USD per 1M **input** tokens for the Anthropic completion model.
    /// Used by [`Self::token_micros`] to compute REAL cost from the token counts
    /// the Anthropic response reports, replacing the flat per-kind estimate when
    /// usage is available (方向三 AI cost realism). Default tracks Claude Sonnet
    /// 4.6 list pricing ($3 / 1M input).
    pub input_micros_per_mtok: u64,
    /// Micro-USD per 1M **output** tokens. Default tracks Claude Sonnet 4.6 list
    /// pricing ($15 / 1M output).
    pub output_micros_per_mtok: u64,
}

impl Default for CostModel {
    fn default() -> Self {
        Self {
            // ~$0.00002 per embedding request — Voyage-scale, coarse.
            embed_micros: 20,
            // ~$0.003 per summary completion — Anthropic-scale, coarse.
            summarize_micros: 3_000,
            // ~$0.001 per moderation call (方向四): a small classification
            // completion (~200-token system+content input, short verdict output)
            // on Sonnet pricing. NOT zero — when a key is configured the worker's
            // `was_paid(Moderate)` is true (a real Anthropic call was made), so a
            // 0 estimate made the highest-volume paid path invisible on the cost
            // dashboard. Real token usage, when surfaced, overrides this estimate.
            moderate_micros: 1_000,
            // ~$0.005 per answer (retrieval + a larger completion), coarse.
            answer_micros: 5_000,
            // Claude Sonnet 4.6 list price: $3 / 1M input, $15 / 1M output
            // (= 3_000_000 / 15_000_000 micro-USD per 1M tokens). Configurable so
            // deployments can match their actual model + negotiated pricing.
            input_micros_per_mtok: 3_000_000,
            output_micros_per_mtok: 15_000_000,
        }
    }
}

/// Divisor for per-1M-token rates: 1 million tokens.
const TOKENS_PER_MTOK: u64 = 1_000_000;

impl CostModel {
    /// The estimated micro-USD cost for a job of `kind`.
    ///
    /// This is the COARSE flat-per-kind estimate, used as a fallback when real
    /// token usage is not available (e.g. the heuristic / no-Anthropic path).
    /// Prefer [`Self::token_micros`] whenever the Anthropic response reported
    /// actual token counts.
    #[must_use]
    pub fn micros_for(&self, kind: AiJobKind) -> u64 {
        match kind {
            AiJobKind::Embed => self.embed_micros,
            AiJobKind::Summarize => self.summarize_micros,
            AiJobKind::Moderate => self.moderate_micros,
            AiJobKind::Answer => self.answer_micros,
        }
    }

    /// Relative cost WEIGHT charged to the per-window AI budget for a job of
    /// `kind` (ROADMAP 方向四). Buckets the per-kind micros estimate into small
    /// integer tiers so the window becomes a cost-weighted ceiling rather than a
    /// flat call count: a tenant can no longer run a window full of maxed-out
    /// completions for the same budget as trivial embeds. Floored at 1 so every
    /// job costs something. With the default model: Embed=1, Moderate=2,
    /// Summarize=3, Answer=5.
    #[must_use]
    pub fn weight_for(&self, kind: AiJobKind) -> u32 {
        match self.micros_for(kind) {
            0..=100 => 1,
            101..=1_500 => 2,
            1_501..=4_000 => 3,
            _ => 5,
        }
    }

    /// REAL micro-USD cost from actual `(input_tokens, output_tokens)`.
    ///
    /// `cost = input * input_rate / 1M + output * output_rate / 1M`, computed in
    /// `u128` to avoid overflow on large token counts, then saturated back to
    /// `u64` micros. This is the billing-grounded figure used in place of the flat
    /// estimate whenever the worker has the token counts from the Anthropic
    /// response (方向三). Integer division truncates sub-micro fractions — a
    /// negligible, consistently-downward rounding for a budget signal.
    #[must_use]
    pub fn token_micros(&self, input_tokens: u32, output_tokens: u32) -> u64 {
        let input = u128::from(input_tokens) * u128::from(self.input_micros_per_mtok)
            / u128::from(TOKENS_PER_MTOK);
        let output = u128::from(output_tokens) * u128::from(self.output_micros_per_mtok)
            / u128::from(TOKENS_PER_MTOK);
        u64::try_from(input + output).unwrap_or(u64::MAX)
    }
}

/// Record the end-to-end duration of one job, labeled by kind.
pub fn record_duration(reg: &Registry, kind: AiJobKind, secs: f64) {
    reg.observe_histogram_labeled(
        names::AI_JOB_DURATION_SECONDS,
        secs,
        &[("kind", kind_label(kind))],
    );
}

/// The `workspace` label value used when a job has no owning workspace.
///
/// Per-tenant cost (ROADMAP 方向三 "per-workspace 成本指标 + 看板") attaches a
/// `workspace` label keyed on the job's workspace UUID. Workspace-less jobs
/// (legacy rooms, system jobs) collapse to this single sentinel series rather
/// than spreading across many — see [`charge_cost`] for the cardinality note.
pub const WORKSPACE_NONE: &str = "none";

/// One paid AI charge, fanned to the per-tenant usage ledger drain task
/// (ROADMAP 第六版 · 方向一·2). Distinct from the Prometheus counter (aggregate,
/// scrape-window) — this carries a single billable event to be persisted.
#[derive(Debug, Clone)]
pub struct UsageEvent {
    pub workspace: Option<uuid::Uuid>,
    pub kind: &'static str,
    pub micros: u64,
}

/// Process-global usage-ledger sink, installed once at boot via [`set_usage_sink`].
/// [`charge_cost`] fans every paid charge here. Both the ai_jobs worker queue and
/// the real-time moderation bot charge through `charge_cost`, so this single sink
/// captures ALL AI spend.
static USAGE_SINK: std::sync::OnceLock<tokio::sync::mpsc::Sender<UsageEvent>> =
    std::sync::OnceLock::new();

/// Install the usage-ledger sink (idempotent — a later call is ignored). Wired at
/// boot to the sender half of the drain task's bounded channel.
pub fn set_usage_sink(tx: tokio::sync::mpsc::Sender<UsageEvent>) {
    let _ = USAGE_SINK.set(tx);
}

/// Charge `micros` to [`names::AI_COST_MICROS_TOTAL`] under both the aggregate
/// `kind` series AND a per-workspace `{kind,workspace}` series.
///
/// Operators want **both** views (方向三 看板): the aggregate per-kind counter
/// for global cost, and the per-tenant breakdown for chargeback / abuse
/// attribution. We therefore record the charge twice — once with no `workspace`
/// label (the original aggregate series, preserved verbatim so existing
/// dashboards/alerts keep working) and once with the `workspace` label.
///
/// **Cardinality:** the per-workspace series is bounded by the number of active
/// tenants — finite and operator-controlled (not attacker-influenceable in the
/// way free-text would be). A `None` workspace collapses to the single
/// [`WORKSPACE_NONE`] sentinel rather than being omitted, so workspace-less spend
/// is still attributable without inventing a per-job label. The aggregate series
/// is always present regardless of label cardinality, so even a tenant explosion
/// never blinds the global cost view.
fn charge_cost(reg: &Registry, kind: AiJobKind, workspace: Option<uuid::Uuid>, micros: u64) {
    let kind = kind_label(kind);
    // Aggregate (per-kind) series — unchanged from before per-workspace labeling,
    // so operators retain the global cost counter they already alert on.
    reg.inc_counter_labeled(names::AI_COST_MICROS_TOTAL, micros, &[("kind", kind)]);
    // Per-workspace breakdown. Build the label value as an owned string so it
    // outlives the borrow; `None` collapses to the bounded sentinel.
    let ws = workspace.map_or_else(|| WORKSPACE_NONE.to_string(), |w| w.to_string());
    reg.inc_counter_labeled(
        names::AI_COST_MICROS_TOTAL,
        micros,
        &[("kind", kind), ("workspace", ws.as_str())],
    );
    // Fan paid charges to the durable per-tenant usage ledger (best-effort,
    // non-blocking try_send — a full/absent sink just drops the row, never blocks
    // the AI hot path nor the counter). Zero-cost (unpaid/no-op) charges are
    // skipped: they exist only to keep the metrics series present, not as billable
    // spend, so the ledger stays a faithful record of actual cost.
    if micros > 0 {
        if let Some(tx) = USAGE_SINK.get() {
            let _ = tx.try_send(UsageEvent { workspace, kind, micros });
        }
    }
}

/// Record the (estimated) cost of one paid job, labeled by kind + workspace.
///
/// `paid` lets the caller suppress the charge for work that did not actually hit
/// a paid upstream (e.g. an idempotent embed no-op, or the moderate stub): an
/// unpaid job still gets a zero-valued series so the `kind` is visible on
/// dashboards, but never inflates the cost counter. A zero estimate is a no-op
/// either way (incrementing a counter by 0 is harmless and keeps the series
/// present once it has been touched).
///
/// `workspace` attaches the per-tenant breakdown (方向三); pass the job's
/// `workspace_id` (`None` for legacy/system jobs → [`WORKSPACE_NONE`]). The
/// aggregate per-kind series is recorded alongside — see [`charge_cost`].
pub fn record_cost(
    reg: &Registry,
    model: &CostModel,
    kind: AiJobKind,
    workspace: Option<uuid::Uuid>,
    paid: bool,
) {
    let micros = if paid { model.micros_for(kind) } else { 0 };
    charge_cost(reg, kind, workspace, micros);
}

/// Record the REAL cost of one job from actual Anthropic token counts, labeled by
/// kind + workspace, against the same [`names::AI_COST_MICROS_TOTAL`] counter as
/// [`record_cost`].
///
/// Cost is `model.token_micros(input, output)` — the billing-grounded figure (方向三)
/// rather than the coarse flat per-kind estimate. The worker calls this on the
/// success path whenever the response surfaced usage; it falls back to
/// [`record_cost`] (the estimate) when usage is absent (e.g. the heuristic
/// no-Anthropic path). Zero tokens record a zero-valued series so the `kind`
/// stays visible on dashboards. `workspace` attaches the per-tenant breakdown
/// (see [`charge_cost`]).
pub fn record_token_cost(
    reg: &Registry,
    model: &CostModel,
    kind: AiJobKind,
    workspace: Option<uuid::Uuid>,
    input_tokens: u32,
    output_tokens: u32,
) {
    let micros = model.token_micros(input_tokens, output_tokens);
    charge_cost(reg, kind, workspace, micros);
}

/// Record a job outcome, labeled by kind + outcome.
pub fn record_outcome(reg: &Registry, kind: AiJobKind, outcome: &str) {
    reg.inc_counter_labeled(
        AI_JOBS_TOTAL,
        1,
        &[("kind", kind_label(kind)), ("outcome", outcome)],
    );
}

/// Set the AI queue-depth gauge to the current outstanding-work count.
///
/// "Outstanding" here is the number of jobs claimed in the current batch that
/// have not yet reached a terminal state — the worker calls this as a batch is
/// claimed (depth up) and as it drains (depth back to zero). It is a per-process
/// view; cross-process queue depth would aggregate in Redis (out of scope, see
/// ROADMAP 方向二).
pub fn set_queue_depth(reg: &Registry, depth: usize) {
    // usize → f64 is exact for any realistic batch/queue depth.
    #[allow(clippy::cast_precision_loss)]
    reg.set_gauge(names::AI_QUEUE_DEPTH, depth as f64);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_label_matches_storage_strings() {
        assert_eq!(kind_label(AiJobKind::Embed), "embed");
        assert_eq!(kind_label(AiJobKind::Summarize), "summarize");
        assert_eq!(kind_label(AiJobKind::Moderate), "moderate");
        assert_eq!(kind_label(AiJobKind::Answer), "answer");
    }

    #[test]
    fn cost_model_maps_each_kind() {
        let m = CostModel::default();
        assert_eq!(m.micros_for(AiJobKind::Embed), m.embed_micros);
        assert_eq!(m.micros_for(AiJobKind::Summarize), m.summarize_micros);
        assert_eq!(m.micros_for(AiJobKind::Moderate), m.moderate_micros);
        assert_eq!(m.micros_for(AiJobKind::Answer), m.answer_micros);
        // Moderate is a real paid call when keyed (方向四) → non-zero estimate,
        // but a small classification, so below the larger summary/answer kinds.
        assert!(m.micros_for(AiJobKind::Moderate) > 0);
        assert!(m.micros_for(AiJobKind::Moderate) < m.micros_for(AiJobKind::Summarize));
        // The LLM kinds dominate the cheap embedding kind.
        assert!(m.micros_for(AiJobKind::Answer) > m.micros_for(AiJobKind::Embed));
        assert!(m.micros_for(AiJobKind::Summarize) > m.micros_for(AiJobKind::Embed));
        assert!(m.micros_for(AiJobKind::Moderate) > m.micros_for(AiJobKind::Embed));
    }

    #[test]
    fn cost_model_is_configurable() {
        let m = CostModel {
            embed_micros: 1,
            summarize_micros: 2,
            moderate_micros: 3,
            answer_micros: 4,
            input_micros_per_mtok: 1_000_000,
            output_micros_per_mtok: 2_000_000,
        };
        assert_eq!(m.micros_for(AiJobKind::Embed), 1);
        assert_eq!(m.micros_for(AiJobKind::Answer), 4);
    }

    #[test]
    fn token_micros_uses_default_sonnet_rates() {
        let m = CostModel::default();
        // 1M input tokens at $3/1M = 3_000_000 micros; 1M output at $15/1M.
        assert_eq!(m.token_micros(1_000_000, 0), 3_000_000);
        assert_eq!(m.token_micros(0, 1_000_000), 15_000_000);
        // A realistic small call: 1234 in / 56 out.
        //   input  = 1234 * 3_000_000 / 1_000_000 = 3702
        //   output =   56 * 15_000_000 / 1_000_000 = 840
        assert_eq!(m.token_micros(1234, 56), 3702 + 840);
        // Zero usage → zero cost.
        assert_eq!(m.token_micros(0, 0), 0);
    }

    #[test]
    fn token_micros_honours_configured_rates() {
        // $1/1M input, $4/1M output.
        let m = CostModel {
            input_micros_per_mtok: 1_000_000,
            output_micros_per_mtok: 4_000_000,
            ..CostModel::default()
        };
        // 500k input = 500_000 micros; 250k output = 1_000_000 micros.
        assert_eq!(m.token_micros(500_000, 250_000), 500_000 + 1_000_000);
    }

    #[test]
    fn token_micros_does_not_overflow_on_large_counts() {
        // u32::MAX tokens at the default rates must not panic / wrap — the u128
        // intermediate keeps it exact, then saturates to u64 if needed.
        let m = CostModel::default();
        let cost = m.token_micros(u32::MAX, u32::MAX);
        // u32::MAX ≈ 4.29e9 tokens; cost is well within u64 range, so it's exact.
        let want = u64::from(u32::MAX) * 3 + u64::from(u32::MAX) * 15;
        assert_eq!(cost, want);
    }

    #[test]
    fn record_token_cost_accumulates_real_cost_per_kind() {
        let r = Registry::new();
        let m = CostModel::default();
        // Two answer calls with real token counts (no workspace → aggregate only).
        record_token_cost(&r, &m, AiJobKind::Answer, None, 1000, 100); // 3000 + 1500 = 4500
        record_token_cost(&r, &m, AiJobKind::Answer, None, 2000, 200); // 6000 + 3000 = 9000
        let out = r.render_prometheus();
        let want = 4500 + 9000;
        assert!(
            out.contains(&format!(
                r#"aero_ai_cost_micros_total{{kind="answer"}} {want}"#
            )),
            "real token cost wrong (want {want}):\n{out}"
        );
    }

    #[tokio::test]
    async fn paid_charge_fans_to_usage_sink() {
        // The sink is a process-global OnceLock shared across the test binary, so
        // filter received events by a UNIQUE workspace to stay robust if a parallel
        // test also charges. Buffer is generous so a concurrent burst can't evict
        // ours before we drain.
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        set_usage_sink(tx); // first setter wins; later set_usage_sink calls are no-ops
        let r = Registry::new();
        let m = CostModel::default();
        let ws = uuid::Uuid::new_v4();

        // A PAID answer charges micros > 0 → fans a ledger event.
        record_cost(&r, &m, AiJobKind::Answer, Some(ws), true);
        let mut ours = None;
        while let Ok(ev) = rx.try_recv() {
            if ev.workspace == Some(ws) {
                ours = Some(ev);
                break;
            }
        }
        let ev = ours.expect("a paid charge fans a usage event for our workspace");
        assert_eq!(ev.kind, "answer");
        assert!(ev.micros > 0, "paid charge carries real micros");

        // An UNPAID charge (micros == 0) must fan NOTHING for our workspace.
        record_cost(&r, &m, AiJobKind::Embed, Some(ws), false);
        let mut saw_unpaid = false;
        while let Ok(ev) = rx.try_recv() {
            if ev.workspace == Some(ws) {
                saw_unpaid = true;
            }
        }
        assert!(!saw_unpaid, "an unpaid (zero-cost) charge must not fan a ledger event");
    }

    #[test]
    fn record_token_cost_zero_tokens_records_zero_series() {
        let r = Registry::new();
        let m = CostModel::default();
        record_token_cost(&r, &m, AiJobKind::Summarize, None, 0, 0);
        let out = r.render_prometheus();
        assert!(
            out.contains(r#"aero_ai_cost_micros_total{kind="summarize"} 0"#),
            "zero-token cost must record a zero series:\n{out}"
        );
    }

    #[test]
    fn weight_for_orders_kinds_by_cost_and_floors_at_one() {
        let m = CostModel::default();
        // Every kind costs at least one unit, and the expensive completions
        // outweigh the cheap embed — so the window is a weighted ceiling (方向四).
        assert!(m.weight_for(AiJobKind::Embed) >= 1);
        assert!(m.weight_for(AiJobKind::Answer) > m.weight_for(AiJobKind::Embed));
        assert!(m.weight_for(AiJobKind::Summarize) > m.weight_for(AiJobKind::Embed));
        assert!(m.weight_for(AiJobKind::Answer) >= m.weight_for(AiJobKind::Summarize));
        assert!(m.weight_for(AiJobKind::Moderate) >= m.weight_for(AiJobKind::Embed));
    }

    #[test]
    fn moderate_paid_job_charges_nonzero_cost() {
        // Regression (方向四): a paid moderation call must NOT record $0 — the
        // worker charges micros_for(Moderate) when Anthropic was actually called.
        let m = CostModel::default();
        assert!(
            m.micros_for(AiJobKind::Moderate) > 0,
            "moderation is a real paid call when keyed, not a stub"
        );
        let r = Registry::new();
        record_cost(&r, &m, AiJobKind::Moderate, None, true);
        let out = r.render_prometheus();
        assert!(
            out.contains(&format!(
                r#"aero_ai_cost_micros_total{{kind="moderate"}} {}"#,
                m.moderate_micros
            )),
            "paid moderation must charge its estimate, not zero:\n{out}"
        );
    }

    #[test]
    fn record_cost_attaches_per_workspace_label_and_keeps_aggregate() {
        let r = Registry::new();
        let m = CostModel::default();
        let ws_a = uuid::Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00aa);
        let ws_b = uuid::Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_00bb);
        // Two paid answers in workspace A, one in workspace B.
        record_cost(&r, &m, AiJobKind::Answer, Some(ws_a), true);
        record_cost(&r, &m, AiJobKind::Answer, Some(ws_a), true);
        record_cost(&r, &m, AiJobKind::Answer, Some(ws_b), true);
        let out = r.render_prometheus();

        // Per-workspace breakdown: A charged twice, B once. Labels render sorted
        // by key, so `{kind,workspace}` (k < w).
        let two = m.answer_micros * 2;
        assert!(
            out.contains(&format!(
                r#"aero_ai_cost_micros_total{{kind="answer",workspace="{ws_a}"}} {two}"#
            )),
            "workspace A cost wrong (want {two}):\n{out}"
        );
        assert!(
            out.contains(&format!(
                r#"aero_ai_cost_micros_total{{kind="answer",workspace="{ws_b}"}} {}"#,
                m.answer_micros
            )),
            "workspace B cost wrong:\n{out}"
        );
        // Aggregate per-kind series is preserved and sums BOTH workspaces.
        let total = m.answer_micros * 3;
        assert!(
            out.contains(&format!(
                r#"aero_ai_cost_micros_total{{kind="answer"}} {total}"#
            )),
            "aggregate cost must sum all workspaces (want {total}):\n{out}"
        );
    }

    #[test]
    fn record_cost_workspaceless_uses_none_sentinel() {
        let r = Registry::new();
        let m = CostModel::default();
        // A workspace-less paid summarize collapses to the bounded sentinel.
        record_cost(&r, &m, AiJobKind::Summarize, None, true);
        let out = r.render_prometheus();
        assert!(
            out.contains(&format!(
                r#"aero_ai_cost_micros_total{{kind="summarize",workspace="{WORKSPACE_NONE}"}} {}"#,
                m.summarize_micros
            )),
            "workspace-less cost must use the '{WORKSPACE_NONE}' sentinel:\n{out}"
        );
        // Aggregate still recorded.
        assert!(
            out.contains(&format!(
                r#"aero_ai_cost_micros_total{{kind="summarize"}} {}"#,
                m.summarize_micros
            )),
            "aggregate cost missing:\n{out}"
        );
    }

    #[test]
    fn record_token_cost_attaches_per_workspace_label() {
        let r = Registry::new();
        let m = CostModel::default();
        let ws = uuid::Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0042);
        record_token_cost(&r, &m, AiJobKind::Answer, Some(ws), 1000, 100); // 4500
        let out = r.render_prometheus();
        assert!(
            out.contains(&format!(
                r#"aero_ai_cost_micros_total{{kind="answer",workspace="{ws}"}} 4500"#
            )),
            "per-workspace real token cost wrong:\n{out}"
        );
        assert!(
            out.contains(r#"aero_ai_cost_micros_total{kind="answer"} 4500"#),
            "aggregate real token cost wrong:\n{out}"
        );
    }

    #[test]
    fn record_duration_observes_labeled_histogram() {
        let r = Registry::new();
        record_duration(&r, AiJobKind::Summarize, 0.25);
        record_duration(&r, AiJobKind::Summarize, 0.75);
        record_duration(&r, AiJobKind::Embed, 0.01);
        let out = r.render_prometheus();
        assert!(
            out.contains(r#"aero_ai_job_duration_seconds_count{kind="summarize"} 2"#),
            "summarize count wrong:\n{out}"
        );
        assert!(
            out.contains(r#"aero_ai_job_duration_seconds_sum{kind="summarize"} 1"#),
            "summarize sum should be 0.25+0.75=1:\n{out}"
        );
        assert!(
            out.contains(r#"aero_ai_job_duration_seconds_count{kind="embed"} 1"#),
            "embed count wrong:\n{out}"
        );
    }

    #[test]
    fn record_cost_accumulates_per_kind_when_paid() {
        let r = Registry::new();
        let m = CostModel::default();
        // Two paid answers + one paid summarize (no workspace → aggregate series).
        record_cost(&r, &m, AiJobKind::Answer, None, true);
        record_cost(&r, &m, AiJobKind::Answer, None, true);
        record_cost(&r, &m, AiJobKind::Summarize, None, true);
        let out = r.render_prometheus();
        let two_answers = m.answer_micros * 2;
        assert!(
            out.contains(&format!(
                r#"aero_ai_cost_micros_total{{kind="answer"}} {two_answers}"#
            )),
            "answer cost wrong (want {two_answers}):\n{out}"
        );
        assert!(
            out.contains(&format!(
                r#"aero_ai_cost_micros_total{{kind="summarize"}} {}"#,
                m.summarize_micros
            )),
            "summarize cost wrong:\n{out}"
        );
    }

    #[test]
    fn record_cost_unpaid_does_not_inflate_counter() {
        let r = Registry::new();
        let m = CostModel::default();
        // An idempotent embed no-op: unpaid → zero charge, but the series exists.
        record_cost(&r, &m, AiJobKind::Embed, None, false);
        let out = r.render_prometheus();
        assert!(
            out.contains(r#"aero_ai_cost_micros_total{kind="embed"} 0"#),
            "unpaid embed must record zero:\n{out}"
        );
        // Now a paid embed adds exactly one unit of the estimate.
        record_cost(&r, &m, AiJobKind::Embed, None, true);
        let out = r.render_prometheus();
        assert!(
            out.contains(&format!(
                r#"aero_ai_cost_micros_total{{kind="embed"}} {}"#,
                m.embed_micros
            )),
            "paid embed cost wrong:\n{out}"
        );
    }

    #[test]
    fn record_outcome_separates_kind_and_outcome_series() {
        let r = Registry::new();
        record_outcome(&r, AiJobKind::Summarize, OUTCOME_SUCCESS);
        record_outcome(&r, AiJobKind::Summarize, OUTCOME_SUCCESS);
        record_outcome(&r, AiJobKind::Summarize, OUTCOME_FAILURE);
        record_outcome(&r, AiJobKind::Embed, OUTCOME_DEAD_LETTER);
        let out = r.render_prometheus();
        assert!(
            out.contains(r#"aero_ai_jobs_total{kind="summarize",outcome="success"} 2"#),
            "success count wrong:\n{out}"
        );
        assert!(
            out.contains(r#"aero_ai_jobs_total{kind="summarize",outcome="failure"} 1"#),
            "failure count wrong:\n{out}"
        );
        assert!(
            out.contains(r#"aero_ai_jobs_total{kind="embed",outcome="dead_letter"} 1"#),
            "dead-letter count wrong:\n{out}"
        );
    }

    #[test]
    fn set_queue_depth_reflects_value() {
        let r = Registry::new();
        set_queue_depth(&r, 8);
        assert!(
            r.render_prometheus().contains("\naero_ai_queue_depth 8\n"),
            "queue depth not set:\n{}",
            r.render_prometheus()
        );
        // Gauge goes back down when the batch drains.
        set_queue_depth(&r, 0);
        assert!(r.render_prometheus().contains("\naero_ai_queue_depth 0\n"));
    }
}
