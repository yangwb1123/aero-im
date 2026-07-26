// ---------- Bot SDK (方向三) ----------

#[derive(serde::Deserialize)]
struct CreateBotReq {
    name: String,
    #[serde(default)]
    icon_url: Option<String>,
    #[serde(default)]
    workspace_id: Option<String>,
}

/// Register a new bot (participant + bot row + initial token).
async fn bot_create(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateBotReq>,
) -> ApiResult<Response> {
    let workspace = req.workspace_id.as_ref().and_then(|w| aero_common::WorkspaceId::from_str(w).ok());
    let name = crate::routes::agents::validate_bot_name(&req.name)?;
    let repo = aero_storage::BotRepo::new(s.pg.clone());
    let bot_id = repo
        .create(auth.participant_id, &name, req.icon_url.as_deref(), workspace)
        .await
        .map_err(AeroError::from)?;
    let token = repo
        .rotate_token(bot_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Internal(anyhow::anyhow!("bot created but token failed")))?;
    let body = Json(serde_json::json!({
        "bot_id": bot_id,
        "token": token,
        "name": name,
    }))
    .into_response();
    // Per-tenant HTTP metrics: stamp the resolved tenant when the request named
    // one (response-extension pass-through; bounded + flag-gated in the layer).
    Ok(match workspace {
        Some(ws) => metrics::attach_workspace_label(body, ws),
        None => body,
    })
}

/// Look up a bot's `owner_id` by its participant id, scoped to the `bots`
/// table only.  Returns `None` when no such bot exists.
///
/// Authorization helper kept in the routes layer: `BotRepo` exposes no
/// fetch-by-id, so we read just the `owner_id` column here to assert ownership
/// before any mutating bot operation. We never expose this row directly.
async fn bot_owner(
    pg: &sqlx::PgPool,
    bot_id: ParticipantId,
) -> Result<Option<ParticipantId>, AeroError> {
    let row = sqlx::query_as::<_, (Uuid,)>("SELECT owner_id FROM bots WHERE id = $1")
        .bind(bot_id.to_uuid())
        .fetch_optional(pg)
        .await
        .map_err(AeroError::from)?;
    Ok(row.map(|(o,)| ParticipantId::from_uuid(o)))
}

/// Assert that `caller` owns the bot `bot_id`.
///
/// Returns [`AeroError::NotFound`] when the bot does not exist and
/// [`AeroError::Forbidden`] when the caller is not its owner. On success the
/// caller is cleared to perform a mutating operation on the bot.
async fn ensure_bot_owner(
    pg: &sqlx::PgPool,
    bot_id: ParticipantId,
    caller: ParticipantId,
) -> Result<(), AeroError> {
    let owner = bot_owner(pg, bot_id)
        .await?
        .ok_or_else(|| AeroError::NotFound(format!("bot {bot_id}")))?;
    if owner != caller {
        return Err(AeroError::Forbidden("not the bot owner".into()));
    }
    Ok(())
}

/// List the calling participant's own bots (across all workspaces).
///
/// Scoped strictly to `owner_id = auth.participant_id` — callers never see
/// bots owned by other participants.
async fn bot_list(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let rows = sqlx::query_as::<_, BotListRow>(
        r"SELECT id, owner_id, name, icon_url, workspace_id, token_hash IS NOT NULL AS has_token, created_at
           FROM bots
          WHERE owner_id = $1
          ORDER BY created_at DESC",
    )
    .bind(auth.participant_id.to_uuid())
    .fetch_all(&s.pg)
    .await
    .map_err(AeroError::from)?;
    let bots: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": ParticipantId::from_uuid(r.id),
                "owner_id": ParticipantId::from_uuid(r.owner_id),
                "name": r.name,
                "icon_url": r.icon_url,
                "workspace_id": r.workspace_id.map(WorkspaceId::from_uuid),
                "has_token": r.has_token,
                "created_at": r.created_at,
            })
        })
        .collect();
    Ok(Json(serde_json::Value::Array(bots)))
}

/// Row shape for [`bot_list`] (mirrors `storage::bot::BotRow`, kept local so
/// this stays a routes-only change).
#[derive(sqlx::FromRow)]
struct BotListRow {
    id: Uuid,
    owner_id: Uuid,
    name: String,
    icon_url: Option<String>,
    workspace_id: Option<Uuid>,
    has_token: bool,
    created_at: time::OffsetDateTime,
}

/// Rotate a bot's token (revokes the old one, returns the new plaintext).
///
/// Only the bot's owner may rotate its token.
async fn bot_rotate_token(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let bot_id = ParticipantId::from_str(&id_str)
        .map_err(|e| AeroError::Invalid(format!("bot id: {e}")))?;
    ensure_bot_owner(&s.pg, bot_id, auth.participant_id).await?;
    let repo = aero_storage::BotRepo::new(s.pg.clone());
    let token = repo
        .rotate_token(bot_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("bot {bot_id}")))?;
    Ok(Json(serde_json::json!({ "token": token })))
}

