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
    validate_whip_ingest(stream.protocol, stream.status)?;
    let (socket, candidate_addr) =
        crate::whip_media::bind_media_socket(&s.ingest_host, s.ingest_port)
            .await
            .map_err(|error| AeroError::Internal(anyhow::anyhow!(error)))?;
    let candidate_host = candidate_addr.ip().to_string();
    let (session, answer) = WhipSession::accept(&sdp_offer, &candidate_host, candidate_addr.port())
        .map_err(map_whip_session_error)?;
    debug_assert_eq!(session.local_addr(), candidate_addr);
    let resource = WhipResource::from_answer(stream.id, answer.to_sdp_string());
    let relay = std::sync::Arc::new(MediaRelay::new());
    let registration = s
        .whip
        .register_publisher(resource.clone(), relay.clone(), candidate_addr, &s.shutdown)
        .map_err(|_| AeroError::Conflict("publisher present".into()))?;
    let stream_dir = s.hls_dir.join(stream.id.to_string());
    if let Err(error) = tokio::fs::create_dir_all(&stream_dir).await {
        s.whip.finish_publisher(stream.id, registration.resource_id);
        return Err(AeroError::Internal(anyhow::anyhow!(
            "create WHIP HLS directory {}: {error}",
            stream_dir.display()
        ))
        .into());
    }
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    let (cleanup_tx, cleanup_rx) = tokio::sync::oneshot::channel();
    let task_registry = s.whip.clone();
    let task_streams = s.streams.clone();
    let task_routes = s.stream_routes.clone();
    let task_cancel = registration.cancel;
    let task_resource_id = registration.resource_id;
    let task_stream_id = stream.id;
    let segment_duration_secs = u32::try_from(aero_live_rtmp::SEGMENT_DURATION_SECS)
        .expect("RTMP HLS segment duration must fit u32");
    s.runtime_tasks.spawn(async move {
        // The sender is released only after this request wins the atomic PG
        // live transition.  Waiting for it (rather than racing shutdown) makes
        // cleanup ownership unambiguous: a competing protocol makes the sender
        // drop and this task must not mark that winner ended.
        let should_run = start_rx.await.is_ok();
        let result = if should_run {
            Box::pin(session.with_relay(relay).run_to_hls_until_cancelled(
                socket,
                stream_dir,
                segment_duration_secs,
                task_cancel.cancelled(),
            ))
            .await
        } else {
            Ok(0)
        };
        // This task is the sole owner of publisher rollback. Keep the current
        // generation reserved until Redis/PG are cleaned so an old task cannot
        // unpublish or mark-ended a replacement publisher.
        if task_registry.cancel_publisher(task_stream_id, task_resource_id) {
            if should_run {
                if let Err(error) = task_routes.unpublish(task_stream_id).await {
                    tracing::warn!(
                        ?error,
                        stream = %task_stream_id,
                        "WHIP task cleanup failed to unpublish stream route"
                    );
                }
                if let Err(error) = task_streams.mark_ended(task_stream_id).await {
                    tracing::warn!(
                        ?error,
                        stream = %task_stream_id,
                        "WHIP task cleanup failed to mark stream ended"
                    );
                }
            }
            task_registry.finish_publisher(task_stream_id, task_resource_id);
        }
        let _ = cleanup_tx.send(());
        match result {
            Ok(segments) => tracing::debug!(
                stream = %task_stream_id,
                segments,
                "WHIP media task ended"
            ),
            Err(error) => tracing::warn!(
                ?error,
                stream = %task_stream_id,
                "WHIP media task failed"
            ),
        }
    });
    let hls_path = format!("/hls/{}/index.m3u8", stream.id);
    let transition = s.streams.mark_live(stream.id, &hls_path).await;
    let mark_error = match transition {
        Ok(aero_storage::MarkLiveOutcome::Started(_)) => None,
        Ok(aero_storage::MarkLiveOutcome::AlreadyLive) => {
            Some(AeroError::Conflict("publisher present".into()))
        }
        Ok(aero_storage::MarkLiveOutcome::NotFound) => Some(AeroError::NotFound("stream".into())),
        Err(error) => Some(AeroError::Internal(anyhow::anyhow!(
            "mark WHIP stream live: {error}"
        ))),
    };
    if let Some(error) = mark_error {
        s.whip.cancel_publisher(stream.id, task_resource_id);
        drop(start_tx);
        let _ = cleanup_rx.await;
        return Err(error.into());
    }
    // Sticky routing (ROADMAP 方向二): advertise that THIS node now ingests the
    // stream, so WHEP pulls landing on other nodes can be redirected here.
    if let Err(e) = s.stream_routes.publish(stream.id, &s.public_base_url).await {
        tracing::warn!(error=?e, stream=%stream.id, "stream route publish failed");
    }
    if start_tx.send(()).is_err() {
        s.whip.cancel_publisher(stream.id, task_resource_id);
        let _ = cleanup_rx.await;
        if let Err(error) = s.stream_routes.unpublish(stream.id).await {
            tracing::warn!(
                ?error,
                stream = %stream.id,
                "failed to roll back WHIP stream route after media task start failure"
            );
        }
        if let Err(error) = s.streams.mark_ended(stream.id).await {
            tracing::warn!(
                ?error,
                stream = %stream.id,
                "failed to roll back WHIP live state after media task start failure"
            );
        }
        return Err(
            AeroError::Internal(anyhow::anyhow!("WHIP media task stopped before start")).into(),
        );
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
    if let Some(publisher) = s.whip.publisher(stream.id) {
        s.whip
            .cancel_publisher(stream.id, publisher.resource.resource_id);
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
    let Some(publisher) = s.whip.publisher(stream_id) else {
        let located = s
            .stream_routes
            .locate(stream_id)
            .await
            .map_err(|e| AeroError::Internal(anyhow::anyhow!("stream route lookup: {e}")))?;
        if let Some(target) = remote_whep_target(&s.public_base_url, located.as_deref(), stream_id)
        {
            use axum::http::{header, HeaderValue};
            let mut resp = StatusCode::TEMPORARY_REDIRECT.into_response();
            resp.headers_mut().insert(
                header::LOCATION,
                HeaderValue::from_str(&target)
                    .map_err(|e| AeroError::Internal(anyhow::anyhow!("redirect target: {e}")))?,
            );
            return Ok(resp);
        }
        return Err(AeroError::NotFound("no live publisher".into()).into());
    };
    let (socket, candidate_addr) =
        crate::whip_media::bind_media_socket(&s.ingest_host, s.ingest_port)
            .await
            .map_err(|error| AeroError::Internal(anyhow::anyhow!(error)))?;
    let candidate_host = candidate_addr.ip().to_string();
    let (session, answer) = WhepSession::accept(&sdp_offer, &candidate_host, candidate_addr.port())
        .map_err(map_whep_session_error)?;
    debug_assert_eq!(session.local_addr(), candidate_addr);
    let viewer_id = ulid::Ulid::new();
    let location =
        axum::http::HeaderValue::from_str(&format!("/whep/resource/{stream_id}/{viewer_id}"))
            .map_err(|error| {
                AeroError::Internal(anyhow::anyhow!("WHEP resource location: {error}"))
            })?;
    let registration = s
        .whip
        .register_viewer(
            stream_id,
            publisher.resource.resource_id,
            viewer_id,
            candidate_addr,
        )
        .ok_or_else(|| AeroError::NotFound("no live publisher".into()))?;
    let source = registration.relay.subscribe();
    let task_registry = s.whip.clone();
    let task_cancel = registration.cancel;
    let publisher_resource_id = registration.publisher_resource_id;
    s.runtime_tasks.spawn(async move {
        let mut run = Box::pin(session.run(socket, source));
        let result = tokio::select! {
            () = task_cancel.cancelled() => None,
            result = &mut run => Some(result),
        };
        task_registry.finish_viewer(stream_id, publisher_resource_id, viewer_id);
        if let Some(Err(error)) = result {
            tracing::warn!(
                ?error,
                stream = %stream_id,
                viewer = %viewer_id,
                "WHEP media task failed"
            );
        } else {
            tracing::debug!(
                stream = %stream_id,
                viewer = %viewer_id,
                "WHEP media task ended"
            );
        }
    });
    let mut resp = (
        StatusCode::CREATED,
        [(header::CONTENT_TYPE, "application/sdp")],
        answer.to_sdp_string(),
    )
        .into_response();
    resp.headers_mut().insert(header::LOCATION, location);
    Ok(resp)
}

async fn whep_delete(
    State(s): State<AppState>,
    Path((stream_id, viewer_id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let stream_id = ulid::Ulid::from_str(&stream_id)
        .map_err(|error| AeroError::Invalid(format!("stream id: {error}")))?;
    let viewer_id = ulid::Ulid::from_str(&viewer_id)
        .map_err(|error| AeroError::Invalid(format!("viewer id: {error}")))?;
    if let Some(publisher) = s.whip.publisher(stream_id) {
        s.whip
            .cancel_viewer(stream_id, publisher.resource.resource_id, viewer_id);
    }
    Ok(StatusCode::NO_CONTENT)
}

fn map_whip_session_error(error: SessionError) -> AeroError {
    match error {
        SessionError::Offer(message) => AeroError::Invalid(format!("whip offer: {message}")),
        other => AeroError::Internal(anyhow::anyhow!(other.to_string())),
    }
}

fn validate_whip_ingest(protocol: StreamProtocol, status: StreamStatus) -> aero_common::Result<()> {
    if protocol != StreamProtocol::Whip {
        return Err(AeroError::Conflict(format!(
            "stream is configured for {protocol:?} ingest"
        )));
    }
    if status == StreamStatus::Live {
        return Err(AeroError::Conflict(
            "stream already has an active publisher".into(),
        ));
    }
    Ok(())
}

fn remote_whep_target(
    local_base: &str,
    located: Option<&str>,
    stream_id: ulid::Ulid,
) -> Option<String> {
    aero_storage::redirect_base(local_base, located)
        .map(|home| format!("{}/whep/{stream_id}", home.trim_end_matches('/')))
}

fn map_whep_session_error(error: SessionError) -> AeroError {
    match error {
        SessionError::Offer(message) => AeroError::Invalid(format!("whep offer: {message}")),
        other => AeroError::Internal(anyhow::anyhow!(other.to_string())),
    }
}
