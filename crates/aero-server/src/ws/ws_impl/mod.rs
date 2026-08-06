//! WebSocket endpoint and protocol.
//!
//! Wire format is JSON; all frames are tagged with `type`. See the design spec
//! for the full message set. Non-browser clients should use an Authorization
//! bearer; browser clients retain the `?token=...` compatibility fallback
//! because the WebSocket constructor cannot set custom headers.
//!
//! ## Reconnect backfill protocol (ROADMAP 方向五)
//!
//! A client that dropped its socket can avoid silently losing messages by
//! Modern clients opt into `/ws?...&cursors=1`: each effective current room
//! resumes from its participant/room delivery cursor, while a room with no
//! cursor receives only the newest bounded history page (older history remains
//! available through ordinary REST `before` pagination). The server then emits
//! `delivery_ready`, after which a client may persist/send `delivery_ack`.
//!
//! Legacy clients can reconnect with `since=<message_id>` and replay each room
//! forward from that global id, oldest-first and capped per room. When both are
//! present, cursor mode wins and `since` remains a rolling-downgrade fallback
//! for an older server. Malformed cursors and replay failures never reject the
//! WebSocket upgrade.
use crate::hub::WsSender;
use crate::state::AppState;
use aero_auth::{Claims, TokenKind};
use aero_common::metrics::{self, names};
use aero_common::{
    Block, CallEvent, CallId, CallKind, CallMode, CallSession, CanvasId, MembershipOp, MessageId,
    NotificationKind, ParticipantId, PinOp, PollId, PollOp, ReactionOp, RoomId, SessionId,
    SfuPublisherDescription, SfuSubscription, StreamEvent,
};
use axum::{
    extract::{
        rejection::QueryRejection,
        ws::{CloseFrame, Message, WebSocketUpgrade},
        ConnectInfo, Query, State,
    },
    http::HeaderMap,
    response::IntoResponse,
};
use credentials::{select_access_token, RedactedAccessToken};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fmt, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};
use ulid::Ulid;

#[derive(Deserialize)]
pub struct WsParams {
    /// Legacy browser fallback. This is deliberately a redacting newtype rather
    /// than `String`, so query bearer material cannot leak through `Debug`.
    #[serde(default)]
    token: Option<RedactedAccessToken>,
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
    /// (`1`/`true`/`yes`), the server resumes each
    /// room from this participant's persisted DELIVERY cursor (multi-device-shared)
    /// instead of a single global cursor. Cursor mode wins when both parameters
    /// are present, allowing a modern browser to keep `since` as a rolling-
    /// downgrade fallback for legacy servers. A free-form string (not
    /// `Option<bool>`) lets clients send `1` without 400-ing the upgrade.
    #[serde(default)]
    cursors: Option<String>,
}

// Browser clients still present their access token in the WebSocket query
// string. Keep a redacted Debug implementation as defence in depth: even if
// this value is accidentally attached to a future trace/event, the bearer
// credential can never be formatted into logs.
impl fmt::Debug for WsParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsParams")
            .field("token", &"[REDACTED]")
            .field("since", &self.since)
            .field("summarize", &self.summarize)
            .field("cursors", &self.cursors)
            .finish()
    }
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ClientFrame {
    /// Subscribe local presence for a room.
    JoinRoom {
        room_id: RoomId,
    },
    /// Send a new message.
    SendMessage {
        room_id: RoomId,
        blocks: Vec<Block>,
        /// Stable sender-generated UUID for retry-safe persistence.
        #[serde(default)]
        client_message_id: Option<uuid::Uuid>,
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
        client_message_id: Option<uuid::Uuid>,
        #[serde(default)]
        reply_to: Option<MessageId>,
        /// Optional sender-set TTL in seconds. If > 0, the message is
        /// hard-deleted by the ephemeral sweep once this many seconds elapse.
        #[serde(default)]
        expires_after_secs: Option<u64>,
    },
    /// Edit an existing message (sender only). `expected_version` is the
    /// optimistic-lock check (migration 0157) — see `EditMessageReq` in
    /// `routes.rs` for the equivalent REST contract.
    EditMessage {
        id: MessageId,
        blocks: Vec<Block>,
        #[serde(default)]
        expected_version: Option<i32>,
    },
    /// Soft-delete a message.
    DeleteMessage {
        id: MessageId,
    },
    /// Recall (撤回) a message: the author or a room admin/owner replaces its
    /// content with the system placeholder and broadcasts the updated message.
    RecallMessage {
        id: MessageId,
    },
    /// Toggle a reaction.
    React {
        message_id: MessageId,
        emoji: String,
    },
    /// Mark a room read up to the given message.
    MarkRead {
        room_id: RoomId,
        last_message_id: MessageId,
    },
    /// Acknowledge durable receipt of a message (ROADMAP 方向三·A). Distinct from
    /// `MarkRead` (visual "seen"): this advances the per-room DELIVERY cursor — the
    /// Last-Known-Good point reconnect resumes each room from. Monotonic on the
    /// transactional `delivery_ordinal`; `seq` is retained only as a diagnostic
    /// bus dedup high-water mark. Database-backfill frames use `seq = 0`.
    /// Best-effort: a failed persist never tears down the socket.
    DeliveryAck {
        room_id: RoomId,
        message_id: MessageId,
        /// Durable per-room creation order. Required for cursor v2; an omitted
        /// value is a legacy ACK and is deliberately ignored rather than
        /// reviving the unsafe `MAX(message_id)` cursor.
        #[serde(default)]
        delivery_ordinal: Option<i64>,
        seq: i64,
    },
    /// Best-effort typing indicator.
    Typing {
        room_id: RoomId,
        on: bool,
    },
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
    CallLeave {
        call_id: CallId,
        room_id: RoomId,
    },
    /// A per-pair mesh offer to one peer in a group call.
    CallOffer {
        call_id: CallId,
        room_id: RoomId,
        to: ParticipantId,
        sdp: String,
    },
    /// Negotiate a browser media leg directly with the server SFU.
    ///
    /// This is deliberately distinct from `CallOffer`, which remains a
    /// peer-to-peer mesh relay for backward compatibility.
    CallSfuOffer {
        call_id: CallId,
        room_id: RoomId,
        sdp: String,
    },
    /// Trickle a browser ICE candidate into its server-owned SFU session.
    CallSfuIce {
        call_id: CallId,
        room_id: RoomId,
        session_generation: u64,
        candidate: serde_json::Value,
    },
    /// Install explicit publisher-track → outbound-transceiver routes after
    /// negotiating enough receive-capable media sections.
    CallSfuSubscribe {
        call_id: CallId,
        room_id: RoomId,
        session_generation: u64,
        revision: u64,
        tracks: Vec<SfuSubscription>,
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
    UnwatchStream {
        stream_id: Ulid,
    },
    /// Post a danmaku line on a stream.
    StreamChat {
        stream_id: Ulid,
        body: String,
    },
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
#[derive(Debug, Clone, Serialize)]
pub(crate) struct DeliveryRoomBarrier {
    pub room_id: RoomId,
    pub delivery_ordinal: i64,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ServerFrame<'a> {
    Welcome {
        participant: ParticipantId,
        capabilities: &'static [&'static str],
    },
    Presence {
        room_id: RoomId,
        online: Vec<ParticipantId>,
    },
    Message {
        message: aero_common::Message,
        #[serde(skip_serializing_if = "Option::is_none")]
        delivery_ordinal: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_message_id: Option<uuid::Uuid>,
    },
    MessageAck {
        client_message_id: uuid::Uuid,
        message: aero_common::Message,
        deduplicated: bool,
    },
    MessageNack {
        client_message_id: uuid::Uuid,
        code: &'a str,
        msg: String,
        retryable: bool,
    },
    /// All reconnect backfill frames have been queued. Cursor-aware clients
    /// must not persist/ACK an interleaved live message before this barrier,
    /// otherwise a disconnect could advance past older frames still in replay.
    DeliveryReady {
        rooms: Vec<DeliveryRoomBarrier>,
    },
    Edited {
        message: aero_common::Message,
    },
    /// A message was recalled (撤回): content replaced by the system placeholder.
    /// Carries the full updated message so clients render the placeholder in
    /// place, mirroring [`Self::Edited`].
    Recalled {
        message: aero_common::Message,
    },
    Deleted {
        room_id: RoomId,
        message_id: MessageId,
        by: ParticipantId,
    },
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
    Typing {
        room_id: RoomId,
        participant: ParticipantId,
        on: bool,
    },
    /// You were mentioned / replied to (targeted to the recipient only).
    Notify {
        room_id: RoomId,
        message_id: MessageId,
        mentioned: ParticipantId,
        by: ParticipantId,
        notify_kind: NotificationKind,
    },
    /// A message was pinned/unpinned in a room (fans out to all members).
    Pin {
        room_id: RoomId,
        message_id: MessageId,
        by: ParticipantId,
        op: PinOp,
    },
    /// A participant joined/left a channel (fans out to all room members).
    Membership {
        room_id: RoomId,
        participant: ParticipantId,
        op: MembershipOp,
    },
    Call {
        event: CallEvent,
    },
    /// Server SDP answer for a `call_sfu_offer`.
    CallSfuAnswer {
        call_id: CallId,
        sdp: String,
        /// Reachable host candidate selected for this media session.
        local_addr: String,
        /// Media IDs accepted from the offer. v1 routing expects sendrecv peers
        /// to use matching mids for their outbound transceivers.
        mids: Vec<String>,
        /// Changes on every accepted offer, even when topology revision does
        /// not. ICE/subscription commands must echo this value.
        session_generation: u64,
        revision: u64,
        publishers: Vec<SfuPublisherDescription>,
        required_recv_slots: usize,
    },
    /// Confirms that a trickled candidate was parsed and applied by the
    /// single-owner media task.
    CallSfuIceAck {
        call_id: CallId,
        session_generation: u64,
    },
    /// Publisher topology changed and the browser must (re)offer enough
    /// receive-capable transceivers before replacing its route plan.
    CallSfuRenegotiate {
        call_id: CallId,
        revision: u64,
        publishers: Vec<SfuPublisherDescription>,
        required_recv_slots: usize,
    },
    /// Confirms the explicit route plan was installed at `revision`.
    CallSfuSubscribed {
        call_id: CallId,
        session_generation: u64,
        revision: u64,
    },
    /// A poll was created/voted/closed in a room (fans out to all members so the
    /// tally stays live).
    Poll {
        room_id: RoomId,
        poll_id: PollId,
        op: PollOp,
    },
    /// One committed append to a canvas's durable operation log. `op_seq`
    /// orders the per-canvas recovery stream; the frame's separately stamped
    /// `seq` orders all room-bus events.
    CanvasOp {
        room_id: RoomId,
        canvas_id: CanvasId,
        op_id: uuid::Uuid,
        op_seq: i64,
        author_id: ParticipantId,
        op: serde_json::Value,
    },
    /// A participant acknowledged seeing a specific message ("Seen by …"; fans
    /// out to all members so the per-message read indicator stays live).
    MessageSeen {
        room_id: RoomId,
        message_id: MessageId,
        participant: ParticipantId,
    },
    /// A participant clicked a Button / picked a Select option on an interactive
    /// message block (fans out to all members so the poster's bot/app sees it live).
    Interaction {
        room_id: RoomId,
        message_id: MessageId,
        participant: ParticipantId,
        action_id: String,
    },
    /// Per-stream interactivity event (danmaku/gift/viewers/status).
    StreamEvent {
        event: StreamEvent,
    },
    Error {
        code: &'a str,
        msg: String,
    },
    Pong,
}

const WS_CAPABILITIES: &[&str] = &[
    "message_ack_v1",
    "delivery_cursor_v2",
    "call_sfu_v1",
    "call_sfu_v2",
];
const SESSION_RECHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
const SERVER_SHUTDOWN_CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

fn server_shutdown_close_frame() -> Message {
    Message::Close(Some(CloseFrame {
        code: 1001,
        reason: std::borrow::Cow::Borrowed("server shutdown"),
    }))
}

pub(crate) fn access_participant(claims: &Claims) -> aero_common::Result<ParticipantId> {
    if claims.kind != TokenKind::Access {
        return Err(aero_common::Error::Unauthorized(
            "websocket requires an access token".into(),
        ));
    }
    if claims.session_id()?.is_none() {
        return Err(aero_common::Error::Unauthorized(
            "websocket access token is missing a session id".into(),
        ));
    }
    claims.participant_id()
}

fn invalid_ws_query_response() -> axum::response::Response {
    (
        axum::http::StatusCode::BAD_REQUEST,
        "invalid websocket query",
    )
        .into_response()
}

// `p` may contain the query bearer, while `headers` may contain Authorization,
// OIDC flow cookies, or proxy credentials. Do not let the generated span capture
// any handler argument.
#[instrument(name = "ws_handshake", skip_all)]
pub async fn handler(
    ws: WebSocketUpgrade,
    query: Result<Query<WsParams>, QueryRejection>,
    State(state): State<AppState>,
    connect_info: Option<ConnectInfo<std::net::SocketAddr>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Capture Query's rejection so its detailed serde error (which is allowed to
    // contain request data) is never rendered or logged by Axum.
    let Ok(Query(p)) = query else {
        return invalid_ws_query_response();
    };
    let access_token = match select_access_token(&headers, p.token) {
        Ok(token) => token,
        Err(error) => {
            warn!(reason = %error, "ws credential rejected");
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                "invalid websocket credential",
            )
                .into_response();
        }
    };
    let Ok(claims) = state.auth.verify_access(access_token.expose()).await else {
        warn!("ws auth failed");
        return (axum::http::StatusCode::UNAUTHORIZED, "invalid token").into_response();
    };
    let pid: ParticipantId = match access_participant(&claims) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "ws access-token check failed");
            return (axum::http::StatusCode::UNAUTHORIZED, "invalid access token").into_response();
        }
    };
    // `/ws` is global and bus fan-out can deliver events before a client sends a
    // room-scoped frame. Enforce the intersection of every restricted workspace
    // membership at the handshake, rather than relying on URL parsing or a later
    // `join_room` guard that would be too late to prevent passive data leakage.
    let client_ip = crate::rate_limit::client_ip(&headers, connect_info.map(|info| info.0));
    if let Err(error) =
        crate::ip_allowlist::assert_participant_network_access(&state, pid, client_ip).await
    {
        warn!(%pid, %client_ip, %error, "ws authorized-network check failed");
        return (
            axum::http::StatusCode::FORBIDDEN,
            "client IP is not authorized for this account's workspaces",
        )
            .into_response();
    }
    let session_id = match claims.session_id() {
        Ok(session_id) => session_id,
        Err(e) => {
            warn!(error = %e, "ws session-id check failed");
            return (axum::http::StatusCode::UNAUTHORIZED, "invalid access token").into_response();
        }
    };
    let access_exp = claims.exp;
    // Best-effort reconnect cursor: a garbage value is simply ignored (no
    // backfill) rather than rejecting the upgrade. See module-level protocol docs.
    let since = parse_resume_cursor(p.since.as_deref());
    let summarize = p.summarize.unwrap_or(false);
    let use_cursors = matches!(
        p.cursors.as_deref().map(str::trim),
        Some("1" | "true" | "yes" | "on")
    );
    ws.on_upgrade(move |socket| {
        run_socket(
            socket,
            state,
            pid,
            session_id,
            access_exp,
            since,
            summarize,
            use_cursors,
        )
    })
}
#[instrument(skip(socket, state, since), fields(%pid))]
// Internal handler with a fixed signature; grouping params into a struct would
// churn every call site for no behavioral gain.
#[allow(clippy::too_many_arguments)]
async fn run_socket(
    socket: axum::extract::ws::WebSocket,
    state: AppState,
    pid: ParticipantId,
    session_id: Option<SessionId>,
    access_exp: u64,
    since: Option<MessageId>,
    summarize: bool,
    use_cursors: bool,
) {
    let (mut sender, mut receiver) = socket.split();
    // `tx` is the ordered socket-output queue. Hub traffic is deliberately
    // registered on a separate bounded queue (`live_tx`) until reconnect
    // backfill has crossed its DeliveryReady barrier. Otherwise a NATS event can
    // race the async history queries and reach the browser ahead of older replay
    // frames, letting the browser persist an ACK past content it has not applied.
    //
    // Both queues are bounded: a slow/stalled client (or an exceptionally busy
    // room while backfill is running) can never grow memory without limit. Hub's
    // existing full-queue policy still drops/evicts through the shared `close`.
    let (tx, mut rx) = mpsc::channel::<Message>(state.ws_config.send_queue_capacity);
    let (live_tx, mut live_rx) = mpsc::channel::<Message>(state.ws_config.send_queue_capacity);
    let close = CancellationToken::new();
    let registered = match session_id {
        Some(session_id) => WsSender::for_session(live_tx, close.clone(), session_id),
        None => WsSender::new(live_tx, close.clone()),
    };
    state.hub.register(pid, registered.clone());
    let now = u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp()).unwrap_or(0);
    let expires_in = std::time::Duration::from_secs(access_exp.saturating_sub(now));
    let expiry = {
        let close = close.clone();
        tokio::spawn(async move {
            tokio::time::sleep(expires_in).await;
            debug!(%pid, "ws access token expired");
            close.cancel();
        })
    };
    let session_watchdog = session_id.map(|session_id| {
        let sessions = aero_storage::SessionRepo::new(state.pg.clone());
        let close = close.clone();
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(SESSION_RECHECK_INTERVAL);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // `interval`'s first tick is immediate. Authentication just checked
            // this row, so consume it and wait a full period before re-querying.
            ticks.tick().await;
            loop {
                tokio::select! {
                    () = close.cancelled() => break,
                    _ = ticks.tick() => {
                        match sessions.is_active(session_id, pid).await {
                            Ok(true) => {}
                            Ok(false) => {
                                debug!(%pid, %session_id, "ws session revoked");
                                close.cancel();
                                break;
                            }
                            Err(e) => {
                                // Fail open on transient DB errors; the next tick
                                // retries, while explicit local revocation still
                                // closes immediately through the Hub.
                                warn!(error = ?e, %pid, %session_id, "ws session check failed");
                            }
                        }
                    }
                }
            }
        })
    });
    let _ = tx.try_send(Message::Text(
        serde_json::to_string(&ServerFrame::Welcome {
            participant: pid,
            capabilities: WS_CAPABILITIES,
        })
        .unwrap_or_default(),
    ));
    let outgoing = {
        let close = close.clone();
        let shutdown = state.shutdown.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => {
                        // Tell browsers/load balancers this is an intentional
                        // rolling-deploy departure, not an abnormal 1006 loss.
                        // The direct sink bypasses a potentially-full Hub queue.
                        let sent = tokio::time::timeout(
                            SERVER_SHUTDOWN_CLOSE_TIMEOUT,
                            sender.send(server_shutdown_close_frame()),
                        )
                        .await;
                        if !matches!(sent, Ok(Ok(()))) {
                            debug!(%pid, "ws shutdown Close frame could not be flushed");
                        }
                        close.cancel();
                        break;
                    }
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
    // Reconnect backfill (ROADMAP 方向五): replay everything the client missed
    // while Hub traffic waits in `live_rx`. The outgoing task drains `tx` so
    // replay applies real back-pressure, and the live forwarder is created only
    // after the barrier is queued. Best-effort: failures never abort the conn.
    if use_cursors {
        // Resume every effective room from its persisted delivery cursor. A room
        // without one receives only a bounded newest-history seed.
        // Cursor ACKs are only valid for messages the application applied.
        // Summary-only replay intentionally stays a legacy presentation mode;
        // cursor v2 always sends the full ordered message pages.
        if let Some(rooms) = backfill_from_cursors(&state, pid, &tx, &close, false).await {
            let _ = tx
                .send(Message::Text(
                    serde_json::to_string(&ServerFrame::DeliveryReady { rooms })
                        .unwrap_or_default(),
                ))
                .await;
        }
    } else if let Some(cursor) = since {
        backfill_since(&state, pid, cursor, &tx, &close, summarize).await;
    }
    // The DeliveryReady send above has completed before this task can enqueue a
    // Hub frame, providing a strict FIFO boundary even when a live event arrived
    // during the database replay. Legacy/no-cursor clients get the same ordering
    // guarantee after their optional `since` backfill.
    let live_forwarder = {
        let tx = tx.clone();
        let close = close.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = close.cancelled() => break,
                    frame = live_rx.recv() => {
                        let Some(frame) = frame else { break };
                        if tx.send(frame).await.is_err() {
                            break;
                        }
                    }
                }
            }
        })
    };
    let mut call_generations = HashMap::new();
    loop {
        tokio::select! {
            // The Hub asked us to drop this connection (laggy / evicted).
            () = close.cancelled() => break,
            incoming = receiver.next() => {
                let Some(Ok(msg)) = incoming else { break };
                match msg {
                    Message::Text(text) => {
                        if let Err(e) =
                            frame::handle_text(&text, &state, pid, &tx, &mut call_generations).await
                        {
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
                    Message::Pong(_) => {}
                }
            }
        }
    }
    let joined_rooms = state.hub.rooms_of(pid);
    let last_socket_closed = state.hub.unregister(pid, &registered);
    cleanup_connection_call_generations(&state, pid, call_generations).await;
    // Cluster-wide presence (ROADMAP 方向一): drop this participant from every
    // room's Redis presence set only after the last local socket is gone.
    // Retry transient Redis failures so a brief timeout doesn't leave a ghost
    // online status until the heartbeat TTL expires. Crashed clients (no graceful
    // disconnect) also age out via the heartbeat TTL, so this is additive safety.
    if last_socket_closed {
        for room in joined_rooms {
            if let Err(e) = retry_leave(&state.presence, room, pid).await {
                warn!(error = ?e, %room, %pid, "redis room-presence leave failed after retries — TTL will clear");
            }
        }
    }
    close.cancel();
    if let Some(session_watchdog) = session_watchdog {
        session_watchdog.abort();
    }
    expiry.abort();
    live_forwarder.abort();
    outgoing.abort();
    info!(%pid, "ws closed");
}

/// Converge only the durable call incarnations owned by this WebSocket.
///
/// A participant may have multiple tabs or may reconnect on another node. The
/// per-connection generation map ensures an old socket can remove its own
/// media/route/roster state without deleting the replacement incarnation.
async fn cleanup_connection_call_generations(
    state: &AppState,
    participant: ParticipantId,
    call_generations: HashMap<CallId, i64>,
) {
    for (call_id, leg_generation) in call_generations {
        let _lifecycle_guard = state.call_supervisor.lock_sfu_lifecycle(call_id).await;
        let local_owned = state
            .call_orchestrator
            .local_leg_generation(call_id, participant)
            .await
            == Some(leg_generation);

        // Commit the exact durable leave before claiming any cluster-visible
        // departure. Retry transient failures briefly; local media is removed
        // for safety even when persistence remains unavailable.
        let mut durable_room = None;
        for attempt in 1u32..=3 {
            let call = match state.calls.get(call_id).await {
                Ok(call) => call,
                Err(error) => {
                    warn!(
                        %call_id,
                        %participant,
                        ?error,
                        attempt,
                        "disconnect: canonical call lookup failed"
                    );
                    if attempt < 3 {
                        tokio::time::sleep(std::time::Duration::from_millis(100 * u64::from(attempt)))
                            .await;
                        continue;
                    }
                    break;
                }
            };
            let Some(call) = call.filter(|call| call.ended_at.is_none()) else {
                break;
            };
            match state
                .im
                .leave_call(participant, call.room_id, call_id, leg_generation)
                .await
            {
                Ok(()) => {
                    durable_room = Some(call.room_id);
                    break;
                }
                Err(aero_common::Error::Conflict(_) | aero_common::Error::NotFound(_) |
aero_common::Error::Forbidden(_)) => break,
                Err(error) => {
                    warn!(
                        %call_id,
                        %participant,
                        ?error,
                        attempt,
                        "disconnect: durable SFU leave failed"
                    );
                    if attempt < 3 {
                        tokio::time::sleep(std::time::Duration::from_millis(100 * u64::from(attempt)))
                            .await;
                    }
                }
            }
        }

        // If the media owner stopped just before the socket closed, supersede
        // its queued lifecycle event while holding the same per-call lock.
        state
            .call_supervisor
            .invalidate_ended_sfu_session(call_id, participant);
        let (removed_sfu, topology) = state
            .call_supervisor
            .remove_sfu_session_generation_with_topology(call_id, participant, leg_generation);
        if let Some(topology) = topology {
            sfu::fan_out_topology(state, &topology, None);
        }
        if local_owned {
            state.hub.call_leave(call_id, participant);
        }
        if let Err(error) = state
            .call_roster
            .leave_generation(call_id, participant, leg_generation)
            .await
        {
            warn!(%call_id, %participant, %error, "disconnect: Redis SFU roster leave failed");
        }
        let call_empty = state
            .call_orchestrator
            .cleanup_group_call_participant_generation(call_id, participant, leg_generation)
            .await;
        if let Some(room_id) = durable_room {
            if let Err(error) =
                sfu::publish_departure(state, participant, call_id, room_id, leg_generation).await
            {
                warn!(%call_id, %participant, %error, "disconnect: SFU topology publish failed");
            }
        }
        if local_owned && call_empty {
            state.call_supervisor.cancel_call(call_id);
        }
        debug!(
            %call_id,
            %participant,
            leg_generation,
            removed_sfu,
            durable_transition = durable_room.is_some(),
            call_empty,
            "disconnect SFU call-leg cleanup completed"
        );
    }
}

/// Retry `presence.leave()` up to 3 times with 100ms backoff to survive
/// transient Redis failures (timeout / connection drop). Returns the last
/// error when all attempts fail; the heartbeat TTL still cleans up eventually.
async fn retry_leave(
    presence: &aero_storage::PresenceStore,
    room: aero_common::RoomId,
    pid: aero_common::ParticipantId,
) -> Result<(), anyhow::Error> {
    let mut last_err = None;
    for attempt in 1u32..=3 {
        match presence.leave(room, pid).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                if attempt < 3 {
                    tokio::time::sleep(std::time::Duration::from_millis(100 * u64::from(attempt)))
                        .await;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("presence leave exhausted retries")))
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
/// Loose BCP-47 comparison on the primary subtag, so `en` and `en-US` are the
/// same language and the server skips a no-op translation.
pub(crate) fn same_lang(a: &str, b: &str) -> bool {
    let primary = |s: &str| s.split(['-', '_']).next().unwrap_or(s).to_ascii_lowercase();
    primary(a) == primary(b)
}

/// Resolve and authorize an existing call using the persisted room before a WS
/// handler mutates any call/SFU/roster state. Errors are returned as ordinary
/// protocol frames so a rejected call claim does not tear down the socket.
async fn active_call_for_frame(
    state: &AppState,
    participant: ParticipantId,
    call_id: CallId,
    claimed_room: RoomId,
    required_mode: Option<CallMode>,
    tx: &mpsc::Sender<Message>,
) -> Option<CallSession> {
    match state
        .im
        .assert_active_call_access(participant, call_id, claimed_room, required_mode)
        .await
    {
        Ok(call) => Some(call),
        Err(error) => {
            let _ = tx.try_send(Message::Text(
                serde_json::to_string(&ServerFrame::Error {
                    code: error.code(),
                    msg: error.to_string(),
                })
                .unwrap_or_default(),
            ));
            None
        }
    }
}

/// Resolve the canonical active SFU call for a new joiner. Unlike
/// `active_call_for_frame`, this intentionally checks room access but not call
/// participation: successful orchestration creates that active participant leg
/// before any subsequent call mutation is relayed.
async fn joinable_call_for_frame(
    state: &AppState,
    participant: ParticipantId,
    call_id: CallId,
    claimed_room: RoomId,
    tx: &mpsc::Sender<Message>,
) -> Option<CallSession> {
    match state
        .im
        .assert_joinable_call_access(participant, call_id, claimed_room, CallMode::Sfu)
        .await
    {
        Ok(call) => Some(call),
        Err(error) => {
            let _ = tx.try_send(Message::Text(
                serde_json::to_string(&ServerFrame::Error {
                    code: error.code(),
                    msg: error.to_string(),
                })
                .unwrap_or_default(),
            ));
            None
        }
    }
}

/// Process spontaneous SFU media-session exits under the server's tracked
/// shutdown lifecycle. Public for the binary boot assembler; protocol details
/// remain in the private SFU WS module.
pub async fn run_sfu_lifecycle_events(
    state: AppState,
    events: mpsc::UnboundedReceiver<crate::sfu_media::SfuSessionEnded>,
    cancel: CancellationToken,
) {
    sfu::run_lifecycle_events(state, events, cancel).await;
}

mod ai_usage;
mod backfill;
mod bus;
mod credentials;
mod delivery;
mod frame;
#[cfg(test)]
#[path = "tests.rs"]
mod send_markdown_tests;
mod sfu;
#[cfg(test)]
#[path = "ws_params_security_tests.rs"]
mod ws_params_security_tests;

// `run_bus_listener` / `run_live_bus_listener` moved to `bus`; re-exported
// here so `crate::ws::ws_impl::run_bus_listener` (the boot path) still resolves.
use backfill::{backfill_from_cursors, backfill_since};
#[cfg(test)]
pub(crate) use backfill::{
    backfill_room_ids, cursor_backfill_plan, initial_backfill_page, BACKFILL_PER_ROOM_LIMIT,
    INITIAL_BACKFILL_PER_ROOM_LIMIT,
};
pub(crate) use backfill::{parse_resume_cursor, truncation_cursor};
pub use bus::{run_bus_listener, run_live_bus_listener};
pub(crate) use frame::send_blocks_frame;
