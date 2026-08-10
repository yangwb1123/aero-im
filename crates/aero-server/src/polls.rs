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
use aero_common::{Error as AeroError, Poll, PollId, PollOp, RoomEvent, RoomId};
use aero_storage::poll::{
    option_count_valid, ClosePollError, CreatePollError, VoteError, MAX_OPTIONS, MIN_OPTIONS,
};
use aero_storage::PollRepo;
use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All poll routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/rooms/:id/polls", post(create_poll).get(list_polls))
        .route("/api/polls/:id", get(get_poll))
        .route("/api/polls/:id/vote", post(vote_poll))
        .route("/api/polls/:id/close", post(close_poll))
}

#[derive(Deserialize)]
struct ListPollsQuery {
    /// `?open=true` restricts to polls still accepting votes.
    #[serde(default)]
    open: bool,
}

/// `GET /api/rooms/:id/polls` — list the room's polls, newest first (`?open=true`
/// for only those still accepting votes). Room-access gated like `create_poll`.
/// Returns poll metadata only; clients fetch a live tally via `GET /api/polls/:id`.
async fn list_polls(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Query(q): Query<ListPollsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_room_access(auth.participant_id, room).await?;
    let polls = poll_repo(&s).list_for_room(room, q.open).await?;
    Ok(Json(serde_json::to_value(polls).map_err(AeroError::from)?))
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
        VoteError::DuplicateOption => AeroError::Invalid("duplicate option index".into()),
        VoteError::EmptyBallot => AeroError::Invalid("ballot is empty".into()),
        VoteError::TooManyOptions => AeroError::Invalid(format!(
            "ballot may contain at most {MAX_OPTIONS} options and no more than the poll defines"
        )),
        VoteError::SingleChoiceMultiple => {
            AeroError::Invalid("single-choice poll requires exactly one option".into())
        }
        VoteError::Forbidden => AeroError::Forbidden("cannot vote in this poll".into()),
        VoteError::NotFound => AeroError::NotFound("poll".into()),
        VoteError::Db(db) => AeroError::from(db),
    }
}

fn map_close_error(e: ClosePollError) -> AeroError {
    match e {
        ClosePollError::NotFound => AeroError::NotFound("poll".into()),
        ClosePollError::Forbidden => {
            AeroError::Forbidden("cannot close a poll after room access is revoked".into())
        }
        ClosePollError::NotCreator => {
            AeroError::Forbidden("only the poll creator may close it".into())
        }
        ClosePollError::Db(db) => AeroError::from(db),
    }
}

fn map_create_error(e: CreatePollError) -> AeroError {
    match e {
        CreatePollError::NotFound => AeroError::NotFound("room".into()),
        CreatePollError::Forbidden => {
            AeroError::Forbidden("cannot create a poll after room access is revoked".into())
        }
        CreatePollError::Db(db) => AeroError::from(db),
    }
}

fn poll_detail_response(poll: &Poll, counts: &[u32], voted: bool) -> serde_json::Value {
    let total = counts.iter().copied().fold(0u32, u32::saturating_add);
    let anonymous = poll.anonymous;
    serde_json::json!({
        "poll": poll,
        "counts": counts,
        "total": total,
        "voted": voted,
        "anonymous": anonymous,
    })
}

// --------------------------------------------------------------- Create

#[derive(Deserialize)]
struct CreatePollReq {
    question: String,
    options: Vec<String>,
    #[serde(default)]
    multi: bool,
    /// When `true` the voter identities are hidden; only totals are returned in
    /// the tally. Defaults to `false` (public voting). Cannot be changed after
    /// creation. Backed by `migrations/0096_polls_anonymous.sql`.
    #[serde(default)]
    anonymous: Option<bool>,
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

    let anonymous = req.anonymous.unwrap_or(false);
    let repo = poll_repo(&s);
    let id = repo
        .create_with_opts(
            room,
            auth.participant_id,
            question,
            &options,
            req.multi,
            anonymous,
        )
        .await
        .map_err(map_create_error)?;
    s.im.broadcast_room_event(
        room,
        RoomEvent::Poll {
            room_id: room,
            poll_id: id,
            op: PollOp::Created,
        },
    )
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
    s.im.assert_room_access(auth.participant_id, poll.room_id)
        .await?;

    let counts = repo.tally(poll_id).await?;
    let voted = repo.has_voted(poll_id, auth.participant_id).await?;
    // Anonymous polls suppress voter identity: `voted` is still returned so the
    // caller knows whether they participated, but `anonymous: true` signals that
    // no voter lists will ever be returned.
    Ok(Json(poll_detail_response(&poll, &counts, voted)))
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
    s.im.assert_room_access(auth.participant_id, poll.room_id)
        .await?;

    // Collect the chosen indices from whichever field the client supplied. For a
    // single-choice poll, `option_idxs` with more than one entry is ambiguous.
    let idxs: Vec<usize> =
        match (poll.multi, req.option_idx, req.option_idxs) {
            (true, _, Some(list)) if !list.is_empty() => list,
            // A lone `option_idx` (no list) is one vote, single- or multi-choice.
            (_, Some(i), None) => vec![i],
            (false, None, Some(list)) if list.len() == 1 => list,
            _ => return Err(AeroError::Invalid(
                "provide `option_idx` for a single-choice poll or non-empty `option_idxs` for a \
                 multi-choice poll"
                    .into(),
            )
            .into()),
        };

    if idxs.len() > MAX_OPTIONS {
        return Err(AeroError::Invalid(format!(
            "a ballot may select at most {MAX_OPTIONS} options"
        ))
        .into());
    }
    repo.vote_ballot(poll_id, auth.participant_id, poll.room_id, &idxs)
        .await
        .map_err(map_vote_error)?;
    s.im.broadcast_room_event(
        poll.room_id,
        RoomEvent::Poll {
            room_id: poll.room_id,
            poll_id,
            op: PollOp::Voted,
        },
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
    s.im.assert_room_access(auth.participant_id, poll.room_id)
        .await?;

    let closed = repo
        .close_authorized(poll_id, auth.participant_id, poll.room_id)
        .await
        .map_err(map_close_error)?;
    if !closed {
        // The transaction-owned creator check makes `false` unambiguously mean
        // this creator had already closed the poll.
        return Ok(Json(
            serde_json::json!({ "poll_id": poll_id, "closed": true }),
        ));
    }
    s.im.broadcast_room_event(
        poll.room_id,
        RoomEvent::Poll {
            room_id: poll.room_id,
            poll_id,
            op: PollOp::Closed,
        },
    )
    .await;
    // ROADMAP 集成三: poll-close → auto-create approval (best-effort, non-blocking).
    // When the poll belongs to a workspace, create an approval addressed to the poll
    // creator with the results summary, so the creator can decide on next steps.
    let ws = s.rooms.room_workspace(poll.room_id).await.ok().flatten();
    if let Some(workspace_id) = ws {
        let tally = poll_repo(&s).tally(poll_id).await.unwrap_or_default();
        let total_votes: u32 = tally.iter().sum();
        if total_votes > 0 {
            let detail_lines: Vec<String> = poll
                .options
                .iter()
                .enumerate()
                .map(|(i, opt)| {
                    // Ratio of two counts ×100 is bounded to 0..=100, so the
                    // f64→u32 narrowing cannot truncate or wrap meaningful data.
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let pct = if total_votes > 0 {
                        (f64::from(tally.get(i).copied().unwrap_or(0)) / f64::from(total_votes) * 100.0)
                            as u32
                    } else {
                        0
                    };
                    format!("  {pct}% — {opt}")
                })
                .collect();
            let details = format!(
                "Poll \"{}\" has closed with {total_votes} vote(s):\n{}",
                poll.question,
                detail_lines.join("\n"),
            );
            let approval_id = aero_storage::ApprovalRepo::new(s.pg.clone())
                .create(
                    workspace_id,
                    auth.participant_id, // requester = poll creator
                    auth.participant_id, // approver = also poll creator (self-approve)
                    &format!("Poll results: {}", poll.question),
                    Some(&details),
                )
                .await;
            match approval_id {
                Ok(_) => tracing::info!(%poll_id, "auto-created approval from poll close"),
                Err(e) => {
                    tracing::warn!(error = ?e, %poll_id, "auto-create approval from poll failed");
                }
            }
        }
    }
    Ok(Json(
        serde_json::json!({ "poll_id": poll_id, "closed": true }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::ParticipantId;

    #[test]
    fn anonymous_poll_detail_does_not_expose_voter_identity() {
        let voter = ParticipantId::new();
        let poll = Poll {
            id: PollId::new(),
            room_id: RoomId::new(),
            created_by: ParticipantId::new(),
            question: "anonymous".into(),
            options: vec!["A".into(), "B".into()],
            multi: false,
            anonymous: true,
            closed_at: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        };

        let detail = poll_detail_response(&poll, &[1, 0], true);
        assert_eq!(detail["anonymous"], true);
        assert_eq!(detail["voted"], true);
        assert!(detail.get("voters").is_none());
        assert!(!detail.to_string().contains(&voter.to_string()));
    }
}
