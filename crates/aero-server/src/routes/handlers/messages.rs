// ----- Messages -----

#[derive(Deserialize)]
struct CreateMessageReq {
    blocks: Vec<aero_common::Block>,
    #[serde(default)]
    reply_to: Option<MessageId>,
    #[serde(default)]
    expires_after_secs: Option<u64>,
    /// Body-level equivalent of the standard `Idempotency-Key` header. UUIDs
    /// are shared with the WebSocket `client_message_id` ledger.
    #[serde(default)]
    client_message_id: Option<uuid::Uuid>,
}

/// `POST /api/rooms/:id/messages` — the REST counterpart of both WS send
/// frames. Access, workspace rate budget, slow mode, TTL validation,
/// idempotency, outbox publication, and durable side effects all use the exact
/// same dispatch function.
async fn create_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    headers: axum::http::HeaderMap,
    Json(req): Json<CreateMessageReq>,
) -> ApiResult<Response> {
    let room = parse_room_id(&room_str)?;
    // Keep the route-local tenant guard explicit for authz review/lint. The
    // shared WS/REST dispatcher repeats it so non-HTTP callers cannot bypass
    // the invariant.
    s.im.assert_room_access(auth.participant_id, room).await?;
    let client_message_id = resolve_message_idempotency_key(&headers, req.client_message_id)?;

    let outcome = crate::ws::send_blocks_frame(
        &s,
        auth.participant_id,
        room,
        req.blocks,
        req.reply_to,
        req.expires_after_secs,
        client_message_id,
    )
    .await?;
    let status = if outcome.deduplicated {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    let location = format!("/api/messages/{}", outcome.message.id);
    let mut response = (
        status,
        Json(serde_json::to_value(&outcome.message).map_err(AeroError::from)?),
    )
        .into_response();
    response.headers_mut().insert(
        "idempotency-replayed",
        HeaderValue::from_static(if outcome.deduplicated {
            "true"
        } else {
            "false"
        }),
    );
    if let Ok(value) = HeaderValue::from_str(&location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    Ok(response)
}

fn resolve_message_idempotency_key(
    headers: &axum::http::HeaderMap,
    body_key: Option<uuid::Uuid>,
) -> Result<Option<uuid::Uuid>, AeroError> {
    let header_key = headers
        .get("idempotency-key")
        .map(|value| {
            value
                .to_str()
                .map_err(|_| AeroError::Invalid("Idempotency-Key must be an ASCII UUID".into()))
                .and_then(|value| {
                    uuid::Uuid::parse_str(value.trim())
                        .map_err(|_| AeroError::Invalid("Idempotency-Key must be a UUID".into()))
                })
        })
        .transpose()?;
    match (body_key, header_key) {
        (Some(body), Some(header)) if body != header => Err(AeroError::Invalid(
            "client_message_id and Idempotency-Key must match when both are supplied".into(),
        )),
        (Some(value), _) | (_, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

#[derive(Deserialize)]
struct EditMessageReq {
    blocks: Vec<aero_common::Block>,
    /// Optimistic-lock check (migration 0157): the `version` the client last
    /// saw this message at. A concurrent edit that already bumped the version
    /// past this fails with 409 instead of silently overwriting it. Omitted by
    /// clients that haven't adopted the check yet — falls back to unprotected
    /// last-write-wins, matching pre-versioning behavior.
    #[serde(default)]
    expected_version: Option<i32>,
}

/// `GET /api/messages/:id` — fetch a single (non-deleted) message, gated on the
/// caller's access to its room. Backs deep-links/permalinks and matches the
/// operation the OpenAPI spec advertises. 404 when the message is missing or
/// soft-deleted; 403 when the caller can't see its room.
async fn get_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id =
        MessageId::from_str(&id_str).map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let m = s
        .messages
        .get(id)
        .await?
        .filter(|m| m.deleted_at.is_none())
        .ok_or_else(|| AeroError::NotFound(format!("message {id}")))?;
    s.im.assert_room_access(auth.participant_id, m.room_id)
        .await?;
    Ok(Json(serde_json::to_value(m).map_err(AeroError::from)?))
}

async fn edit_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<EditMessageReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id =
        MessageId::from_str(&id_str).map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let room =
        s.im.assert_message_edit_preflight(auth.participant_id, id)
            .await?;
    aero_im_core::validate_blocks(&req.blocks)?;
    crate::ws_rate::check_ws_rate_room(&s, room).await?;
    let slowmode =
        crate::message_send_policy::reserve_slowmode(&s, auth.participant_id, room).await?;
    let result =
        s.im.edit_message(auth.participant_id, id, req.blocks, req.expected_version)
            .await;
    let m = slowmode.finish(result).await?;
    Ok(Json(serde_json::to_value(m).map_err(AeroError::from)?))
}

async fn delete_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<StatusCode> {
    let id =
        MessageId::from_str(&id_str).map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    // REST and WebSocket deliberately converge on the same service method:
    // tenant access, author authorization, audit, tombstone, and durable event
    // append are one invariant instead of two subtly different implementations.
    s.im.delete_message(auth.participant_id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ToggleReactionReq {
    emoji: String,
}

async fn toggle_reaction(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<ToggleReactionReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id =
        MessageId::from_str(&id_str).map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let op =
        s.im.toggle_reaction(auth.participant_id, id, &req.emoji)
            .await?;
    Ok(Json(serde_json::json!({
        "message_id": id,
        "emoji": req.emoji,
        "op": op,
    })))
}

#[derive(Deserialize)]
struct ReactionsBatchReq {
    message_ids: Vec<String>,
}

/// Max message ids one reactions-batch request may resolve. The gateway body cap
/// bounds the request loosely; this is the explicit per-request ceiling so a
/// single call can't fan into an arbitrarily large `ANY($1)` scan.
const MAX_REACTIONS_BATCH: usize = 256;

async fn reactions_batch(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<ReactionsBatchReq>,
) -> ApiResult<Json<serde_json::Value>> {
    if req.message_ids.len() > MAX_REACTIONS_BATCH {
        return Err(AeroError::Invalid(format!(
            "too many message_ids: {} (max {MAX_REACTIONS_BATCH})",
            req.message_ids.len()
        ))
        .into());
    }
    let ids: Vec<MessageId> = req
        .message_ids
        .iter()
        .map(|s| MessageId::from_str(s))
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    // Membership-scoped: only reactions on messages in rooms the caller belongs to
    // (the storage JOIN room_members is the boundary). A message id the caller
    // can't access is silently absent — no cross-room reaction-count / reactor-id
    // leak (IDOR).
    let summaries =
        s.im.reactions_for_accessible(auth.participant_id, &ids)
            .await?;
    let summaries_json: serde_json::Map<String, serde_json::Value> = summaries
        .into_iter()
        .map(|(mid, list)| {
            (
                mid.to_string(),
                serde_json::to_value(list).unwrap_or(serde_json::Value::Null),
            )
        })
        .collect();
    Ok(Json(serde_json::Value::Object(summaries_json)))
}

#[cfg(test)]
mod create_message_tests {
    use super::*;

    #[test]
    fn idempotency_header_and_body_share_one_uuid_contract() {
        let key = uuid::Uuid::new_v4();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "idempotency-key",
            HeaderValue::from_str(&format!(" {key} ")).unwrap(),
        );
        assert_eq!(
            resolve_message_idempotency_key(&headers, None).unwrap(),
            Some(key)
        );
        assert_eq!(
            resolve_message_idempotency_key(&headers, Some(key)).unwrap(),
            Some(key)
        );
        assert!(resolve_message_idempotency_key(&headers, Some(uuid::Uuid::new_v4())).is_err());
    }

    #[test]
    fn malformed_idempotency_header_is_rejected() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("idempotency-key", HeaderValue::from_static("not-a-uuid"));
        assert!(resolve_message_idempotency_key(&headers, None).is_err());
        assert_eq!(
            resolve_message_idempotency_key(&axum::http::HeaderMap::new(), None).unwrap(),
            None
        );
    }
}
