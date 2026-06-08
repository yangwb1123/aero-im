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
//! NOTE (future seam): peak / average *concurrent* viewers are intentionally not
//! reported — there is no viewer-count sampling table to aggregate. See
//! [`aero_storage::StreamStatsRepo`] for the seam.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, Result as AeroResult};
use aero_storage::StreamStatsRepo;
use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};
use ulid::Ulid;

use crate::error::ApiResult;
use crate::state::AppState;

/// Mount the stream-analytics route. Folded into the main router by
/// [`crate::routes::build`]; kept separate so the analytics surface lives next to
/// its own storage repo, additively over the live/stream path.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/streams/:id/analytics", get(stream_analytics))
}

fn parse_stream_id(s: &str) -> AeroResult<Ulid> {
    Ulid::from_str(s).map_err(|e| AeroError::Invalid(format!("stream id: {e}")))
}

/// `GET /api/streams/:id/analytics` — owner-only creator dashboard for one stream.
///
/// Resolves the stream (`404` if unknown), asserts the caller owns it (`403`
/// otherwise), then returns the [`StreamAnalytics`](aero_storage::StreamAnalytics)
/// aggregate as JSON.
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
        return Err(AeroError::Forbidden("only the stream owner may view its analytics".into()).into());
    }

    let stats = StreamStatsRepo::new(s.pg.clone())
        .analytics(stream_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(stats).map_err(AeroError::from)?))
}
