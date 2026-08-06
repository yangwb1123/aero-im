//! Graceful shutdown signal handler.
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub(crate) async fn shutdown_signal(
    shutting_down: Arc<std::sync::atomic::AtomicBool>,
    drain: Duration,
    ai_shutdown: CancellationToken,
) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("ctrl-c handler failed");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler failed")
            .recv()
            .await
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        _ = terminate => {}
    }

    tracing::info!("shutdown signal received, starting graceful shutdown");
    shutting_down.store(true, std::sync::atomic::Ordering::Relaxed);
    if !drain.is_zero() {
        tracing::info!(
            drain_secs = drain.as_secs(),
            "draining: /health/ready now 503, waiting before close"
        );
        tokio::time::sleep(drain).await;
    }
    ai_shutdown.cancel();
}
