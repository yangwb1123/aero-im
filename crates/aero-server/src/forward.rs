//! Message forwarding / share HTTP surface.
//!
//! Forward an existing message's content into another room the caller belongs
//! to (Slack "Forward"). The original content blocks are copied verbatim and
//! prefixed with a `forwarded_message` provenance [`Block::Card`], so the
//! recipient room sees who/where the message came from. An optional `comment`
//! becomes a leading text block.
//!
//! Thin handler — the two access invariants ("caller can SEE the source" and
//! "caller can POST in the destination") are enforced via
//! [`ImService::assert_room_access`](aero_im_core::ImService::assert_room_access),
//! and the actual persistence + broadcast + notifications happen in
//! [`ImService::send_message`](aero_im_core::ImService::send_message). Mounted
//! via [`routes`] and `.merge`d into the main router, mirroring
//! [`crate::workspaces`]. No new storage repo, id, or migration is needed — the
//! forward provenance rides in the new message's blocks.
//!
//! Mentions in the source message are STRIPPED from the copy: a forward should
//! not spuriously notify people who were `@`-mentioned in the original room
//! (notifications are driven off `Block::Mention`, see
//! `ImService::dispatch_notifications`).

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Block, Error as AeroError, Message, MessageId, ParticipantId, RoomId};
use axum::{
    extract::{Path, State},
    routing::post,
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All forward/share routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/messages/:id/forward", post(forward_message))
}

fn parse_message(s: &str) -> Result<MessageId, AeroError> {
    MessageId::from_str(s).map_err(|e| AeroError::Invalid(format!("message id: {e}")))
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

#[derive(Deserialize)]
struct ForwardReq {
    /// Destination room the caller is forwarding the message into.
    to_room: String,
    /// Optional note rendered as a leading text block above the forwarded card.
    #[serde(default)]
    comment: Option<String>,
}

/// `POST /api/messages/:id/forward` — forward a message's content into another
/// room. The caller must be able to see the source room AND post in the
/// destination room. Returns the newly-created (forwarded) message.
async fn forward_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<ForwardReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let src = parse_message(&id_str)?;
    let to_room = parse_room(&req.to_room)?;

    // Fetch the source message; 404 if missing or soft-deleted.
    let source = s
        .messages
        .get(src)
        .await?
        .filter(|m| m.deleted_at.is_none())
        .ok_or_else(|| AeroError::NotFound(format!("message {src}")))?;

    // Two access gates: the caller must be able to SEE the source room, and be
    // able to POST in the destination room. Both go through the single tenant
    // guard (workspace + room membership). `send_message` re-checks destination
    // membership, but asserting here yields a clean 403/404 before we build.
    s.im.assert_room_access(auth.participant_id, source.room_id).await?;
    s.im.assert_room_access(auth.participant_id, to_room).await?;

    let blocks = build_forward_blocks(&source, req.comment.as_deref(), auth.participant_id);

    let message = s
        .im
        .send_message(auth.participant_id, to_room, blocks, None, None)
        .await?;
    Ok(Json(serde_json::to_value(message).map_err(AeroError::from)?))
}

/// Schema tag carried on the provenance [`Block::Card`] of a forwarded message.
const FORWARDED_SCHEMA: &str = "forwarded_message";

/// Build the block list for a forwarded message. PURE (no I/O), so the
/// provenance payload, comment placement, and mention-stripping are
/// exhaustively unit-tested without a database or bus.
///
/// Layout, in order:
/// 1. an optional leading [`Block::text`] when `comment` is a non-empty string,
/// 2. a `forwarded_message` [`Block::Card`] recording the source's id, room,
///    sender, and who forwarded it,
/// 3. the source's original content blocks, copied verbatim EXCEPT
///    [`Block::Mention`]s, which are stripped so the forward doesn't notify the
///    people mentioned in the original room.
#[must_use]
pub fn build_forward_blocks(
    source: &Message,
    comment: Option<&str>,
    forwarded_by: ParticipantId,
) -> Vec<Block> {
    let mut blocks = Vec::new();

    // Leading comment (only when present and not just whitespace).
    if let Some(text) = comment {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            blocks.push(Block::text(trimmed));
        }
    }

    // Provenance card — lets clients render a "Forwarded from …" affordance and
    // preserves the chain even after the source is edited/deleted.
    blocks.push(Block::Card {
        schema: FORWARDED_SCHEMA.to_owned(),
        payload: serde_json::json!({
            "source_message_id": source.id.to_string(),
            "source_room_id": source.room_id.to_string(),
            "source_sender_id": source.sender_id.to_string(),
            "forwarded_by": forwarded_by.to_string(),
        }),
    });

    // Original content, mentions stripped (forwards must not re-notify mentionees).
    blocks.extend(
        source
            .blocks
            .iter()
            .filter(|b| !matches!(b, Block::Mention { .. }))
            .cloned(),
    );

    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_common::{FileKind, MessageId, RoomId};

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

    /// The provenance card records the source id/room/sender and the forwarder.
    #[test]
    fn card_payload_carries_full_provenance() {
        let source = sample_source(vec![Block::text("hello world")]);
        let forwarder = ParticipantId::new();

        let blocks = build_forward_blocks(&source, None, forwarder);

        // No comment ⇒ card is first.
        let Block::Card { schema, payload } = &blocks[0] else {
            panic!("first block should be the provenance card, got {:?}", blocks[0]);
        };
        assert_eq!(schema, FORWARDED_SCHEMA);
        assert_eq!(payload["source_message_id"], source.id.to_string());
        assert_eq!(payload["source_room_id"], source.room_id.to_string());
        assert_eq!(payload["source_sender_id"], source.sender_id.to_string());
        assert_eq!(payload["forwarded_by"], forwarder.to_string());
    }

    /// Original content blocks are included verbatim after the card.
    #[test]
    fn original_content_is_included_after_card() {
        let source = sample_source(vec![
            Block::text("first"),
            Block::Code { lang: "rs".into(), content: "fn main() {}".into() },
            Block::File {
                blob_id: aero_common::BlobId::new(),
                kind: FileKind::Document,
                name: "report.pdf".into(),
                size: 10,
            },
        ]);

        let blocks = build_forward_blocks(&source, None, ParticipantId::new());

        // [card, text, code, file]
        assert_eq!(blocks.len(), 4);
        assert!(matches!(blocks[0], Block::Card { .. }));
        assert!(matches!(&blocks[1], Block::Text { content, .. } if content == "first"));
        assert!(matches!(&blocks[2], Block::Code { content, .. } if content == "fn main() {}"));
        assert!(matches!(&blocks[3], Block::File { name, .. } if name == "report.pdf"));
    }

    /// A non-empty comment is placed FIRST, before the provenance card.
    #[test]
    fn comment_is_placed_before_the_card() {
        let source = sample_source(vec![Block::text("body")]);

        let blocks = build_forward_blocks(&source, Some("  look at this  "), ParticipantId::new());

        // [comment, card, body] — comment trimmed.
        assert_eq!(blocks.len(), 3);
        assert!(
            matches!(&blocks[0], Block::Text { content, .. } if content == "look at this"),
            "comment should lead, trimmed: {:?}",
            blocks[0]
        );
        assert!(matches!(blocks[1], Block::Card { .. }));
        assert!(matches!(&blocks[2], Block::Text { content, .. } if content == "body"));
    }

    /// An absent / whitespace-only comment produces no leading text block.
    #[test]
    fn blank_comment_is_omitted() {
        let source = sample_source(vec![Block::text("body")]);

        for comment in [None, Some(""), Some("   ")] {
            let blocks = build_forward_blocks(&source, comment, ParticipantId::new());
            // [card, body] — no comment block.
            assert_eq!(blocks.len(), 2, "comment={comment:?}");
            assert!(matches!(blocks[0], Block::Card { .. }), "comment={comment:?}");
            assert!(matches!(&blocks[1], Block::Text { content, .. } if content == "body"));
        }
    }

    /// Mentions in the source are stripped from the copy so the forward doesn't
    /// spuriously notify the people mentioned in the original room.
    #[test]
    fn mentions_are_stripped_from_the_copy() {
        let mentioned = ParticipantId::new();
        let source = sample_source(vec![
            Block::text("hey"),
            Block::Mention { participant: mentioned },
            Block::text("see this"),
        ]);

        let blocks = build_forward_blocks(&source, None, ParticipantId::new());

        // [card, "hey", "see this"] — the Mention is gone.
        assert_eq!(blocks.len(), 3);
        assert!(matches!(blocks[0], Block::Card { .. }));
        assert!(
            !blocks.iter().any(|b| matches!(b, Block::Mention { .. })),
            "no Mention block should survive a forward"
        );
        assert!(matches!(&blocks[1], Block::Text { content, .. } if content == "hey"));
        assert!(matches!(&blocks[2], Block::Text { content, .. } if content == "see this"));
    }
}
