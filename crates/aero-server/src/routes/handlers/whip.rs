// ----- WHIP / WHEP -----

async fn whip_post(
    State(s): State<AppState>,
    Path(stream_key): Path<String>,
    sdp_offer: String,
) -> ApiResult<axum::response::Response> {
    use axum::http::{header, HeaderValue, StatusCode};
    let stream = s
        .streams
        .get_by_key(&stream_key)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("stream".into()))?;
    let resource = accept_whip_offer(&stream, &sdp_offer, &s.ingest_host, s.ingest_port)
        .map_err(|e| match e {
            WhipError::InvalidSdp(m) => AeroError::Invalid(m),
            WhipError::Conflict => AeroError::Conflict("publisher present".into()),
            WhipError::NotFound => AeroError::NotFound("stream".into()),
            WhipError::Internal(m) => AeroError::Internal(anyhow::anyhow!(m)),
        })?;
    s.whip
        .insert(resource.clone())
        .map_err(|_| AeroError::Conflict("publisher present".into()))?;
    // Sticky routing (ROADMAP 方向二): advertise that THIS node now ingests the
    // stream, so WHEP pulls landing on other nodes can be redirected here.
    if let Err(e) = s.stream_routes.publish(stream.id, &s.public_base_url).await {
        tracing::warn!(error=?e, stream=%stream.id, "stream route publish failed");
    }
    let hls_path = format!("/hls/{}/index.m3u8", stream.id);
    let was_live = matches!(stream.status, StreamStatus::Live);
    if let Err(e) = s.streams.mark_live(stream.id, &hls_path).await {
        tracing::warn!(error=?e, "mark live failed");
    } else if !was_live {
        // Announce the go-live on the live bus so the out-of-band golive_bot can fan
        // out "went live" notices to the creator's followers (durable activity feed),
        // without touching this ingest hot path. Only on the idle/ended->live edge,
        // so a republish of an already-live stream does not re-notify. Best-effort.
        // Funnels through LiveService so the event carries the publish-time `"seq"`
        // stamp like every other StreamEvent (ROADMAP 第三版 方向一).
        s.live.publish_go_live(stream.id).await;
        // Fire outgoing webhooks for the stream.live event on the stream's room
        // (if the stream is room-bound). Best-effort: any error is logged and
        // swallowed so the WHIP ingest path is never delayed.
        if let Some(room_id) = stream.room_id {
            let webhook_repo = aero_storage::WebhookRepo::new(s.pg.clone());
            let delivery_repo = aero_storage::WebhookDeliveryRepo::new(s.pg.clone());
            match webhook_repo.list_outgoing_for_room_event(room_id, "stream.live").await {
                Ok(targets) => {
                    let payload = serde_json::json!({
                        "kind": "stream.live",
                        "stream_id": stream.id.to_string(),
                        "title": stream.title,
                        "owner_id": stream.owner_id,
                    });
                    let now = time::OffsetDateTime::now_utc().unix_timestamp();
                    let sender = aero_storage::ReqwestSender::new();
                    for target in targets {
                        let delivery = aero_storage::build_delivery(
                            &target.url,
                            &target.secret,
                            &payload,
                            now,
                        );
                        let event_id = Some(stream.id.to_string());
                        match delivery_repo.record_attempt(target.id, event_id.as_deref()).await {
                            // `None` = this stream.live event was already delivered to
                            // this endpoint (idempotent claim); skip the duplicate POST.
                            Ok(None) => {}
                            Ok(Some(delivery_id)) => {
                                use aero_storage::WebhookSender;
                                match sender.deliver(&delivery).await {
                                    // Delivery-log bookkeeping keys on the numeric
                                    // status (this one-off fire has no breaker).
                                    Ok(resp) if (200..300).contains(&resp.status) => {
                                        let _ = delivery_repo
                                            .mark_delivered(delivery_id, i32::from(resp.status))
                                            .await;
                                    }
                                    Ok(resp) => {
                                        let _ = delivery_repo
                                            .mark_failed_with_backoff(
                                                delivery_id,
                                                1,
                                                Some(i32::from(resp.status)),
                                                "non-2xx response",
                                            )
                                            .await;
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            error = ?e,
                                            hook = %target.id,
                                            "stream.live webhook delivery failed"
                                        );
                                        let _ = delivery_repo
                                            .mark_failed_with_backoff(
                                                delivery_id,
                                                1,
                                                None,
                                                "transport error",
                                            )
                                            .await;
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = ?e,
                                    hook = %target.id,
                                    "stream.live webhook record_attempt failed"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = ?e, stream = %stream.id, "stream.live webhook lookup failed");
                }
            }
        }
    }
    let mut resp = (
        StatusCode::CREATED,
        [(header::CONTENT_TYPE, "application/sdp")],
        resource.answer_sdp.clone(),
    )
        .into_response();
    resp.headers_mut().insert(
        header::LOCATION,
        // The WHIP resource is addressed by the publisher's stream KEY (the secret
        // ingest credential), NOT the public stream id — so only the publisher can
        // tear the session down (see whip_delete).
        HeaderValue::from_str(&format!("/whip/resource/{}", stream.stream_key))
            .unwrap_or_else(|_| HeaderValue::from_static("/whip/resource")),
    );
    Ok(resp)
}

async fn whip_delete(
    State(s): State<AppState>,
    Path(stream_key): Path<String>,
) -> ApiResult<StatusCode> {
    // Identify the resource by the publisher's stream KEY, not the public stream
    // id: the id appears in HLS/WHEP playback URLs, so keying the teardown on it
    // let ANYONE end any live stream (DoS). Possession of the key — the same secret
    // that authorized publishing — authorizes ending it.
    let stream = s
        .streams
        .get_by_key(&stream_key)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("stream".into()))?;
    s.whip.remove(stream.id);
    if let Err(e) = s.stream_routes.unpublish(stream.id).await {
        tracing::warn!(error=?e, stream=%stream.id, "stream route unpublish failed");
    }
    if let Err(e) = s.streams.mark_ended(stream.id).await {
        tracing::warn!(error=?e, "mark ended failed");
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn whep_post(
    State(s): State<AppState>,
    Path(stream_id_str): Path<String>,
    sdp_offer: String,
) -> ApiResult<axum::response::Response> {
    use axum::http::{header, StatusCode};
    let stream_id = ulid::Ulid::from_str(&stream_id_str)
        .map_err(|e| AeroError::Invalid(format!("stream id: {e}")))?;
    // A live publisher must exist for there to be anything to play back. If it
    // isn't on THIS node, the stream may be ingested elsewhere: sticky routing
    // (ROADMAP 方向二) redirects the viewer to the owning node, so a stream
    // ingested on node A is playable from node B without inter-node media relay.
    if s.whip.get(stream_id).is_none() {
        let located = s
            .stream_routes
            .locate(stream_id)
            .await
            .map_err(|e| AeroError::Internal(anyhow::anyhow!("stream route lookup: {e}")))?;
        if let Some(home) = aero_storage::redirect_base(&s.public_base_url, located.as_deref()) {
            use axum::http::{header, HeaderValue};
            let target = format!("{}/whep/{}", home.trim_end_matches('/'), stream_id);
            let mut resp = StatusCode::TEMPORARY_REDIRECT.into_response();
            resp.headers_mut().insert(
                header::LOCATION,
                HeaderValue::from_str(&target)
                    .map_err(|e| AeroError::Internal(anyhow::anyhow!("redirect target: {e}")))?,
            );
            return Ok(resp);
        }
        return Err(AeroError::NotFound("no live publisher".into()).into());
    }
    // Negotiate a real WHEP *sendonly* SDP answer for the viewer's recvonly offer
    // (str0m via `WhepSession`). NOTE: the WHIP->WHEP media relay — forwarding the
    // publisher's RTP into this egress session — and browser playback are not yet
    // wired; that path requires a live publisher + browser (absent from CI).
    let answer = accept_whep_offer(&sdp_offer, &s.ingest_host, s.ingest_port).map_err(|e| match e {
        SessionError::Offer(m) => AeroError::Invalid(format!("whep offer: {m}")),
        SessionError::Addr(h, p) => {
            AeroError::Internal(anyhow::anyhow!("whep egress addr {h}:{p}"))
        }
        other => AeroError::Internal(anyhow::anyhow!(other.to_string())),
    })?;
    let resp = (
        StatusCode::CREATED,
        [(header::CONTENT_TYPE, "application/sdp")],
        answer,
    )
        .into_response();
    Ok(resp)
}

