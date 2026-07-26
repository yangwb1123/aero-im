// ----- Messages -----

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
    let id = MessageId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let m = s
        .messages
        .get(id)
        .await?
        .filter(|m| m.deleted_at.is_none())
        .ok_or_else(|| AeroError::NotFound(format!("message {id}")))?;
    s.im.assert_room_access(auth.participant_id, m.room_id).await?;
    Ok(Json(serde_json::to_value(m).map_err(AeroError::from)?))
}

async fn edit_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<EditMessageReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = MessageId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let m = s
        .im
        .edit_message(auth.participant_id, id, req.blocks, req.expected_version)
        .await?;
    Ok(Json(serde_json::to_value(m).map_err(AeroError::from)?))
}

async fn delete_message(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<StatusCode> {
    let id = MessageId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let started = std::time::Instant::now();

    // Pre-fetch the message before the delete so the audit trail can record
    // the room AND a content digest (the blocks are cleared by the soft-delete,
    // so this is the only chance to capture "what was deleted").
    let Some(pre) = s.messages.get(id).await.map_err(AeroError::from)? else {
        return Err(AeroError::NotFound(format!("message {id}")).into());
    };
    let room_id = pre.room_id;
    // Char-boundary-safe 120-char summary so the audit row stays compact.
    let digest: String = pre.searchable_text().chars().take(120).collect();

    match s.rooms.room_workspace(room_id).await.map_err(AeroError::from)? {
        // Tenant-owned room: transactional delete + audit (ROADMAP 第三版 方向五
        // 审计事务化) — if the audit row can't be written the delete rolls back
        // and the client gets a 5xx, never a silently-unaudited delete.
        Some(ws) => {
            // Mirrors `ImService::delete_message` authorization exactly:
            // re-deleting is an idempotent no-op (checked FIRST, so it never
            // 403s), then only the sender may delete.
            if pre.deleted_at.is_some() {
                return Ok(StatusCode::NO_CONTENT);
            }
            if pre.sender_id != auth.participant_id {
                return Err(AeroError::Forbidden("only sender may delete".into()).into());
            }
            let detail = serde_json::json!({ "room_id": room_id, "digest": digest });
            let deleted = s
                .messages
                .soft_delete_audited(id, ws, Some(auth.participant_id), detail)
                .await
                .map_err(AeroError::from)?;
            // `false` = lost a race with a concurrent delete — already gone, so
            // no event/metric replay (the winner emitted them).
            if deleted {
                s.im.broadcast_room_event(
                    room_id,
                    aero_common::RoomEvent::Deleted {
                        room_id,
                        message_id: id,
                        by: auth.participant_id,
                    },
                )
                .await;
                aero_common::metrics::inc_counter(
                    aero_common::metrics::names::MESSAGES_DELETED_TOTAL,
                    1,
                );
                // Metric parity with `ImService::delete_message`, which times
                // the legacy (non-audited) path under the same label.
                aero_common::metrics::observe_histogram_labeled(
                    aero_common::metrics::names::MESSAGE_PROCESSING_DURATION_SECONDS,
                    started.elapsed().as_secs_f64(),
                    &[("op", "delete")],
                );
            }
        }
        // Legacy room with no owning workspace: there is no audit trail to write
        // into, so keep the original (service) delete path unchanged.
        None => s.im.delete_message(auth.participant_id, id).await?,
    }

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
    let id = MessageId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    let op = s.im.toggle_reaction(auth.participant_id, id, &req.emoji).await?;
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
    let summaries = s.im.reactions_for_accessible(auth.participant_id, &ids).await?;
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

