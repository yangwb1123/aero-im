//! Scheduled / recurring AI digests.
//!
//! A participant subscribes to a recurring AI digest of a ROOM or a WORKSPACE on a
//! `daily` / `weekly` cadence. Thin handlers over
//! [`DigestSubscriptionRepo`](aero_storage::DigestSubscriptionRepo); the actual
//! summarization + delivery happens in the background [`run_digest_dispatcher`],
//! which mirrors [`crate::recurring::run_recurring_dispatcher`]: poll due rows,
//! summarize the target via the AI backend, deliver the summary, then advance
//! `next_run_at` by the frequency.
//!
//! Membership-gated at create time exactly like the rest of the app: a room digest
//! requires room access ([`assert_room_access`](aero_im_core::ImService::assert_room_access));
//! a workspace digest requires workspace membership (via the shared
//! [`WorkspaceRepo`](aero_storage::WorkspaceRepo), mirroring [`crate::workspace_ask`]).
//! `GET` / `DELETE` are owner-scoped (the caller can only see / remove their own).
//! Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;
use std::sync::Arc;

use aero_auth::AuthUser;
use aero_common::{
    Block, DigestSubscriptionId, Error as AeroError, ParticipantId, RoomId, WorkspaceId,
};
use aero_im_core::ImService;
use aero_storage::{
    digest_next_run_at, validate_digest_frequency, DigestSubscriptionRepo, DigestTarget,
};
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::{AiBackend, AppState};

/// All digest-subscription routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/digests", post(create_digest).get(list_digests))
        .route("/api/digests/:id", axum::routing::delete(delete_digest))
}

/// Upper bound on how many messages a single digest summarizes (mirrors the
/// summarize/catchup window caps).
const DIGEST_WINDOW: usize = 200;

/// Build a [`DigestSubscriptionRepo`] from shared state, over the shared pool.
fn repo(s: &AppState) -> DigestSubscriptionRepo {
    DigestSubscriptionRepo::new(s.pg.clone())
}

fn parse_digest(s: &str) -> Result<DigestSubscriptionId, AeroError> {
    DigestSubscriptionId::from_str(s.trim())
        .map_err(|e| AeroError::Invalid(format!("digest id: {e}")))
}

#[derive(Deserialize)]
struct CreateDigestReq {
    /// The room to digest. Exactly one of `room_id` / `workspace_id` must be set.
    #[serde(default)]
    room_id: Option<String>,
    /// The workspace to digest. Exactly one of `room_id` / `workspace_id` must be set.
    #[serde(default)]
    workspace_id: Option<String>,
    /// The cadence (`daily` / `weekly`).
    frequency: String,
}

/// `POST /api/digests` — subscribe to a recurring AI digest of a room or a
/// workspace. Requires room access (room digest) or workspace membership
/// (workspace digest). Rejects an unknown frequency, or a body that does not set
/// exactly one target (`400`). The first run is one cadence step from now. Returns
/// the created subscription.
async fn create_digest(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateDigestReq>,
) -> ApiResult<Json<serde_json::Value>> {
    // Exactly one target — mirror the table's room-XOR-workspace CHECK at the edge.
    let target = match (req.room_id.as_deref(), req.workspace_id.as_deref()) {
        (Some(r), None) => {
            let room = RoomId::from_str(r.trim())
                .map_err(|e| AeroError::Invalid(format!("room id: {e}")))?;
            // Tenant + room-membership guard before staging a room digest.
            s.im.assert_room_access(auth.participant_id, room).await?;
            DigestTarget::Room(room)
        }
        (None, Some(w)) => {
            let ws = WorkspaceId::from_str(w.trim())
                .map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))?;
            assert_workspace_member(&s, ws, auth.participant_id).await?;
            DigestTarget::Workspace(ws)
        }
        _ => {
            return Err(AeroError::Invalid(
                "exactly one of room_id / workspace_id must be set".into(),
            )
            .into())
        }
    };

    let frequency = req.frequency.trim();
    if !validate_digest_frequency(frequency) {
        return Err(AeroError::Invalid("frequency must be daily or weekly".into()).into());
    }
    let now = time::OffsetDateTime::now_utc();
    let first_run = digest_next_run_at(frequency, now)
        .ok_or_else(|| AeroError::Invalid("frequency must be daily or weekly".into()))?;

    let id = repo(&s)
        .create(auth.participant_id, target, frequency, first_run)
        .await
        .map_err(AeroError::from)?;
    // Re-read so the response carries the full, canonical row.
    let row = repo(&s)
        .list_for(auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .into_iter()
        .find(|d| d.id == id)
        .ok_or_else(|| AeroError::NotFound("digest subscription".into()))?;
    Ok(Json(serde_json::to_value(row).map_err(AeroError::from)?))
}

/// `GET /api/digests` — the caller's own digest subscriptions, newest first.
/// Owner-scoped (no per-target access check needed — only the caller's rows are
/// returned).
async fn list_digests(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let listed = repo(&s).list_for(auth.participant_id).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(listed).map_err(AeroError::from)?))
}

/// `DELETE /api/digests/:id` — cancel one of the caller's own digest
/// subscriptions. Owner-scoped: a `404` if it isn't the caller's (someone else's
/// or unknown).
async fn delete_digest(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = parse_digest(&id_str)?;
    let deleted = repo(&s)
        .delete(id, auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    if !deleted {
        return Err(AeroError::NotFound(format!("digest subscription {id}")).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// Assert the caller is a member of the workspace, rejecting non-members with a
/// `403`. Mirrors [`crate::workspace_ask`]'s membership gate.
async fn assert_workspace_member(
    s: &AppState,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    s.workspaces
        .member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))?;
    Ok(())
}

/// Background digest worker: every `interval_secs` it lists the subscriptions now
/// due ([`DigestSubscriptionRepo::due`]) and, for each, summarizes its target via
/// the AI backend, delivers the summary, then advances `next_run_at` to the next
/// occurrence of its frequency ([`DigestSubscriptionRepo::reschedule`]).
///
/// Delivery:
/// - A ROOM digest is posted into the room as a message FROM the subscriber via
///   [`ImService::send_message`] (the subscriber was a member at create time; if
///   they have since left, the send fails and is logged + skipped — the
///   subscription is still rescheduled).
/// - A WORKSPACE digest is delivered into the subscriber's activity feed (there is
///   no single room to post into), via the same store the rest of the app uses.
///
/// Best-effort per subscription — a single summarize/deliver failure or an empty
/// summary is logged and skipped, never aborting the batch. The subscription is
/// ALWAYS rescheduled after an attempt, so a transient failure advances
/// `next_run_at` rather than wedging the worker in a tight retry loop. Runs until
/// the cancellation token fires.
///
/// Not spawned here — the orchestrator `tokio::spawn`s it in the binary alongside
/// `run_recurring_dispatcher`.
pub async fn run_digest_dispatcher(state: AppState, cancel: tokio_util::sync::CancellationToken) {
    let repo = DigestSubscriptionRepo::new(state.pg.clone());
    let activity = aero_storage::ActivityFeedRepo::new(state.pg.clone());
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tracing::info!("digest dispatcher started");

    // No AI backend → nothing to summarize; the dispatcher idles (still honoring
    // shutdown) rather than busy-looping over rows it can't process.
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                tracing::info!("digest dispatcher shutting down");
                return;
            }
            _ = tick.tick() => {}
        }

        let Some(ai) = state.ai.as_ref() else { continue };

        let now = time::OffsetDateTime::now_utc();
        let due = match repo.due(now).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = ?e, "digest dispatcher: due() failed");
                continue;
            }
        };
        if due.is_empty() {
            continue;
        }
        tracing::debug!(count = due.len(), "digest dispatcher: firing due subscriptions");

        for sub in due {
            deliver_one(&state.im, ai.as_ref(), &activity, &sub).await;
            // Always advance so a failed firing does not wedge the worker on the
            // same overdue row.
            if let Some(next) = digest_next_run_at(&sub.frequency, now) {
                if let Err(e) = repo.reschedule(sub.id, next).await {
                    tracing::warn!(error = ?e, digest_id = %sub.id, "digest dispatcher: reschedule failed");
                }
            } else {
                tracing::warn!(
                    digest_id = %sub.id,
                    frequency = %sub.frequency,
                    "digest dispatcher: unknown frequency, cannot reschedule"
                );
            }
        }
    }
}

/// Summarize + deliver one due subscription. Best-effort: any failure (summarize
/// error, empty summary, send/insert error) is logged and swallowed so the batch
/// continues and the caller can still reschedule.
async fn deliver_one(
    im: &Arc<ImService>,
    ai: &dyn AiBackend,
    activity: &aero_storage::ActivityFeedRepo,
    sub: &aero_storage::DigestSubscription,
) {
    let summary = match sub.target {
        DigestTarget::Room(room) => ai.summarize_room(room, DIGEST_WINDOW).await,
        DigestTarget::Workspace(ws) => {
            ai.summarize_workspace(sub.participant_id, ws, DIGEST_WINDOW).await
        }
    };
    let summary = match summary {
        Ok(s) if !s.trim().is_empty() => s,
        Ok(_) => {
            tracing::debug!(digest_id = %sub.id, "digest dispatcher: empty summary, nothing to deliver");
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, digest_id = %sub.id, "digest dispatcher: summarize failed");
            return;
        }
    };

    match sub.target {
        DigestTarget::Room(room) => {
            let blocks = vec![Block::text(format!("📰 定时摘要 ({}):\n{summary}", sub.frequency))];
            if let Err(e) = im.send_message(sub.participant_id, room, blocks, None, None).await {
                tracing::warn!(
                    error = ?e,
                    digest_id = %sub.id,
                    room = %room,
                    "digest dispatcher: room delivery failed (subscription still rescheduled)"
                );
            }
        }
        DigestTarget::Workspace(_) => {
            // No single room to post into — deliver as a personal activity-feed
            // notice (the "user's note" delivery seam).
            let kind = "digest";
            if let Err(e) = activity
                .insert(sub.participant_id, kind, None, None, &summary)
                .await
            {
                tracing::warn!(
                    error = ?e,
                    digest_id = %sub.id,
                    "digest dispatcher: workspace digest activity-feed delivery failed"
                );
            }
        }
    }
}
