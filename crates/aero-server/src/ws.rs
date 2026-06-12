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
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientFrame {
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
    /// Edit an existing message (sender only).
    EditMessage { id: MessageId, blocks: Vec<Block> },
    /// Soft-delete a message.
    DeleteMessage { id: MessageId },
    /// Toggle a reaction.
    React { message_id: MessageId, emoji: String },
    /// Mark a room read up to the given message.
    MarkRead { room_id: RoomId, last_message_id: MessageId },
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
enum ServerFrame<'a> {
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
    ws.on_upgrade(move |socket| run_socket(socket, state, pid, since))
}

#[instrument(skip(socket, state, since), fields(%pid))]
async fn run_socket(
    socket: WebSocket,
    state: AppState,
    pid: ParticipantId,
    since: Option<MessageId>,
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
                    biased;
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
        backfill_since(&state, pid, cursor, &tx, &close).await;
    }

    loop {
        tokio::select! {
            // The Hub asked us to drop this connection (laggy / evicted).
            () = close.cancelled() => break,
            incoming = receiver.next() => {
                let Some(Ok(msg)) = incoming else { break };
                match msg {
                    Message::Text(text) => {
                        if let Err(e) = handle_text(&text, &state, pid, &tx).await {
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

async fn handle_text(
    text: &str,
    state: &AppState,
    pid: ParticipantId,
    tx: &mpsc::Sender<Message>,
) -> anyhow::Result<()> {
    let frame: ClientFrame = serde_json::from_str(text)?;
    match frame {
        ClientFrame::Ping => {
            let _ = tx.try_send(Message::Text(
                serde_json::to_string(&ServerFrame::Pong).unwrap_or_default(),
            ));
            // Treat the client heartbeat as a presence keep-alive: re-stamp every
            // room this connection has joined so an idle-but-connected member does
            // not age out of the cluster-wide presence set. Best-effort.
            for room in state.hub.rooms_of(pid) {
                if let Err(e) = state.presence.heartbeat(room, pid).await {
                    warn!(error = ?e, %room, %pid, "redis room-presence heartbeat failed");
                }
            }
        }
        ClientFrame::JoinRoom { room_id } => {
            if !state.rooms.is_member(room_id, pid).await? {
                let _ = tx.try_send(Message::Text(
                    serde_json::to_string(&ServerFrame::Error {
                        code: "forbidden",
                        msg: "not a member".into(),
                    })
                    .unwrap_or_default(),
                ));
                return Ok(());
            }
            state.hub.join_room(room_id, pid);
            // Cluster-wide presence (ROADMAP 方向一): stamp this participant into
            // the room's Redis presence set so other nodes count them. Best-effort
            // — a Redis miss only degrades the roster to node-local.
            if let Err(e) = state.presence.join(room_id, pid).await {
                warn!(error = ?e, %room_id, %pid, "redis room-presence join failed");
            }
            // Prefer the cluster-wide roster for the Presence frame; fall back to
            // this node's local view if Redis is unreachable / empty.
            let online = match state.presence.members(room_id).await {
                Ok(members) if !members.is_empty() => members,
                _ => state.hub.room_members_online(room_id),
            };
            let _ = tx.try_send(Message::Text(
                serde_json::to_string(&ServerFrame::Presence { room_id, online })
                    .unwrap_or_default(),
            ));
            debug!(%pid, %room_id, "joined room");
        }
        ClientFrame::SendMessage { room_id, blocks, reply_to, expires_after_secs } => {
            // Tenant guard: the sender must belong to BOTH the room's workspace and
            // the room before a message is accepted. `ImService::send_message`
            // re-checks room membership (a distinct, retained check); this adds the
            // workspace-membership dimension. Maps to the WS `error` frame on denial.
            state.im.assert_room_access(pid, room_id).await?;
            // Tenant fairness (ROADMAP3 方向五): sends are the highest-volume
            // write path, so they are charged against the room's workspace
            // budget — after the access check (so non-members cannot drain a
            // victim's budget), surfacing as a WS `error` frame when over.
            crate::ws_rate::check_ws_rate_room(state, room_id).await?;
            // Slowmode enforcement (migration 0107): if the room has a slowmode
            // interval, reject the send unless enough time has elapsed since the
            // sender's last message. Checked AFTER access/rate guards so only real
            // members burn through the interval; `unwrap_or(0)` is fail-open.
            let slowmode = state.rooms.get_slowmode(room_id).await.unwrap_or(0);
            if slowmode > 0 {
                let last_msg: Option<(time::OffsetDateTime,)> = sqlx::query_as(
                    "SELECT created_at FROM messages \
                     WHERE room_id = $1 AND sender_id = $2 AND deleted_at IS NULL \
                     ORDER BY created_at DESC LIMIT 1",
                )
                .bind(room_id.to_uuid())
                .bind(pid.to_uuid())
                .fetch_optional(&state.pg)
                .await
                .unwrap_or(None);
                if let Some((last_at,)) = last_msg {
                    let elapsed =
                        (time::OffsetDateTime::now_utc() - last_at).whole_seconds();
                    if elapsed < slowmode as i64 {
                        return Err(aero_common::Error::Invalid(format!(
                            "slowmode: wait {}s before sending again",
                            slowmode as i64 - elapsed
                        ))
                        .into());
                    }
                }
            }
            let expires_at = expires_after_secs
                .filter(|&s| s > 0)
                .map(|s| time::OffsetDateTime::now_utc() + time::Duration::seconds(s as i64));
            state.im.send_message(pid, room_id, blocks, reply_to, expires_at).await?;
        }
        ClientFrame::EditMessage { id, blocks } => {
            state.im.edit_message(pid, id, blocks).await?;
        }
        ClientFrame::DeleteMessage { id } => {
            state.im.delete_message(pid, id).await?;
        }
        ClientFrame::React { message_id, emoji } => {
            state.im.toggle_reaction(pid, message_id, &emoji).await?;
        }
        ClientFrame::MarkRead { room_id, last_message_id } => {
            state.im.mark_read(pid, room_id, last_message_id).await?;
        }
        ClientFrame::Typing { room_id, on } => {
            state.im.typing(pid, room_id, on).await?;
        }
        ClientFrame::CallInvite { room_id, kind, mode, sdp } => {
            state
                .im
                .start_call(pid, room_id, kind, mode.unwrap_or(CallMode::P2p), sdp)
                .await?;
        }
        ClientFrame::CallAnswer { call_id, room_id, to, sdp } => {
            state
                .im
                .relay_call_event(
                    room_id,
                    CallEvent::Answer { call_id, from: pid, to, sdp },
                )
                .await?;
            // Mark the call answered so a later End is not flagged as missed
            // (first answer wins; best-effort — never fails the relay).
            if let Err(e) = state.calls.mark_answered(call_id).await {
                tracing::warn!(error = ?e, %call_id, "mark_answered failed");
            }
        }
        ClientFrame::CallIce { call_id, room_id, to, candidate } => {
            state
                .im
                .relay_call_event(
                    room_id,
                    CallEvent::Ice { call_id, from: pid, to, candidate },
                )
                .await?;
        }
        ClientFrame::CallEnd { call_id, room_id, reason } => {
            state
                .im
                .relay_call_event(
                    room_id,
                    CallEvent::End {
                        call_id,
                        room_id,
                        by: pid,
                        reason: reason.unwrap_or_else(|| "ended".into()),
                    },
                )
                .await?;
            // Missed-call: if the call ended while never answered, drop a durable
            // "call_missed" notice into each callee's activity feed (Wave 21).
            // Out-of-band + best-effort — never fails the call-end relay.
            match state.calls.unanswered_callees(call_id).await {
                Ok(Some((initiator, callees))) => {
                    let feed = aero_storage::ActivityFeedRepo::new(state.pg.clone());
                    // Resolve the caller's name once for the mobile push title;
                    // only needed when a push gateway is actually configured.
                    let push_enabled = state.push.any_enabled();
                    let initiator_name = if push_enabled {
                        state
                            .participants
                            .get(initiator)
                            .await
                            .ok()
                            .flatten()
                            .map_or_else(|| "Someone".to_string(), |p| p.display_name)
                    } else {
                        String::new()
                    };
                    for callee in callees {
                        if callee == initiator {
                            continue;
                        }
                        if let Err(e) = feed
                            .insert(callee, "call_missed", Some(initiator), Some(call_id.0), "Missed call")
                            .await
                        {
                            tracing::warn!(error = ?e, %callee, "missed-call activity insert failed");
                        }
                        // Best-effort mobile push so an offline callee still sees the
                        // missed call. No DND/snooze gate here (a missed call during
                        // DND is still worth surfacing); out-of-band + best-effort, so
                        // it can never fail the call-end relay.
                        if push_enabled {
                            let payload = aero_push::PushPayload {
                                title: format!("Missed call from {initiator_name}"),
                                body: String::new(),
                                room_id: Some(room_id.to_string()),
                                message_id: None,
                                badge: None,
                            };
                            crate::push_bot::push_to_participant(state, callee, &payload).await;
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = ?e, %call_id, "unanswered_callees lookup failed"),
            }
            // Post-call AI recap (Wave 23): if the call left a transcript and an
            // AI backend is wired, summarize what was said and store it onto the
            // call session. Out-of-band + best-effort — never fails the call-end
            // relay; degrades to a heuristic digest when no LLM key is configured.
            if let Some(ai) = &state.ai {
                let transcripts = aero_storage::CallTranscriptRepo::new(state.pg.clone());
                match transcripts.lines(call_id).await {
                    Ok(lines) if !lines.is_empty() => {
                        let joined = lines
                            .iter()
                            .map(|l| format!("{}: {}", l.speaker_id, l.text))
                            .collect::<Vec<_>>()
                            .join("\n");
                        match ai.summarize_text(&joined).await {
                            Ok(recap) if !recap.trim().is_empty() => {
                                if let Err(e) = transcripts.set_recap(call_id, &recap).await {
                                    tracing::warn!(error = ?e, %call_id, "call recap store failed");
                                }
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!(error = %e, %call_id, "call recap summarize failed");
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(error = ?e, %call_id, "call transcript lines lookup failed");
                    }
                }
            }
        }
        ClientFrame::CallCaption { call_id, room_id, text, lang, target_lang, is_final } => {
            let text = text.trim().to_string();
            if text.is_empty() {
                return Ok(());
            }
            // Translate only *final* lines, only when a distinct target language
            // is set and an AI backend is available. Interim lines relay verbatim
            // to keep latency low.
            let (translated, translated_lang) = match (is_final, target_lang.as_deref(), &state.ai) {
                (true, Some(target), Some(ai))
                    if lang.as_deref().map_or(true, |l| !same_lang(l, target)) =>
                {
                    match ai.translate(&text, target).await {
                        Ok(t) if !t.trim().is_empty() => {
                            (Some(t), Some(target.to_string()))
                        }
                        _ => (None, None),
                    }
                }
                _ => (None, None),
            };
            // Persist final caption lines as a durable call transcript (the
            // post-call AI recap is generated on CallEnd from these). Store the
            // translated text when one was produced, else the original. Best-effort
            // — never blocks or fails the low-latency caption relay.
            if is_final {
                let line = translated.as_deref().unwrap_or(text.as_str());
                if let Err(e) = aero_storage::CallTranscriptRepo::new(state.pg.clone())
                    .append(call_id, pid, line)
                    .await
                {
                    tracing::warn!(error = ?e, %call_id, "call transcript append failed");
                }
            }
            state
                .im
                .relay_call_event(
                    room_id,
                    CallEvent::Caption {
                        call_id,
                        room_id,
                        from: pid,
                        text,
                        lang,
                        translated,
                        translated_lang,
                        is_final,
                    },
                )
                .await?;
        }
        ClientFrame::CallJoin { room_id, kind, call_id } => {
            if !state.rooms.is_member(room_id, pid).await? {
                let _ = tx.try_send(Message::Text(
                    serde_json::to_string(&ServerFrame::Error {
                        code: "forbidden",
                        msg: "not a member".into(),
                    })
                    .unwrap_or_default(),
                ));
                return Ok(());
            }
            let call_id = match call_id {
                Some(c) => c,
                None => {
                    let c = CallId::new();
                    // Best-effort session row; the live roster is in the Hub.
                    if let Err(e) =
                        state.calls.start(c, room_id, pid, kind, CallMode::Sfu, &[]).await
                    {
                        warn!(error = ?e, "persist group call session failed");
                    }
                    c
                }
            };
            // Local Hub keeps the roster for per-process mesh delivery; its
            // return value is the fallback set of already-present peers.
            let local_existing = state.hub.call_join(call_id, pid);
            // Redis is the cluster-wide roster (ROADMAP 方向二): register self,
            // then source the "whom to connect to" set from Redis (minus self) so
            // a joiner sees peers connected to *other* nodes too. `join` doubles
            // as the heartbeat restamp. On any Redis error, fall back to the local
            // Hub roster so a single-node call still works.
            if let Err(e) = state.call_roster.join(call_id, pid).await {
                warn!(error = ?e, %call_id, "redis call-roster join failed");
            }
            let existing = call_peers_excluding(state, call_id, pid, local_existing).await;
            // Tell the joiner who is already in the call (whom to connect to).
            state
                .im
                .relay_call_event(
                    room_id,
                    CallEvent::Roster { call_id, to: pid, members: existing, kind },
                )
                .await?;
            // Tell the room a new peer joined.
            state
                .im
                .relay_call_event(room_id, CallEvent::Join { call_id, room_id, from: pid, kind })
                .await?;
            // Additively drive the cross-node orchestrator (ROADMAP4 方向二):
            // register the participant in the SFU router (so the call-route
            // heartbeat has a populated roster) + the cluster CallRouteRegistry,
            // and compute the bridge topology; on a BridgeTo, ensure one bridge
            // per peer node. Best-effort — a failure never disturbs the working
            // full-mesh CallEvent path above, and single-node always serves local.
            match state.call_orchestrator.join_group_call(call_id, room_id, pid, kind).await {
                Ok(join) => {
                    if let aero_live_webrtc::CallTopology::BridgeTo(urls) = join.topology {
                        let spawned = state.call_supervisor.ensure_bridges(call_id, &urls).await;
                        debug!(%call_id, peers = urls.len(), spawned, "cross-node call bridges ensured");
                    }
                }
                Err(e) => {
                    warn!(error = ?e, %call_id, "call orchestrator join failed (full-mesh unaffected)");
                }
            }
        }
        ClientFrame::CallLeave { call_id, room_id } => {
            state.hub.call_leave(call_id, pid);
            if let Err(e) = state.call_roster.leave(call_id, pid).await {
                warn!(error = ?e, %call_id, "redis call-roster leave failed");
            }
            state
                .im
                .relay_call_event(room_id, CallEvent::Leave { call_id, room_id, from: pid })
                .await?;
            // Additively unregister from the cross-node orchestrator (ROADMAP4):
            // drop the SFU + registry mapping; on the last local participant,
            // cancel this call's bridges. Best-effort.
            if state.call_orchestrator.leave_group_call(call_id, pid).await {
                let cancelled = state.call_supervisor.cancel_call(call_id);
                debug!(%call_id, cancelled, "last local call participant left; bridges cancelled");
            }
        }
        ClientFrame::CallOffer { call_id, room_id, to, sdp } => {
            state
                .im
                .relay_call_event(room_id, CallEvent::Offer { call_id, from: pid, to, sdp })
                .await?;
        }
        ClientFrame::WatchStream { stream_id, since } => {
            // Local Hub still tracks watchers for per-process event fan-out...
            state.hub.watch_stream(stream_id, pid);
            // ...while Redis is the cluster-wide source of the viewer COUNT
            // (ROADMAP 方向二). `join` also (re)stamps the heartbeat, so a
            // re-watch keeps the entry alive without a separate keep-alive.
            if let Err(e) = state.stream_viewers.join(stream_id, pid).await {
                warn!(error = ?e, %stream_id, "redis stream-viewer join failed");
            }
            // Replay danmaku so the new watcher has context. With a `since` cursor
            // (re-watch / late join) replay everything strictly newer up to a
            // bounded window; without it, just a small recent tail.
            let since_cursor = since.as_deref().and_then(|c| Ulid::from_string(c.trim()).ok());
            let replay_limit = if since_cursor.is_some() { 200 } else { 30 };
            if let Ok(lines) = state.live.recent_chat_since(stream_id, since_cursor, replay_limit).await {
                let replayed = lines.len();
                let last_id = lines.last().map(|l| l.id);
                for line in lines {
                    let frame = ServerFrame::StreamEvent { event: StreamEvent::Chat(line) };
                    let _ = tx.try_send(Message::Text(
                        serde_json::to_string(&frame).unwrap_or_default(),
                    ));
                }
                // Truncation signal (ROADMAP 第三版 方向一), cursor catch-up
                // only (the no-cursor tail is deliberately bounded context, not
                // a complete replay): a capped catch-up tells the client to
                // continue via REST `GET /api/streams/:id/chat?since=`.
                if since_cursor.is_some() {
                    if let Some(next_since) =
                        truncation_cursor(replayed, replay_limit, last_id)
                    {
                        let frame = serde_json::json!({
                            "type": "backfill",
                            "stream_id": stream_id,
                            "truncated": true,
                            "next_since": next_since,
                        });
                        let _ = tx.try_send(Message::Text(frame.to_string()));
                    }
                }
            }
            // Also replay a small recent-gift tail so a late joiner has gift
            // context too (no cursor: gifts have their own id space, and the
            // `since` cursor above is danmaku-only).
            if let Ok(gifts) = state.live.recent_gifts(stream_id, 10).await {
                for gift in gifts {
                    let frame = ServerFrame::StreamEvent { event: StreamEvent::Gift(gift) };
                    let _ = tx.try_send(Message::Text(
                        serde_json::to_string(&frame).unwrap_or_default(),
                    ));
                }
            }
            let count = stream_viewer_count(state, stream_id).await;
            state.live.publish_viewers(stream_id, count).await;
        }
        ClientFrame::UnwatchStream { stream_id } => {
            state.hub.unwatch_stream(stream_id, pid);
            if let Err(e) = state.stream_viewers.leave(stream_id, pid).await {
                warn!(error = ?e, %stream_id, "redis stream-viewer leave failed");
            }
            let count = stream_viewer_count(state, stream_id).await;
            state.live.publish_viewers(stream_id, count).await;
        }
        ClientFrame::StreamChat { stream_id, body } => {
            // Reject a banned/timed-out poster before the line is accepted/broadcast
            // (mirrors the REST `stream_chat_post` guard).
            if aero_storage::StreamModRepo::new(state.participants.pool().clone())
                .is_banned(stream_id, pid, time::OffsetDateTime::now_utc())
                .await?
            {
                return Err(aero_common::Error::Forbidden(
                    "banned from this stream's chat".into(),
                )
                .into());
            }
            // Enforce Twitch-style chat modes (slow mode / follower-only /
            // subscriber-only) before the line is accepted/broadcast (mirrors the
            // REST `stream_chat_post` guard).
            crate::stream_chat_modes::enforce_chat_modes(state, stream_id, pid).await?;
            // Subscriber-badge flag (migration 0082): mirrors the REST guard.
            let is_sub = state.live.subscriber_flag(stream_id, pid).await;
            state.live.post_chat(pid, stream_id, body, is_sub).await?;
        }
        ClientFrame::StreamGift { stream_id, gift_id, qty, nonce } => {
            let qty = qty.unwrap_or(1);
            let (_, inserted) =
                state.live.send_gift(pid, stream_id, &gift_id, qty, nonce.as_deref()).await?;
            // Feed the gift into the hype train (mirrors the REST gift handler);
            // best-effort, never fails the send. Skip on an idempotent retry so a
            // resend can't double-count the train.
            if inserted {
                crate::hype_train::on_gift(state, stream_id, pid, qty).await;
            }
        }
    }
    Ok(())
}

/// Per-room cap on reconnect backfill replay, so a client that has been away for
/// a long time can't make a single connection replay an unbounded history (it
/// can keep paging via the REST `?since=` route). Matches the keyset page window
/// `MessageRepo::list_since` clamps to.
const BACKFILL_PER_ROOM_LIMIT: i64 = 200;

/// Parse a best-effort reconnect/resume cursor. `None` (absent) and any value
/// that fails to decode as a [`MessageId`] both yield `None` — backfill is
/// purely additive, so a bad cursor must degrade to "no backfill" rather than
/// fail the connection. Pure + total, so it unit-tests without any I/O.
#[must_use]
fn parse_resume_cursor(raw: Option<&str>) -> Option<MessageId> {
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
fn truncation_cursor<T: Copy>(replayed: usize, limit: i64, last: Option<T>) -> Option<T> {
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
fn authoritative_count(redis: anyhow::Result<u64>, local_fallback: u32) -> u32 {
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
fn backfill_room_ids(rooms: &[aero_common::Room]) -> Vec<RoomId> {
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
        let missed = match state.messages.list_since(room, cursor, BACKFILL_PER_ROOM_LIMIT).await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = ?e, %room, "reconnect backfill: list_since failed");
                continue;
            }
        };
        let room_count = missed.len();
        let last_id = missed.last().map(|m| m.id);
        for message in missed {
            let frame = ServerFrame::Message { message };
            let json = serde_json::to_string(&frame).unwrap_or_default();
            tokio::select! {
                biased;
                // Connection is going away — stop replaying.
                () = close.cancelled() => return,
                res = tx.send(Message::Text(json)) => {
                    if res.is_err() {
                        return; // receiver gone
                    }
                }
            }
            replayed += 1;
        }
        // Truncation signal (ROADMAP 第三版 方向一): a replay that hit the
        // per-room cap may have left newer messages behind. Tell the client
        // explicitly so it continues via REST
        // `GET /api/rooms/:id/messages?since=<next_since>` instead of silently
        // missing the remainder.
        if let Some(next_since) = truncation_cursor(room_count, BACKFILL_PER_ROOM_LIMIT, last_id) {
            let frame = serde_json::json!({
                "type": "backfill",
                "room_id": room,
                "truncated": true,
                "next_since": next_since,
            });
            tokio::select! {
                biased;
                () = close.cancelled() => return,
                res = tx.send(Message::Text(frame.to_string())) => {
                    if res.is_err() {
                        return; // receiver gone
                    }
                }
            }
        }
    }
    if replayed > 0 {
        debug!(%pid, replayed, "reconnect backfill replayed missed messages");
    }
}

/// Backoff between bus resubscribe attempts after the subscription stream ends
/// (NATS reconnect/drop) or a subscribe call fails. Short — fan-out is offline
/// until we reconnect — but non-zero so a hard-down NATS can't spin a tight loop.
const BUS_RESUBSCRIBE_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

/// Background loop that subscribes to `im.room.*` and pushes each [`RoomEvent`]
/// into the local Hub. Started once per process at boot; runs until the process
/// exits, resubscribing across NATS reconnects (see [`BUS_RESUBSCRIBE_BACKOFF`]).
pub async fn run_bus_listener(state: AppState) -> anyhow::Result<()> {
    use aero_bus::EventBus;
    let bus: Arc<dyn EventBus> = state.bus.clone();
    // Resubscribe loop: a NATS reconnect/drop ends the subscription stream. Without
    // this outer loop the function would return and the boot-time task would exit,
    // silently stopping room-event fan-out on this process *forever*. The durable
    // consumer ("aero-server") resumes from its committed cursor on resubscribe, so
    // acked events are not re-sent and unacked ones are redelivered (at-least-once).
    loop {
        let mut stream = match bus.subscribe("im.room.*", Some("aero-server")).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "im.room.* subscribe failed; retrying");
                tokio::time::sleep(BUS_RESUBSCRIBE_BACKOFF).await;
                continue;
            }
        };
        info!("bus listener started");
        while let Some(sub) = stream.next().await {
            handle_room_event_sub(&state, sub).await;
        }
        warn!("im.room.* subscription stream ended; resubscribing");
        tokio::time::sleep(BUS_RESUBSCRIBE_BACKOFF).await;
    }
}

/// Process one `im.room.*` bus message: lift the publish-time `seq` stamp, decode
/// the [`RoomEvent`] (with a legacy `MessageEnvelope` fallback), fan it out to the
/// local Hub, and ack — or nack an undecodable payload.
async fn handle_room_event_sub(state: &AppState, sub: Box<dyn aero_bus::Subscription + Send>) {
    // Parse the raw JSON first so the publish-time `"seq"` stamp (ROADMAP
    // 第三版 方向一) can be lifted off the payload — `RoomEvent`'s serde
    // deserialization ignores the unknown key, so the stamp must be read
    // before the typed decode. Legacy unstamped payloads yield `None`.
    let parsed: serde_json::Result<(RoomEvent, Option<u64>)> =
        serde_json::from_slice::<serde_json::Value>(sub.payload()).and_then(|value| {
            let seq = aero_bus::extract_seq(&value);
            Ok((serde_json::from_value::<RoomEvent>(value)?, seq))
        });
    match parsed {
        Ok((event, seq)) => {
            // NotifyBatch (ROADMAP 方向二) is published once for the whole
            // recipient set; expand it here into one targeted `notify` frame
            // per recipient so each client receives an ordinary frame and
            // never sees the others. Equivalent to the old per-recipient
            // Notify publishes, minus the O(N) NATS traffic.
            if let RoomEvent::NotifyBatch { room_id, message_id, by, recipients } = &event {
                for target in recipients {
                    let frame = room_event_to_frame_json(
                        &RoomEvent::Notify {
                            room_id: *room_id,
                            message_id: *message_id,
                            mentioned: target.participant,
                            by: *by,
                            kind: target.kind,
                        },
                        seq,
                    );
                    state.hub.fan_out_raw(&[target.participant], &frame);
                }
                let _ = sub.ack().await;
                return;
            }
            let room = event.room_id();
            let recipients = match event.explicit_recipients() {
                list if !list.is_empty() => list,
                _ => {
                    if let Some(rid) = room {
                        state.rooms.members(rid).await.unwrap_or_default()
                    } else {
                        Vec::new()
                    }
                }
            };
            // Message-throughput counter (ROADMAP 方向四): the bus is the single
            // source of truth for accepted messages — every chat message (REST
            // or WS) lands here exactly once before fan-out — so counting only
            // `Message` events here is the non-double-counting choke point
            // (other RoomEvent variants are edits/reactions/typing, not new msgs).
            if matches!(event, RoomEvent::Message(_)) {
                metrics::inc_counter(names::MESSAGES_SENT_TOTAL, 1);
            }
            let frame = room_event_to_frame_json(&event, seq);
            state.hub.fan_out_raw(&recipients, &frame);
            let _ = sub.ack().await;
        }
        Err(e) => {
            // Compatibility: accept the legacy raw MessageEnvelope payload too.
            if let Ok(env) = serde_json::from_slice::<aero_common::MessageEnvelope>(sub.payload()) {
                let recipients = if env.recipients.is_empty() {
                    state.rooms.members(env.message.room_id).await.unwrap_or_default()
                } else {
                    env.recipients.clone()
                };
                // Legacy envelope path also carries exactly one new message.
                metrics::inc_counter(names::MESSAGES_SENT_TOTAL, 1);
                let frame = serde_json::json!({
                    "type": "message",
                    "message": env.message,
                });
                state.hub.fan_out_raw(&recipients, &frame.to_string());
                let _ = sub.ack().await;
                return;
            }
            // Undecodable by any known schema (typed RoomEvent *and* legacy
            // envelope both failed). That's a deterministic failure on the raw
            // bytes — redelivery will never succeed — so ACK-drop it rather than
            // nack, otherwise the durable consumer redelivers this poison message
            // forever. The counter flags a producer/schema mismatch to alert on.
            warn!(error = ?e, "bad envelope on bus — dropping (poison)");
            metrics::inc_counter(names::BUS_POISON_DROPPED_TOTAL, 1);
            let _ = sub.ack().await;
        }
    }
}

/// Loose BCP-47 comparison on the primary subtag, so `en` and `en-US` are the
/// same language and the server skips a no-op translation.
fn same_lang(a: &str, b: &str) -> bool {
    let primary = |s: &str| s.split(['-', '_']).next().unwrap_or(s).to_ascii_lowercase();
    primary(a) == primary(b)
}

#[cfg(test)]
mod tests {
    use super::{
        authoritative_count, backfill_room_ids, parse_resume_cursor, same_lang,
        stamped_frame_json, truncation_cursor, BACKFILL_PER_ROOM_LIMIT, ClientFrame, ServerFrame,
    };
    use aero_common::{MessageId, ParticipantId, Room, RoomId, RoomKind};
    use ulid::Ulid;

    #[test]
    fn watch_stream_frame_carries_optional_since_cursor() {
        // Legacy client (no `since`) → None, so the replay behaviour is unchanged.
        let without: ClientFrame = serde_json::from_str(
            r#"{"type":"watch_stream","stream_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV"}"#,
        )
        .expect("parse watch_stream without since");
        match without {
            ClientFrame::WatchStream { since, .. } => assert!(since.is_none()),
            _ => panic!("expected WatchStream"),
        }
        // With `since` → carried through so the watch handler can replay catch-up.
        let with: ClientFrame = serde_json::from_str(
            r#"{"type":"watch_stream","stream_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","since":"01ARZ3NDEKTSV4RRFFQ69G5FZZ"}"#,
        )
        .expect("parse watch_stream with since");
        match with {
            ClientFrame::WatchStream { since, .. } => {
                assert_eq!(since.as_deref(), Some("01ARZ3NDEKTSV4RRFFQ69G5FZZ"));
            }
            _ => panic!("expected WatchStream"),
        }
    }

    #[test]
    fn same_lang_matches_on_primary_subtag() {
        assert!(same_lang("en", "en-US"));
        assert!(same_lang("zh-CN", "zh-Hans"));
        assert!(same_lang("EN", "en"));
        assert!(!same_lang("en", "zh"));
        assert!(!same_lang("zh-CN", "en-US"));
    }

    #[test]
    fn resume_cursor_is_best_effort() {
        // Absent and malformed cursors both degrade to "no backfill" (None), never
        // an error — backfill is purely additive and must not fail the upgrade.
        assert!(parse_resume_cursor(None).is_none());
        assert!(parse_resume_cursor(Some("")).is_none());
        assert!(parse_resume_cursor(Some("not-a-ulid")).is_none());
        // A valid id round-trips, with surrounding whitespace tolerated.
        let id = MessageId::new();
        assert_eq!(parse_resume_cursor(Some(&id.to_string())), Some(id));
        assert_eq!(parse_resume_cursor(Some(&format!("  {id}  "))), Some(id));
    }

    #[test]
    fn authoritative_count_prefers_redis_then_falls_back() {
        // Redis Ok is authoritative even when it disagrees with the local count.
        assert_eq!(authoritative_count(Ok(7), 3), 7);
        // Redis error ⇒ fall back to the process-local count (no worse than today).
        assert_eq!(authoritative_count(Err(anyhow::anyhow!("down")), 3), 3);
        // A count that overflows u32 saturates rather than wrapping/panicking.
        assert_eq!(authoritative_count(Ok(u64::from(u32::MAX) + 1), 0), u32::MAX);
        // Zero from Redis is honored (e.g. last viewer just left, cluster-wide).
        assert_eq!(authoritative_count(Ok(0), 9), 0);
    }

    fn room_with_id(id: RoomId) -> Room {
        Room {
            id,
            kind: RoomKind::Group,
            name: None,
            created_by: ParticipantId::new(),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn backfill_selects_every_room_id_in_order() {
        // Empty membership ⇒ nothing to replay.
        assert!(backfill_room_ids(&[]).is_empty());
        // Otherwise: exactly the ids of every room the participant belongs to,
        // order-preserving (so replay follows the membership listing order).
        let a = RoomId::new();
        let b = RoomId::new();
        let c = RoomId::new();
        let rooms = [room_with_id(a), room_with_id(b), room_with_id(c)];
        assert_eq!(backfill_room_ids(&rooms), vec![a, b, c]);
    }

    #[test]
    fn backfill_per_room_limit_matches_keyset_window() {
        // The replay cap equals the storage keyset clamp ceiling, so a single
        // reconnect never over-replays beyond one page per room.
        assert_eq!(BACKFILL_PER_ROOM_LIMIT, 200);
        // Sanity: it is a usable id-orderable cursor type (compile-time check that
        // the backfill path keys on a time-sortable MessageId/Ulid).
        let _ = Ulid::new();
    }

    #[test]
    fn truncation_cursor_fires_only_when_replay_hits_the_cap() {
        let last = MessageId::new();
        // Under the cap (including zero rows): complete replay, no signal.
        assert_eq!(truncation_cursor(0, 200, Some(last)), None);
        assert_eq!(truncation_cursor(199, 200, Some(last)), None);
        // Exactly at the cap: possibly cut short → continue from the last id.
        assert_eq!(truncation_cursor(200, 200, Some(last)), Some(last));
        // Defensive: above the cap still signals (storage clamps, but a future
        // limit change must fail safe toward "tell the client to continue").
        assert_eq!(truncation_cursor(201, 200, Some(last)), Some(last));
        // No last id (empty replay) can never produce a cursor.
        assert_eq!(truncation_cursor::<MessageId>(200, 200, None), None);
    }

    #[test]
    fn stamped_frame_carries_optional_top_level_seq() {
        let frame = ServerFrame::Typing {
            room_id: RoomId::new(),
            participant: ParticipantId::new(),
            on: true,
        };
        // With a seq: stamped as a top-level key next to `type`.
        let with: serde_json::Value =
            serde_json::from_str(&stamped_frame_json(&frame, Some(9))).expect("valid json");
        assert_eq!(with.get("type").and_then(|v| v.as_str()), Some("typing"));
        assert_eq!(with.get("seq").and_then(serde_json::Value::as_u64), Some(9));
        // Without: byte-shape identical to the legacy frame (no `seq` key), so
        // unstamped events pass through unchanged.
        let without: serde_json::Value =
            serde_json::from_str(&stamped_frame_json(&frame, None)).expect("valid json");
        assert!(without.get("seq").is_none());
    }
}

/// Background loop that subscribes to `live.stream.*` and pushes each
/// [`StreamEvent`] to local watchers of that stream. Uses an *ephemeral*
/// consumer: live interactivity is broadcast (every instance must see every
/// event to fan out to its own watchers) and a few dropped danmaku across a
/// restart are immaterial. Started once per process at boot.
pub async fn run_live_bus_listener(state: AppState) -> anyhow::Result<()> {
    use aero_bus::EventBus;
    let bus: Arc<dyn EventBus> = state.bus.clone();
    // Resubscribe across NATS reconnects — see `run_bus_listener`. The consumer is
    // ephemeral by design (live interactivity is broadcast; a few dropped danmaku
    // across a reconnect are immaterial), but the *loop* itself must survive so this
    // process keeps fanning StreamEvents to its local watchers instead of going dark.
    loop {
        let mut stream = match bus.subscribe("live.stream.*", None).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "live.stream.* subscribe failed; retrying");
                tokio::time::sleep(BUS_RESUBSCRIBE_BACKOFF).await;
                continue;
            }
        };
        info!("live bus listener started");
        while let Some(sub) = stream.next().await {
            // Same two-phase decode as `run_bus_listener`: lift the publish-time
            // `"seq"` stamp off the raw JSON before the typed decode drops it.
            let parsed: serde_json::Result<(StreamEvent, Option<u64>)> =
                serde_json::from_slice::<serde_json::Value>(sub.payload()).and_then(|value| {
                    let seq = aero_bus::extract_seq(&value);
                    Ok((serde_json::from_value::<StreamEvent>(value)?, seq))
                });
            match parsed {
                Ok((event, seq)) => {
                    let watchers = state.hub.stream_watchers(event.stream_id());
                    if !watchers.is_empty() {
                        let frame = ServerFrame::StreamEvent { event };
                        let json = stamped_frame_json(&frame, seq);
                        state.hub.fan_out_raw(&watchers, &json);
                    }
                    let _ = sub.ack().await;
                }
                Err(e) => {
                    // Poison payload (see run_bus_listener): ack-drop, never nack —
                    // an undecodable StreamEvent will never decode on redelivery.
                    warn!(error = ?e, "bad StreamEvent on bus — dropping (poison)");
                    metrics::inc_counter(names::BUS_POISON_DROPPED_TOTAL, 1);
                    let _ = sub.ack().await;
                }
            }
        }
        warn!("live.stream.* subscription stream ended; resubscribing");
        tokio::time::sleep(BUS_RESUBSCRIBE_BACKOFF).await;
    }
}

/// Serialize a server frame, carrying `seq` through as an optional top-level
/// key (omitted when `None`, so legacy events/clients are byte-identical to
/// before). The seq lets the browser dedup at-least-once redeliveries and
/// order events that carry no id of their own (ROADMAP 第三版 方向一).
fn stamped_frame_json(frame: &ServerFrame<'_>, seq: Option<u64>) -> String {
    match serde_json::to_value(frame) {
        Ok(mut value) => {
            aero_bus::stamp_seq(&mut value, seq);
            value.to_string()
        }
        Err(_) => "{\"type\":\"error\",\"code\":\"serialize\",\"msg\":\"\"}".into(),
    }
}

/// Translate a `RoomEvent` into the JSON wire frame the browser expects,
/// carrying the bus payload's `seq` stamp (if any) onto the frame.
fn room_event_to_frame_json(event: &RoomEvent, seq: Option<u64>) -> String {
    let frame: ServerFrame<'_> = match event.clone() {
        RoomEvent::Message(env) => ServerFrame::Message { message: env.message },
        RoomEvent::Edited(m) => ServerFrame::Edited { message: m },
        RoomEvent::Deleted { room_id, message_id, by } => {
            ServerFrame::Deleted { room_id, message_id, by }
        }
        RoomEvent::Reaction { room_id, message_id, participant, emoji, op } => {
            ServerFrame::Reaction { room_id, message_id, participant, emoji, op }
        }
        RoomEvent::Read { room_id, participant, last_message_id, at } => {
            ServerFrame::Read { room_id, participant, last_message_id, at }
        }
        RoomEvent::Typing { room_id, participant, on } => {
            ServerFrame::Typing { room_id, participant, on }
        }
        RoomEvent::Notify { room_id, message_id, mentioned, by, kind } => {
            ServerFrame::Notify { room_id, message_id, mentioned, by, notify_kind: kind }
        }
        // Unreachable in practice: the bus listener expands NotifyBatch into one
        // per-recipient Notify frame before calling this. Mapped defensively to
        // the first recipient's Notify so the conversion stays total.
        RoomEvent::NotifyBatch { room_id, message_id, by, recipients } => {
            let (mentioned, notify_kind) = recipients
                .first()
                .map_or((by, NotificationKind::Mention), |t| (t.participant, t.kind));
            ServerFrame::Notify { room_id, message_id, mentioned, by, notify_kind }
        }
        RoomEvent::Pin { room_id, message_id, by, op } => {
            ServerFrame::Pin { room_id, message_id, by, op }
        }
        RoomEvent::Membership { room_id, participant, op } => {
            ServerFrame::Membership { room_id, participant, op }
        }
        RoomEvent::Call(call) => ServerFrame::Call { event: call },
        RoomEvent::Poll { room_id, poll_id, op } => ServerFrame::Poll { room_id, poll_id, op },
        RoomEvent::MessageSeen { room_id, message_id, participant } => {
            ServerFrame::MessageSeen { room_id, message_id, participant }
        }
        RoomEvent::Interaction { room_id, message_id, participant, action_id } => {
            ServerFrame::Interaction { room_id, message_id, participant, action_id }
        }
    };
    stamped_frame_json(&frame, seq)
}
