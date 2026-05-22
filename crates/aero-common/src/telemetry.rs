//! `tracing` + OpenTelemetry initialization.
//!
//! Call [`init`] exactly once at process startup.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::TracerProvider;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::config::TelemetryConfig;

/// Initializes structured logging and (optionally) OTLP trace export.
///
/// Returns a guard that flushes the tracer provider on drop.
pub fn init(cfg: &TelemetryConfig, service_name: &'static str) -> TelemetryGuard {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&cfg.log_level));

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_level(true);

    let registry = tracing_subscriber::registry().with(env_filter).with(fmt_layer);

    let provider = if let Some(endpoint) = cfg.otlp_endpoint.as_deref() {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()
            .expect("build OTLP span exporter");
        let provider = TracerProvider::builder()
            .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
            .with_resource(opentelemetry_sdk::Resource::new(vec![
                opentelemetry::KeyValue::new("service.name", service_name),
            ]))
            .build();
        let tracer = provider.tracer(service_name);
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
        registry.with(otel_layer).init();
        Some(provider)
    } else {
        registry.init();
        None
    };

    TelemetryGuard { provider }
}

pub struct TelemetryGuard {
    provider: Option<TracerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(p) = self.provider.take() {
            let _ = p.shutdown();
        }
    }
}
