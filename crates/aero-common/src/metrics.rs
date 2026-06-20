//! Lightweight, dependency-light metrics registry + Prometheus text exposition.
//!
//! ROADMAP 方向四 (observability) calls out that a realtime media `SaaS` "cannot
//! fly blind": we need counters / gauges / histograms for message throughput,
//! WS connection count, AI job latency / cost / queue depth, DB pool
//! saturation, and HTTP request count / latency by route + status — plus a
//! Prometheus `/metrics` exposition so the gateway can scrape them.
//!
//! This module is a **self-contained, additive foundation**. It deliberately
//! avoids heavy metrics frameworks (and the `opentelemetry-otlp` version churn,
//! see [`otlp`]) in favour of a small hand-rolled registry over
//! [`std::sync::atomic`] + [`dashmap::DashMap`]. It is fully unit-testable with
//! no network: register → increment / observe → render → assert.
//!
//! # Quick start
//!
//! ```
//! use aero_common::metrics;
//!
//! // Free-function helpers operate on a process-global registry.
//! metrics::inc_counter(metrics::names::MESSAGES_SENT_TOTAL, 1);
//! metrics::set_gauge(metrics::names::WS_CONNECTIONS, 3.0);
//! metrics::observe_histogram(metrics::names::HTTP_REQUEST_DURATION_SECONDS, 0.042);
//!
//! // Render Prometheus text exposition (what `/metrics` would return).
//! let body = metrics::render_prometheus();
//! assert!(body.contains("aero_messages_sent_total"));
//! ```
//!
//! # Labels
//!
//! Every helper has a `*_labeled` sibling taking `&[(&str, &str)]` label pairs.
//! Labels are normalised (sorted by key) so the same logical series always maps
//! to one time series regardless of argument order.
//!
//! ```
//! use aero_common::metrics;
//!
//! let r = metrics::Registry::new();
//! r.inc_counter_labeled(
//!     metrics::names::HTTP_REQUESTS_TOTAL,
//!     1,
//!     &[("route", "/api/messages"), ("status", "200")],
//! );
//! let out = r.render_prometheus();
//! assert!(out.contains(r#"aero_http_requests_total{route="/api/messages",status="200"} 1"#));
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use dashmap::DashMap;

/// Metric-name constants for the key signals ROADMAP 方向四 enumerates.
///
/// Using constants keeps emit sites and dashboards in lockstep (no typo'd
/// series names). Names follow Prometheus conventions: `snake_case`, the
/// `aero_` namespace prefix, a unit suffix where applicable, and `_total` for
/// counters.
pub mod names {
    // --- Message throughput ---
    /// Counter: messages accepted / fanned out.
    pub const MESSAGES_SENT_TOTAL: &str = "aero_messages_sent_total";
    /// Counter: messages edited (successful edits only).
    pub const MESSAGES_EDITED_TOTAL: &str = "aero_messages_edited_total";
    /// Counter: messages soft-deleted (successful deletes only).
    pub const MESSAGES_DELETED_TOTAL: &str = "aero_messages_deleted_total";
    /// Histogram: end-to-end message-mutation latency in seconds, labeled by
    /// `op` (`send` / `edit` / `delete`). Answers "messages per second" and
    /// surfaces hot-path regressions in the IM core.
    pub const MESSAGE_PROCESSING_DURATION_SECONDS: &str =
        "aero_message_processing_duration_seconds";

    // --- WebSocket connections (gauge, up/down) ---
    /// Gauge: currently-open WebSocket connections.
    pub const WS_CONNECTIONS: &str = "aero_ws_connections";

    // --- AI jobs ---
    /// Histogram: AI job end-to-end latency in seconds.
    pub const AI_JOB_DURATION_SECONDS: &str = "aero_ai_job_duration_seconds";
    /// Counter: estimated AI spend in micro-USD (integer-friendly; divide by 1e6
    /// for dollars). Stored as a counter so it is monotonic and rate-able.
    pub const AI_COST_MICROS_TOTAL: &str = "aero_ai_cost_micros_total";
    /// Gauge: AI job queue depth (pending jobs).
    pub const AI_QUEUE_DEPTH: &str = "aero_ai_queue_depth";

    // --- Database connection pool ---
    /// Gauge: in-use DB pool connections.
    pub const DB_POOL_IN_USE: &str = "aero_db_pool_in_use";
    /// Gauge: configured DB pool size (max connections).
    pub const DB_POOL_SIZE: &str = "aero_db_pool_size";

    // --- Event bus (NATS publish errors + consumer backlog) ---
    /// Counter: NATS publish errors (fan-out failures). Non-zero under sustained
    /// NATS pressure; a spike indicates cluster messaging degradation.
    pub const NATS_PUBLISH_ERRORS_TOTAL: &str = "aero_nats_publish_errors_total";
    /// Gauge: undelivered messages pending in a durable consumer. Sampled every
    /// 30 s. A growing value means the consumer is falling behind — alerts when
    /// the AI worker is throttled or the WS fan-out can't keep up.
    pub const NATS_CONSUMER_PENDING_MESSAGES: &str = "aero_nats_consumer_pending_messages";
    /// Counter: bus messages dropped because their payload could not be decoded by
    /// any known schema. These are acked (not nacked) so a permanently-malformed
    /// payload can't redeliver forever; a non-zero value flags a producer/schema
    /// mismatch and should alert.
    pub const BUS_POISON_DROPPED_TOTAL: &str = "aero_bus_poison_dropped_total";
    /// Gauge: AI jobs in `dead` status (exhausted retries). Non-zero means
    /// moderation / summarisation is silently failing; alert at threshold > 0.
    pub const AI_DEAD_LETTER_QUEUE_SIZE: &str = "aero_ai_dlq_size";
    /// Counter: outgoing-webhook deliveries skipped because the endpoint's circuit
    /// breaker was open (the endpoint has been failing). A sustained non-zero rate
    /// means a chronically-down receiver — the breaker is shielding us from
    /// hammering it (ROADMAP5 方向一).
    pub const WEBHOOK_BREAKER_OPEN_SKIPS_TOTAL: &str = "aero_webhook_breaker_open_skips_total";

    // --- Live media ingest ---
    /// Gauge: active WHIP ingest sessions (WebRTC streams currently being ingested).
    /// Updated every 15 s alongside the DB pool gauges.
    pub const LIVE_WHIP_SESSIONS: &str = "aero_live_whip_sessions";

    // --- HTTP (RED metrics, labeled by route + status) ---
    /// Counter: HTTP requests, labeled `route` + `status`.
    pub const HTTP_REQUESTS_TOTAL: &str = "aero_http_requests_total";
    /// Histogram: HTTP request duration in seconds, labeled `route` (+ `status`).
    pub const HTTP_REQUEST_DURATION_SECONDS: &str = "aero_http_request_duration_seconds";
}

/// Default histogram bucket upper bounds (seconds), tuned for request/job
/// latency. The implicit `+Inf` bucket is always appended at render time.
///
/// Covers ~1 ms up to ~10 s, the useful range for HTTP handlers and AI jobs.
pub const DEFAULT_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// The kind of a registered metric. Determines how it renders (`# TYPE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// Monotonically increasing value (resets only on restart).
    Counter,
    /// Value that can go up or down.
    Gauge,
    /// Distribution summarised into cumulative buckets + sum + count.
    Histogram,
}

impl MetricKind {
    const fn as_prom(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

/// A normalised label set: sorted key→value pairs.
///
/// Sorting makes the series identity order-independent, so
/// `{a="1",b="2"}` and `{b="2",a="1"}` collapse to the same series.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct LabelSet(Vec<(String, String)>);

impl LabelSet {
    fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        let mut v: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, val)| ((*k).to_string(), (*val).to_string()))
            .collect();
        v.sort();
        v.dedup_by(|a, b| a.0 == b.0); // duplicate keys are a caller bug; keep one
        Self(v)
    }

    /// Renders the Prometheus label block, e.g. `{route="/x",status="200"}`.
    /// Returns an empty string when there are no labels. `extra` lets the
    /// histogram renderer append a synthetic label (the `le` bucket bound).
    fn render(&self, extra: Option<(&str, &str)>) -> String {
        if self.0.is_empty() && extra.is_none() {
            return String::new();
        }
        let mut out = String::from("{");
        let mut first = true;
        for (k, v) in &self.0 {
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(k);
            out.push_str("=\"");
            escape_label_value_into(&mut out, v);
            out.push('"');
        }
        if let Some((k, v)) = extra {
            if !first {
                out.push(',');
            }
            out.push_str(k);
            out.push_str("=\"");
            escape_label_value_into(&mut out, v);
            out.push('"');
        }
        out.push('}');
        out
    }
}

/// Escapes a label value per the Prometheus text format (`\`, `"`, `\n`).
fn escape_label_value_into(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
}

/// Bit-encoded `f64` value (atomics only operate on integers).
#[derive(Debug)]
struct AtomicF64(AtomicU64);

impl AtomicF64 {
    fn new(v: f64) -> Self {
        Self(AtomicU64::new(v.to_bits()))
    }
    fn set(&self, v: f64) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
    fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }
    /// Atomic add via compare-and-swap (handles concurrent inc/dec on gauges).
    fn add(&self, delta: f64) {
        let mut cur = self.0.load(Ordering::Relaxed);
        loop {
            let next = (f64::from_bits(cur) + delta).to_bits();
            match self
                .0
                .compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }
    }
}

/// A single histogram series: cumulative bucket counters + running sum + count.
#[derive(Debug)]
struct Histogram {
    bounds: Vec<f64>,
    /// One counter per bound; `bucket_counts[i]` = observations `<= bounds[i]`.
    bucket_counts: Vec<AtomicU64>,
    sum: AtomicF64,
    count: AtomicU64,
}

impl Histogram {
    fn new(bounds: &[f64]) -> Self {
        Self {
            bounds: bounds.to_vec(),
            bucket_counts: bounds.iter().map(|_| AtomicU64::new(0)).collect(),
            sum: AtomicF64::new(0.0),
            count: AtomicU64::new(0),
        }
    }

    fn observe(&self, value: f64) {
        for (i, bound) in self.bounds.iter().enumerate() {
            if value <= *bound {
                self.bucket_counts[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.sum.add(value);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// Cumulative count at or below `bounds[i]` (already cumulative since each
    /// observation increments every bucket whose bound it satisfies).
    fn cumulative(&self, i: usize) -> u64 {
        self.bucket_counts[i].load(Ordering::Relaxed)
    }
}

/// Storage for one metric name, parameterised over label sets.
#[derive(Debug)]
enum Family {
    Counter(DashMap<LabelSet, AtomicU64>),
    Gauge(DashMap<LabelSet, AtomicF64>),
    Histogram {
        bounds: Vec<f64>,
        series: DashMap<LabelSet, Histogram>,
    },
}

/// A registered metric: its kind, help text, and per-label-set storage.
#[derive(Debug)]
struct Metric {
    kind: MetricKind,
    help: String,
    family: Family,
}

/// A thread-safe registry of counters / gauges / histograms.
///
/// Cheap to share: wrap in `Arc` (or use the process-global one via the
/// free functions / [`global`]). All mutating operations take `&self`.
///
/// Metrics are created lazily on first touch with sensible defaults; use
/// [`Registry::register_help`] / [`Registry::register_histogram`] up-front if
/// you want custom help text or histogram buckets.
#[derive(Debug, Default)]
pub struct Registry {
    metrics: DashMap<String, Metric>,
}

impl Registry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            metrics: DashMap::new(),
        }
    }

    /// Sets (or overrides) the `# HELP` text for a metric, creating it with the
    /// given kind if it does not yet exist. Idempotent for matching kinds.
    pub fn register_help(&self, name: &str, kind: MetricKind, help: &str) {
        let mut entry = self
            .metrics
            .entry(name.to_string())
            .or_insert_with(|| Metric {
                kind,
                help: String::new(),
                family: empty_family(kind, DEFAULT_BUCKETS),
            });
        entry.help = help.to_string();
    }

    /// Registers a histogram with explicit bucket bounds (and optional help).
    /// No-op if a histogram with this name already exists (buckets are fixed at
    /// creation; re-registering does not migrate existing observations).
    pub fn register_histogram(&self, name: &str, buckets: &[f64], help: &str) {
        self.metrics.entry(name.to_string()).or_insert_with(|| {
            let mut bounds = buckets.to_vec();
            bounds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            Metric {
                kind: MetricKind::Histogram,
                help: help.to_string(),
                family: Family::Histogram {
                    bounds,
                    series: DashMap::new(),
                },
            }
        });
    }

    // ---- Counters ----

    /// Adds `delta` to an unlabeled counter (creating it on first use).
    pub fn inc_counter(&self, name: &str, delta: u64) {
        self.inc_counter_labeled(name, delta, &[]);
    }

    /// Adds `delta` to a labeled counter (creating it on first use).
    ///
    /// If `name` was already registered as a non-counter, the call is ignored
    /// (kind conflicts are a programming error; we never panic in a hot path).
    pub fn inc_counter_labeled(&self, name: &str, delta: u64, labels: &[(&str, &str)]) {
        let metric = self
            .metrics
            .entry(name.to_string())
            .or_insert_with(|| Metric {
                kind: MetricKind::Counter,
                help: String::new(),
                family: Family::Counter(DashMap::new()),
            });
        if let Family::Counter(map) = &metric.family {
            map.entry(LabelSet::from_pairs(labels))
                .or_insert_with(|| AtomicU64::new(0))
                .fetch_add(delta, Ordering::Relaxed);
        }
    }

    // ---- Gauges ----

    /// Sets an unlabeled gauge to `value`.
    pub fn set_gauge(&self, name: &str, value: f64) {
        self.set_gauge_labeled(name, value, &[]);
    }

    /// Sets a labeled gauge to `value`.
    pub fn set_gauge_labeled(&self, name: &str, value: f64, labels: &[(&str, &str)]) {
        let metric = self.gauge_family(name);
        if let Family::Gauge(map) = &metric.family {
            let key = LabelSet::from_pairs(labels);
            if let Some(g) = map.get(&key) {
                g.set(value);
            } else {
                map.insert(key, AtomicF64::new(value));
            }
        }
    }

    /// Adds `delta` (may be negative) to an unlabeled gauge.
    pub fn add_gauge(&self, name: &str, delta: f64) {
        self.add_gauge_labeled(name, delta, &[]);
    }

    /// Adds `delta` (may be negative) to a labeled gauge.
    pub fn add_gauge_labeled(&self, name: &str, delta: f64, labels: &[(&str, &str)]) {
        let metric = self.gauge_family(name);
        if let Family::Gauge(map) = &metric.family {
            map.entry(LabelSet::from_pairs(labels))
                .or_insert_with(|| AtomicF64::new(0.0))
                .add(delta);
        }
    }

    /// Convenience: `add_gauge(name, 1.0)` — e.g. a connection opened.
    pub fn inc_gauge(&self, name: &str) {
        self.add_gauge(name, 1.0);
    }

    /// Convenience: `add_gauge(name, -1.0)` — e.g. a connection closed.
    pub fn dec_gauge(&self, name: &str) {
        self.add_gauge(name, -1.0);
    }

    fn gauge_family(&self, name: &str) -> dashmap::mapref::one::RefMut<'_, String, Metric> {
        self.metrics
            .entry(name.to_string())
            .or_insert_with(|| Metric {
                kind: MetricKind::Gauge,
                help: String::new(),
                family: Family::Gauge(DashMap::new()),
            })
    }

    // ---- Histograms ----

    /// Records `value` into an unlabeled histogram (default buckets on first use).
    pub fn observe_histogram(&self, name: &str, value: f64) {
        self.observe_histogram_labeled(name, value, &[]);
    }

    /// Records `value` into a labeled histogram (default buckets on first use).
    pub fn observe_histogram_labeled(&self, name: &str, value: f64, labels: &[(&str, &str)]) {
        let metric = self
            .metrics
            .entry(name.to_string())
            .or_insert_with(|| Metric {
                kind: MetricKind::Histogram,
                help: String::new(),
                family: Family::Histogram {
                    bounds: DEFAULT_BUCKETS.to_vec(),
                    series: DashMap::new(),
                },
            });
        if let Family::Histogram { bounds, series } = &metric.family {
            series
                .entry(LabelSet::from_pairs(labels))
                .or_insert_with(|| Histogram::new(bounds))
                .observe(value);
        }
    }

    /// Renders the entire registry in Prometheus text exposition format.
    ///
    /// Output is deterministic (metrics sorted by name, series sorted by label
    /// set) so it is stable for snapshot tests and diff-friendly scrapes.
    #[must_use]
    pub fn render_prometheus(&self) -> String {
        // Snapshot names sorted for deterministic output.
        let mut names: Vec<String> = self.metrics.iter().map(|e| e.key().clone()).collect();
        names.sort();

        let mut out = String::new();
        for name in names {
            let Some(metric) = self.metrics.get(&name) else {
                continue;
            };
            if !metric.help.is_empty() {
                out.push_str("# HELP ");
                out.push_str(&name);
                out.push(' ');
                // HELP text: escape backslash and newline per spec.
                for ch in metric.help.chars() {
                    match ch {
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        other => out.push(other),
                    }
                }
                out.push('\n');
            }
            out.push_str("# TYPE ");
            out.push_str(&name);
            out.push(' ');
            out.push_str(metric.kind.as_prom());
            out.push('\n');

            match &metric.family {
                Family::Counter(map) => render_scalar(&mut out, &name, map),
                Family::Gauge(map) => render_scalar(&mut out, &name, map),
                Family::Histogram { series, .. } => render_histogram(&mut out, &name, series),
            }
        }
        out
    }
}

/// Helper trait so `render_scalar` can read both atomic value types uniformly.
trait AtomicValue {
    fn read_f64(&self) -> f64;
}
impl AtomicValue for AtomicU64 {
    fn read_f64(&self) -> f64 {
        // u64 → f64: exact for the magnitudes counters realistically reach.
        #[allow(clippy::cast_precision_loss)]
        {
            self.load(Ordering::Relaxed) as f64
        }
    }
}
impl AtomicValue for AtomicF64 {
    fn read_f64(&self) -> f64 {
        self.get()
    }
}

fn render_scalar<V: AtomicValue>(out: &mut String, name: &str, map: &DashMap<LabelSet, V>) {
    let mut rows: Vec<(LabelSet, f64)> = map
        .iter()
        .map(|e| (e.key().clone(), e.value().read_f64()))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    for (labels, value) in rows {
        out.push_str(name);
        out.push_str(&labels.render(None));
        out.push(' ');
        push_f64(out, value);
        out.push('\n');
    }
}

fn render_histogram(out: &mut String, name: &str, series: &DashMap<LabelSet, Histogram>) {
    let mut keys: Vec<LabelSet> = series.iter().map(|e| e.key().clone()).collect();
    keys.sort();
    for labels in keys {
        let Some(h) = series.get(&labels) else {
            continue;
        };
        let total = h.count.load(Ordering::Relaxed);
        // Cumulative bucket lines.
        for (i, bound) in h.bounds.iter().enumerate() {
            let mut bound_str = String::new();
            push_f64(&mut bound_str, *bound);
            out.push_str(name);
            out.push_str("_bucket");
            out.push_str(&labels.render(Some(("le", &bound_str))));
            out.push(' ');
            push_u64(out, h.cumulative(i));
            out.push('\n');
        }
        // +Inf bucket == total count.
        out.push_str(name);
        out.push_str("_bucket");
        out.push_str(&labels.render(Some(("le", "+Inf"))));
        out.push(' ');
        push_u64(out, total);
        out.push('\n');
        // _sum and _count.
        out.push_str(name);
        out.push_str("_sum");
        out.push_str(&labels.render(None));
        out.push(' ');
        push_f64(out, h.sum.get());
        out.push('\n');
        out.push_str(name);
        out.push_str("_count");
        out.push_str(&labels.render(None));
        out.push(' ');
        push_u64(out, total);
        out.push('\n');
    }
}

/// Formats an `f64` the way Prometheus expects: integers without a trailing
/// `.0`, otherwise the shortest round-trippable representation.
fn push_f64(out: &mut String, v: f64) {
    use std::fmt::Write as _;
    if v.is_nan() {
        out.push_str("NaN");
    } else if v.is_infinite() {
        out.push_str(if v > 0.0 { "+Inf" } else { "-Inf" });
    } else if v.fract() == 0.0 && v.abs() < 1e15 {
        // Render whole numbers without a decimal point (e.g. `3`, not `3.0`).
        #[allow(clippy::cast_possible_truncation)]
        let as_i = v as i64;
        let _ = write!(out, "{as_i}");
    } else {
        let _ = write!(out, "{v}");
    }
}

fn push_u64(out: &mut String, v: u64) {
    use std::fmt::Write as _;
    let _ = write!(out, "{v}");
}

fn empty_family(kind: MetricKind, buckets: &[f64]) -> Family {
    match kind {
        MetricKind::Counter => Family::Counter(DashMap::new()),
        MetricKind::Gauge => Family::Gauge(DashMap::new()),
        MetricKind::Histogram => Family::Histogram {
            bounds: buckets.to_vec(),
            series: DashMap::new(),
        },
    }
}

// ---------------------------------------------------------------------------
// Process-global registry + free-function ergonomics
// ---------------------------------------------------------------------------

static GLOBAL: OnceLock<Registry> = OnceLock::new();

/// Returns the process-global [`Registry`], initialising it (with help text for
/// the well-known [`names`]) on first access.
///
/// `aero-server` can call [`render_prometheus`] (or `global().render_prometheus()`)
/// from a `/metrics` handler in a follow-up — this crate intentionally does not
/// wire the route (that lives in `aero-server`).
pub fn global() -> &'static Registry {
    GLOBAL.get_or_init(|| {
        let r = Registry::new();
        register_known_metrics(&r);
        r
    })
}

/// Registers help text + histogram buckets for the well-known ROADMAP signals.
/// Safe to call on any registry; used to seed [`global`].
pub fn register_known_metrics(r: &Registry) {
    r.register_help(
        names::MESSAGES_SENT_TOTAL,
        MetricKind::Counter,
        "Total messages accepted and fanned out.",
    );
    r.register_help(
        names::WS_CONNECTIONS,
        MetricKind::Gauge,
        "Currently-open WebSocket connections.",
    );
    r.register_histogram(
        names::AI_JOB_DURATION_SECONDS,
        DEFAULT_BUCKETS,
        "AI job end-to-end latency in seconds.",
    );
    r.register_help(
        names::AI_COST_MICROS_TOTAL,
        MetricKind::Counter,
        "Estimated AI spend in micro-USD (divide by 1e6 for dollars).",
    );
    r.register_help(
        names::AI_QUEUE_DEPTH,
        MetricKind::Gauge,
        "Pending AI jobs in the queue.",
    );
    r.register_help(
        names::DB_POOL_IN_USE,
        MetricKind::Gauge,
        "In-use database pool connections.",
    );
    r.register_help(
        names::DB_POOL_SIZE,
        MetricKind::Gauge,
        "Configured database pool size (max connections).",
    );
    r.register_help(
        names::HTTP_REQUESTS_TOTAL,
        MetricKind::Counter,
        "Total HTTP requests by route and status.",
    );
    r.register_histogram(
        names::HTTP_REQUEST_DURATION_SECONDS,
        DEFAULT_BUCKETS,
        "HTTP request duration in seconds by route.",
    );
}

/// Renders the process-global registry in Prometheus text format.
#[must_use]
pub fn render_prometheus() -> String {
    global().render_prometheus()
}

/// Adds `delta` to an unlabeled counter on the global registry.
pub fn inc_counter(name: &str, delta: u64) {
    global().inc_counter(name, delta);
}

/// Adds `delta` to a labeled counter on the global registry.
pub fn inc_counter_labeled(name: &str, delta: u64, labels: &[(&str, &str)]) {
    global().inc_counter_labeled(name, delta, labels);
}

/// Sets an unlabeled gauge on the global registry.
pub fn set_gauge(name: &str, value: f64) {
    global().set_gauge(name, value);
}

/// Sets a labeled gauge on the global registry.
pub fn set_gauge_labeled(name: &str, value: f64, labels: &[(&str, &str)]) {
    global().set_gauge_labeled(name, value, labels);
}

/// Increments an unlabeled gauge by 1 on the global registry.
pub fn inc_gauge(name: &str) {
    global().inc_gauge(name);
}

/// Decrements an unlabeled gauge by 1 on the global registry.
pub fn dec_gauge(name: &str) {
    global().dec_gauge(name);
}

/// Records an observation into an unlabeled histogram on the global registry.
pub fn observe_histogram(name: &str, value: f64) {
    global().observe_histogram(name, value);
}

/// Records an observation into a labeled histogram on the global registry.
pub fn observe_histogram_labeled(name: &str, value: f64, labels: &[(&str, &str)]) {
    global().observe_histogram_labeled(name, value, labels);
}

// ---------------------------------------------------------------------------
// OTLP metrics export (OPT-IN; Prometheus exposition above stays the default)
// ---------------------------------------------------------------------------

/// Optional OTLP metrics push path.
///
/// The Prometheus exposition above remains the **default and always-supported**
/// scrape path — nothing here changes that. This module adds an *opt-in*
/// periodic-push exporter that delivers the OpenTelemetry metrics pipeline to an
/// OTLP/gRPC collector, mirroring the OTLP **trace** wiring in
/// [`crate::telemetry`] (same `opentelemetry-otlp` 0.26 pipeline API, same
/// `tonic` transport, same `opentelemetry_sdk::runtime::Tokio`).
///
/// # Env gating
///
/// [`install_from_env`] only builds the exporter when **both**:
/// - `AERO_OTLP_METRICS` is truthy (`1`/`true`/`yes`/`on`, case-insensitive), and
/// - an OTLP endpoint is available — either the explicit `AERO_OTLP_ENDPOINT`
///   env (single-underscore, matching the trace-config override style) or, as a
///   fallback, the same `telemetry.otlp_endpoint` already used for traces, which
///   the caller passes in.
///
/// When the gate is off (the default), nothing is installed and Prometheus stays
/// the only metrics path. The decision itself is factored into the pure,
/// unit-tested [`resolve_from_env`] so the gating logic is verifiable without a
/// collector or a Tokio runtime.
///
/// # Staging seam
///
/// Building the exporter is real and compiles, but actually *delivering* points
/// requires a live OTLP collector reachable at the endpoint (e.g. an
/// OpenTelemetry Collector or Jaeger/Tempo at `:4317`). With no collector the
/// `PeriodicReader` simply logs export failures on its interval — it never takes
/// the process down — so end-to-end delivery is a staging/prod concern, exactly
/// like the trace exporter.
pub mod otlp {
    /// Default push interval when none is configured (seconds).
    pub const DEFAULT_INTERVAL_SECS: u64 = 15;

    /// Configuration for the OTLP metrics push exporter.
    #[derive(Debug, Clone)]
    pub struct OtlpExportConfig {
        /// OTLP collector endpoint, e.g. `http://localhost:4317`.
        pub endpoint: String,
        /// Export interval in seconds.
        pub interval_secs: u64,
        /// Value reported as the `service.name` resource attribute.
        pub service_name: String,
    }

    impl OtlpExportConfig {
        /// Builds a config from a collector endpoint with a sane default interval
        /// and service name.
        #[must_use]
        pub fn new(endpoint: impl Into<String>) -> Self {
            Self {
                endpoint: endpoint.into(),
                interval_secs: DEFAULT_INTERVAL_SECS,
                service_name: "aero".to_owned(),
            }
        }

        /// Overrides the `service.name` resource attribute.
        #[must_use]
        pub fn with_service_name(mut self, service_name: impl Into<String>) -> Self {
            self.service_name = service_name.into();
            self
        }
    }

    /// Held for the process lifetime to keep the push pipeline alive; its `Drop`
    /// flushes and shuts the meter provider down cleanly (mirrors the trace
    /// `TelemetryGuard`). Dropping it stops the periodic export.
    pub struct OtlpMetricsGuard {
        provider: Option<opentelemetry_sdk::metrics::SdkMeterProvider>,
    }

    impl std::fmt::Debug for OtlpMetricsGuard {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("OtlpMetricsGuard")
                .field("installed", &self.provider.is_some())
                .finish()
        }
    }

    impl Drop for OtlpMetricsGuard {
        fn drop(&mut self) {
            if let Some(p) = self.provider.take() {
                // Best-effort final flush; never panic out of Drop.
                let _ = p.shutdown();
            }
        }
    }

    /// Outcome of [`install_from_env`], so the caller can log/branch on whether
    /// the optional OTLP path actually engaged without unwrapping a guard.
    #[derive(Debug)]
    pub enum InstallOutcome {
        /// `AERO_OTLP_METRICS` was off (or unset) — Prometheus stays the path.
        Disabled,
        /// Gate was on but no endpoint could be resolved from env/config.
        NoEndpoint,
        /// Exporter installed; hold the guard for the process lifetime.
        Installed(OtlpMetricsGuard),
    }

    /// Pure env-resolution: returns `Some(endpoint)` when the OTLP metrics path
    /// should be installed, `None` otherwise. Splitting this out keeps the gate
    /// logic unit-testable (no runtime / collector / globals touched).
    ///
    /// `get_env` abstracts `std::env::var` so tests can drive it deterministically.
    /// `config_endpoint` is the trace endpoint already resolved from config
    /// (`telemetry.otlp_endpoint`), used as a fallback when `AERO_OTLP_ENDPOINT`
    /// is unset.
    pub fn resolve_from_env<F>(get_env: F, config_endpoint: Option<&str>) -> Option<String>
    where
        F: Fn(&str) -> Option<String>,
    {
        if !get_env("AERO_OTLP_METRICS").is_some_and(|v| is_truthy(&v)) {
            return None;
        }
        // Explicit env wins; otherwise reuse the trace endpoint from config.
        get_env("AERO_OTLP_ENDPOINT")
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .or_else(|| config_endpoint.map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned))
    }

    /// Truthy parse for the gate flag: `1`/`true`/`yes`/`on` (case-insensitive).
    #[must_use]
    pub fn is_truthy(raw: &str) -> bool {
        matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    }

    /// Env-gated entry point: build & install the OTLP metrics exporter iff the
    /// gate is on and an endpoint resolves. Reads `AERO_OTLP_METRICS` /
    /// `AERO_OTLP_ENDPOINT` from the process env, falling back to `config_endpoint`
    /// (the trace endpoint from `telemetry.otlp_endpoint`).
    ///
    /// Must run inside a Tokio runtime (the `PeriodicReader` spawns the push task).
    /// Never errors out of the happy path: on exporter-build failure it logs and
    /// reports [`InstallOutcome::NoEndpoint`]-equivalent disable so telemetry can
    /// never take the process down.
    #[must_use]
    pub fn install_from_env(service_name: &str, config_endpoint: Option<&str>) -> InstallOutcome {
        let Some(endpoint) = resolve_from_env(
            |k| std::env::var(k).ok(),
            config_endpoint,
        ) else {
            // Distinguish "gate off" from "gate on but no endpoint" for clearer logs.
            let gate_on = std::env::var("AERO_OTLP_METRICS").is_ok_and(|v| is_truthy(&v));
            return if gate_on {
                InstallOutcome::NoEndpoint
            } else {
                InstallOutcome::Disabled
            };
        };

        let cfg = OtlpExportConfig::new(endpoint).with_service_name(service_name);
        match install(&cfg) {
            Ok(guard) => {
                tracing::info!(
                    service = service_name,
                    endpoint = %cfg_endpoint_for_log(&cfg),
                    "OTLP metrics export wired (Prometheus exposition still default)"
                );
                InstallOutcome::Installed(guard)
            }
            Err(e) => {
                tracing::warn!(error = e, "OTLP metrics export disabled (exporter build failed)");
                InstallOutcome::NoEndpoint
            }
        }
    }

    // Tiny helper so the log line above never borrows a moved value.
    fn cfg_endpoint_for_log(cfg: &OtlpExportConfig) -> String {
        cfg.endpoint.clone()
    }

    /// Builds and installs a periodic-push OTLP `SdkMeterProvider`, registering it
    /// as the global OpenTelemetry meter provider and returning a guard that keeps
    /// it alive (and flushes on drop).
    ///
    /// This mirrors [`crate::telemetry::init`]'s trace wiring: the same
    /// `opentelemetry_otlp::new_pipeline()` builder, a `tonic` exporter pointed at
    /// `cfg.endpoint`, and `opentelemetry_sdk::runtime::Tokio` driving the periodic
    /// reader. The Prometheus exposition path is independent and unaffected.
    ///
    /// # Errors
    /// Returns the underlying exporter/pipeline build error (e.g. an invalid
    /// endpoint) as a string. Callers in the boot path prefer [`install_from_env`],
    /// which downgrades any such error to a logged no-op.
    pub fn install(cfg: &OtlpExportConfig) -> Result<OtlpMetricsGuard, String> {
        use opentelemetry::KeyValue;
        use opentelemetry_otlp::WithExportConfig;

        let interval = std::time::Duration::from_secs(if cfg.interval_secs == 0 {
            DEFAULT_INTERVAL_SECS
        } else {
            cfg.interval_secs
        });

        let provider = opentelemetry_otlp::new_pipeline()
            .metrics(opentelemetry_sdk::runtime::Tokio)
            .with_exporter(
                opentelemetry_otlp::new_exporter()
                    .tonic()
                    .with_endpoint(cfg.endpoint.clone()),
            )
            .with_period(interval)
            .with_resource(opentelemetry_sdk::Resource::new(vec![KeyValue::new(
                "service.name",
                cfg.service_name.clone(),
            )]))
            .build()
            .map_err(|e| format!("build OTLP metrics pipeline: {e}"))?;

        // Make this the process-wide meter provider so any OTel instruments
        // created via `opentelemetry::global::meter(..)` flow to the collector.
        opentelemetry::global::set_meter_provider(provider.clone());

        Ok(OtlpMetricsGuard {
            provider: Some(provider),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_increments_and_renders() {
        let r = Registry::new();
        r.register_help("aero_test_total", MetricKind::Counter, "a test counter");
        r.inc_counter("aero_test_total", 1);
        r.inc_counter("aero_test_total", 4);
        let out = r.render_prometheus();
        assert!(out.contains("# HELP aero_test_total a test counter"));
        assert!(out.contains("# TYPE aero_test_total counter"));
        assert!(
            out.contains("\naero_test_total 5\n"),
            "expected counter value 5, got:\n{out}"
        );
    }

    #[test]
    fn labeled_counter_keeps_series_separate_and_sorts_labels() {
        let r = Registry::new();
        r.inc_counter_labeled(
            names::HTTP_REQUESTS_TOTAL,
            1,
            &[("route", "/a"), ("status", "200")],
        );
        // Same labels in reverse order must hit the SAME series.
        r.inc_counter_labeled(
            names::HTTP_REQUESTS_TOTAL,
            2,
            &[("status", "200"), ("route", "/a")],
        );
        r.inc_counter_labeled(
            names::HTTP_REQUESTS_TOTAL,
            7,
            &[("route", "/b"), ("status", "500")],
        );
        let out = r.render_prometheus();
        assert!(
            out.contains(r#"aero_http_requests_total{route="/a",status="200"} 3"#),
            "merged series wrong:\n{out}"
        );
        assert!(
            out.contains(r#"aero_http_requests_total{route="/b",status="500"} 7"#),
            "second series wrong:\n{out}"
        );
    }

    #[test]
    fn gauge_set_add_inc_dec() {
        let r = Registry::new();
        r.set_gauge(names::WS_CONNECTIONS, 10.0);
        r.inc_gauge(names::WS_CONNECTIONS);
        r.inc_gauge(names::WS_CONNECTIONS);
        r.dec_gauge(names::WS_CONNECTIONS);
        // 10 + 1 + 1 - 1 = 11
        let out = r.render_prometheus();
        assert!(
            out.contains("\naero_ws_connections 11\n"),
            "expected gauge 11, got:\n{out}"
        );
        assert!(out.contains("# TYPE aero_ws_connections gauge"));

        // Negative add works (gauge can go down past where set put it).
        r.add_gauge(names::WS_CONNECTIONS, -11.0);
        assert!(r.render_prometheus().contains("\naero_ws_connections 0\n"));
    }

    #[test]
    fn histogram_buckets_sum_and_count() {
        let r = Registry::new();
        r.register_histogram(
            names::HTTP_REQUEST_DURATION_SECONDS,
            &[0.01, 0.1, 1.0],
            "req duration",
        );
        for v in [0.005, 0.05, 0.5, 5.0] {
            r.observe_histogram(names::HTTP_REQUEST_DURATION_SECONDS, v);
        }
        let out = r.render_prometheus();
        // Cumulative buckets: <=0.01 →1, <=0.1 →2, <=1.0 →3, +Inf →4.
        assert!(out.contains(r#"aero_http_request_duration_seconds_bucket{le="0.01"} 1"#));
        assert!(out.contains(r#"aero_http_request_duration_seconds_bucket{le="0.1"} 2"#));
        assert!(out.contains(r#"aero_http_request_duration_seconds_bucket{le="1"} 3"#));
        assert!(out.contains(r#"aero_http_request_duration_seconds_bucket{le="+Inf"} 4"#));
        assert!(out.contains("aero_http_request_duration_seconds_count 4"));
        // sum = 0.005 + 0.05 + 0.5 + 5.0 = 5.555
        assert!(
            out.contains("aero_http_request_duration_seconds_sum 5.555"),
            "sum wrong:\n{out}"
        );
        assert!(out.contains("# TYPE aero_http_request_duration_seconds histogram"));
    }

    #[test]
    fn labeled_histogram_per_series() {
        let r = Registry::new();
        r.observe_histogram_labeled(names::AI_JOB_DURATION_SECONDS, 0.2, &[("model", "sonnet")]);
        r.observe_histogram_labeled(names::AI_JOB_DURATION_SECONDS, 0.3, &[("model", "sonnet")]);
        r.observe_histogram_labeled(names::AI_JOB_DURATION_SECONDS, 9.0, &[("model", "haiku")]);
        let out = r.render_prometheus();
        assert!(out.contains(r#"aero_ai_job_duration_seconds_count{model="sonnet"} 2"#));
        assert!(out.contains(r#"aero_ai_job_duration_seconds_count{model="haiku"} 1"#));
        // sonnet sum = 0.5
        assert!(out.contains(r#"aero_ai_job_duration_seconds_sum{model="sonnet"} 0.5"#));
    }

    #[test]
    fn label_values_are_escaped() {
        let r = Registry::new();
        r.inc_counter_labeled("aero_escaping_total", 1, &[("path", r#"a"b\c"#)]);
        let out = r.render_prometheus();
        assert!(
            out.contains(r#"aero_escaping_total{path="a\"b\\c"} 1"#),
            "escaping wrong:\n{out}"
        );
    }

    #[test]
    fn whole_numbers_render_without_decimal() {
        let r = Registry::new();
        r.set_gauge("aero_whole", 42.0);
        r.set_gauge_labeled("aero_frac", 0.25, &[]);
        let out = r.render_prometheus();
        assert!(out.contains("\naero_whole 42\n"), "whole:\n{out}");
        assert!(out.contains("\naero_frac 0.25\n"), "frac:\n{out}");
    }

    #[test]
    fn ai_cost_counter_accumulates() {
        let r = Registry::new();
        // 1.25 USD worth of calls expressed in micro-USD.
        r.inc_counter(names::AI_COST_MICROS_TOTAL, 1_000_000);
        r.inc_counter(names::AI_COST_MICROS_TOTAL, 250_000);
        assert!(r
            .render_prometheus()
            .contains("\naero_ai_cost_micros_total 1250000\n"));
    }

    #[test]
    fn db_pool_saturation_gauges() {
        let r = Registry::new();
        r.set_gauge(names::DB_POOL_SIZE, 16.0);
        r.set_gauge(names::DB_POOL_IN_USE, 4.0);
        let out = r.render_prometheus();
        assert!(out.contains("\naero_db_pool_size 16\n"));
        assert!(out.contains("\naero_db_pool_in_use 4\n"));
    }

    #[test]
    fn output_is_deterministically_sorted_by_name() {
        let r = Registry::new();
        r.inc_counter("aero_zzz_total", 1);
        r.inc_counter("aero_aaa_total", 1);
        let out = r.render_prometheus();
        let aaa = out.find("aero_aaa_total").expect("aaa present");
        let zzz = out.find("aero_zzz_total").expect("zzz present");
        assert!(aaa < zzz, "metrics not sorted by name:\n{out}");
    }

    #[test]
    fn global_registry_is_seeded_with_known_help() {
        // The global is shared across tests in this binary; only assert on
        // monotonic / presence facts that other tests cannot invalidate.
        inc_counter(names::MESSAGES_SENT_TOTAL, 1);
        let out = render_prometheus();
        assert!(out.contains("# TYPE aero_messages_sent_total counter"));
        assert!(out.contains("# HELP aero_ws_connections"));
        assert!(out.contains("# TYPE aero_ai_job_duration_seconds histogram"));
    }

    #[test]
    fn otlp_export_is_opt_in_and_off_by_default() {
        // The config shape is stable; the default interval is the documented one.
        let cfg = otlp::OtlpExportConfig::new("http://localhost:4317");
        assert_eq!(cfg.interval_secs, otlp::DEFAULT_INTERVAL_SECS);

        // OTLP metrics export is now wired but OPT-IN: with the gate flag absent,
        // env-resolution yields None so Prometheus exposition stays the path.
        // (The real `install` builds a Tokio-runtime push pipeline — a staging
        // seam — so it is exercised via the pure gating tests in `otlp_tests`,
        // not by spinning a runtime here.)
        let resolved = otlp::resolve_from_env(|_| None, Some("http://localhost:4317"));
        assert_eq!(resolved, None, "gate off -> no OTLP install, Prometheus default");
    }

    #[test]
    fn empty_label_set_renders_no_braces() {
        let ls = LabelSet::from_pairs(&[]);
        assert_eq!(ls.render(None), "");
        // An unlabeled counter must render with no `{}` block.
        let r = Registry::new();
        r.inc_counter("aero_unlabeled_total", 1);
        assert!(r
            .render_prometheus()
            .contains("\naero_unlabeled_total 1\n"));
    }
}

#[cfg(test)]
mod otlp_tests {
    use super::otlp::{is_truthy, resolve_from_env, OtlpExportConfig, DEFAULT_INTERVAL_SECS};
    use std::collections::HashMap;

    /// Build a deterministic `get_env` closure over a fixed map, so the pure
    /// gating logic can be driven without touching the real process env.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> =
            pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn truthy_flag_parsing() {
        for v in ["1", "true", "TRUE", "Yes", "on", " on "] {
            assert!(is_truthy(v), "{v:?} should be truthy");
        }
        for v in ["0", "false", "no", "off", "", "maybe"] {
            assert!(!is_truthy(v), "{v:?} should be falsey");
        }
    }

    #[test]
    fn gate_off_resolves_to_none_even_with_endpoint() {
        // Endpoint present but the gate flag is absent -> Prometheus stays the path.
        let get = env_of(&[("AERO_OTLP_ENDPOINT", "http://collector:4317")]);
        assert_eq!(resolve_from_env(get, Some("http://cfg:4317")), None);

        // Gate explicitly off also resolves to None.
        let get = env_of(&[
            ("AERO_OTLP_METRICS", "false"),
            ("AERO_OTLP_ENDPOINT", "http://collector:4317"),
        ]);
        assert_eq!(resolve_from_env(get, None), None);
    }

    #[test]
    fn gate_on_prefers_explicit_env_endpoint() {
        let get = env_of(&[
            ("AERO_OTLP_METRICS", "1"),
            ("AERO_OTLP_ENDPOINT", "  http://explicit:4317  "),
        ]);
        // Explicit env wins and is trimmed; config fallback is ignored.
        assert_eq!(
            resolve_from_env(get, Some("http://cfg:4317")),
            Some("http://explicit:4317".to_owned())
        );
    }

    #[test]
    fn gate_on_falls_back_to_config_endpoint() {
        let get = env_of(&[("AERO_OTLP_METRICS", "true")]);
        assert_eq!(
            resolve_from_env(get, Some("http://cfg:4317")),
            Some("http://cfg:4317".to_owned())
        );
    }

    #[test]
    fn gate_on_but_no_endpoint_resolves_to_none() {
        // Gate on, but neither explicit env nor config supplies an endpoint.
        let get = env_of(&[("AERO_OTLP_METRICS", "yes")]);
        assert_eq!(resolve_from_env(get, None), None);

        // Blank/whitespace endpoints are treated as absent (env and config).
        let get = env_of(&[("AERO_OTLP_METRICS", "yes"), ("AERO_OTLP_ENDPOINT", "   ")]);
        assert_eq!(resolve_from_env(get, Some("   ")), None);
    }

    #[test]
    fn export_config_builder_defaults_and_overrides() {
        let cfg = OtlpExportConfig::new("http://localhost:4317");
        assert_eq!(cfg.endpoint, "http://localhost:4317");
        assert_eq!(cfg.interval_secs, DEFAULT_INTERVAL_SECS);
        assert_eq!(cfg.service_name, "aero");

        let cfg = cfg.with_service_name("aero-server");
        assert_eq!(cfg.service_name, "aero-server");
    }
}
