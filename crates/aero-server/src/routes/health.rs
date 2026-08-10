//! Health-check endpoints: liveness, readiness, combined probe.
//!
//! Split from the monolithic `routes.rs` (`REFACTOR_PLAN.md` Step 7 clean-up).
//! See also `crate::metrics::metrics_handler` for the Prometheus `/metrics`
//! endpoint.

use axum::{extract::State, http::StatusCode, routing::get, Json, Router};

use crate::state::AppState;

/// All health-check routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/health/live", get(health_live))
        .route("/health/ready", get(health_ready))
}

/// Probe each backing dependency (PG / Redis / NATS) with a short timeout.
/// Each result is `"ok"` / `"fail"` / `"timeout"`. Shared by `/health` and
/// `/health/ready` so the two never drift.
async fn probe_deps(s: &AppState) -> (&'static str, &'static str, &'static str, &'static str) {
    use std::time::Duration;
    let pg_ok = tokio::time::timeout(Duration::from_secs(2), async {
        sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(s.participants.pool())
            .await
    })
    .await;
    let pg = match pg_ok {
        Ok(Ok(_)) => "ok",
        Ok(Err(_)) => "fail",
        Err(_) => "timeout",
    };

    let redis_ok =
        tokio::time::timeout(Duration::from_secs(2), async { s.presence.ping().await }).await;
    let redis = match redis_ok {
        Ok(Ok(())) => "ok",
        Ok(Err(_)) => "fail",
        Err(_) => "timeout",
    };

    // NATS: bus reference is required at boot; if the connection has dropped,
    // downstream publish/subscribe will start logging warnings. Surface "ok"
    // here unless we can cheaply probe. We do a fire-and-forget publish on a
    // subject the IM_EVENTS stream covers — sub-millisecond when up, errors
    // out almost immediately when down.
    let nats_ok = tokio::time::timeout(Duration::from_secs(2), async {
        s.bus
            .publish("im.events.health", bytes::Bytes::from_static(b"ping"))
            .await
    })
    .await;
    let nats = match nats_ok {
        Ok(Ok(())) => "ok",
        Ok(Err(_)) => "fail",
        Err(_) => "timeout",
    };

    let blob = probe_blob(s).await;
    (pg, redis, nats, blob)
}

/// Commercial readiness is based on the durable local projection. A reachable
/// central service is intentionally not a probe dependency: accepted tenants
/// continue during a short billing/audit outage, while missing or expired
/// first projections remain fail-closed.
async fn probe_commercial(s: &AppState) -> &'static str {
    let Some(runtime) = &s.snaplink_commercial else {
        return "disabled";
    };
    match tokio::time::timeout(std::time::Duration::from_secs(2), runtime.ready()).await {
        Ok(Ok(true)) => "ok",
        Ok(Ok(false)) => "not_ready",
        Ok(Err(_)) => "fail",
        Err(_) => "timeout",
    }
}

/// Probe the default and every configured regional blob backend.
///
/// This must run even when the default backend is local: a deployment may pair
/// local default storage with remote residency buckets. A short timeout turns a
/// hung regional endpoint into `"timeout"` instead of stalling readiness.
async fn probe_blob(s: &AppState) -> &'static str {
    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        s.region_router.health_check(),
    )
    .await
    {
        Ok(Ok(())) => "ok",
        Ok(Err(_)) => "fail",
        Err(_) => "timeout",
    }
}

/// Legacy combined health endpoint (kept for backward-compat). Always 200; the
/// body's `status` is `"ok"` only when every dependency probes healthy.
async fn health(State(s): State<AppState>) -> Json<serde_json::Value> {
    let (pg, redis, nats, blob) = probe_deps(&s).await;
    let commercial = probe_commercial(&s).await;
    let commercial_ok = matches!(commercial, "ok" | "disabled");
    let overall = if pg == "ok" && redis == "ok" && nats == "ok" && blob == "ok" && commercial_ok {
        "ok"
    } else {
        "degraded"
    };

    Json(serde_json::json!({
        "status": overall,
        "deps": {
            "postgres": pg,
            "redis": redis,
            "nats": nats,
            "blob": blob,
            "snaplink_commercial": commercial,
        },
        // Surface the active blob backend (s3/local) so operators can confirm
        // storage is wired as intended.
        "blob_backend": s.blob_backend,
        "version": env!("CARGO_PKG_VERSION"),
        // Connection pool stats for capacity planning and bottleneck detection.
        "pool": {
            "pg": {
                "size": s.participants.pool().size(),
                "idle": s.participants.pool().num_idle(),
            },
        },
    }))
}

/// Liveness probe (k8s `livenessProbe`): the process is up and serving. Always
/// 200 — it must *not* depend on PG/Redis/NATS, or a transient backend blip
/// would get the pod killed and restarted (making the outage worse).
async fn health_live() -> impl axum::response::IntoResponse {
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
        })),
    )
}

/// Pure readiness decision, split out so the policy is unit-testable without a
/// live PG/Redis/NATS. Draining (graceful shutdown in progress) takes
/// precedence: the pod must leave the LB rotation immediately, regardless of
/// dependency health. Otherwise ready only when every dependency probed healthy.
fn readiness_decision(shutting_down: bool, deps_ok: bool) -> (StatusCode, &'static str) {
    if shutting_down {
        (StatusCode::SERVICE_UNAVAILABLE, "draining")
    } else if deps_ok {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not_ready")
    }
}

/// Readiness probe (k8s `readinessProbe`): 200 only when every dependency is
/// reachable, else 503 so the pod is pulled from the load-balancer rotation
/// until it recovers (without being restarted). During graceful shutdown it
/// returns 503 `"draining"` immediately so the LB stops routing new traffic
/// before the pod stops accepting.
async fn health_ready(State(s): State<AppState>) -> impl axum::response::IntoResponse {
    // Draining short-circuits the dependency probe — once shutdown has begun the
    // answer is 503 regardless, and skipping the probe avoids needless backend
    // calls during teardown.
    if s.shutting_down.load(std::sync::atomic::Ordering::Relaxed) {
        let (status, state) = readiness_decision(true, false);
        return (
            status,
            Json(serde_json::json!({
                "status": state,
                "version": env!("CARGO_PKG_VERSION"),
            })),
        );
    }
    let (pg, redis, nats, blob) = probe_deps(&s).await;
    let commercial = probe_commercial(&s).await;
    let deps_ok = pg == "ok"
        && redis == "ok"
        && nats == "ok"
        && blob == "ok"
        && matches!(commercial, "ok" | "disabled");
    let (status, state) = readiness_decision(false, deps_ok);
    (
        status,
        Json(serde_json::json!({
            "status": state,
            "deps": {
                "postgres": pg,
                "redis": redis,
                "nats": nats,
                "blob": blob,
                "snaplink_commercial": commercial,
            },
            "version": env!("CARGO_PKG_VERSION"),
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt as _; // `oneshot`

    /// The liveness handler takes no state, so it mounts on a state-free router
    /// and is fully testable offline (it must never touch PG/Redis/NATS).
    #[tokio::test]
    async fn health_live_returns_200_without_dependencies() {
        let app: Router = Router::new().route("/health/live", get(health_live));
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/health/live")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["status"], "ok");
    }

    #[test]
    fn readiness_decision_draining_is_503() {
        let (status, state) = readiness_decision(true, true);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(state, "draining");
    }

    #[test]
    fn readiness_decision_healthy_is_200() {
        let (status, state) = readiness_decision(false, true);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(state, "ready");
    }

    #[test]
    fn readiness_decision_degraded_is_503() {
        let (status, state) = readiness_decision(false, false);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(state, "not_ready");
    }
}
