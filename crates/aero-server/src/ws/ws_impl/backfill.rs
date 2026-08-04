//! WebSocket reconnect backfill.
//!
//! Cursor-capable clients resume each effective room from its durable
//! participant/room cursor. A room without a cursor is not silently omitted:
//! it receives only the newest initial-history window, oldest-first. Older
//! history remains available through the ordinary REST `before` pagination, so
//! a first connection can never trigger an unbounded replay.

use super::{DeliveryRoomBarrier, ServerFrame};
use crate::state::AppState;
use aero_common::{MessageId, ParticipantId, RoomId};
use axum::extract::ws::Message;
use std::collections::HashMap;
use std::str::FromStr;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// Per-room cap for a forward reconnect replay. The client can continue via
/// REST `?since=` when a gap exceeds this window.
pub(crate) const BACKFILL_PER_ROOM_LIMIT: i64 = 200;
/// A cursor-less room follows the same semantics as opening room history:
/// replay only the newest page, with older messages reachable via REST
/// `?before=`/scroll-back rather than eagerly replaying the entire room.
pub(crate) const INITIAL_BACKFILL_PER_ROOM_LIMIT: i64 = 50;

/// Parse a best-effort reconnect cursor. Invalid input disables the legacy
/// global cursor without rejecting the WebSocket upgrade.
#[must_use]
pub(crate) fn parse_resume_cursor(raw: Option<&str>) -> Option<MessageId> {
    raw.and_then(|s| MessageId::from_str(s.trim()).ok())
}

/// Return a REST continuation cursor when a capped forward replay may have
/// more rows.
#[must_use]
pub(crate) fn truncation_cursor<T: Copy>(
    replayed: usize,
    limit: i64,
    last: Option<T>,
) -> Option<T> {
    let replayed = i64::try_from(replayed).unwrap_or(i64::MAX);
    if replayed >= limit {
        last
    } else {
        None
    }
}

/// Extract room ids without changing the repository's stable ordering.
#[must_use]
pub(crate) fn backfill_room_ids(rooms: &[aero_common::Room]) -> Vec<RoomId> {
    rooms.iter().map(|r| r.id).collect()
}

/// Plan one replay for every current room. `Some(message)` means a forward
/// cursor replay; `None` means a bounded initial-history replay. Stale cursors
/// for rooms outside `rooms` are deliberately ignored.
#[must_use]
pub(crate) fn cursor_backfill_plan(
    rooms: &[RoomId],
    cursors: &[aero_storage::DeliveryCursor],
) -> Vec<(RoomId, Option<i64>)> {
    let by_room: HashMap<RoomId, i64> = cursors
        .iter()
        .map(|cursor| (cursor.room_id, cursor.last_delivery_ordinal))
        .collect();
    rooms
        .iter()
        .copied()
        .map(|room| (room, by_room.get(&room).copied()))
        .collect()
}

/// Convert a newest-first repository page into the bounded chronological
/// initial replay window.
#[must_use]
#[cfg(test)]
pub(crate) fn initial_backfill_page<T>(mut newest_first: Vec<T>, limit: i64) -> (Vec<T>, bool) {
    let cap = usize::try_from(limit.max(0)).unwrap_or(usize::MAX);
    let truncated = newest_first.len() > cap;
    newest_first.truncate(cap);
    newest_first.reverse();
    (newest_first, truncated)
}

/// Current rooms that still pass the same full workspace/room/deactivation/2FA
/// guard as an ordinary room data request. Backfill is content delivery, so a
/// stale `room_members` row alone is not authorization.
async fn effective_backfill_rooms(state: &AppState, pid: ParticipantId) -> Option<Vec<RoomId>> {
    let rooms = match state.rooms.rooms_for(pid).await {
        Ok(rooms) => rooms,
        Err(error) => {
            warn!(?error, %pid, "reconnect backfill: list rooms failed");
            return None;
        }
    };
    let mut effective = Vec::with_capacity(rooms.len());
    for room in backfill_room_ids(&rooms) {
        match state.im.assert_room_access(pid, room).await {
            Ok(()) => effective.push(room),
            Err(
                error @ (aero_common::Error::Forbidden(_)
                | aero_common::Error::Unauthorized(_)
                | aero_common::Error::NotFound(_)),
            ) => {
                debug!(%pid, %room, %error, "reconnect backfill: inaccessible room skipped");
            }
            Err(error) => {
                // An infrastructure failure is not evidence that the room is
                // inaccessible. Withholding `delivery_ready` keeps client ACKs
                // fail-closed so a transient guard read cannot skip its gap.
                warn!(%pid, %room, %error, "reconnect backfill: room authorization failed");
                return None;
            }
        }
    }
    Some(effective)
}

/// Legacy global-cursor replay. Kept for clients/servers that predate
/// participant-room delivery cursors.
pub(super) async fn backfill_since(
    state: &AppState,
    pid: ParticipantId,
    cursor: MessageId,
    tx: &mpsc::Sender<Message>,
    close: &CancellationToken,
    summarize: bool,
) {
    let Some(rooms) = effective_backfill_rooms(state, pid).await else {
        return;
    };
    let mut replayed = 0usize;
    for room in rooms {
        match replay_room_since(state, room, cursor, tx, close, summarize).await {
            Ok(count) => replayed += count,
            Err(()) => return,
        }
    }
    if replayed > 0 {
        debug!(%pid, replayed, "reconnect backfill replayed missed messages");
    }
}

/// Per-room durable-cursor replay. Every effective current room is included:
/// rooms with a cursor replay forward from it; rooms without one receive a
/// bounded newest-history window.
pub(super) async fn backfill_from_cursors(
    state: &AppState,
    pid: ParticipantId,
    tx: &mpsc::Sender<Message>,
    close: &CancellationToken,
    summarize: bool,
) -> Option<Vec<DeliveryRoomBarrier>> {
    let Some(rooms) = effective_backfill_rooms(state, pid).await else {
        return None;
    };
    let cursors = match state.delivery_cursors.cursors_for(pid).await {
        Ok(cursors) => cursors,
        Err(error) => {
            warn!(?error, %pid, "reconnect backfill: list delivery cursors failed");
            return None;
        }
    };

    let mut replayed = 0usize;
    let mut barriers = Vec::with_capacity(rooms.len());
    for (room, cursor) in cursor_backfill_plan(&rooms, &cursors) {
        let result = if let Some(ordinal) = cursor {
            replay_room_after_ordinal(state, room, ordinal, tx, close, summarize).await
        } else {
            replay_room_initial(state, room, tx, close, summarize).await
        };
        match result {
            Ok((count, delivery_ordinal)) => {
                replayed += count;
                barriers.push(DeliveryRoomBarrier {
                    room_id: room,
                    delivery_ordinal,
                });
            }
            Err(()) => return None,
        }
    }
    if replayed > 0 {
        debug!(%pid, replayed, "reconnect backfill (per-room cursors) replayed messages");
    }
    Some(barriers)
}

/// Replay every durable message after one room-delivery ordinal. The query is
/// deliberately paged but has no fixed total-page ceiling: `delivery_ready`
/// certifies a complete prefix, so emitting it after a truncated page would let
/// a later live ACK skip the unresolved gap.
async fn replay_room_after_ordinal(
    state: &AppState,
    room: RoomId,
    cursor: i64,
    tx: &mpsc::Sender<Message>,
    close: &CancellationToken,
    summarize: bool,
) -> Result<(usize, i64), ()> {
    let mut after = cursor.max(0);
    let mut replayed = 0usize;
    loop {
        let page = match state
            .messages
            .list_delivery_after(room, after, BACKFILL_PER_ROOM_LIMIT)
            .await
        {
            Ok(messages) => messages,
            Err(error) => {
                warn!(?error, %room, after, "reconnect backfill: ordinal page failed");
                return Err(());
            }
        };
        if page.is_empty() {
            break;
        }
        let page_len = page.len();
        let page_last = page
            .last()
            .map_or(after, |(_, delivery_ordinal)| *delivery_ordinal);
        if summarize {
            let messages: Vec<_> = page.iter().map(|(message, _)| message.clone()).collect();
            send_summary(room, &messages, false, false, tx, close).await?;
            replayed += page_len;
        } else {
            for (message, ordinal) in page {
                send_message(message, Some(ordinal), tx, close).await?;
                replayed += 1;
            }
        }
        after = page_last;
        if page_len < usize::try_from(BACKFILL_PER_ROOM_LIMIT).unwrap_or(usize::MAX) {
            break;
        }
    }
    Ok((replayed, after))
}

/// Replay one room forward from a durable/legacy cursor.
async fn replay_room_since(
    state: &AppState,
    room: RoomId,
    cursor: MessageId,
    tx: &mpsc::Sender<Message>,
    close: &CancellationToken,
    summarize: bool,
) -> Result<usize, ()> {
    let missed = match state
        .messages
        .list_since(room, cursor, BACKFILL_PER_ROOM_LIMIT + 1)
        .await
    {
        Ok(messages) => messages,
        Err(error) => {
            warn!(?error, %room, "reconnect backfill: list_since failed");
            return Err(());
        }
    };
    let room_count = missed.len();
    let cap = usize::try_from(BACKFILL_PER_ROOM_LIMIT).unwrap_or(usize::MAX);
    let mut replayed = 0usize;

    let next_since = if summarize {
        let last_id = missed.last().map(|message| message.id);
        send_summary(room, &missed, false, room_count > cap, tx, close).await?;
        replayed += room_count;
        truncation_cursor(room_count, BACKFILL_PER_ROOM_LIMIT, last_id)
    } else {
        let mut last_replayed = None;
        for message in missed.into_iter().take(cap) {
            last_replayed = Some(message.id);
            send_message(message, None, tx, close).await?;
            replayed += 1;
        }
        if room_count > cap {
            last_replayed
        } else {
            None
        }
    };

    if let Some(next_since) = next_since {
        send_json(
            serde_json::json!({
                "type": "backfill",
                "room_id": room,
                "truncated": true,
                "next_since": next_since,
            }),
            tx,
            close,
        )
        .await?;
    }
    Ok(replayed)
}

/// Replay the newest initial-history page for a room without a durable cursor.
/// `list_recent` is newest-first; reverse the selected newest page before
/// sending so application handlers observe chronological order.
async fn replay_room_initial(
    state: &AppState,
    room: RoomId,
    tx: &mpsc::Sender<Message>,
    close: &CancellationToken,
    summarize: bool,
) -> Result<(usize, i64), ()> {
    let mut recent = match state
        .messages
        .list_recent_delivery(room, INITIAL_BACKFILL_PER_ROOM_LIMIT + 1)
        .await
    {
        Ok(messages) => messages,
        Err(error) => {
            warn!(?error, %room, "reconnect backfill: list_recent failed");
            return Err(());
        }
    };
    let cap = usize::try_from(INITIAL_BACKFILL_PER_ROOM_LIMIT).unwrap_or(usize::MAX);
    let truncated = recent.len() > cap;
    if truncated {
        let remove = recent.len() - cap;
        recent.drain(..remove);
    }
    let next_before = recent.first().map(|(message, _)| message.id);
    let replayed = recent.len();
    let barrier = recent
        .last()
        .map_or(0, |(_, delivery_ordinal)| *delivery_ordinal);

    if summarize {
        let messages: Vec<_> = recent.iter().map(|(message, _)| message.clone()).collect();
        send_summary(room, &messages, true, truncated, tx, close).await?;
    } else {
        for (message, ordinal) in recent {
            send_message(message, Some(ordinal), tx, close).await?;
        }
    }

    // This marker distinguishes a bounded first-page seed from a forward gap.
    // Existing clients ignore it (no `next_since`); cursor-aware clients can
    // continue older history with normal REST `?before=<next_before>` paging.
    send_json(
        serde_json::json!({
            "type": "backfill",
            "room_id": room,
            "initial": true,
            "truncated": truncated,
            "next_before": next_before,
        }),
        tx,
        close,
    )
    .await?;
    Ok((replayed, barrier))
}

async fn send_message(
    message: aero_common::Message,
    delivery_ordinal: Option<i64>,
    tx: &mpsc::Sender<Message>,
    close: &CancellationToken,
) -> Result<(), ()> {
    let json = serde_json::to_string(&ServerFrame::Message {
        message,
        delivery_ordinal,
        client_message_id: None,
    })
    .unwrap_or_default();
    send_text(json, tx, close).await
}

async fn send_summary(
    room: RoomId,
    messages: &[aero_common::Message],
    initial: bool,
    truncated: bool,
    tx: &mpsc::Sender<Message>,
    close: &CancellationToken,
) -> Result<(), ()> {
    let participants: std::collections::BTreeSet<ParticipantId> =
        messages.iter().map(|message| message.sender_id).collect();
    let snippet = messages
        .last()
        .map(|message| {
            let text = message.searchable_text();
            let truncated_text: String = text.chars().take(120).collect();
            if text.chars().count() > 120 {
                format!("{truncated_text}…")
            } else {
                truncated_text
            }
        })
        .unwrap_or_default();
    send_json(
        serde_json::json!({
            "type": "backfill_summary",
            "room_id": room,
            "total": messages.len(),
            "participant_count": participants.len(),
            "snippet": snippet,
            "initial": initial,
            "truncated": truncated,
        }),
        tx,
        close,
    )
    .await
}

async fn send_json(
    value: serde_json::Value,
    tx: &mpsc::Sender<Message>,
    close: &CancellationToken,
) -> Result<(), ()> {
    send_text(value.to_string(), tx, close).await
}

async fn send_text(
    text: String,
    tx: &mpsc::Sender<Message>,
    close: &CancellationToken,
) -> Result<(), ()> {
    tokio::select! {
        () = close.cancelled() => Err(()),
        result = tx.send(Message::Text(text)) => result.map_err(|_| ()),
    }
}
