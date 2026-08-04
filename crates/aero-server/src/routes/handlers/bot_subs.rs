// ---------- Bot event subscriptions ----------

#[derive(serde::Deserialize)]
struct CreateSubReq {
    event_type: String,
    #[serde(default)]
    filters: Option<serde_json::Value>,
    #[serde(default)]
    webhook_url: Option<String>,
}

const MAX_BOT_EVENT_TYPE_BYTES: usize = 64;
const MAX_BOT_FILTER_JSON_BYTES: usize = 4_096;
const MAX_BOT_FILTER_ID_BYTES: usize = 64;
const MAX_BOT_ACTION_ID_BYTES: usize = 256;
const MAX_BOT_WEBHOOK_URL_BYTES: usize = 2_048;

fn normalize_bot_event_type(raw: &str) -> Result<String, AeroError> {
    let event_type = raw.trim();
    if event_type.is_empty() || event_type.len() > MAX_BOT_EVENT_TYPE_BYTES {
        return Err(AeroError::Invalid(format!(
            "event_type must be between 1 and {MAX_BOT_EVENT_TYPE_BYTES} bytes"
        )));
    }
    if !event_type
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(AeroError::Invalid(
            "event_type must contain only lowercase ASCII letters, digits, or '_'".into(),
        ));
    }
    Ok(event_type.to_owned())
}

fn normalize_bot_filters(
    filters: Option<serde_json::Value>,
) -> Result<serde_json::Value, AeroError> {
    let filters = filters.unwrap_or_else(|| serde_json::json!({}));
    if serde_json::to_vec(&filters).map_err(AeroError::from)?.len() > MAX_BOT_FILTER_JSON_BYTES {
        return Err(AeroError::Invalid(format!(
            "filters must be at most {MAX_BOT_FILTER_JSON_BYTES} bytes"
        )));
    }
    let mut object = filters
        .as_object()
        .cloned()
        .ok_or_else(|| AeroError::Invalid("filters must be a JSON object".into()))?;
    for key in object.keys() {
        if !matches!(key.as_str(), "room_id" | "workspace_id" | "action_id") {
            return Err(AeroError::Invalid(format!(
                "unsupported filters key '{key}'"
            )));
        }
    }
    for key in ["room_id", "workspace_id"] {
        if let Some(value) = object.get_mut(key) {
            let raw = value
                .as_str()
                .ok_or_else(|| AeroError::Invalid(format!("filters.{key} must be a string")))?;
            let normalized = raw.trim();
            if normalized.is_empty() || normalized.len() > MAX_BOT_FILTER_ID_BYTES {
                return Err(AeroError::Invalid(format!(
                    "filters.{key} must be between 1 and {MAX_BOT_FILTER_ID_BYTES} bytes"
                )));
            }
            *value = serde_json::Value::String(normalized.to_owned());
        }
    }
    if let Some(value) = object.get_mut("action_id") {
        let raw = value
            .as_str()
            .ok_or_else(|| AeroError::Invalid("filters.action_id must be a string".into()))?;
        let normalized = raw.trim();
        if normalized.is_empty() || normalized.len() > MAX_BOT_ACTION_ID_BYTES {
            return Err(AeroError::Invalid(format!(
                "filters.action_id must be between 1 and {MAX_BOT_ACTION_ID_BYTES} bytes"
            )));
        }
        *value = serde_json::Value::String(normalized.to_owned());
    }
    Ok(serde_json::Value::Object(object))
}

fn normalize_bot_webhook_url(raw: Option<String>) -> Result<Option<String>, AeroError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let url = raw.trim();
    if url.is_empty() || url.len() > MAX_BOT_WEBHOOK_URL_BYTES {
        return Err(AeroError::Invalid(format!(
            "webhook_url must be between 1 and {MAX_BOT_WEBHOOK_URL_BYTES} bytes"
        )));
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(AeroError::Invalid("webhook_url must be http(s)".into()));
    }
    Ok(Some(url.to_owned()))
}

#[cfg(test)]
fn ensure_bot_webhook_scope(
    webhook_url: Option<&str>,
    room: Option<RoomId>,
    workspace: Option<WorkspaceId>,
) -> Result<(), AeroError> {
    if webhook_url.is_some() && room.is_none() && workspace.is_none() {
        return Err(AeroError::Invalid(
            "external webhook subscriptions require filters.room_id or filters.workspace_id".into(),
        ));
    }
    Ok(())
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
    let bot_id =
        ParticipantId::from_str(&id_str).map_err(|e| AeroError::Invalid(format!("bot id: {e}")))?;
    let event_type = normalize_bot_event_type(&req.event_type)?;
    let webhook_url = normalize_bot_webhook_url(req.webhook_url)?;
    let filters = normalize_bot_filters(req.filters)?;

    // SSRF guard: `webhook_url` is fetched by the SERVER (bot_dispatch.rs) on every
    // matching event, so — exactly like the room-level outgoing webhook gate — a
    // destination resolving to loopback/private/link-local (incl. the cloud
    // metadata IP) must be rejected at creation. `None` (WS-delivered bot) skips
    // the check entirely; there's no URL to validate.
    if let Some(url) = webhook_url.as_deref() {
        crate::webhooks::assert_webhook_url_safe(url).await?;
    }
    let (sub_id, webhook_secret) = aero_storage::BotRepo::new(s.pg.clone())
        .subscribe_authorized(
            bot_id,
            auth.participant_id,
            &event_type,
            &filters,
            webhook_url.as_deref(),
        )
        .await?;
    let mut response = serde_json::json!({
        "id": sub_id,
        "event_type": event_type,
    });
    if let Some(secret) = webhook_secret {
        response["secret"] = serde_json::Value::String(secret);
    }
    Ok(Json(response))
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
    let bot_id =
        ParticipantId::from_str(&id_str).map_err(|e| AeroError::Invalid(format!("bot id: {e}")))?;
    let subs = aero_storage::BotRepo::new(s.pg.clone())
        .list_subscriptions_authorized(bot_id, auth.participant_id)
        .await?;
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
    aero_storage::BotRepo::new(s.pg.clone())
        .delete_subscription_authorized(bot_id, sub_id, auth.participant_id)
        .await?;
    Ok(Json(serde_json::json!({ "deleted": true })))
}

/// Rotate one webhook subscription's HMAC secret.
///
/// The bot owner is the only authorized caller. The new plaintext is returned
/// exactly once and list endpoints continue to omit it.
async fn bot_rotate_subscription_secret(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((bot_str, sub_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let bot_id = ParticipantId::from_str(&bot_str)
        .map_err(|e| AeroError::Invalid(format!("bot id: {e}")))?;
    let sub_id = Uuid::from_str(&sub_str)
        .map_err(|e| AeroError::Invalid(format!("subscription id: {e}")))?;
    let secret = aero_storage::BotRepo::new(s.pg.clone())
        .rotate_subscription_secret_authorized(bot_id, sub_id, auth.participant_id)
        .await?;
    Ok(Json(serde_json::json!({
        "id": sub_id,
        "secret": secret,
    })))
}

/// Query for [`bot_list_deliveries`]: an optional `limit` (clamped in storage).
#[derive(serde::Deserialize)]
struct DeliveriesQuery {
    #[serde(default)]
    limit: Option<i64>,
}

/// List a bot's recent webhook delivery records (migration 0147), newest first.
///
/// Surfaces per-attempt `delivered`/`failed` outcomes plus durable `dead` DLQ
/// summaries, so an owner can distinguish a transient retry from terminally
/// parked delivery.
///
/// Only the bot's owner may view its deliveries (records correlate to the bot's
/// subscriptions, which can embed `webhook_url` secrets).
async fn bot_list_deliveries(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<DeliveriesQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let bot_id =
        ParticipantId::from_str(&id_str).map_err(|e| AeroError::Invalid(format!("bot id: {e}")))?;
    let deliveries = aero_storage::BotRepo::new(s.pg.clone())
        .list_deliveries_authorized(bot_id, auth.participant_id, q.limit.unwrap_or(100))
        .await?;
    Ok(Json(
        serde_json::to_value(deliveries).map_err(AeroError::from)?,
    ))
}

/// Requeue one terminal bot delivery with a fresh retry budget.
///
/// The route first asserts ownership of the bot, then the storage update scopes
/// the delivery by that same bot id and requires its current state to be
/// `dead`. The original producer event id and immutable body stay unchanged.
async fn bot_requeue_delivery(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((bot_str, delivery_str)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let bot_id = ParticipantId::from_str(&bot_str)
        .map_err(|e| AeroError::Invalid(format!("bot id: {e}")))?;
    let delivery_id = Uuid::from_str(&delivery_str)
        .map_err(|e| AeroError::Invalid(format!("delivery id: {e}")))?;
    aero_storage::BotRepo::new(s.pg.clone())
        .requeue_delivery_authorized(bot_id, delivery_id, auth.participant_id)
        .await?;
    Ok(Json(serde_json::json!({
        "id": delivery_id,
        "status": "pending",
        "attempts": 0,
    })))
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
    let list = s
        .participants
        .search(&p.q, limit)
        .await
        .map_err(AeroError::from)?;
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

#[cfg(test)]
mod bot_subscription_input_tests {
    use super::*;

    #[test]
    fn event_type_is_canonical_and_bounded() {
        assert_eq!(normalize_bot_event_type("  message  ").unwrap(), "message");
        assert!(normalize_bot_event_type("Message").is_err());
        assert!(normalize_bot_event_type(&"a".repeat(MAX_BOT_EVENT_TYPE_BYTES + 1)).is_err());
    }

    #[test]
    fn filters_reject_unknown_or_oversized_fields() {
        assert!(normalize_bot_filters(Some(serde_json::json!({ "unknown": "x" }))).is_err());
        assert!(normalize_bot_filters(Some(serde_json::json!({
            "action_id": "x".repeat(MAX_BOT_ACTION_ID_BYTES + 1)
        })))
        .is_err());
        assert!(normalize_bot_filters(Some(serde_json::json!({
            "action_id": "x".repeat(MAX_BOT_FILTER_JSON_BYTES)
        })))
        .is_err());
    }

    #[test]
    fn external_delivery_requires_explicit_tenant_scope() {
        assert!(ensure_bot_webhook_scope(Some("https://example.test"), None, None).is_err());
        assert!(
            ensure_bot_webhook_scope(Some("https://example.test"), Some(RoomId::new()), None)
                .is_ok()
        );
        assert!(ensure_bot_webhook_scope(None, None, None).is_ok());
    }

    #[test]
    fn webhook_url_is_trimmed_and_byte_bounded() {
        assert_eq!(
            normalize_bot_webhook_url(Some("  https://example.test/hook  ".into())).unwrap(),
            Some("https://example.test/hook".into())
        );
        assert!(normalize_bot_webhook_url(Some(format!(
            "https://{}",
            "x".repeat(MAX_BOT_WEBHOOK_URL_BYTES)
        )))
        .is_err());
    }
}
