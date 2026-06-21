//! WebSocket endpoint and protocol.
//!
//! Wire format is JSON; all frames are tagged with `type`. See the design spec
//! for the full message set. JWT is passed via `?token=...` query parameter
//! because browser WebSocket clients can't set custom headers.
//!
//! ## Reconnect backfill protocol (ROADMAP 方向五)
//!
//! A client that dropped its socket can avoid silently losing messages by
//! reconnecting with an extra query param: `/ws?token=...&since=<message_id>`,
//! where `<message_id>` is the id of the **last message it successfully
//! received** (any room). On connect, before any live event resumes, the server
//! replays every message created strictly after that cursor — across all rooms
//! the participant belongs to — as ordinary `message` frames (via
//! [`aero_storage::MessageRepo::list_since`], oldest-first, capped per room).
//! `since` is best-effort: a malformed/garbage cursor is ignored (the client
//! simply gets no backfill) rather than failing the upgrade, and replay
//! failures never abort the connection. The forward REST complement is
//! `GET /api/rooms/:id/messages?since=<message_id>`.
use std::str::FromStr;
use std::sync::Arc;
use aero_common::metrics::{self, names};
use aero_common::{
    Block, CallEvent, CallId, CallKind, CallMode, MembershipOp, MessageId, NotificationKind,
    ParticipantId, PinOp, PollId, PollOp, ReactionOp, RoomEvent, RoomId, StreamEvent,
};
use ulid::Ulid;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::IntoResponse,
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};
use crate::hub::WsSender;
use crate::state::AppState;
#[derive(Debug, Deserialize)]
pub struct WsParams {
    token: String,
    /// Optional reconnect cursor: the id of the last message the client already
    /// has. On connect the server backfills everything created after it across
    /// the participant's rooms before live events resume. See the module docs.
    #[serde(default)]
    since: Option<String>,
    /// When true, the reconnect backfill sends a room-level summary
    /// (participant count, message count, last-snippet) instead of every
    /// individual message. The client can request full details per room.
    /// Defaults to false (full per-message replay).
    #[serde(default)]
    summarize: Option<bool>,
    /// Opt-in per-room delivery-cursor backfill (ROADMAP 方向三·A). When truthy
    /// (`1`/`true`/`yes`) AND no explicit `since` is given, the server resumes each
    /// room from this participant's persisted DELIVERY cursor (multi-device-shared)
    /// instead of a single global cursor. Ignored when `since` is present (explicit
    /// wins, for back-compat). A free-form string (not `Option<bool>`) so a `1`
    /// from a client is lenient — like `since` — rather than 400-ing the whole
    /// upgrade. Absent/empty ⇒ off.
    #[serde(default)]
    cursors: Option<String>,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ClientFrame {
    /// Subscribe local presence for a room.
    JoinRoom { room_id: RoomId },
    /// Send a new message.
    SendMessage {
        room_id: RoomId,
        blocks: Vec<Block>,
        #[serde(default)]
        reply_to: Option<MessageId>,
        /// Optional sender-set TTL in seconds. If > 0, the message is
        /// hard-deleted by the ephemeral sweep once this many seconds elapse.
        #[serde(default)]
        expires_after_secs: Option<u64>,
    },
    /// Send a new message authored as Markdown text (ROADMAP6 方向三 富文本管线
    /// edge entry). Purely additive over `SendMessage { blocks }`: the only
    /// difference is the body arrives as a Markdown string, which the edge parses
    /// into the same `Vec<Block>` via [`aero_common::markdown::parse_markdown_to_blocks`]
    /// before going down the *identical* send path (access / rate / slowmode /
    /// ephemeral TTL). Structured-block clients are entirely unaffected.
    ///
    /// `@mention`s currently land as nil-id `Block::Mention` markers (the parser
    /// only captures `display_name`; resolving it to a `ParticipantId` needs a
    /// workspace-scoped lookup) — see the handler note. This is a follow-up, not a
    /// regression: nil-id mentions are a harmless no-op in the notify pipeline.
    SendMarkdown {
        room_id: RoomId,
        markdown: String,
        #[serde(default)]
        reply_to: Option<MessageId>,
        /// Optional sender-set TTL in seconds. If > 0, the message is
        /// hard-deleted by the ephemeral sweep once this many seconds elapse.
        #[serde(default)]
        expires_after_secs: Option<u64>,
    },
    /// Edit an existing message (sender only).
    EditMessage { id: MessageId, blocks: Vec<Block> },
    /// Soft-delete a message.
    DeleteMessage { id: MessageId },
    /// Toggle a reaction.
    React { message_id: MessageId, emoji: String },
    /// Mark a room read up to the given message.
    MarkRead { room_id: RoomId, last_message_id: MessageId },
    /// Acknowledge durable receipt of a message (ROADMAP 方向三·A). Distinct from
    /// `MarkRead` (visual "seen"): this advances the per-room DELIVERY cursor — the
    /// Last-Known-Good point reconnect resumes each room from. Monotonic on `seq`
    /// (the per-subject bus seq the client already dedupes on), so redelivery and
    /// racing multi-device ACKs are harmless. Best-effort: a failed persist never
    /// tears down the socket.
    DeliveryAck { room_id: RoomId, message_id: MessageId, seq: i64 },
    /// Best-effort typing indicator.
    Typing { room_id: RoomId, on: bool },
    /// Start a call (1:1 or group).
    CallInvite {
        room_id: RoomId,
        kind: CallKind,
        #[serde(default)]
        mode: Option<CallMode>,
        sdp: String,
    },
    /// Answer an incoming call.
    CallAnswer {
        call_id: CallId,
        room_id: RoomId,
        to: ParticipantId,
        sdp: String,
    },
    /// Trickle an ICE candidate.
    CallIce {
        call_id: CallId,
        room_id: RoomId,
        to: ParticipantId,
        candidate: serde_json::Value,
    },
    /// End a call.
    CallEnd {
        call_id: CallId,
        room_id: RoomId,
        #[serde(default)]
        reason: Option<String>,
    },
    /// Live caption line during a call. When `target_lang` is set and differs
    /// from `lang`, the server translates *final* lines via the AI backend.
    CallCaption {
        call_id: CallId,
        room_id: RoomId,
        text: String,
        #[serde(default)]
        lang: Option<String>,
        #[serde(default)]
        target_lang: Option<String>,
        #[serde(default)]
        is_final: bool,
    },
    /// Join (or start) a group call (P6 mesh). Omit `call_id` to start a new one.
    CallJoin {
        room_id: RoomId,
        kind: CallKind,
        #[serde(default)]
        call_id: Option<CallId>,
    },
    /// Leave a group call.
    CallLeave { call_id: CallId, room_id: RoomId },
    /// A per-pair mesh offer to one peer in a group call.
    CallOffer {
        call_id: CallId,
        room_id: RoomId,
        to: ParticipantId,
        sdp: String,
    },
    /// Start watching a live stream (danmaku/gift fan-out + viewer count).
    WatchStream {
        stream_id: Ulid,
        /// Optional forward catch-up cursor: the last chat-line id the client has
        /// rendered. Lines strictly newer are replayed on watch, so a re-watch or
        /// late join doesn't silently miss the danmaku in between.
        #[serde(default)]
        since: Option<String>,
    },
    /// Stop watching a live stream.
    UnwatchStream { stream_id: Ulid },
    /// Post a danmaku line on a stream.
    StreamChat { stream_id: Ulid, body: String },
    /// Send a gift on a stream (`qty` defaults to 1).
    StreamGift {
        stream_id: Ulid,
        gift_id: String,
        #[serde(default)]
        qty: Option<u32>,
        /// Optional client-supplied idempotency token; a retried send with the
        /// same `nonce` records/broadcasts the gift exactly once.
        #[serde(default)]
        nonce: Option<String>,
    },
    Ping,
}
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ServerFrame<'a> {
    Welcome { participant: ParticipantId },
    Presence { room_id: RoomId, online: Vec<ParticipantId> },
    Message { message: aero_common::Message },
    Edited { message: aero_common::Message },
    Deleted { room_id: RoomId, message_id: MessageId, by: ParticipantId },
    Reaction {
        room_id: RoomId,
        message_id: MessageId,
        participant: ParticipantId,
        emoji: String,
        op: ReactionOp,
    },
    Read {
        room_id: RoomId,
        participant: ParticipantId,
        last_message_id: MessageId,
        at: time::OffsetDateTime,
    },
    Typing { room_id: RoomId, participant: ParticipantId, on: bool },
    /// You were mentioned / replied to (targeted to the recipient only).
    Notify {
        room_id: RoomId,
        message_id: MessageId,
        mentioned: ParticipantId,
        by: ParticipantId,
        notify_kind: NotificationKind,
    },
    /// A message was pinned/unpinned in a room (fans out to all members).
    Pin { room_id: RoomId, message_id: MessageId, by: ParticipantId, op: PinOp },
    /// A participant joined/left a channel (fans out to all room members).
    Membership { room_id: RoomId, participant: ParticipantId, op: MembershipOp },
    Call { event: CallEvent },
    /// A poll was created/voted/closed in a room (fans out to all members so the
    /// tally stays live).
    Poll { room_id: RoomId, poll_id: PollId, op: PollOp },
    /// A participant acknowledged seeing a specific message ("Seen by …"; fans
    /// out to all members so the per-message read indicator stays live).
    MessageSeen { room_id: RoomId, message_id: MessageId, participant: ParticipantId },
    /// A participant clicked a Button / picked a Select option on an interactive
    /// message block (fans out to all members so the poster's bot/app sees it live).
    Interaction {
        room_id: RoomId,
        message_id: MessageId,
        participant: ParticipantId,
        action_id: String,
    },
    /// Per-stream interactivity event (danmaku/gift/viewers/status).
    StreamEvent { event: StreamEvent },
    Error { code: &'a str, msg: String },
    Pong,
}
#[instrument(skip(ws, state))]
pub async fn handler(
    ws: WebSocketUpgrade,
    Query(p): Query<WsParams>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let claims = match state.auth.verify(&p.token) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "ws auth failed");
            return (axum::http::StatusCode::UNAUTHORIZED, "invalid token").into_response();
        }
    };
    let pid: ParticipantId = match claims.participant_id() {
        Ok(p) => p,
        Err(_) => {
            return (axum::http::StatusCode::UNAUTHORIZED, "invalid sub").into_response();
        }
    };
    // Best-effort reconnect cursor: a garbage value is simply ignored (no
    // backfill) rather than rejecting the upgrade. See module-level protocol docs.
    let since = parse_resume_cursor(p.since.as_deref());
    let summarize = p.summarize.unwrap_or(false);
    let use_cursors = matches!(
        p.cursors.as_deref().map(str::trim),
        Some("1" | "true" | "yes" | "on")
    );
    ws.on_upgrade(move |socket| run_socket(socket, state, pid, since, summarize, use_cursors))
}
#[instrument(skip(socket, state, since), fields(%pid))]
async fn run_socket(
    socket: axum::extract::ws::WebSocket,
    state: AppState,
    pid: ParticipantId,
    since: Option<MessageId>,
    summarize: bool,
    use_cursors: bool,
) {
    let (mut sender, mut receiver) = socket.split();
    // Bounded outbound queue: a slow/stalled client can never make the
    // broadcaster grow memory without limit (OOM guard). On a full queue the Hub
    // drops the frame and — per config — disconnects the client via `close`.
    let (tx, mut rx) = mpsc::channel::<Message>(state.ws_config.send_queue_capacity);
    let close = CancellationToken::new();
    state.hub.register(pid, WsSender::new(tx.clone(), close.clone()));
    let _ = tx.try_send(Message::Text(
        serde_json::to_string(&ServerFrame::Welcome { participant: pid }).unwrap_or_default(),
    ));
    let outgoing = {
        let close = close.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                        () = close.cancelled() => break,
                    msg = rx.recv() => match msg {
                        Some(msg) => {
                            if sender.send(msg).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    },
                }
            }
        })
    };
    // Reconnect backfill (ROADMAP 方向五): before live events resume, replay
    // everything the client missed while disconnected. Done after the outgoing
    // task is draining `tx` (so we apply real back-pressure instead of dropping)
    // and before the receive loop (so the catch-up is chronological and lands
    // ahead of any new live frames). Best-effort: failures never abort the conn.
    if let Some(cursor) = since {
        backfill_since(&state, pid, cursor, &tx, &close, summarize).await;
    } else if use_cursors {
        // No explicit `since`: resume each room from its persisted delivery cursor
        // (ROADMAP 方向三·A, multi-device-shared per-room catch-up).
        backfill_from_cursors(&state, pid, &tx, &close, summarize).await;
    }
    loop {
        tokio::select! {
            // The Hub asked us to drop this connection (laggy / evicted).
            () = close.cancelled() => break,
            incoming = receiver.next() => {
                let Some(Ok(msg)) = incoming else { break };
                match msg {
                    Message::Text(text) => {
                        if let Err(e) = frame::handle_text(&text, &state, pid, &tx).await {
                            let _ = tx.try_send(Message::Text(
                                serde_json::to_string(&ServerFrame::Error {
                                    code: "handler",
                                    msg: e.to_string(),
                                })
                                .unwrap_or_default(),
                            ));
                        }
                    }
                    Message::Ping(p) => {
                        let _ = tx.try_send(Message::Pong(p));
                    }
                    Message::Close(_) => break,
                    Message::Binary(_) => {
                        let _ = tx.try_send(Message::Text(
                            serde_json::to_string(&ServerFrame::Error {
                                code: "unsupported_frame",
                                msg: "binary frames are not accepted; use text frames only".into(),
                            })
                            .unwrap_or_default(),
                        ));
                    }
                    _ => {}
                }
            }
        }
    }
    // Cluster-wide presence (ROADMAP 方向一): drop this participant from every
    // room's Redis presence set BEFORE the hub purges its local reverse index.
    // Best-effort — crashed clients also age out via the heartbeat TTL.
    for room in state.hub.rooms_of(pid) {
        if let Err(e) = state.presence.leave(room, pid).await {
            warn!(error = ?e, %room, %pid, "redis room-presence leave failed");
        }
    }
    let registered = WsSender::new(tx, close.clone());
    state.hub.unregister(pid, &registered);
    close.cancel();
    outgoing.abort();
    info!(%pid, "ws closed");
}
/// Per-room cap on reconnect backfill replay, so a client that has been away for
/// a long time can't make a single connection replay an unbounded history (it
/// can keep paging via the REST `?since=` route). Matches the keyset page window
/// `MessageRepo::list_since` clamps to.
pub(crate) const BACKFILL_PER_ROOM_LIMIT: i64 = 200;
/// Parse a best-effort reconnect/resume cursor. `None` (absent) and any value
/// that fails to decode as a [`MessageId`] both yield `None` — backfill is
/// purely additive, so a bad cursor must degrade to "no backfill" rather than
/// fail the connection. Pure + total, so it unit-tests without any I/O.
#[must_use]
pub(crate) fn parse_resume_cursor(raw: Option<&str>) -> Option<MessageId> {
    raw.and_then(|s| MessageId::from_str(s.trim()).ok())
}
/// Truncation decision for a capped replay (ROADMAP 第三版 方向一): when a
/// replay returned exactly `limit` rows it may have been cut short, so the
/// client must be told to continue via the REST `?since=` route from the last
/// replayed id. Returns that continuation cursor, or `None` when the replay
/// fit under the cap (or replayed nothing). A full-but-complete page yields one
/// harmless extra REST round-trip that returns empty — never a missed message.
/// Pure + total, so it unit-tests without any I/O.
#[must_use]
pub(crate) fn truncation_cursor<T: Copy>(replayed: usize, limit: i64, last: Option<T>) -> Option<T> {
    let replayed = i64::try_from(replayed).unwrap_or(i64::MAX);
    if replayed >= limit {
        last
    } else {
        None
    }
}
/// Pick the authoritative cluster-wide count from the Redis result, falling back
/// to the process-local Hub value when Redis is unavailable so the reported
/// number is never worse than today's single-node behaviour.
///
/// This is the injectable seam for the ROADMAP 方向二 "Redis-global count"
/// requirement: the Watch/Unwatch and roster paths call Redis and feed the
/// result here, so the *direction* (prefer Redis) and the *fallback* (use local
/// on error) are unit-testable without a live Redis.
#[must_use]
pub(crate) fn authoritative_count(redis: anyhow::Result<u64>, local_fallback: u32) -> u32 {
    // `map_or` consumes the result by value (Ok ⇒ Redis count, saturating into
    // u32; Err ⇒ the process-local fallback).
    redis.map_or(local_fallback, |n| u32::try_from(n).unwrap_or(u32::MAX))
}
/// Cluster-wide viewer count for a stream: Redis global `count`, falling back to
/// the local Hub count if Redis errs.
async fn stream_viewer_count(state: &AppState, stream_id: Ulid) -> u32 {
    let local = state.hub.stream_viewer_count(stream_id);
    authoritative_count(state.stream_viewers.count(stream_id).await, local)
}
/// The set of call peers a freshly-joined participant must connect to, sourced
/// from the cluster-wide Redis roster (so peers on other nodes are included),
/// with `joiner` removed. On any Redis error, returns `local_fallback` (the
/// Hub's already-present set) so a single-node call still works.
async fn call_peers_excluding(
    state: &AppState,
    call_id: CallId,
    joiner: ParticipantId,
    local_fallback: Vec<ParticipantId>,
) -> Vec<ParticipantId> {
    match state.call_roster.roster(call_id).await {
        Ok(members) => members.into_iter().filter(|p| *p != joiner).collect(),
        Err(e) => {
            warn!(error = ?e, %call_id, "redis call-roster read failed; using local roster");
            local_fallback
        }
    }
}
/// The rooms whose history we replay on reconnect: every room the participant
/// belongs to. Factored out (and kept pure over the room list) so the
/// selection/iteration logic is unit-testable without a database.
#[must_use]
pub(crate) fn backfill_room_ids(rooms: &[aero_common::Room]) -> Vec<RoomId> {
    rooms.iter().map(|r| r.id).collect()
}
/// Replay messages missed since `cursor` for every room the participant belongs
/// to, oldest-first, as ordinary `message` frames — the WS half of ROADMAP
/// 方向五 reconnect backfill. Best-effort throughout: a failed room lookup or a
/// failed per-room query is logged and skipped, never aborting the connection.
/// Sends honour `close` so a torn-down connection stops replaying immediately,
/// and use the bounded channel's back-pressure (await, not `try_send`) so a
/// large catch-up is delivered rather than silently dropped.
async fn backfill_since(
    state: &AppState,
    pid: ParticipantId,
    cursor: MessageId,
    tx: &mpsc::Sender<Message>,
    close: &CancellationToken,
    summarize: bool,
) {
    let rooms = match state.rooms.rooms_for(pid).await {
        Ok(rs) => rs,
        Err(e) => {
            warn!(error = ?e, %pid, "reconnect backfill: list rooms failed");
            return;
        }
    };
    let mut replayed = 0usize;
    for room in backfill_room_ids(&rooms) {
        // Every room replays from the SAME global `cursor` (legacy `?since=`).
        match replay_room_since(state, room, cursor, tx, close, summarize).await {
            Ok(n) => replayed += n,
            Err(()) => return, // connection gone mid-replay
        }
    }
    if replayed > 0 {
        debug!(%pid, replayed, "reconnect backfill replayed missed messages");
    }
}

/// Per-room reconnect backfill from each room's persisted DELIVERY cursor
/// (ROADMAP 方向三·A · opt-in `?cursors=1`). Unlike [`backfill_since`] — which
/// replays every room from one global id — this resumes each room from exactly
/// what *this participant has ACKed receiving there*, so a second device sharing
/// the (participant, room) cursor only sees genuinely-new messages (no per-device
/// re-replay), and rooms the client is already caught up on send nothing.
///
/// Rooms whose cursor exists but where the participant is no longer a member are
/// skipped (a stale cursor for a left room must never replay). Best-effort: a
/// failed cursor/room lookup logs and returns.
async fn backfill_from_cursors(
    state: &AppState,
    pid: ParticipantId,
    tx: &mpsc::Sender<Message>,
    close: &CancellationToken,
    summarize: bool,
) {
    let cursors = match state.delivery_cursors.cursors_for(pid).await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = ?e, %pid, "reconnect backfill: list delivery cursors failed");
            return;
        }
    };
    if cursors.is_empty() {
        return;
    }
    // Current membership: a cursor for a room the participant has since left must
    // not replay (membership can change between disconnect and reconnect).
    let member_rooms: std::collections::HashSet<RoomId> = match state.rooms.rooms_for(pid).await {
        Ok(rs) => backfill_room_ids(&rs).into_iter().collect(),
        Err(e) => {
            warn!(error = ?e, %pid, "reconnect backfill: list rooms failed");
            return;
        }
    };
    let mut replayed = 0usize;
    for cur in cursors {
        if !member_rooms.contains(&cur.room_id) {
            continue;
        }
        match replay_room_since(
            state,
            cur.room_id,
            cur.last_delivered_message_id,
            tx,
            close,
            summarize,
        )
        .await
        {
            Ok(n) => replayed += n,
            Err(()) => return,
        }
    }
    if replayed > 0 {
        debug!(%pid, replayed, "reconnect backfill (per-room cursors) replayed missed messages");
    }
}

/// Replay one room's messages newer than `cursor`, oldest-first. Returns the
/// number of frames replayed, or `Err(())` when the connection went away
/// mid-replay (the caller must stop the whole backfill). A per-room query error
/// is logged and yields `Ok(0)` (skip this room, keep going) — best-effort, never
/// aborting on a single bad room. Shared by [`backfill_since`] (one global cursor
/// for every room) and [`backfill_from_cursors`] (per-room delivery cursors).
async fn replay_room_since(
    state: &AppState,
    room: RoomId,
    cursor: MessageId,
    tx: &mpsc::Sender<Message>,
    close: &CancellationToken,
    summarize: bool,
) -> Result<usize, ()> {
    // Fetch one PAST the replay cap: a full `cap + 1` page is the reliable
    // "there is more behind the cap" signal that drives the truncation frame
    // below (and keeps the burst bounded — a long offline gap must not dump
    // thousands of rows over the socket; the client pulls the remainder via
    // REST). list_since HONOURS this limit (it previously hardcoded 500).
    let missed = match state
        .messages
        .list_since(room, cursor, BACKFILL_PER_ROOM_LIMIT + 1)
        .await
    {
        Ok(m) => m,
        Err(e) => {
            warn!(error = ?e, %room, "reconnect backfill: list_since failed");
            return Ok(0);
        }
    };
    let room_count = missed.len();
    let mut replayed = 0usize;
    // Per-room replay cap. The full per-message replay below stops at
    // BACKFILL_PER_ROOM_LIMIT and the client pulls the remainder via REST.
    // CRITICAL: the truncation cursor must be the last *replayed* id, not the
    // last *missed* id — otherwise the REST continuation (`id > since`) starts
    // past the whole gap and returns nothing (regression: list_since once
    // replayed everything yet still signalled truncation pointing at the tail
    // → smoke_roadmap3_wave_c REST-continuation returned 0).
    let cap = usize::try_from(BACKFILL_PER_ROOM_LIMIT).unwrap_or(usize::MAX);
    let next_since: Option<MessageId> = if summarize {
        // Summarized backfill: one room-level summary stands in for the whole
        // gap (not a per-message burst), so it is not subject to the cap.
        let last_id = missed.last().map(|m| m.id);
        let participants: std::collections::BTreeSet<ParticipantId> =
            missed.iter().map(|m| m.sender_id).collect();
        let snippet = missed.last().map(|m| {
            let text = m.searchable_text();
            let truncated: String = text.chars().take(120).collect();
            if text.chars().count() > 120 { format!("{truncated}…") } else { truncated }
        }).unwrap_or_default();
        let frame = serde_json::json!({
            "type": "backfill_summary",
            "room_id": room,
            "total": room_count,
            "participant_count": participants.len(),
            "snippet": snippet,
        });
        tokio::select! {
            () = close.cancelled() => return Err(()),
            res = tx.send(Message::Text(frame.to_string())) => {
                if res.is_err() { return Err(()); }
            }
        }
        // Still track replayed count for the debug log.
        replayed += room_count;
        truncation_cursor(room_count, BACKFILL_PER_ROOM_LIMIT, last_id)
    } else {
        // Full per-message replay (legacy behaviour), capped at the per-room
        // limit; `last_replayed` tracks the gap boundary for the cursor.
        let mut last_replayed = None;
        for message in missed.into_iter().take(cap) {
            last_replayed = Some(message.id);
            let frame = ServerFrame::Message { message };
            let json = serde_json::to_string(&frame).unwrap_or_default();
            tokio::select! {
                    () = close.cancelled() => return Err(()),
                res = tx.send(Message::Text(json)) => {
                    if res.is_err() {
                        return Err(()); // receiver gone
                    }
                }
            }
            replayed += 1;
        }
        // Signal truncation only when the gap genuinely exceeded the cap; the
        // cursor is the last replayed id so REST resumes exactly at the gap.
        if room_count > cap { last_replayed } else { None }
    };
    // Truncation signal (ROADMAP 第三版 方向一): newer messages were left
    // behind, so tell the client to continue via REST
    // `GET /api/rooms/:id/messages?since=<next_since>` instead of silently
    // missing the remainder.
    if let Some(next_since) = next_since {
        let frame = serde_json::json!({
            "type": "backfill",
            "room_id": room,
            "truncated": true,
            "next_since": next_since,
        });
        tokio::select! {
            () = close.cancelled() => return Err(()),
            res = tx.send(Message::Text(frame.to_string())) => {
                if res.is_err() { return Err(()); }
            }
        }
    }
    Ok(replayed)
}
/// Loose BCP-47 comparison on the primary subtag, so `en` and `en-US` are the
/// same language and the server skips a no-op translation.
pub(crate) fn same_lang(a: &str, b: &str) -> bool {
    let primary = |s: &str| s.split(['-', '_']).next().unwrap_or(s).to_ascii_lowercase();
    primary(a) == primary(b)
}

mod frame;
mod bus;
#[cfg(test)]
#[path = "tests.rs"]
mod send_markdown_tests;

// `run_bus_listener` / `run_live_bus_listener` moved to `bus`; re-exported
// here so `crate::ws::ws_impl::run_bus_listener` (the boot path) still resolves.
pub use bus::{run_bus_listener, run_live_bus_listener};
