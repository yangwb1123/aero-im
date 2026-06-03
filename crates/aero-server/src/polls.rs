//! Polls HTTP surface: create / vote / tally / close.
//!
//! Thin handlers — room access is enforced via
//! [`ImService::assert_room_access`](aero_im_core::ImService::assert_room_access),
//! and every storage invariant (single- vs multi-choice replace, closed-poll
//! rejection, creator-only close) lives in
//! [`PollRepo`](aero_storage::PollRepo). Mounted via [`routes`] and `.merge`d into
//! the main router, mirroring [`crate::workspaces`].
//!
//! The repo is constructed inline from the shared pool
//! (`s.participants.pool().clone()`) so no new `AppState` field is needed; live
//! tally refreshes are broadcast as [`RoomEvent::Poll`](aero_common::RoomEvent)
//! through [`ImService::broadcast_room_event`](aero_im_core::ImService::broadcast_room_event).

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, PollId, PollOp, RoomEvent, RoomId};
use aero_storage::poll::{option_count_valid, VoteError, MAX_OPTIONS, MIN_OPTIONS};
use aero_storage::PollRepo;
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All poll routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/rooms/:id/polls", post(create_poll))
        .route("/api/polls/:id", get(get_poll))
        .route("/api/polls/:id/vote", post(vote_poll))
        .route("/api/polls/:id/close", post(close_poll))
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

fn parse_poll(s: &str) -> Result<PollId, AeroError> {
    PollId::from_str(s).map_err(|e| AeroError::Invalid(format!("poll id: {e}")))
}

/// Construct a `PollRepo` over the shared pool (no dedicated `AppState` field).
fn poll_repo(s: &AppState) -> PollRepo {
    PollRepo::new(s.participants.pool().clone())
}

/// Map a storage [`VoteError`] to a clean client-facing API error.
fn map_vote_error(e: VoteError) -> AeroError {
    match e {
        VoteError::Closed => AeroError::Conflict("poll is closed".into()),
        VoteError::OutOfRange => AeroError::Invalid("option index out of range".into()),
        VoteError::NotFound => AeroError::NotFound("poll".into()),
        VoteError::Db(db) => AeroError::from(db),
    }
}

// --------------------------------------------------------------- Create

#[derive(Deserialize)]
struct CreatePollReq {
    question: String,
    options: Vec<String>,
    #[serde(default)]
    multi: bool,
}

/// `POST /api/rooms/:id/polls` — create a poll in a room (member-only). Validates
/// the question is non-empty and the option count is `2..=10`, then broadcasts
/// `Poll(Created)` and returns the created poll.
async fn create_poll(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<CreatePollReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;

    let question = req.question.trim();
    if question.is_empty() {
        return Err(AeroError::Invalid("question is empty".into()).into());
    }
    // Trim option labels and drop blanks before counting, so " " never pads a
    // poll to the minimum.
    let options: Vec<String> = req
        .options
        .iter()
        .map(|o| o.trim().to_owned())
        .filter(|o| !o.is_empty())
        .collect();
    if !option_count_valid(options.len()) {
        return Err(AeroError::Invalid(format!(
            "a poll needs between {MIN_OPTIONS} and {MAX_OPTIONS} non-empty options"
        ))
        .into());
    }

    let repo = poll_repo(&s);
    let id = repo
        .create(room, auth.participant_id, question, &options, req.multi)
        .await?;
    s.im
        .broadcast_room_event(room, RoomEvent::Poll { room_id: room, poll_id: id, op: PollOp::Created })
        .await;

    let poll = repo
        .get(id)
        .await?
        .ok_or_else(|| AeroError::Internal(anyhow::anyhow!("poll vanished after create")))?;
    Ok(Json(serde_json::to_value(poll).map_err(AeroError::from)?))
}

// ------------------------------------------------------------------ Get

/// `GET /api/polls/:id` — the poll, its live tally, and whether the caller has
/// voted. Access is gated on the poll's room.
async fn get_poll(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(poll_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let poll_id = parse_poll(&poll_str)?;
    let repo = poll_repo(&s);
    let poll = repo
        .get(poll_id)
        .await?
        .ok_or_else(|| AeroError::NotFound(format!("poll {poll_id}")))?;
    s.im.assert_room_access(auth.participant_id, poll.room_id).await?;

    let counts = repo.tally(poll_id).await?;
    let total: u32 = counts.iter().copied().fold(0u32, u32::saturating_add);
    let voted = repo.has_voted(poll_id, auth.participant_id).await?;
    Ok(Json(serde_json::json!({
        "poll": poll,
        "counts": counts,
        "total": total,
        "voted": voted,
    })))
}

// ----------------------------------------------------------------- Vote

#[derive(Deserialize)]
struct VoteReq {
    /// Single-choice ballot: the chosen option index.
    #[serde(default)]
    option_idx: Option<usize>,
    /// Multi-choice ballot: the chosen option indices.
    #[serde(default)]
    option_idxs: Option<Vec<usize>>,
}

/// `POST /api/polls/:id/vote` — cast a vote. Body is `{option_idx}` for a
/// single-choice poll or `{option_idxs}` for a multi-choice one. Resolves the
/// poll's room for the access check, applies the votes, then broadcasts
/// `Poll(Voted)`.
async fn vote_poll(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(poll_str): Path<String>,
    Json(req): Json<VoteReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let poll_id = parse_poll(&poll_str)?;
    let repo = poll_repo(&s);
    // Resolve the room for the access guard (and to learn `multi`).
    let poll = repo
        .get(poll_id)
        .await?
        .ok_or_else(|| AeroError::NotFound(format!("poll {poll_id}")))?;
    s.im.assert_room_access(auth.participant_id, poll.room_id).await?;

    // Collect the chosen indices from whichever field the client supplied. For a
    // single-choice poll, `option_idxs` with more than one entry is ambiguous.
    let idxs: Vec<usize> = match (poll.multi, req.option_idx, req.option_idxs) {
        (true, _, Some(list)) if !list.is_empty() => list,
        // A lone `option_idx` (no list) is one vote, single- or multi-choice.
        (_, Some(i), None) => vec![i],
        (false, None, Some(list)) if list.len() == 1 => list,
        _ => {
            return Err(AeroError::Invalid(
                "provide `option_idx` for a single-choice poll or non-empty `option_idxs` for a \
                 multi-choice poll"
                    .into(),
            )
            .into())
        }
    };

    for idx in idxs {
        repo.vote(poll_id, auth.participant_id, idx, poll.multi)
            .await
            .map_err(map_vote_error)?;
    }
    s.im
        .broadcast_room_event(
            poll.room_id,
            RoomEvent::Poll { room_id: poll.room_id, poll_id, op: PollOp::Voted },
        )
        .await;

    let counts = repo.tally(poll_id).await?;
    let total: u32 = counts.iter().copied().fold(0u32, u32::saturating_add);
    Ok(Json(serde_json::json!({
        "poll_id": poll_id,
        "counts": counts,
        "total": total,
        "voted": true,
    })))
}

// ---------------------------------------------------------------- Close

/// `POST /api/polls/:id/close` — close a poll (creator only). The creator check
/// is enforced atomically in the repo's `UPDATE ... WHERE created_by = ...`; a
/// non-creator (or an already-closed poll) yields `403`. On a real close,
/// broadcasts `Poll(Closed)`.
async fn close_poll(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(poll_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let poll_id = parse_poll(&poll_str)?;
    let repo = poll_repo(&s);
    let poll = repo
        .get(poll_id)
        .await?
        .ok_or_else(|| AeroError::NotFound(format!("poll {poll_id}")))?;
    // Room access first (a non-member can't even see the poll exists).
    s.im.assert_room_access(auth.participant_id, poll.room_id).await?;

    let closed = repo.close(poll_id, auth.participant_id).await?;
    if !closed {
        // Either not the creator, or already closed. Distinguish: an
        // already-closed poll is idempotent-OK; a non-creator is forbidden.
        if poll.is_closed() {
            return Ok(Json(serde_json::json!({ "poll_id": poll_id, "closed": true })));
        }
        return Err(AeroError::Forbidden("only the poll creator may close it".into()).into());
    }
    s.im
        .broadcast_room_event(
            poll.room_id,
            RoomEvent::Poll { room_id: poll.room_id, poll_id, op: PollOp::Closed },
        )
        .await;
    Ok(Json(serde_json::json!({ "poll_id": poll_id, "closed": true })))
}
