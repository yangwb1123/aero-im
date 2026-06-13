//! `tracing` initialization + optional OTLP trace export.
//!
//! Call [`init`] exactly once at process startup. Returns a guard that must
//! be held for the lifetime of the process (its `Drop` flushes buffered spans).
//!
//! Logs always go to stdout (env-filtered). When `telemetry.otlp_endpoint` is
//! set, spans are ALSO batch-exported via OTLP/gRPC to that collector (e.g.
//! Jaeger at `:4317`). Building the exporter is best-effort: any failure logs a
//! warning and falls back to stdout-only so telemetry can never take the process
//! down. Must be called inside a Tokio runtime (the batch exporter spawns a task).

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

use crate::config::TelemetryConfig;

/// Initializes structured logging and (optionally) OTLP trace export.
pub fn init(cfg: &TelemetryConfig, service_name: &'static str) -> TelemetryGuard {
    // Install the W3C TraceContext propagator so inbound HTTP / NATS `traceparent`
    // headers can be extracted and outbound calls can carry the current context —
    // the basis for cross-process trace continuity (ROADMAP5 方向二). Harmless when
    // OTLP export is unconfigured (the active context is then empty).
    install_trace_propagator();

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.log_level));

    // `AERO_LOG_FORMAT=json` switches stdout to newline-delimited JSON that
    // FLATTENS the current span's fields into each event (ROADMAP5 方向二) — so
    // `request_id` (set on the per-request `http_request` span) and the active
    // `trace_id`/`span_id` land in every structured log line, letting an operator
    // pivot from an `x-request-id` to that request's logs and trace. Any other
    // value (the default) keeps the human-readable console format for local dev.
    let json_logs = std::env::var("AERO_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    let fmt_layer = if json_logs {
        tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(true)
            .with_span_list(false)
            .with_target(true)
            .boxed()
    } else {
        tracing_subscriber::fmt::layer().with_target(true).with_level(true).boxed()
    };

    let registry = tracing_subscriber::registry().with(env_filter).with(fmt_layer);

    // Best-effort OTLP provider: None when unconfigured or on build failure.
    let provider = cfg.otlp_endpoint.as_deref().and_then(|endpoint| {
        match build_otlp_provider(endpoint, service_name, cfg.trace_sample_rate) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("OTLP export disabled (exporter build failed): {e:#}");
                None
            }
        }
    });

    if let Some(p) = provider {
        use opentelemetry::trace::TracerProvider as _;
        let tracer = p.tracer(service_name);
        registry
            .with(tracing_opentelemetry::layer().with_tracer(tracer))
            .init();
        tracing::info!(service = service_name, "OTLP trace export wired");
        TelemetryGuard { provider: Some(p) }
    } else {
        registry.init();
        TelemetryGuard { provider: None }
    }
}

/// Install the global W3C `TraceContext` propagator. Idempotent; called by
/// [`init`], exposed for tests / alternate setups.
pub fn install_trace_propagator() {
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );
}

/// Inject the **current** span's W3C trace context into `carrier` (a header map),
/// so a downstream process reading those headers can continue the same trace.
/// A no-op (leaves `carrier` untouched) when no span context is active — e.g. OTLP
/// export is unconfigured, so producers pay nothing in that mode.
// The OTel `Injector`/`Extractor` impls are only provided for the default-hasher
// `HashMap`, so the parameter can't be generalized over `BuildHasher`.
#[allow(clippy::implicit_hasher)]
pub fn inject_trace_context(carrier: &mut std::collections::HashMap<String, String>) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    let cx = tracing::Span::current().context();
    opentelemetry::global::get_text_map_propagator(|p| p.inject_context(&cx, carrier));
}

/// The current span's W3C `traceparent` string, if any — convenience over
/// [`inject_trace_context`] for producers that stamp a single header onto a
/// payload (e.g. the NATS event envelope). `None` when no trace context is active.
#[must_use]
pub fn current_traceparent() -> Option<String> {
    let mut carrier = std::collections::HashMap::new();
    inject_trace_context(&mut carrier);
    carrier.remove("traceparent")
}

/// Extract a remote W3C trace context from `carrier`. Attach it to a consuming
/// span via [`OpenTelemetrySpanExt::set_parent`](tracing_opentelemetry::OpenTelemetrySpanExt::set_parent)
/// so the consumer's work nests under the producer's trace.
// See `inject_trace_context`: the OTel `Extractor` impl pins the default hasher.
#[allow(clippy::implicit_hasher)]
#[must_use]
pub fn extract_trace_context(
    carrier: &std::collections::HashMap<String, String>,
) -> opentelemetry::Context {
    opentelemetry::global::get_text_map_propagator(|p| p.extract(carrier))
}

/// Parent `span` to a remote W3C `traceparent`, nesting a consumer span under the
/// producer's trace (so e.g. a NATS-bus consumer's work shows up under the
/// message-send trace). No-op for an empty/unparseable value. Keeps the
/// `tracing-opentelemetry` dependency contained to this crate.
pub fn set_span_parent_from_traceparent(span: &tracing::Span, traceparent: &str) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    if traceparent.is_empty() {
        return;
    }
    let mut carrier = std::collections::HashMap::new();
    carrier.insert("traceparent".to_string(), traceparent.to_string());
    span.set_parent(extract_trace_context(&carrier));
}

#[cfg(test)]
mod json_log_tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    /// A `MakeWriter` that appends every log line to a shared buffer, so a test can
    /// inspect exactly what the formatter wrote.
    #[derive(Clone, Default)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for BufWriter {
        type Writer = BufWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// The JSON formatter with `with_current_span(true)` FLATTENS the active span's
    /// fields into each event line — so a `request_id` set on the enclosing
    /// `http_request` span appears in the structured log, which is the whole point
    /// of the `AERO_LOG_FORMAT=json` mode (an operator can pivot from an
    /// `x-request-id` to that request's logs). Verified with a thread-local
    /// subscriber so it neither needs nor disturbs the global `init`.
    #[test]
    fn json_logs_flatten_current_span_request_id() {
        let buf = BufWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_current_span(true)
            .with_span_list(false)
            .with_writer(buf.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("http_request", request_id = "req-abc-123");
            let _enter = span.enter();
            tracing::info!(target: "test", "handled the request");
        });

        let out = String::from_utf8(buf.0.lock().unwrap().clone()).expect("utf8 log");
        assert!(out.contains("request_id"), "span field key present in JSON: {out}");
        assert!(out.contains("req-abc-123"), "span field value present in JSON: {out}");
        assert!(out.contains("handled the request"), "the event message is present: {out}");
        // It really is JSON (the event renders as an object with a "fields"/message).
        assert!(out.trim_start().starts_with('{'), "line is a JSON object: {out}");
    }
}

#[cfg(test)]
mod trace_propagation_tests {
    use super::{extract_trace_context, install_trace_propagator};
    use opentelemetry::trace::{
        SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState,
    };
    use std::collections::HashMap;

    /// The W3C trace context survives an inject → header-map → extract round-trip:
    /// the trace id a producer stamps is the trace id a consumer recovers. This is
    /// the cross-process propagation primitive (verifiable without a collector —
    /// the collector only renders the resulting tree).
    #[test]
    fn trace_context_round_trips_through_a_carrier() {
        install_trace_propagator();
        let trace_id = TraceId::from_bytes([7u8; 16]);
        let span_id = SpanId::from_bytes([3u8; 8]);
        let sc = SpanContext::new(trace_id, span_id, TraceFlags::SAMPLED, true, TraceState::default());
        let cx = opentelemetry::Context::new().with_remote_span_context(sc);

        let mut carrier: HashMap<String, String> = HashMap::new();
        opentelemetry::global::get_text_map_propagator(|p| {
            p.inject_context(&cx, &mut carrier);
        });
        assert!(carrier.contains_key("traceparent"), "traceparent injected: {carrier:?}");

        let extracted = extract_trace_context(&carrier);
        assert_eq!(
            extracted.span().span_context().trace_id(),
            trace_id,
            "the producer's trace id is recovered by the consumer",
        );
    }
}

/// Attribute key that forces a span (and its trace) to be recorded+sampled
/// regardless of the configured `AERO_TRACE_SAMPLE_RATE`. See
/// [`ForcePrioritySampler`] for the override semantics and `wiring_notes` for
/// how high-priority call sites set it.
pub const FORCE_SAMPLE_KEY: &str = "aero.force_sample";

/// Conventional OpenTelemetry priority attribute. A value `> 0` also forces the
/// span to be sampled, matching the classic `sampling.priority` Jaeger idiom.
pub const SAMPLING_PRIORITY_KEY: &str = "sampling.priority";

/// A [`ShouldSample`] wrapper that force-samples high-priority spans.
///
/// In `should_sample` it inspects the span's start `attributes`: if any carry
/// `aero.force_sample = true` (bool) or `sampling.priority > 0` (i64), it
/// returns [`SamplingDecision::RecordAndSample`] outright. Otherwise it
/// delegates verbatim to the wrapped `inner` sampler — so the normal
/// `ParentBased(TraceIdRatioBased(rate))` probabilistic behavior is preserved
/// for ordinary spans while critical operations (login, message-send,
/// call-create) are always captured even when `AERO_TRACE_SAMPLE_RATE` is low.
///
/// # Marking a span as high-priority
///
/// `tracing-opentelemetry` maps a span field named `aero.force_sample` onto an
/// `OTel` attribute of the same key. A high-priority call site therefore opts in
/// by declaring the field on its `#[instrument]` span, e.g.:
///
/// ```ignore
/// #[tracing::instrument(fields(aero.force_sample = true))]
/// async fn login(/* … */) { /* … */ }
/// ```
///
/// (Such call sites live in other crates and are intentionally NOT modified
/// here.)
#[derive(Clone, Debug)]
struct ForcePrioritySampler {
    inner: opentelemetry_sdk::trace::Sampler,
}

impl ForcePrioritySampler {
    /// Wrap `inner`, force-sampling spans that carry a priority marker.
    fn new(inner: opentelemetry_sdk::trace::Sampler) -> Self {
        Self { inner }
    }

    /// True when `attributes` request forced sampling via either marker key.
    fn is_forced(attributes: &[opentelemetry::KeyValue]) -> bool {
        use opentelemetry::Value;
        attributes.iter().any(|kv| match kv.key.as_str() {
            FORCE_SAMPLE_KEY => matches!(kv.value, Value::Bool(true)),
            SAMPLING_PRIORITY_KEY => matches!(kv.value, Value::I64(p) if p > 0),
            _ => false,
        })
    }
}

impl opentelemetry_sdk::trace::ShouldSample for ForcePrioritySampler {
    fn should_sample(
        &self,
        parent_context: Option<&opentelemetry::Context>,
        trace_id: opentelemetry::trace::TraceId,
        name: &str,
        span_kind: &opentelemetry::trace::SpanKind,
        attributes: &[opentelemetry::KeyValue],
        links: &[opentelemetry::trace::Link],
    ) -> opentelemetry::trace::SamplingResult {
        if Self::is_forced(attributes) {
            use opentelemetry::trace::TraceContextExt as _;
            return opentelemetry::trace::SamplingResult {
                decision: opentelemetry::trace::SamplingDecision::RecordAndSample,
                attributes: Vec::new(),
                // Preserve any inbound trace state, matching the SDK samplers.
                trace_state: parent_context.map_or_else(
                    opentelemetry::trace::TraceState::default,
                    |ctx| ctx.span().span_context().trace_state().clone(),
                ),
            };
        }
        self.inner
            .should_sample(parent_context, trace_id, name, span_kind, attributes, links)
    }
}

/// Build a batch-exporting tracer provider pointed at an OTLP/gRPC collector.
///
/// `sample_rate` is the trace sampling ratio in `[0.0, 1.0]` (already clamped by
/// the config layer). It is applied as a parent-based `TraceIdRatioBased` sampler
/// so root spans are sampled at `sample_rate` while child spans honor the parent
/// decision. That sampler is wrapped in a [`ForcePrioritySampler`] so spans
/// tagged with [`FORCE_SAMPLE_KEY`] / [`SAMPLING_PRIORITY_KEY`] are always
/// recorded+sampled regardless of `sample_rate`.
fn build_otlp_provider(
    endpoint: &str,
    service_name: &'static str,
    sample_rate: f64,
) -> anyhow::Result<opentelemetry_sdk::trace::TracerProvider> {
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::WithExportConfig;

    let sampler = ForcePrioritySampler::new(opentelemetry_sdk::trace::Sampler::ParentBased(
        Box::new(opentelemetry_sdk::trace::Sampler::TraceIdRatioBased(
            sample_rate,
        )),
    ));

    // opentelemetry-otlp 0.26 pipeline API: install_batch returns the
    // TracerProvider (which we keep in the guard to flush on shutdown).
    let provider = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(
            opentelemetry_otlp::new_exporter()
                .tonic()
                .with_endpoint(endpoint.to_owned()),
        )
        .with_trace_config(
            opentelemetry_sdk::trace::Config::default()
                .with_sampler(sampler)
                .with_resource(opentelemetry_sdk::Resource::new(vec![KeyValue::new(
                    "service.name",
                    service_name,
                )])),
        )
        .install_batch(opentelemetry_sdk::runtime::Tokio)?;
    Ok(provider)
}

pub struct TelemetryGuard {
    provider: Option<opentelemetry_sdk::trace::TracerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        // Flush buffered spans to the collector before exit.
        if let Some(p) = self.provider.take() {
            let _ = p.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{SamplingDecision, SpanKind, TraceId};
    use opentelemetry::KeyValue;
    use opentelemetry_sdk::trace::ShouldSample as _;

    /// Run the sampler against a root span (no parent) with the given start attrs.
    fn decide(
        sampler: &ForcePrioritySampler,
        attributes: &[KeyValue],
    ) -> SamplingDecision {
        sampler
            .should_sample(
                None,
                TraceId::from_bytes([1; 16]),
                "test-span",
                &SpanKind::Internal,
                attributes,
                &[],
            )
            .decision
    }

    #[test]
    fn force_sample_attribute_overrides_always_off_inner() {
        // Inner AlwaysOff would Drop everything; the marker must flip that.
        let sampler = ForcePrioritySampler::new(opentelemetry_sdk::trace::Sampler::AlwaysOff);
        let decision = decide(&sampler, &[KeyValue::new(FORCE_SAMPLE_KEY, true)]);
        assert_eq!(decision, SamplingDecision::RecordAndSample);
    }

    #[test]
    fn sampling_priority_above_zero_overrides_always_off_inner() {
        let sampler = ForcePrioritySampler::new(opentelemetry_sdk::trace::Sampler::AlwaysOff);
        let decision = decide(&sampler, &[KeyValue::new(SAMPLING_PRIORITY_KEY, 1_i64)]);
        assert_eq!(decision, SamplingDecision::RecordAndSample);
    }

    #[test]
    fn unmarked_span_delegates_to_inner() {
        // Without a marker the wrapper must defer to the inner Drop decision.
        let sampler = ForcePrioritySampler::new(opentelemetry_sdk::trace::Sampler::AlwaysOff);
        assert_eq!(decide(&sampler, &[]), SamplingDecision::Drop);

        // A falsey marker (false / priority 0) is NOT a force request.
        assert_eq!(
            decide(&sampler, &[KeyValue::new(FORCE_SAMPLE_KEY, false)]),
            SamplingDecision::Drop
        );
        assert_eq!(
            decide(&sampler, &[KeyValue::new(SAMPLING_PRIORITY_KEY, 0_i64)]),
            SamplingDecision::Drop
        );

        // Delegation also passes through an AlwaysOn inner unchanged.
        let on = ForcePrioritySampler::new(opentelemetry_sdk::trace::Sampler::AlwaysOn);
        assert_eq!(decide(&on, &[]), SamplingDecision::RecordAndSample);
    }
}
