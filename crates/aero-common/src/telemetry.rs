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

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::config::TelemetryConfig;

/// Initializes structured logging and (optionally) OTLP trace export.
pub fn init(cfg: &TelemetryConfig, service_name: &'static str) -> TelemetryGuard {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.log_level));

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_level(true);

    let registry = tracing_subscriber::registry().with(env_filter).with(fmt_layer);

    // Best-effort OTLP provider: None when unconfigured or on build failure.
    let provider = cfg.otlp_endpoint.as_deref().and_then(|endpoint| {
        match build_otlp_provider(endpoint, service_name) {
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

/// Build a batch-exporting tracer provider pointed at an OTLP/gRPC collector.
fn build_otlp_provider(
    endpoint: &str,
    service_name: &'static str,
) -> anyhow::Result<opentelemetry_sdk::trace::TracerProvider> {
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::WithExportConfig;

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
            opentelemetry_sdk::trace::Config::default().with_resource(
                opentelemetry_sdk::Resource::new(vec![KeyValue::new("service.name", service_name)]),
            ),
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
