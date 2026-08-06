//! Per-frame WebSocket handlers: the `ClientFrame` dispatch (`handle_text`)
//! and the shared structured/Markdown send dispatch (`send_blocks_frame`).
use super::{
    warn, debug, AppState, CallId, ParticipantId, RoomId, Block, MessageId, mpsc, Message,
    ClientFrame, ServerFrame, CallMode, active_call_for_frame, CallEvent, same_lang,
    joinable_call_for_frame, call_peers_excluding, Ulid, StreamEvent, truncation_cursor,
    stream_viewer_count,
};
use std::collections::HashMap;

mod call_lifecycle;
mod message_policy;
use call_lifecycle::rollback_group_join_generation;
#[cfg(test)]
use message_policy::postgres_code_retryable;
use message_policy::{message_expiration, message_request_hash, send_error_retryable};

pub(super) async fn handle_text(
    text: &str,
    state: &AppState,
    pid: ParticipantId,
    tx: &mpsc::Sender<Message>,
    call_generations: &mut HashMap<CallId, i64>,
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
            for (&call_id, &leg_generation) in call_generations.iter() {
                match state
                    .call_roster
                    .heartbeat_generation(call_id, pid, leg_generation)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        debug!(
                            %call_id,
                            %pid,
                            leg_generation,
                            "stale call-roster heartbeat rejected"
                        );
                    }
                    Err(e) => {
                        warn!(
                            error = ?e,
                            %call_id,
                            %pid,
                            "redis call-roster heartbeat failed"
                        );
                    }
                }
            }
        }
        ClientFrame::JoinRoom { room_id } => {
            if state.im.assert_room_access(pid, room_id).await.is_err() {
                let _ = tx.try_send(Message::Text(
                    serde_json::to_string(&ServerFrame::Error {
                        code: "forbidden",
                        msg: "room access denied".into(),
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
        ClientFrame::SendMessage {
            room_id,
            blocks,
            client_message_id,
            reply_to,
            expires_after_secs,
        } => {
            // Structured-blocks send. Delegates to the shared dispatch so this
            // path's behaviour is unchanged: see `send_blocks_frame`.
            complete_send_frame(
                state,
                pid,
                room_id,
                blocks,
                reply_to,
                expires_after_secs,
                client_message_id,
                tx,
            )
            .await?;
        }
        ClientFrame::SendMarkdown {
            room_id,
            markdown,
            client_message_id,
            reply_to,
            expires_after_secs,
        } => {
            // Markdown edge entry (ROADMAP6 方向三): parse the Markdown body into
            // the same `Vec<Block>` the structured `SendMessage` path carries, then
            // run the *identical* dispatch (access / rate / slowmode / TTL / send).
            let blocks = aero_common::markdown::parse_markdown_to_blocks(&markdown);
            complete_send_frame(
                state,
                pid,
                room_id,
                blocks,
                reply_to,
                expires_after_secs,
                client_message_id,
                tx,
            )
            .await?;
        }
        ClientFrame::EditMessage {
            id,
            blocks,
            expected_version,
        } => {
            let room = state.im.assert_message_edit_preflight(pid, id).await?;
            aero_im_core::validate_blocks(&blocks)?;
            crate::ws_rate::check_ws_rate_room(state, room).await?;
            let slowmode = crate::message_send_policy::reserve_slowmode(state, pid, room).await?;
            let result = state
                .im
                .edit_message(pid, id, blocks, expected_version)
                .await;
            slowmode.finish(result).await?;
        }
        ClientFrame::DeleteMessage { id } => {
            state.im.delete_message(pid, id).await?;
        }
        ClientFrame::React { message_id, emoji } => {
            state.im.toggle_reaction(pid, message_id, &emoji).await?;
        }
        ClientFrame::MarkRead {
            room_id,
            last_message_id,
        } => {
            state.im.mark_read(pid, room_id, last_message_id).await?;
        }
        ClientFrame::DeliveryAck {
            room_id,
            message_id,
            delivery_ordinal,
            seq,
        } => {
            super::delivery::handle_ack(state, pid, tx, room_id, message_id, delivery_ordinal, seq)
                .await?;
        }
        ClientFrame::Typing { room_id, on } => {
            state.im.typing(pid, room_id, on).await?;
        }
        ClientFrame::CallInvite {
            room_id,
            kind,
            mode,
            sdp,
        } => {
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
        ClientFrame::CallAnswer {
            call_id,
            room_id,
            to,
            sdp,
        } => {
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
            if active_call_for_frame(state, pid, call_id, room_id, None, tx)
                .await
                .is_none()
            {
                return Ok(());
            }
            state
                .im
                .relay_call_event(
                    pid,
                    room_id,
                    CallEvent::Answer {
                        call_id,
                        from: pid,
                        to,
                        sdp,
                    },
                )
                .await?;
        }
        ClientFrame::CallIce {
            call_id,
            room_id,
            to,
            candidate,
        } => {
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
            if active_call_for_frame(state, pid, call_id, room_id, None, tx)
                .await
                .is_none()
            {
                return Ok(());
            }
            state
                .im
                .relay_call_event(
                    pid,
                    room_id,
                    CallEvent::Ice {
                        call_id,
                        from: pid,
                        to,
                        candidate,
                    },
                )
                .await?;
        }
        ClientFrame::CallEnd {
            call_id,
            room_id,
            reason,
        } => {
            let reason = reason.unwrap_or_else(|| "ended".into());
            // Serialize the authoritative End transition with CallJoin,
            // server-media offer commit, and spontaneous owner-task cleanup.
            // If an offer commits first, this cleanup sees and removes it; if
            // End commits first, the offer's active-call fence rejects it.
            let lifecycle_guard = state.call_supervisor.lock_sfu_lifecycle(call_id).await;
            if active_call_for_frame(state, pid, call_id, room_id, None, tx)
                .await
                .is_none()
            {
                return Ok(());
            }
            state
                .im
                .relay_call_event(
                    pid,
                    room_id,
                    CallEvent::End {
                        call_id,
                        room_id,
                        by: pid,
                        reason: reason.clone(),
                    },
                )
                .await?;
            // End owns the whole SFU lifecycle, not just signaling: cancel all
            // browser media tasks, both local/cluster rosters, the shared call
            // egress, and bridge pulls. The relay above atomically committed the
            // durable End transition; none of this cleanup runs when that write
            // fails or loses a concurrent End race.
            state.hub.call_end(call_id);
            if let Err(error) = state.call_roster.clear(call_id).await {
                warn!(%call_id, ?error, "Redis call roster clear failed on end");
            }
            state.call_supervisor.cancel_call(call_id);
            state.call_orchestrator.cleanup_ended_call(call_id).await;
            call_generations.remove(&call_id);
            drop(lifecycle_guard);
            // Missed-call: if the call ended while never answered, drop a durable
            // "call_missed" notice into each callee's activity feed (Wave 21).
            // Out-of-band + best-effort — never fails the call-end relay.
            match state.calls.unanswered_callees(call_id).await {
                Ok(Some((initiator, callees))) => {
                    let feed = aero_storage::ActivityFeedRepo::new(state.pg.clone());
                    // Resolve the caller's name once for the mobile push title;
                    // only needed when a push gateway is actually configured.
                    let push_enabled = state.push.any_enabled();
                    let _initiator_name = if push_enabled {
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
                            .insert(
                                callee,
                                "call_missed",
                                Some(initiator),
                                Some(call_id.0),
                                "Missed call",
                            )
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
                        match super::ai_usage::summarize(state, ai.as_ref(), room_id, &joined).await
                        {
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
        ClientFrame::CallCaption {
            call_id,
            room_id,
            text,
            lang,
            target_lang,
            is_final,
        } => {
            let text = text.trim().to_string();
            if text.is_empty() {
                return Ok(());
            }
            // Resolve the call's canonical room BEFORE persisting a transcript:
            // room membership against only the client-supplied room would let a
            // member of room A append captions to a call in room B.
            if active_call_for_frame(state, pid, call_id, room_id, None, tx)
                .await
                .is_none()
            {
                return Ok(());
            }
            // Translate only *final* lines, only when a distinct target language
            // is set and an AI backend is available. Interim lines relay verbatim
            // to keep latency low.
            let (translated, translated_lang) = match (is_final, target_lang.as_deref(), &state.ai)
            {
                (true, Some(target), Some(ai))
                    if lang.as_deref().map_or(true, |l| !same_lang(l, target)) =>
                {
                    match super::ai_usage::translate(state, ai.as_ref(), room_id, &text, target)
                        .await
                    {
                        Ok(t) if !t.trim().is_empty() => (Some(t), Some(target.to_string())),
                        Ok(_) => (None, None),
                        Err(error) => {
                            tracing::warn!(%error, %call_id, "call caption translation failed");
                            (None, None)
                        }
                    }
                }
                _ => (None, None),
            };
            // Translation can await an external model. Revalidate after that
            // latency before either transcript persistence or signaling.
            if active_call_for_frame(state, pid, call_id, room_id, None, tx)
                .await
                .is_none()
            {
                return Ok(());
            }
            // Persist final caption lines as a durable call transcript (the
            // post-call AI recap is generated on CallEnd from these). Store the
            // translated text when one was produced, else the original. Best-effort
            // — never blocks or fails the low-latency caption relay.
            if is_final {
                let line = translated.as_deref().unwrap_or(text.as_str());
                if let Err(e) = aero_storage::CallTranscriptRepo::new(state.pg.clone())
                    .append_authorized(call_id, pid, room_id, line)
                    .await
                {
                    tracing::warn!(error = ?e, %call_id, "call transcript append failed");
                }
            }
            state
                .im
                .relay_call_event(
                    pid,
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
        ClientFrame::CallJoin {
            room_id,
            kind,
            call_id,
        } => {
            let (call_id, canonical_kind) = if let Some(call_id) = call_id {
                let Some(call) = joinable_call_for_frame(state, pid, call_id, room_id, tx).await
                else {
                    return Ok(());
                };
                (call_id, call.kind)
            } else {
                // A server-minted id is the only call-creation path. Existing
                // client-provided ids must resolve above; otherwise an attacker
                // could choose a foreign id and rely on a uniqueness conflict.
                if let Err(error) = state.im.assert_room_access(pid, room_id).await {
                    let _ = tx.try_send(Message::Text(
                        serde_json::to_string(&ServerFrame::Error {
                            code: error.code(),
                            msg: error.to_string(),
                        })
                        .unwrap_or_default(),
                    ));
                    return Ok(());
                }
                (CallId::new(), kind)
            };
            // A queued spontaneous media-exit event must either converge fully
            // before this logical rejoin, or observe the new incarnation and
            // do nothing. The per-call striped lock prevents its async DB/
            // Redis cleanup from interleaving with the mutations below.
            let _sfu_lifecycle_guard = state.call_supervisor.lock_sfu_lifecycle(call_id).await;
            // Full-mesh admission control (P1-4): the orchestrator is the single
            // authoritative gate. Run it *first*, before any roster mutation or
            // relay, so a rejected (call-full) join never lands in the Hub roster,
            // never gets told whom to connect to, and the room is never told it
            // joined. A reconnect of an existing member is admitted (dedup by pid).
            let join = match state
                .call_orchestrator
                .join_group_call(call_id, room_id, pid, canonical_kind)
                .await
            {
                Ok(join) => join,
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
                    // Fail closed: no Hub/Redis/SFU mutation is allowed unless the
                    // canonical call row was created or validated successfully.
                    let code = match &e {
                        aero_im_call::OrchestratorError::NotFound(_) => "call_not_found",
                        aero_im_call::OrchestratorError::Forbidden(_) => "forbidden",
                        aero_im_call::OrchestratorError::Conflict(_) => "call_conflict",
                        aero_im_call::OrchestratorError::CallFull(_) => "call_full",
                        aero_im_call::OrchestratorError::Db(_) => "call_unavailable",
                    };
                    warn!(error = ?e, %call_id, "call orchestrator join rejected");
                    let _ = tx.try_send(Message::Text(
                        serde_json::to_string(&ServerFrame::Error {
                            code,
                            msg: e.to_string(),
                        })
                        .unwrap_or_default(),
                    ));
                    return Ok(());
                }
            };
            // The orchestrator persists the participant leg and mutates the
            // router/route registry. Re-read before touching any second-layer
            // roster so an End that won during those awaits rolls the join back.
            if active_call_for_frame(state, pid, call_id, room_id, Some(CallMode::Sfu), tx)
                .await
                .is_none()
            {
                rollback_group_join_generation(state, call_id, pid, join.leg_generation).await;
                return Ok(());
            }

            // Ensure bridge intent while still inside the mutation phase. The
            // final canonical re-read below then either admits all mutations or
            // tears them down; no local topology is created after that fence.
            let desired_bridges = match &join.topology {
                aero_live_webrtc::CallTopology::ServeLocal => &[][..],
                aero_live_webrtc::CallTopology::BridgeTo(urls) => urls.as_slice(),
            };
            let (spawned, cancelled) = state
                .call_supervisor
                .reconcile_bridges(call_id, desired_bridges)
                .await;
            debug!(
                %call_id,
                peers = desired_bridges.len(),
                spawned,
                cancelled,
                "cross-node call bridges reconciled on local join"
            );
            // Local Hub keeps the roster for per-process mesh delivery; its
            // return value is the fallback set of already-present peers.
            let local_existing = state.hub.call_join(call_id, pid);
            // Redis is the cluster-wide roster (ROADMAP 方向二): register self,
            // then source the "whom to connect to" set from Redis (minus self) so
            // a joiner sees peers connected to *other* nodes too. `join` doubles
            // as the heartbeat restamp. On any Redis error, fall back to the local
            // Hub roster so a single-node call still works.
            match state
                .call_roster
                .join_generation(call_id, pid, join.leg_generation)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    rollback_group_join_generation(state, call_id, pid, join.leg_generation).await;
                    let _ = tx.try_send(Message::Text(
                        serde_json::to_string(&ServerFrame::Error {
                            code: "stale_call_leg",
                            msg: "a newer call join superseded this connection".into(),
                        })
                        .unwrap_or_default(),
                    ));
                    return Ok(());
                }
                Err(e) => {
                    warn!(error = ?e, %call_id, "redis call-roster join failed");
                }
            }

            // This is the final mutation fence for Join↔End. If End already
            // committed (including cleanup that ran between the prior check and
            // these writes), remove the newly-added router/Hub/Redis/bridge
            // state. If End begins after this read, its own idempotent cleanup
            // observes and removes everything above.
            if active_call_for_frame(state, pid, call_id, room_id, Some(CallMode::Sfu), tx)
                .await
                .is_none()
            {
                rollback_group_join_generation(state, call_id, pid, join.leg_generation).await;
                return Ok(());
            }
            let existing = call_peers_excluding(state, call_id, pid, local_existing).await;
            // Tell the joiner who is already in the call (whom to connect to).
            if let Err(error) = state
                .im
                .relay_call_event(
                    pid,
                    room_id,
                    CallEvent::Roster {
                        call_id,
                        to: pid,
                        members: existing,
                        kind: canonical_kind,
                        leg_generation: join.leg_generation,
                    },
                )
                .await
            {
                rollback_group_join_generation(state, call_id, pid, join.leg_generation).await;
                return Err(error.into());
            }
            // Tell the room a new peer joined.
            if let Err(error) = state
                .im
                .relay_call_event(
                    pid,
                    room_id,
                    CallEvent::Join {
                        call_id,
                        room_id,
                        from: pid,
                        kind: canonical_kind,
                        leg_generation: join.leg_generation,
                    },
                )
                .await
            {
                rollback_group_join_generation(state, call_id, pid, join.leg_generation).await;
                return Err(error.into());
            }
            state
                .call_supervisor
                .invalidate_ended_sfu_session(call_id, pid);
            if let Some(previous_generation) = call_generations
                .get(&call_id)
                .copied()
                .filter(|previous| *previous != join.leg_generation)
            {
                let (_, topology) = state
                    .call_supervisor
                    .remove_sfu_session_generation_with_topology(call_id, pid, previous_generation);
                if let Some(topology) = topology {
                    super::sfu::fan_out_topology(state, &topology, None);
                }
            }
            call_generations.insert(call_id, join.leg_generation);
        }
        ClientFrame::CallLeave { call_id, room_id } => {
            let Some(leg_generation) = call_generations.get(&call_id).copied() else {
                let _ = tx.try_send(Message::Text(
                    serde_json::to_string(&ServerFrame::Error {
                        code: "stale_call_leg",
                        msg: "this connection has not joined that call".into(),
                    })
                    .unwrap_or_default(),
                ));
                return Ok(());
            };
            let _sfu_lifecycle_guard = state.call_supervisor.lock_sfu_lifecycle(call_id).await;
            // Persist and publish Leave before mutating live media state. A
            // stale generation still owns its old node-local media, but must
            // not alter the replacement database/Redis/Hub incarnation.
            let durable_transition = match state
                .im
                .leave_call(pid, room_id, call_id, leg_generation)
                .await
            {
                Ok(()) => true,
                Err(aero_common::Error::Conflict(_)) => false,
                Err(error) => return Err(error.into()),
            };
            let local_owned = state
                .call_orchestrator
                .local_leg_generation(call_id, pid)
                .await
                == Some(leg_generation);

            let (removed_sfu, topology) = state
                .call_supervisor
                .remove_sfu_session_generation_with_topology(call_id, pid, leg_generation);
            if local_owned {
                state.hub.call_leave(call_id, pid);
            }
            if let Some(topology) = topology {
                super::sfu::fan_out_topology(state, &topology, None);
            }
            match state
                .call_roster
                .leave_generation(call_id, pid, leg_generation)
                .await
            {
                Ok(_) => {}
                Err(e) => warn!(error = ?e, %call_id, "redis call-roster leave failed"),
            }
            if durable_transition && removed_sfu {
                if let Err(error) =
                    super::sfu::publish_departure(state, pid, call_id, room_id, leg_generation)
                        .await
                {
                    warn!(%call_id, %pid, %error, "call leave publisher departure publish failed");
                }
            }
            let router_empty = state
                .call_orchestrator
                .cleanup_group_call_participant_generation(call_id, pid, leg_generation)
                .await;
            if local_owned && router_empty {
                state.call_supervisor.cancel_call(call_id);
            }
            if call_generations.get(&call_id) == Some(&leg_generation) {
                call_generations.remove(&call_id);
            }
            debug!(
                %call_id,
                removed_sfu,
                router_empty,
                "call participant leave cleanup completed"
            );
        }
        ClientFrame::CallOffer {
            call_id,
            room_id,
            to,
            sdp,
        } => {
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
            if active_call_for_frame(state, pid, call_id, room_id, None, tx)
                .await
                .is_none()
            {
                return Ok(());
            }
            state
                .im
                .relay_call_event(
                    pid,
                    room_id,
                    CallEvent::Offer {
                        call_id,
                        from: pid,
                        to,
                        sdp,
                    },
                )
                .await?;
        }
        ClientFrame::CallSfuOffer {
            call_id,
            room_id,
            sdp,
        } => {
            let Some(leg_generation) = call_generations.get(&call_id).copied() else {
                let _ = tx.try_send(Message::Text(
                    serde_json::to_string(&ServerFrame::Error {
                        code: "stale_call_leg",
                        msg: "join the call before negotiating server media".into(),
                    })
                    .unwrap_or_default(),
                ));
                return Ok(());
            };
            super::sfu::handle_offer(state, pid, call_id, room_id, leg_generation, sdp, tx).await?;
        }
        ClientFrame::CallSfuIce {
            call_id,
            room_id,
            session_generation,
            candidate,
        } => {
            super::sfu::handle_ice(
                state,
                pid,
                call_id,
                room_id,
                session_generation,
                candidate,
                tx,
            )
            .await?;
        }
        ClientFrame::CallSfuSubscribe {
            call_id,
            room_id,
            session_generation,
            revision,
            tracks,
        } => {
            super::sfu::handle_subscribe(
                state,
                pid,
                super::sfu::SubscribeRequest {
                    call_id,
                    room_id,
                    session_generation,
                    revision,
                    tracks,
                },
                tx,
            )
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
            let since_cursor = since
                .as_deref()
                .and_then(|c| Ulid::from_string(c.trim()).ok());
            let replay_limit = if since_cursor.is_some() { 200 } else { 30 };
            if let Ok(lines) = state
                .live
                .recent_chat_since(stream_id, since_cursor, replay_limit)
                .await
            {
                let replayed = lines.len();
                let last_id = lines.last().map(|l| l.id);
                for line in lines {
                    let frame = ServerFrame::StreamEvent {
                        event: StreamEvent::Chat(line),
                    };
                    let _ = tx.try_send(Message::Text(
                        serde_json::to_string(&frame).unwrap_or_default(),
                    ));
                }
                // Truncation signal (ROADMAP 第三版 方向一), cursor catch-up
                // only (the no-cursor tail is deliberately bounded context, not
                // a complete replay): a capped catch-up tells the client to
                // continue via REST `GET /api/streams/:id/chat?since=`.
                if since_cursor.is_some() {
                    if let Some(next_since) = truncation_cursor(replayed, replay_limit, last_id) {
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
                    let frame = ServerFrame::StreamEvent {
                        event: StreamEvent::Gift(gift),
                    };
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
            // REST and WebSocket deliberately share one authoritative posting
            // gate so bans and configurable modes cannot drift by transport.
            crate::stream_chat_modes::enforce_chat_post(state, stream_id, pid).await?;
            // Subscriber-badge flag (migration 0082): mirrors the REST guard.
            let is_sub = state.live.subscriber_flag(stream_id, pid).await;
            state.live.post_chat(pid, stream_id, body, is_sub).await?;
        }
        ClientFrame::StreamGift {
            stream_id,
            gift_id,
            qty,
            nonce,
        } => {
            let qty = qty.unwrap_or(1);
            let (_, inserted) = state
                .live
                .send_gift(pid, stream_id, &gift_id, qty, nonce.as_deref())
                .await?;
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

#[allow(clippy::too_many_arguments)]
async fn complete_send_frame(
    state: &AppState,
    pid: ParticipantId,
    room_id: RoomId,
    blocks: Vec<Block>,
    reply_to: Option<MessageId>,
    expires_after_secs: Option<u64>,
    client_message_id: Option<uuid::Uuid>,
    tx: &mpsc::Sender<Message>,
) -> anyhow::Result<()> {
    let result = send_blocks_frame(
        state,
        pid,
        room_id,
        blocks,
        reply_to,
        expires_after_secs,
        client_message_id,
    )
    .await;
    match (client_message_id, result) {
        (Some(client_message_id), Ok(outcome)) => {
            let payload = serde_json::to_string(&ServerFrame::MessageAck {
                client_message_id,
                message: outcome.message,
                deduplicated: outcome.deduplicated,
            })?;
            if let Err(error) = tx.try_send(Message::Text(payload)) {
                warn!(?error, %client_message_id, "message ACK queue full or closed");
            }
            Ok(())
        }
        (Some(client_message_id), Err(error)) => {
            let payload = serde_json::to_string(&ServerFrame::MessageNack {
                client_message_id,
                code: error.code(),
                msg: error.to_string(),
                retryable: send_error_retryable(&error),
            })?;
            if let Err(queue_error) = tx.try_send(Message::Text(payload)) {
                warn!(
                    ?queue_error,
                    %client_message_id,
                    "message NACK queue full or closed"
                );
            }
            Ok(())
        }
        (None, Ok(_)) => Ok(()),
        (None, Err(error)) => Err(error.into()),
    }
}

/// Shared send dispatch for both the structured (`SendMessage`) and Markdown
/// (`SendMarkdown`) edge frames: workspace/room access guard → canonical body
/// and TTL validation → tenant rate budget → token-owned slowmode reservation →
/// `ImService::send_message`.
///
/// Factored out so the Markdown entry can reuse the *exact* validation/limit
/// path without duplicating it, and so `SendMessage`'s behaviour is preserved
/// verbatim — the only difference between the two frames is how `blocks` is
/// produced (carried structurally vs. parsed from Markdown at the edge).
pub(crate) async fn send_blocks_frame(
    state: &AppState,
    pid: ParticipantId,
    room_id: RoomId,
    blocks: Vec<Block>,
    reply_to: Option<MessageId>,
    expires_after_secs: Option<u64>,
    client_message_id: Option<uuid::Uuid>,
) -> aero_common::Result<aero_im_core::SendMessageOutcome> {
    // Tenant guard: the sender must belong to BOTH the room's workspace and
    // the room before a message is accepted. `ImService::send_message`
    // re-checks room membership (a distinct, retained check); this adds the
    // workspace-membership dimension. Maps to the WS `error` frame on denial.
    state.im.assert_room_access(pid, room_id).await?;
    let request_hash = client_message_id
        .map(|_| message_request_hash(room_id, &blocks, reply_to, expires_after_secs))
        .transpose()?;
    if let (Some(client_message_id), Some(request_hash)) = (client_message_id, request_hash) {
        if let Some(message) = state
            .messages
            .find_by_client_message_id(pid, client_message_id, &request_hash)
            .await?
        {
            // A process may have committed the canonical message and crashed
            // before its post-commit fast publish. Retrying the sender key should
            // also nudge that durable outbox row instead of merely ACKing it.
            let outbox = aero_storage::EventOutboxRepo::new(state.pg.clone());
            match outbox.pending_for_message(message.id).await {
                Ok(Some(row)) => {
                    if let Err(error) = state.im.dispatch_event_outbox_id(row.id).await {
                        warn!(
                            ?error,
                            message_id = %message.id,
                            "idempotent send outbox retry failed"
                        );
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    warn!(
                        ?error,
                        message_id = %message.id,
                        "idempotent send outbox lookup failed"
                    );
                }
            }
            if let Err(error) = state.im.dispatch_message_side_effects_for(message.id).await {
                warn!(
                    ?error,
                    message_id = %message.id,
                    "idempotent send side-effect retry failed"
                );
            }
            return Ok(aero_im_core::SendMessageOutcome {
                message,
                deduplicated: true,
            });
        }
    }
    // Pure canonical validation and TTL resolution happen before the Redis
    // slow-mode reservation. A malformed body must not consume a window even
    // though the service repeats these checks at its trust boundary.
    aero_im_core::validate_blocks(&blocks)?;
    let expires_at = message_expiration(expires_after_secs, time::OffsetDateTime::now_utc())?;
    // Tenant fairness (ROADMAP3 方向五): sends are the highest-volume
    // write path, so they are charged against the room's workspace
    // budget — after the access check (so non-members cannot drain a
    // victim's budget), surfacing as a WS `error` frame when over.
    crate::ws_rate::check_ws_rate_room(state, room_id).await?;
    // Slowmode enforcement (migration 0107): if the room has a slowmode
    // interval, reject the send unless enough time has elapsed since the
    // sender's last message. Checked AFTER access/rate/body guards so only a
    // valid write attempt reserves the interval; definitive service failures
    // compare-and-delete this request's random token.
    let slowmode = crate::message_send_policy::reserve_slowmode(state, pid, room_id).await?;
    let result = match (client_message_id, request_hash) {
        (Some(client_message_id), Some(request_hash)) => {
            state
                .im
                .send_message_idempotent(
                    pid,
                    room_id,
                    blocks,
                    reply_to,
                    expires_at,
                    client_message_id,
                    request_hash,
                )
                .await
        }
        _ => state
            .im
            .send_message(pid, room_id, blocks, reply_to, expires_at)
            .await
            .map(|message| aero_im_core::SendMessageOutcome {
                message,
                deduplicated: false,
            }),
    };
    slowmode.finish(result).await
}

#[cfg(test)]
mod tests;
