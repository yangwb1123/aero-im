//! `tracing` initialization.
//!
//! Call [`init`] exactly once at process startup. Returns a guard that must
//! be held for the lifetime of the process.
//!
//! OTLP export is intentionally deferred to P2 — the API surface of
//! `opentelemetry-otlp` is in flux and we don't want compile breakage on every
//! point release. Logs go to stdout in JSON-ish format with env-filter.

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::config::TelemetryConfig;

/// Initializes structured logging.
pub fn init(cfg: &TelemetryConfig, service_name: &'static str) -> TelemetryGuard {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.log_level));

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_level(true);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .init();

    if let Some(endpoint) = cfg.otlp_endpoint.as_deref() {
        tracing::warn!(
            endpoint,
            service = service_name,
            "OTLP export not yet wired; see telemetry.rs"
        );
    }

    TelemetryGuard { _private: () }
}

pub struct TelemetryGuard {
    _private: (),
}
