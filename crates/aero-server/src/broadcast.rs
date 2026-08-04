//! Multi-channel broadcast / "post to many" HTTP surface.
//!
//! [`crate::forward`] copies a message's content into ONE destination room. This
//! extends that to a fan-out: post a copy of a source message into SEVERAL rooms
//! at once (Slack "post to multiple channels"). It reuses forward's exact
//! machinery — [`build_forward_blocks`](crate::forward::build_forward_blocks)
//! builds the provenance card + mention-stripped copy, and
//! [`ImService::send_message`](aero_im_core::ImService::send_message) persists +
//! broadcasts + notifies for each target — so every per-room invariant holds.
//!
//! Access invariants: the caller must be able to SEE the source message's room
//! AND be able to POST in EACH target room. The source gate fails the whole
//! request (you may not broadcast a message you cannot see); a per-target gate
//! failure is isolated — that room lands in `failed` while the others still
//! receive the copy. The response is `{ sent: [RoomId], failed: [{room_id,
//! error}] }`. Mounted via [`routes`] and `.merge`d into the main router.

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, MessageId, RoomId};
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::forward::build_forward_blocks;
use crate::state::AppState;

/// All multi-channel broadcast routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/messages/:id/broadcast", post(broadcast_message))
}

fn parse_message(s: &str) -> Result<MessageId, AeroError> {
    MessageId::from_str(s).map_err(|e| AeroError::Invalid(format!("message id: {e}")))
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

/// Hard cap on broadcast fan-out per request. Without it `room_ids` is bounded
/// only by the body-size limit (~1M ULIDs fit in 33 MiB), and each target costs
/// an `assert_room_access` round-trip — so one request could force ~1M DB
/// lookups. A real user broadcasts to a handful of rooms; beyond this, split the
/// call. Returns 400 when exceeded.
const MAX_BROADCAST_TARGETS: usize = 100;

#[derive(Deserialize)]
struct BroadcastReq {
    /// Destination rooms to post a copy of the message into.
    room_ids: Vec<String>,
    /// Optional note rendered as a leading text block above the forwarded card
    /// (same placement as a single forward's comment).
    #[serde(default)]
    comment: Option<String>,
}

/// `POST /api/messages/:id/broadcast` — post a copy of a message into several
/// rooms at once. The caller must be able to see the source room; each target is
/// gated independently. Returns the list of rooms successfully posted to and the
/// list that failed (with a per-room error). 404 if the source message is
/// missing or soft-deleted; 400 on an empty/invalid `room_ids`.
async fn broadcast_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<BroadcastReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let src = parse_message(&id_str)?;

    // Resolve + 404 the source (missing or soft-deleted) BEFORE touching targets.
    let source = s
        .messages
        .get(src)
        .await?
        .filter(|m| m.deleted_at.is_none())
        .ok_or_else(|| AeroError::NotFound(format!("message {src}")))?;

    // The caller must be able to SEE the source room. A failure here aborts the
    // whole broadcast (you can't fan out a message you can't see).
    s.im.assert_room_access(auth.participant_id, source.room_id)
        .await?;

    if req.room_ids.is_empty() {
        return Err(AeroError::Invalid("room_ids must not be empty".into()).into());
    }
    if req.room_ids.len() > MAX_BROADCAST_TARGETS {
        return Err(AeroError::Invalid(format!(
            "too many targets: {} (max {MAX_BROADCAST_TARGETS} per broadcast)",
            req.room_ids.len()
        ))
        .into());
    }

    // De-duplicate while preserving order: a repeated target shouldn't post twice.
    let mut seen = std::collections::BTreeSet::new();
    let mut sent: Vec<RoomId> = Vec::new();
    let mut failed: Vec<serde_json::Value> = Vec::new();

    for raw in &req.room_ids {
        // A malformed id is a per-target failure, not a 400 for the whole batch.
        let target = match parse_room(raw) {
            Ok(r) => r,
            Err(e) => {
                failed.push(serde_json::json!({ "room_id": raw, "error": e.to_string() }));
                continue;
            }
        };
        if !seen.insert(target) {
            continue; // duplicate target — already handled.
        }

        // Per-target POST gate. A failure isolates to this room.
        if let Err(e) = s.im.assert_room_access(auth.participant_id, target).await {
            failed.push(serde_json::json!({ "room_id": target, "error": e.to_string() }));
            continue;
        }

        // Same provenance + mention-stripping as a single forward, per target.
        let blocks = build_forward_blocks(&source, req.comment.as_deref(), auth.participant_id);
        match s
            .im
            .send_message(auth.participant_id, target, blocks, None, None)
            .await
        {
            Ok(_) => sent.push(target),
            Err(e) => {
                failed.push(serde_json::json!({ "room_id": target, "error": e.to_string() }));
            }
        }
    }

    Ok(Json(serde_json::json!({ "sent": sent, "failed": failed })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::{Block, Message, ParticipantId};

    fn sample_source(blocks: Vec<Block>) -> Message {
        Message {
            id: MessageId::new(),
            room_id: RoomId::new(),
            sender_id: ParticipantId::new(),
            blocks,
            reply_to: None,
            metadata: serde_json::Value::Null,
            created_at: time::OffsetDateTime::now_utc(),
            edited_at: None,
            deleted_at: None,
            expires_at: None,
            version: 1,
        }
    }

    /// The broadcast copy built for each target is identical to a single forward:
    /// a provenance card + the mention-stripped original content. Asserting this
    /// over multiple targets covers the per-target build (the per-room send is
    /// already tested in `crate::forward`).
    #[test]
    fn build_over_multiple_targets_strips_mentions_and_keeps_provenance() {
        let mentioned = ParticipantId::new();
        let source = sample_source(vec![
            Block::text("hello"),
            Block::Mention {
                participant: mentioned,
            },
            Block::text("team"),
        ]);
        let actor = ParticipantId::new();

        // Building the copy is per-target but identical; verify the shape holds
        // for each of several targets.
        for _ in 0..3 {
            let blocks = build_forward_blocks(&source, Some("fyi"), actor);
            // [comment, card, "hello", "team"] — Mention stripped.
            assert!(matches!(&blocks[0], Block::Text { content, .. } if content == "fyi"));
            assert!(
                matches!(blocks[1], Block::Card { .. }),
                "provenance card present"
            );
            assert!(
                !blocks.iter().any(|b| matches!(b, Block::Mention { .. })),
                "no Mention survives a broadcast copy"
            );
            assert!(blocks
                .iter()
                .any(|b| matches!(b, Block::Text { content, .. } if content == "hello")));
            assert!(blocks
                .iter()
                .any(|b| matches!(b, Block::Text { content, .. } if content == "team")));
        }
    }
}
