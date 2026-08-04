//! Stream / creator analytics HTTP API — owner-only per-stream dashboard.
//!
//! Additive layer over the new [`aero_storage::StreamStatsRepo`]. The stream
//! *owner* (`stream.owner_id == auth.participant_id`) reads a single aggregate
//! snapshot of their stream: gift count / units / coin revenue, chat line count,
//! unique chatters, and stream duration — all rolled up from already-persisted
//! data (`stream_gifts`, `stream_chat`, `streams.started_at/ended_at`). Read-only;
//! nothing here touches existing modules' code, and there is no new table.
//!
//! Authorization mirrors [`crate::stream_mod`]: the stream is resolved via the
//! shared [`StreamRepo`](aero_storage::StreamRepo) (`404` if unknown) and the
//! caller must own it (`403` otherwise) before any analytics are returned.
//!
//! Concurrent-viewer history: the response now also carries `peak_viewers` /
//! `avg_viewers` / `viewer_samples`, aggregated from the
//! `stream_viewer_samples` table (migration 0072) via
//! [`StreamViewerSampleRepo`](aero_storage::StreamViewerSampleRepo). Those rows
//! are produced by the background [`run_viewer_sampler`], which snapshots the
//! live Redis viewer count for each currently-live stream on an interval.

use std::str::FromStr;
use std::time::Duration;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, Result as AeroResult};
use aero_storage::{StreamRepo, StreamStatsRepo, StreamViewerSampleRepo, StreamViewerStore};
use axum::{
    extract::{Path, Query, State},
    routing::get,
    Json, Router,
};
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use crate::error::ApiResult;
use crate::state::AppState;
use crate::task_shutdown;

/// Default sampling cadence for [`run_viewer_sampler`]: snapshot every live
/// stream's concurrent-viewer count once per this interval.
pub const VIEWER_SAMPLE_INTERVAL: Duration = Duration::from_secs(30);

/// Mount the stream-analytics route. Folded into the main router by
/// [`crate::routes::build`]; kept separate so the analytics surface lives next to
/// its own storage repo, additively over the live/stream path.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/streams/:id/analytics", get(stream_analytics))
        .route("/api/streams/:id/retention-curve", get(retention_curve))
}

fn parse_stream_id(s: &str) -> AeroResult<Ulid> {
    Ulid::from_str(s).map_err(|e| AeroError::Invalid(format!("stream id: {e}")))
}

/// `GET /api/streams/:id/analytics` — owner-only creator dashboard for one stream.
///
/// Resolves the stream (`404` if unknown), asserts the caller owns it (`403`
/// otherwise), then returns the [`StreamAnalytics`](aero_storage::StreamAnalytics)
/// aggregate as JSON, extended with the concurrent-viewer history
/// (`peak_viewers` / `avg_viewers` / `viewer_samples`) aggregated from the
/// `stream_viewer_samples` table.
async fn stream_analytics(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream_id = parse_stream_id(&id_str)?;
    let stream = s
        .streams
        .get(stream_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("stream {stream_id}")))?;
    if stream.owner_id != auth.participant_id {
        return Err(
            AeroError::Forbidden("only the stream owner may view its analytics".into()).into(),
        );
    }

    let stats = StreamStatsRepo::new(s.pg.clone())
        .analytics(stream_id)
        .await
        .map_err(AeroError::from)?;
    // Concurrent-viewer history (peak / avg) from the sampling table — closes the
    // "not yet sampled" seam noted in aero_storage::stream_stats.
    let viewers = StreamViewerSampleRepo::new(s.pg.clone())
        .stats(stream_id)
        .await
        .map_err(AeroError::from)?;
    let mut body = serde_json::to_value(stats).map_err(AeroError::from)?;
    if let Some(obj) = body.as_object_mut() {
        obj.insert("peak_viewers".into(), serde_json::json!(viewers.peak));
        obj.insert("avg_viewers".into(), serde_json::json!(viewers.avg));
        obj.insert("viewer_samples".into(), serde_json::json!(viewers.samples));
    }
    Ok(Json(body))
}

#[derive(serde::Deserialize)]
struct RetentionQuery {
    /// Width of each retention bucket in seconds (default: 60).
    #[serde(default = "default_bucket_secs")]
    bucket_secs: i64,
}

fn default_bucket_secs() -> i64 {
    60
}

/// `GET /api/streams/:id/retention-curve?bucket_secs=60` — viewer retention curve
/// for the stream: peak-relative retention percentage bucketed by time offset from
/// the stream start. Owner-only.
async fn retention_curve(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<RetentionQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream_id = parse_stream_id(&id_str)?;
    let stream = s
        .streams
        .get(stream_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("stream {stream_id}")))?;
    if stream.owner_id != auth.participant_id {
        return Err(AeroError::Forbidden(
            "only the stream owner may view its retention curve".into(),
        )
        .into());
    }
    let curve = StreamViewerSampleRepo::new(s.pg.clone())
        .retention_curve(stream_id, q.bucket_secs)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "curve": curve })))
}

/// Background sampler: every [`VIEWER_SAMPLE_INTERVAL`], snapshot the live
/// concurrent-viewer count for each currently-live stream into
/// `stream_viewer_samples`, so peak / average concurrent viewers can be
/// aggregated after the fact (the live count itself is ephemeral Redis state).
///
/// Spawned by the binary alongside the other dispatchers; it loops until the
/// future is dropped. A per-tick failure (PG/Redis blip) is logged and the loop
/// continues — a missed sample is harmless, the aggregate is over whatever rows
/// landed. The Redis viewer count is read via [`StreamViewerStore::count`], the
/// same source the live viewer-count broadcast uses, so a sample reflects the
/// true cluster-wide audience.
pub async fn run_viewer_sampler(
    pg: aero_storage::PgPool,
    viewers: StreamViewerStore,
    stream_repo: StreamRepo,
) {
    run_viewer_sampler_until_cancelled(pg, viewers, stream_repo, CancellationToken::new()).await;
}

/// Run the concurrent-viewer sampler until `cancel` is triggered.
pub async fn run_viewer_sampler_until_cancelled(
    pg: aero_storage::PgPool,
    viewers: StreamViewerStore,
    stream_repo: StreamRepo,
    cancel: CancellationToken,
) {
    let samples = StreamViewerSampleRepo::new(pg);
    let mut tick = tokio::time::interval(VIEWER_SAMPLE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tracing::info!(
        interval_secs = VIEWER_SAMPLE_INTERVAL.as_secs(),
        "concurrent-viewer sampler started"
    );
    loop {
        if task_shutdown::tick_or_cancelled(&mut tick, &cancel).await {
            return;
        }
        let live = match stream_repo.list_live().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = ?e, "viewer sampler: list_live failed");
                continue;
            }
        };
        for stream in live {
            match viewers.count(stream.id).await {
                Ok(count) => {
                    if let Err(e) = samples.record(stream.id, count).await {
                        tracing::warn!(error = ?e, stream = %stream.id, "viewer sampler: record failed");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = ?e, stream = %stream.id, "viewer sampler: count failed");
                }
            }
            if cancel.is_cancelled() {
                return;
            }
        }
    }
}
