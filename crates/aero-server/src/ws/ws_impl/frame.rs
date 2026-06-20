//! Per-frame WebSocket handlers: the `ClientFrame` dispatch (`handle_text`)
//! and the shared structured/Markdown send dispatch (`send_blocks_frame`).
//!
//! Moved verbatim from `ws_impl` (the per-frame handlers); behaviour unchanged.
use super::*;

pub(super) async fn handle_text(
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
            // Structured-blocks send. Delegates to the shared dispatch so this
            // path's behaviour is unchanged: see `send_blocks_frame`.
            send_blocks_frame(state, pid, room_id, blocks, reply_to, expires_after_secs)
                .await?;
        }
        ClientFrame::SendMarkdown { room_id, markdown, reply_to, expires_after_secs } => {
            // Markdown edge entry (ROADMAP6 方向三): parse the Markdown body into
            // the same `Vec<Block>` the structured `SendMessage` path carries, then
            // run the *identical* dispatch (access / rate / slowmode / TTL / send).
            // Purely additive — `SendMessage` is untouched.
            //
            // @mention follow-up: `parse_markdown_to_blocks` emits nil-id
            // `Block::Mention` markers (it only captures `display_name`; resolving
            // a `ParticipantId` needs a workspace-scoped handle lookup, which is a
            // larger change deferred per the conservative-first rule). A nil-id
            // mention is a harmless no-op in the notify pipeline, so the text lands
            // correctly today and mention resolution is a follow-up.
            let blocks = aero_common::markdown::parse_markdown_to_blocks(&markdown);
            send_blocks_frame(state, pid, room_id, blocks, reply_to, expires_after_secs)
                .await?;
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
            // Validate the SDP offer before it is relayed to peers. An unchecked
            // blob could be megabytes of junk (DoS amplification) or not an SDP
            // at all; reject malformed payloads back to the sender, never relay.
            if let Err(e) = aero_signaling::validate_sdp(&sdp) {
                let _ = tx.try_send(Message::Text(
                    serde_json::to_string(&ServerFrame::Error {
                        code: "invalid_call",
                        msg: e.to_string(),
                    })
                    .unwrap_or_default(),
                ));
                return Ok(());
            }
            state
                .im
                .start_call(pid, room_id, kind, mode.unwrap_or(CallMode::P2p), sdp)
                .await?;
        }
        ClientFrame::CallAnswer { call_id, room_id, to, sdp } => {
            // Validate the SDP answer before relaying (size cap + `v=0` shape).
            if let Err(e) = aero_signaling::validate_sdp(&sdp) {
                let _ = tx.try_send(Message::Text(
                    serde_json::to_string(&ServerFrame::Error {
                        code: "invalid_call",
                        msg: e.to_string(),
                    })
                    .unwrap_or_default(),
                ));
                return Ok(());
            }
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
            // Validate the ICE candidate (size cap + `candidate:` prefix + mid
            // typing) before relaying, so garbage never reaches a peer's
            // `new RTCIceCandidate(init)` constructor.
            if let Err(e) = aero_signaling::validate_ice_candidate(&candidate) {
                let _ = tx.try_send(Message::Text(
                    serde_json::to_string(&ServerFrame::Error {
                        code: "invalid_call",
                        msg: e.to_string(),
                    })
                    .unwrap_or_default(),
                ));
                return Ok(());
            }
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
                        // ROADMAP6 方向四: resolve the caller's profile through the
                        // per-process TTL cache instead of a raw repo.get — this
                        // display-name read repeats across every missed-call push
                        // and is exactly the round-trip the cache exists to absorb.
                        state
                            .participant_cache
                            .get_or_fetch(initiator, &state.participants)
                            .await
                            .ok()
                            .flatten()
                            .map_or_else(|| "Someone".to_string(), |p| p.display_name.clone())
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
                                title: String::new(),
                                body: String::new(),
                                room_id: Some(room_id.to_string()),
                                message_id: None,
                                badge: None,
                                collapse_key: None,
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
            // Full-mesh admission control (P1-4): the orchestrator is the single
            // authoritative gate. Run it *first*, before any roster mutation or
            // relay, so a rejected (call-full) join never lands in the Hub roster,
            // never gets told whom to connect to, and the room is never told it
            // joined. A reconnect of an existing member is admitted (dedup by pid).
            let join = match state.call_orchestrator.join_group_call(call_id, room_id, pid, kind).await {
                Ok(join) => Some(join),
                Err(aero_im_call::OrchestratorError::CallFull(cap)) => {
                    warn!(%call_id, cap, %pid, "group-call join rejected: mesh full");
                    let _ = tx.try_send(Message::Text(
                        serde_json::to_string(&ServerFrame::Error {
                            code: "call_full",
                            msg: format!(
                                "group call is full (max {cap} participants for mesh calls)"
                            ),
                        })
                        .unwrap_or_default(),
                    ));
                    return Ok(());
                }
                Err(e) => {
                    // Other orchestrator failures (e.g. DB/registry) are best-effort
                    // and must not disturb the working full-mesh CallEvent path.
                    warn!(error = ?e, %call_id, "call orchestrator join failed (full-mesh unaffected)");
                    None
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
            // The orchestrator already registered the participant in the SFU
            // router + cluster CallRouteRegistry above; here we only act on the
            // computed bridge topology. On a BridgeTo, ensure one bridge per peer
            // node. Best-effort — a failure never disturbs the full-mesh path.
            if let Some(join) = join {
                if let aero_live_webrtc::CallTopology::BridgeTo(urls) = join.topology {
                    let spawned = state.call_supervisor.ensure_bridges(call_id, &urls).await;
                    debug!(%call_id, peers = urls.len(), spawned, "cross-node call bridges ensured");
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
            // Validate the per-pair mesh SDP offer before relaying.
            if let Err(e) = aero_signaling::validate_sdp(&sdp) {
                let _ = tx.try_send(Message::Text(
                    serde_json::to_string(&ServerFrame::Error {
                        code: "invalid_call",
                        msg: e.to_string(),
                    })
                    .unwrap_or_default(),
                ));
                return Ok(());
            }
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
/// Shared send dispatch for both the structured (`SendMessage`) and Markdown
/// (`SendMarkdown`) edge frames: workspace/room access guard → tenant rate
/// budget → slowmode interval → ephemeral TTL → `ImService::send_message`.
///
/// Factored out so the Markdown entry can reuse the *exact* validation/limit
/// path without duplicating it, and so `SendMessage`'s behaviour is preserved
/// verbatim — the only difference between the two frames is how `blocks` is
/// produced (carried structurally vs. parsed from Markdown at the edge).
async fn send_blocks_frame(
    state: &AppState,
    pid: ParticipantId,
    room_id: RoomId,
    blocks: Vec<Block>,
    reply_to: Option<MessageId>,
    expires_after_secs: Option<u64>,
) -> anyhow::Result<()> {
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
            let elapsed = (time::OffsetDateTime::now_utc() - last_at).whole_seconds();
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
    Ok(())
}
