//! Tests for [`super`], split out to keep the parent under the 1200-line HARD limit.

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
fn global_labeled_gauge_add_wrapper_balances() {
    const NAME: &str = "aero_test_labeled_add_gauge";
    add_gauge_labeled(NAME, 1.0, &[("stage", "test")]);
    add_gauge_labeled(NAME, -1.0, &[("stage", "test")]);
    assert!(render_prometheus().contains("aero_test_labeled_add_gauge{stage=\"test\"} 0"));
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
fn webhook_delivery_metrics_have_registered_help_and_types() {
    let r = Registry::new();
    register_known_metrics(&r);
    let out = r.render_prometheus();
    for expected in [
        "# HELP aero_webhook_delivery_queue_depth ",
        "# TYPE aero_webhook_delivery_queue_depth gauge",
        "# HELP aero_webhook_delivery_in_flight ",
        "# TYPE aero_webhook_delivery_in_flight gauge",
        "# HELP aero_webhook_delivery_duration_seconds ",
        "# TYPE aero_webhook_delivery_duration_seconds histogram",
        "# HELP aero_webhook_delivery_outcomes_total ",
        "# TYPE aero_webhook_delivery_outcomes_total counter",
    ] {
        assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
    }
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
    assert_eq!(
        resolved, None,
        "gate off -> no OTLP install, Prometheus default"
    );
}

#[test]
fn empty_label_set_renders_no_braces() {
    let ls = LabelSet::from_pairs(&[]);
    assert_eq!(ls.render(None), "");
    // An unlabeled counter must render with no `{}` block.
    let r = Registry::new();
    r.inc_counter("aero_unlabeled_total", 1);
    assert!(r.render_prometheus().contains("\naero_unlabeled_total 1\n"));
}

// (Originally a sibling `mod otlp_tests`; flattened into this `tests` module
// during the split — `super::otlp` still resolves to `metrics::otlp`.)
use super::otlp::{is_truthy, resolve_from_env, OtlpExportConfig, DEFAULT_INTERVAL_SECS};
use std::collections::HashMap;

/// Build a deterministic `get_env` closure over a fixed map, so the pure
/// gating logic can be driven without touching the real process env.
fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
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

#[derive(Clone, Debug)]
struct SharedManualReader(std::sync::Arc<opentelemetry_sdk::metrics::ManualReader>);

impl opentelemetry_sdk::metrics::reader::TemporalitySelector for SharedManualReader {
    fn temporality(
        &self,
        kind: opentelemetry_sdk::metrics::InstrumentKind,
    ) -> opentelemetry_sdk::metrics::data::Temporality {
        self.0.temporality(kind)
    }
}

impl opentelemetry_sdk::metrics::reader::MetricReader for SharedManualReader {
    fn register_pipeline(&self, pipeline: std::sync::Weak<opentelemetry_sdk::metrics::Pipeline>) {
        self.0.register_pipeline(pipeline);
    }

    fn collect(
        &self,
        metrics: &mut opentelemetry_sdk::metrics::data::ResourceMetrics,
    ) -> opentelemetry::metrics::Result<()> {
        self.0.collect(metrics)
    }

    fn force_flush(&self) -> opentelemetry::metrics::Result<()> {
        self.0.force_flush()
    }

    fn shutdown(&self) -> opentelemetry::metrics::Result<()> {
        self.0.shutdown()
    }
}

#[test]
fn otel_mirror_emits_registry_updates_with_dynamic_labels() {
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry::KeyValue;
    use opentelemetry_sdk::metrics::data::{
        Gauge as OtelGauge, Histogram as OtelHistogram, ResourceMetrics, Sum,
    };
    use opentelemetry_sdk::metrics::reader::MetricReader as _;

    const COUNTER: &str = "aero_test_otel_mirror_total";
    const GAUGE: &str = "aero_test_otel_mirror_gauge";
    const HISTOGRAM: &str = "aero_test_otel_mirror_seconds";

    let registry = Registry::new();
    registry.register_help(COUNTER, MetricKind::Counter, "mirror counter");
    registry.register_help(GAUGE, MetricKind::Gauge, "mirror gauge");
    registry.register_histogram(HISTOGRAM, &[0.1, 0.5], "mirror histogram");

    // These pre-install values remain in Prometheus but must not be replayed
    // through a no-op instrument cached before the SDK provider exists.
    registry.inc_counter_labeled(COUNTER, 40, &[("route", "/before"), ("status", "200")]);
    registry.set_gauge_labeled(GAUGE, 99.0, &[("shard", "blue")]);
    registry.observe_histogram_labeled(HISTOGRAM, 9.0, &[("kind", "answer")]);

    let reader = SharedManualReader(std::sync::Arc::new(
        opentelemetry_sdk::metrics::ManualReader::builder().build(),
    ));
    let provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_reader(reader.clone())
        .build();
    assert!(registry.install_otel_meter(provider.meter("aero-common-test")));

    registry.inc_counter_labeled(COUNTER, 2, &[("status", "200"), ("route", "/after")]);
    registry.set_gauge_labeled(GAUGE, 4.0, &[("shard", "blue")]);
    registry.add_gauge_labeled(GAUGE, 2.5, &[("shard", "blue")]);
    registry.observe_histogram_labeled(HISTOGRAM, 0.05, &[("kind", "answer")]);
    registry.observe_histogram_labeled(HISTOGRAM, 0.75, &[("kind", "answer")]);

    let mut collected = ResourceMetrics {
        resource: opentelemetry_sdk::Resource::default(),
        scope_metrics: Vec::new(),
    };
    reader.collect(&mut collected).expect("collect metrics");
    let metric = |name: &str| {
        collected
            .scope_metrics
            .iter()
            .flat_map(|scope| &scope.metrics)
            .find(|metric| metric.name == name)
            .unwrap_or_else(|| panic!("missing OTel metric {name}"))
    };

    let counter = metric(COUNTER)
        .data
        .as_any()
        .downcast_ref::<Sum<u64>>()
        .expect("counter aggregation");
    assert_eq!(counter.data_points.len(), 1, "pre-install series leaked");
    let counter_point = counter
        .data_points
        .iter()
        .find(|point| point.attributes.contains(&KeyValue::new("route", "/after")))
        .expect("labeled counter point");
    assert_eq!(counter_point.value, 2, "pre-install count was replayed");
    assert!(counter_point
        .attributes
        .contains(&KeyValue::new("status", "200")));

    let gauge = metric(GAUGE)
        .data
        .as_any()
        .downcast_ref::<OtelGauge<f64>>()
        .expect("gauge aggregation");
    assert!((gauge.data_points[0].value - 6.5).abs() < f64::EPSILON);
    assert!(gauge.data_points[0]
        .attributes
        .contains(&KeyValue::new("shard", "blue")));

    let histogram = metric(HISTOGRAM)
        .data
        .as_any()
        .downcast_ref::<OtelHistogram<f64>>()
        .expect("histogram aggregation");
    assert_eq!(histogram.data_points[0].count, 2);
    assert!((histogram.data_points[0].sum - 0.8).abs() < f64::EPSILON);
    assert_eq!(histogram.data_points[0].bounds, [0.1, 0.5]);
    assert!(histogram.data_points[0]
        .attributes
        .contains(&KeyValue::new("kind", "answer")));

    let prometheus = registry.render_prometheus();
    assert!(prometheus.contains(r#"aero_test_otel_mirror_total{route="/before",status="200"} 40"#));
    assert!(prometheus.contains("aero_test_otel_mirror_seconds_count{kind=\"answer\"} 3"));
}
