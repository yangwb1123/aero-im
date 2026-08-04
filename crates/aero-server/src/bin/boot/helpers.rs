//! Utility helpers reused across boot modules.
use std::time::Duration;

/// Connect to a startup dependency with bounded exponential backoff.
pub(crate) async fn connect_with_retry<T, E, F, Fut>(
    what: &str,
    attempts: u32,
    mut f: F,
) -> anyhow::Result<T>
where
    E: std::fmt::Display,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let attempts = attempts.max(1);
    let mut delay = Duration::from_secs(1);
    for attempt in 1..=attempts {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < attempts => {
                tracing::warn!(
                    dependency = %what, attempt, max = attempts, error = %e,
                    backoff_secs = delay.as_secs(), "connect failed; retrying"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(30));
            }
            Err(e) => {
                anyhow::bail!("{what} connect failed after {attempts} attempts: {e}");
            }
        }
    }
    unreachable!("loop returns on the final attempt")
}

/// Resolve the mobile push gateways from environment.
pub(crate) fn build_push_gateways() -> aero_server::state::PushGateways {
    use std::sync::Arc;

    fn static_bearer_provider(env_key: &'static str) -> aero_push::TokenProvider {
        Arc::new(move || {
            let key = env_key;
            Box::pin(async move {
                std::env::var(key).map_err(|_| {
                    aero_push::PushError::Auth(format!(
                        "{key} unset (no push credential configured)"
                    ))
                })
            })
        })
    }

    let fcm = std::env::var("AERO_PUSH_FCM_PROJECT").ok().map(|project| {
        let gw = aero_push::FcmGateway::new(project, static_bearer_provider("AERO_PUSH_FCM_TOKEN"));
        Arc::new(gw) as Arc<dyn aero_push::PushGateway>
    });
    let apns = std::env::var("AERO_PUSH_APNS_TOPIC").ok().map(|topic| {
        let gw = aero_push::ApnsGateway::new(topic, static_bearer_provider("AERO_PUSH_APNS_TOKEN"));
        Arc::new(gw) as Arc<dyn aero_push::PushGateway>
    });
    aero_server::state::PushGateways { fcm, apns }
}

/// Resolve the RTMP-shaped backing address for SRT ingest.
pub(crate) fn srt_backing_rtmp_addr(rtmp_addr: &std::net::SocketAddr) -> std::net::SocketAddr {
    match std::env::var("AERO__LIVE__SRT_LISTEN")
        .ok()
        .and_then(|s| s.trim().parse::<std::net::SocketAddr>().ok())
    {
        Some(srt) => {
            let backing_port = srt.port().checked_sub(1);
            match backing_port {
                Some(p) => std::net::SocketAddr::new(srt.ip(), p),
                None => *rtmp_addr,
            }
        }
        None => *rtmp_addr,
    }
}
