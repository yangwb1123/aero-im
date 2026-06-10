//! Hype-train HTTP API + gift-path hook (escalating combo-gift momentum).
//!
//! A "hype train" is an escalating momentum session on a live stream: rapid
//! successive gifts accumulate units within a sliding window and advance a
//! `level` (Twitch Hype Train). The escalation arithmetic is the pure state
//! machine in [`aero_storage::hype_train`]; this module exposes the read endpoint
//! and the [`on_gift`] hook the gift handlers call after a gift is recorded.
//!
//! [`on_gift`] feeds a gift's `qty` into the train, advancing the level, then
//! broadcasts a [`StreamEvent::HypeTrain`] on the stream's subject so watchers'
//! clients render the escalating meter. It is best-effort: a storage/broadcast
//! hiccup is logged and must never fail the underlying gift send.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, StreamEvent};
use aero_storage::HypeTrainRepo;
use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};
use time::OffsetDateTime;
use ulid::Ulid;

use crate::error::ApiResult;
use crate::state::AppState;

/// Mount the hype-train read route, folded into the main router by
/// [`crate::routes::build`].
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/streams/:id/hype-train", get(get_hype_train))
}

fn parse_stream(s: &str) -> Result<Ulid, AeroError> {
    Ulid::from_str(s.trim()).map_err(|e| AeroError::Invalid(format!("stream id: {e}")))
}

fn repo(s: &AppState) -> HypeTrainRepo {
    HypeTrainRepo::new(s.pg.clone())
}

/// `GET /api/streams/:id/hype-train` — the stream's current (active, un-lapsed)
/// hype-train session, or `{"hype_train": null}` when none is running. Any
/// authenticated viewer may read it.
async fn get_hype_train(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let stream = parse_stream(&id_str)?;
    let session = repo(&s)
        .current(stream, OffsetDateTime::now_utc())
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "hype_train": session })))
}

/// Hook the gift path: feed `qty` units into `stream`'s hype train (started fresh
/// if none is active), advancing the escalation state machine, then broadcast a
/// [`StreamEvent::HypeTrain`] so watchers see the level/contribution/expiry.
///
/// Best-effort and infallible to the caller: any storage error is logged and
/// swallowed so a hype-train hiccup never fails the gift send that triggered it.
pub async fn on_gift(state: &AppState, stream: Ulid, sender: ParticipantId, qty: u32) {
    let now = OffsetDateTime::now_utc();
    match HypeTrainRepo::new(state.pg.clone())
        .add_contribution(stream, sender, qty, now)
        .await
    {
        Ok(session) => {
            let event = StreamEvent::HypeTrain {
                stream_id: stream,
                level: u32::try_from(session.level.max(0)).unwrap_or(0),
                contribution: u32::try_from(session.contribution.max(0)).unwrap_or(0),
                expires_at: session.expires_at,
            };
            state.live.broadcast(&event).await;
        }
        Err(e) => tracing::warn!(error = ?e, %stream, "hype-train add_contribution failed"),
    }
}
