// ---------- Bot SDK (方向三) ----------

#[derive(serde::Deserialize)]
struct CreateBotReq {
    name: String,
    #[serde(default)]
    icon_url: Option<String>,
    #[serde(default)]
    workspace_id: Option<String>,
}

const MAX_BOT_ICON_URL_BYTES: usize = 2_048;

fn validate_bot_icon_url(raw: Option<&str>) -> Result<Option<String>, AeroError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let icon_url = raw.trim();
    if icon_url.is_empty() {
        return Ok(None);
    }
    if icon_url.len() > MAX_BOT_ICON_URL_BYTES {
        return Err(AeroError::Invalid(format!(
            "icon_url must be at most {MAX_BOT_ICON_URL_BYTES} bytes"
        )));
    }
    Ok(Some(icon_url.to_owned()))
}

/// Register a new bot (participant + bot row + initial token).
async fn bot_create(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateBotReq>,
) -> ApiResult<Response> {
    let workspace = match req.workspace_id.as_deref() {
        Some(raw) => Some(
            WorkspaceId::from_str(raw.trim())
                .map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))?,
        ),
        None => None,
    };
    let name = crate::routes::agents::validate_bot_name(&req.name)?;
    let icon_url = validate_bot_icon_url(req.icon_url.as_deref())?;
    let repo = aero_storage::BotRepo::new(s.pg.clone());
    let (bot_id, token) = repo
        .create_authorized_with_token(auth.participant_id, &name, icon_url.as_deref(), workspace)
        .await?;
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

/// List the calling participant's own bots (across all workspaces).
///
/// Scoped strictly to `owner_id = auth.participant_id` — callers never see
/// bots owned by other participants.
async fn bot_list(State(s): State<AppState>, auth: AuthUser) -> ApiResult<Json<serde_json::Value>> {
    let rows = sqlx::query_as::<_, BotListRow>(
        r"SELECT id, owner_id, name, icon_url, workspace_id, token_hash IS NOT NULL AS has_token, created_at
           FROM bots
          WHERE owner_id = $1
          ORDER BY created_at DESC
          LIMIT $2",
    )
    .bind(auth.participant_id.to_uuid())
    .bind(aero_storage::MAX_BOTS_PER_OWNER)
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
    let bot_id =
        ParticipantId::from_str(&id_str).map_err(|e| AeroError::Invalid(format!("bot id: {e}")))?;
    let token = aero_storage::BotRepo::new(s.pg.clone())
        .rotate_token_authorized(bot_id, auth.participant_id)
        .await?;
    Ok(Json(serde_json::json!({ "token": token })))
}

#[cfg(test)]
mod bot_input_bound_tests {
    use super::*;

    #[test]
    fn icon_url_is_trimmed_and_byte_bounded() {
        assert_eq!(
            validate_bot_icon_url(Some("  https://example.test/icon.png  ")).unwrap(),
            Some("https://example.test/icon.png".into())
        );
        assert_eq!(validate_bot_icon_url(Some("   ")).unwrap(), None);
        assert!(validate_bot_icon_url(Some(&"x".repeat(MAX_BOT_ICON_URL_BYTES + 1))).is_err());
    }
}
