// ---------- Bot event subscriptions ----------

#[derive(serde::Deserialize)]
struct CreateSubReq {
    event_type: String,
    #[serde(default)]
    filters: Option<serde_json::Value>,
    #[serde(default)]
    webhook_url: Option<String>,
}

/// Subscribe a bot to an event type.
///
/// Only the bot's owner may add subscriptions.
async fn bot_create_subscription(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<CreateSubReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let bot_id = ParticipantId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("bot id: {e}")))?;
    ensure_bot_owner(&s.pg, bot_id, auth.participant_id).await?;
    // SSRF guard: `webhook_url` is fetched by the SERVER (bot_dispatch.rs) on every
    // matching event, so — exactly like the room-level outgoing webhook gate — a
    // destination resolving to loopback/private/link-local (incl. the cloud
    // metadata IP) must be rejected at creation. `None` (WS-delivered bot) skips
    // the check entirely; there's no URL to validate.
    if let Some(url) = req.webhook_url.as_deref() {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(AeroError::Invalid("webhook_url must be http(s)".into()).into());
        }
        crate::webhooks::assert_webhook_url_safe(url).await?;
    }
    let repo = aero_storage::BotRepo::new(s.pg.clone());
    let sub_id = repo
        .subscribe(bot_id, &req.event_type, req.filters.as_ref(), req.webhook_url.as_deref())
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({ "id": sub_id, "event_type": req.event_type })))
}

/// List a bot's event subscriptions.
///
/// Only the bot's owner may view its subscriptions (they can embed
/// `webhook_url` secrets).
async fn bot_list_subscriptions(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let bot_id = ParticipantId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("bot id: {e}")))?;
    ensure_bot_owner(&s.pg, bot_id, auth.participant_id).await?;
    let repo = aero_storage::BotRepo::new(s.pg.clone());
    let subs = repo.list_subscriptions(bot_id).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(subs).map_err(AeroError::from)?))
}

/// Delete a bot's event subscription.
///
/// Only the bot's owner may delete its subscriptions.
async fn bot_delete_subscription(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((bot_str, sub_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let bot_id = ParticipantId::from_str(&bot_str)
        .map_err(|e| AeroError::Invalid(format!("bot id: {e}")))?;
    let sub_id = Uuid::from_str(&sub_str)
        .map_err(|e| AeroError::Invalid(format!("subscription id: {e}")))?;
    ensure_bot_owner(&s.pg, bot_id, auth.participant_id).await?;
    let repo = aero_storage::BotRepo::new(s.pg.clone());
    let deleted = repo.delete_subscription(sub_id, bot_id).await.map_err(AeroError::from)?;
    if !deleted {
        return Err(AeroError::NotFound("subscription".into()).into());
    }
    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// Query for [`bot_list_deliveries`]: an optional `limit` (clamped in storage).
#[derive(serde::Deserialize)]
struct DeliveriesQuery {
    #[serde(default)]
    limit: Option<i64>,
}

/// List a bot's recent webhook delivery records (migration 0147), newest first.
///
/// Surfaces the previously-invisible bot-subscription delivery outcomes the
/// dispatcher now records (per-attempt `delivered`/`failed` + HTTP status +
/// error), so a bot owner can see whether their subscriptions are actually
/// reaching their `webhook_url`.
///
/// Only the bot's owner may view its deliveries (records correlate to the bot's
/// subscriptions, which can embed `webhook_url` secrets).
async fn bot_list_deliveries(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<DeliveriesQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let bot_id = ParticipantId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("bot id: {e}")))?;
    ensure_bot_owner(&s.pg, bot_id, auth.participant_id).await?;
    let repo = aero_storage::BotRepo::new(s.pg.clone());
    let deliveries = repo
        .list_deliveries_for_bot(bot_id, q.limit.unwrap_or(100))
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(deliveries).map_err(AeroError::from)?))
}

async fn get_participant(
    State(s): State<AppState>,
    _auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = ParticipantId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))?;
    let p = s
        .participants
        .get(id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("participant".into()))?;
    Ok(Json(serde_json::to_value(p).map_err(AeroError::from)?))
}

#[derive(Deserialize)]
struct ParticipantSearchQuery {
    q: String,
    #[serde(default)]
    limit: Option<i64>,
}

async fn search_participants(
    State(s): State<AppState>,
    _auth: AuthUser,
    Query(p): Query<ParticipantSearchQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let limit = p.limit.unwrap_or(20);
    let list = s.participants.search(&p.q, limit).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(list).map_err(AeroError::from)?))
}

async fn list_room_members(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&room_str)?;
    // Tenant guard (workspace + room membership) — supersedes the prior bare
    // room-membership check before listing the room's members.
    s.im.assert_room_access(auth.participant_id, room).await?;
    let ids = s.rooms.members(room).await.map_err(AeroError::from)?;
    // Resolve to full Participant objects.
    let mut out = Vec::with_capacity(ids.len());
    for pid in ids {
        if let Ok(Some(p)) = s.participants.get(pid).await {
            out.push(p);
        }
    }
    Ok(Json(serde_json::to_value(out).map_err(AeroError::from)?))
}

