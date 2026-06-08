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
/// - `Moderate` — currently a local stub (no paid call) → zero.
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
            // Local stub today: no paid upstream → no estimated spend.
            moderate_micros: 0,
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

/// Record the (estimated) cost of one paid job, labeled by kind.
///
/// `paid` lets the caller suppress the charge for work that did not actually hit
/// a paid upstream (e.g. an idempotent embed no-op, or the moderate stub): an
/// unpaid job still gets a zero-valued series so the `kind` is visible on
/// dashboards, but never inflates the cost counter. A zero estimate is a no-op
/// either way (incrementing a counter by 0 is harmless and keeps the series
/// present once it has been touched).
pub fn record_cost(reg: &Registry, model: &CostModel, kind: AiJobKind, paid: bool) {
    let micros = if paid { model.micros_for(kind) } else { 0 };
    reg.inc_counter_labeled(
        names::AI_COST_MICROS_TOTAL,
        micros,
        &[("kind", kind_label(kind))],
    );
}

/// Record the REAL cost of one job from actual Anthropic token counts, labeled by
/// kind, against the same [`names::AI_COST_MICROS_TOTAL`] counter as
/// [`record_cost`].
///
/// Cost is `model.token_micros(input, output)` — the billing-grounded figure (方向三)
/// rather than the coarse flat per-kind estimate. The worker calls this on the
/// success path whenever the response surfaced usage; it falls back to
/// [`record_cost`] (the estimate) when usage is absent (e.g. the heuristic
/// no-Anthropic path). Zero tokens record a zero-valued series so the `kind`
/// stays visible on dashboards.
pub fn record_token_cost(
    reg: &Registry,
    model: &CostModel,
    kind: AiJobKind,
    input_tokens: u32,
    output_tokens: u32,
) {
    let micros = model.token_micros(input_tokens, output_tokens);
    reg.inc_counter_labeled(
        names::AI_COST_MICROS_TOTAL,
        micros,
        &[("kind", kind_label(kind))],
    );
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
        // Moderate is a local stub → no estimated spend.
        assert_eq!(m.micros_for(AiJobKind::Moderate), 0);
        // The LLM kinds dominate the cheap embedding kind.
        assert!(m.micros_for(AiJobKind::Answer) > m.micros_for(AiJobKind::Embed));
        assert!(m.micros_for(AiJobKind::Summarize) > m.micros_for(AiJobKind::Embed));
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
        // Two answer calls with real token counts.
        record_token_cost(&r, &m, AiJobKind::Answer, 1000, 100); // 3000 + 1500 = 4500
        record_token_cost(&r, &m, AiJobKind::Answer, 2000, 200); // 6000 + 3000 = 9000
        let out = r.render_prometheus();
        let want = 4500 + 9000;
        assert!(
            out.contains(&format!(
                r#"aero_ai_cost_micros_total{{kind="answer"}} {want}"#
            )),
            "real token cost wrong (want {want}):\n{out}"
        );
    }

    #[test]
    fn record_token_cost_zero_tokens_records_zero_series() {
        let r = Registry::new();
        let m = CostModel::default();
        record_token_cost(&r, &m, AiJobKind::Summarize, 0, 0);
        let out = r.render_prometheus();
        assert!(
            out.contains(r#"aero_ai_cost_micros_total{kind="summarize"} 0"#),
            "zero-token cost must record a zero series:\n{out}"
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
        // Two paid answers + one paid summarize.
        record_cost(&r, &m, AiJobKind::Answer, true);
        record_cost(&r, &m, AiJobKind::Answer, true);
        record_cost(&r, &m, AiJobKind::Summarize, true);
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
        record_cost(&r, &m, AiJobKind::Embed, false);
        let out = r.render_prometheus();
        assert!(
            out.contains(r#"aero_ai_cost_micros_total{kind="embed"} 0"#),
            "unpaid embed must record zero:\n{out}"
        );
        // Now a paid embed adds exactly one unit of the estimate.
        record_cost(&r, &m, AiJobKind::Embed, true);
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
