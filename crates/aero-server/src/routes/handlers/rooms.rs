// ----- Rooms -----

/// The legacy / default workspace (tenant) that all pre-tenancy data was
/// backfilled into by migration `0006_workspaces.sql` — the all-zero UUID, i.e.
/// the `WorkspaceId` whose underlying u128 is 0. Single-tenant clients that omit
/// a `workspace_id` (room creation) or `?workspace_id=` (room listing) operate
/// against this workspace, so existing callers keep working unchanged while the
/// required `rooms.workspace_id` (NOT NULL, no default) is always supplied.
const DEFAULT_WORKSPACE_ID: WorkspaceId = WorkspaceId(ulid::Ulid(0));

/// Resolve the workspace for a room operation: the explicitly-requested one when
/// present, else [`DEFAULT_WORKSPACE_ID`]. Pure, so the "provided vs absent"
/// default-selection rule is unit-tested offline (Postgres absent in CI).
///
/// # Errors
/// [`AeroError::Invalid`] when a present id fails to decode as a [`WorkspaceId`].
fn resolve_workspace_id(requested: Option<&str>) -> AeroResult<WorkspaceId> {
    match requested {
        Some(raw) => WorkspaceId::from_str(raw.trim())
            .map_err(|e| AeroError::Invalid(format!("workspace id: {e}"))),
        None => Ok(DEFAULT_WORKSPACE_ID),
    }
}

#[derive(Deserialize)]
struct CreateRoomReq {
    kind: String,
    name: Option<String>,
    /// Optional tenant the channel is created in. Absent ⇒ [`DEFAULT_WORKSPACE_ID`]
    /// (keeps single-tenant clients working). The handler routes through
    /// [`ImService::create_room_in_workspace`](aero_im_core::ImService::create_room_in_workspace)
    /// either way, so the room always carries its required `workspace_id`.
    #[serde(default)]
    workspace_id: Option<String>,
}

fn validate_generic_room_kind(kind: RoomKind) -> AeroResult<RoomKind> {
    if kind == RoomKind::Direct {
        Err(AeroError::Invalid(
            "direct rooms must be created through /api/dm".into(),
        ))
    } else {
        Ok(kind)
    }
}

async fn create_room(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateRoomReq>,
) -> ApiResult<Response> {
    let kind = validate_generic_room_kind(parse_room_kind(&req.kind)?)?;
    let workspace = resolve_workspace_id(req.workspace_id.as_deref())?;
    // Normalize the name like create_workspace: trim, treat blank as "no name"
    // (DM/group rooms are legitimately unnamed), and cap length so a malformed or
    // multi-megabyte name can't be persisted + rendered. `rooms.name` is nullable.
    let name = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty());
    if let Some(n) = name {
        if n.chars().count() > 128 {
            return Err(AeroError::Invalid("room name too long (max 128 chars)".into()).into());
        }
    }
    // Tenant choke point: verifies workspace membership + channel-create privilege
    // and persists `rooms.workspace_id` (fixes the NOT-NULL room-create regression
    // the old `create_room` hit after migration 0006).
    let room =
        s.im.create_room_in_workspace(
            auth.participant_id,
            workspace,
            kind,
            name.map(str::to_owned),
        )
        .await?;
    let body = Json(serde_json::to_value(room).map_err(AeroError::from)?).into_response();
    // Per-tenant HTTP metrics (response-extension pass-through): we already
    // resolved the owning workspace above, so stamp it for `http_metrics_layer`.
    // Cheap + bounded; ignored unless `AERO_PER_TENANT_METRICS` is on.
    Ok(metrics::attach_workspace_label(body, workspace))
}

#[derive(Deserialize)]
struct ListRoomsQuery {
    /// Optional tenant scope. Present ⇒ only the caller's rooms in that workspace
    /// (`rooms_for_in_workspace`); absent ⇒ all the caller's rooms (legacy behavior).
    #[serde(default)]
    workspace_id: Option<String>,
}

async fn list_rooms(
    State(s): State<AppState>,
    auth: AuthUser,
    Query(q): Query<ListRoomsQuery>,
) -> ApiResult<Response> {
    // Scoped to a workspace when `?workspace_id=` is given; otherwise unchanged
    // (every room the caller belongs to, across tenants).
    let (rooms, scope) = match q.workspace_id.as_deref() {
        Some(raw) => {
            let ws = WorkspaceId::from_str(raw.trim())
                .map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))?;
            // A scoped listing is workspace data even when the caller happens to
            // have no room edges. Reject retained membership rows that no longer
            // satisfy account/deactivation/mandatory-2FA policy before querying.
            crate::routes::helpers::assert_effective_workspace_member(
                &s,
                ws,
                auth.participant_id,
            )
            .await?;
            let rooms = s
                .rooms
                .rooms_for_in_workspace(auth.participant_id, ws)
                .await
                .map_err(AeroError::from)?;
            // Only this branch knows a single owning tenant; the cross-tenant
            // listing below stays un-`workspace`-labeled (no marker stamped).
            (rooms, Some(ws))
        }
        None => (s.im.list_my_rooms(auth.participant_id).await?, None),
    };
    let body = Json(serde_json::to_value(rooms).map_err(AeroError::from)?).into_response();
    Ok(match scope {
        Some(ws) => metrics::attach_workspace_label(body, ws),
        None => body,
    })
}

#[derive(Deserialize)]
struct AddMemberReq {
    participant_id: String,
}

async fn add_member(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<AddMemberReq>,
) -> ApiResult<axum::http::StatusCode> {
    let room = parse_room_id(&room_str)?;
    // Tenant guard: the actor must belong to BOTH the room's workspace and the
    // room itself before they may add anyone. `ImService::add_member` re-checks
    // the actor's room membership (a distinct, retained check).
    s.im.assert_room_access(auth.participant_id, room).await?;
    let member = ParticipantId::from_str(&req.participant_id)
        .map_err(|e| AeroError::Invalid(format!("participant id: {e}")))?;
    s.im.add_member(auth.participant_id, room, member).await?;
    s.room_member_cache.invalidate(&room);
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct HistoryQuery {
    /// Backward keyset cursor (exclusive): page toward older messages.
    before: Option<String>,
    /// Forward keyset cursor (exclusive): catch up on messages created AFTER this
    /// id, ascending — the reconnect-backfill complement of `before`
    /// (ROADMAP 方向五). Mutually exclusive with `before`.
    since: Option<String>,
    limit: Option<i64>,
}

/// Default history page size when the client omits `limit`.
const DEFAULT_HISTORY_LIMIT: i64 = 100;
/// Hard ceiling on a history page, mirroring the clamp the storage keyset
/// queries (`list_recent` / `list_since`) apply. Applied here too so the cap is
/// validated at the edge and unit-testable without a database.
const MAX_HISTORY_LIMIT: i64 = 200;

/// Resolve the effective page size: default when absent, clamped into
/// `[1, MAX_HISTORY_LIMIT]`. Pure, so the cap/floor is unit-tested offline.
#[must_use]
fn history_limit(requested: Option<i64>) -> i64 {
    requested
        .unwrap_or(DEFAULT_HISTORY_LIMIT)
        .clamp(1, MAX_HISTORY_LIMIT)
}

/// Parse an optional `MessageId` cursor query param, mapping a decode failure to
/// an `Invalid` API error tagged with `field` (e.g. `"before"` / `"since"`).
fn parse_cursor(raw: Option<&str>, field: &str) -> AeroResult<Option<MessageId>> {
    raw.map(|s| MessageId::from_str(s.trim()))
        .transpose()
        .map_err(|e| AeroError::Invalid(format!("{field} id: {e}")))
}

/// Only an explicitly older, backward page tolerates replica lag. The newest
/// page and forward reconnect catch-up are read-after-write/convergence paths.
fn history_query_consistency(
    before: Option<MessageId>,
    since: Option<MessageId>,
    has_newer_visible_message: bool,
) -> aero_storage::QueryConsistency {
    if before.is_some() && since.is_none() && has_newer_visible_message {
        aero_storage::QueryConsistency::Eventual
    } else {
        aero_storage::QueryConsistency::Strong
    }
}

/// Prove on primary that a `before` cursor really denotes an older page.
///
/// Merely receiving `before=...` is not enough: a client can send a future ULID,
/// in which case `id < before` is actually the newest page. Keeping that case on
/// primary preserves the latest/read-your-writes contract without relying on
/// callers to choose a truthful cursor.
async fn has_newer_visible_message(
    pool: &sqlx::PgPool,
    room: RoomId,
    before: MessageId,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        r"SELECT EXISTS(
              SELECT 1
                FROM messages
               WHERE room_id = $1
                 AND id > $2
                 AND deleted_at IS NULL
                 AND (expires_at IS NULL OR expires_at > now())
          )",
    )
    .bind(room.to_uuid())
    .bind(before.to_uuid())
    .fetch_one(pool)
    .await
}

async fn room_history(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&room_str)?;
    // Tenant guard: workspace + room membership. Subsumes the bare room-membership
    // check the forward (`since`) branch used to do, and complements the
    // membership check `ImService::history` does on the backward (`before`) path.
    s.im.assert_room_access(auth.participant_id, room).await?;
    // Tenant fairness (ROADMAP3 方向五): charge this read against the room's
    // workspace budget — AFTER the access check so non-members cannot drain a
    // victim workspace's budget by spamming its room ids.
    crate::ws_rate::check_ws_rate_room(&s, room).await?;
    let limit = history_limit(q.limit);
    // `before` pages backward, `since` pages forward — combining them is
    // ambiguous, so reject rather than silently pick one.
    if q.before.is_some() && q.since.is_some() {
        return Err(AeroError::Invalid("before and since are mutually exclusive".into()).into());
    }
    let since = parse_cursor(q.since.as_deref(), "since")?;
    let before = parse_cursor(q.before.as_deref(), "before")?;

    if let Some(after) = since {
        // Forward catch-up (ROADMAP 方向五). Access already asserted above;
        // `ImService::history` only exposes the backward path, so read forward here.
        // KEYSET page: honour the clamped `limit` and return at most that many,
        // ascending by id. The client continues by passing the last returned id as
        // the next `since`, stopping when a page is shorter than `limit` (the web
        // client's `pullRoomSince` does exactly this). Previously `limit` was
        // discarded and the query hardcoded `LIMIT 500`, so a continuation past 500
        // silently truncated with no signal — messages 501..N were lost for any
        // client that trusted the (incorrect) "returns all" contract.
        let msgs = s
            .messages
            .list_since(room, after, limit)
            .await
            .map_err(AeroError::from)?;
        return Ok(Json(serde_json::to_value(msgs).map_err(AeroError::from)?));
    }

    // Full authorization above ran on primary. Only old backward pages go to
    // the optional replica; the newest page stays strong for read-after-write.
    let has_newer = match before {
        Some(cursor) => has_newer_visible_message(&s.pg, room, cursor)
            .await
            .map_err(AeroError::from)?,
        None => false,
    };
    let consistency = history_query_consistency(before, since, has_newer);
    let messages = aero_storage::MessageRepo::new(s.query_router.repo_pool(consistency));
    let msgs = match messages.list_recent(room, before, limit).await {
        Ok(messages) => messages,
        Err(error)
            if consistency == aero_storage::QueryConsistency::Eventual
                && s.query_router.has_replica() =>
        {
            tracing::warn!(
                %room,
                ?error,
                "history read replica failed; retrying the old page on primary"
            );
            s.messages
                .list_recent(room, before, limit)
                .await
                .map_err(AeroError::from)?
        }
        Err(error) => return Err(AeroError::from(error).into()),
    };
    Ok(Json(serde_json::to_value(msgs).map_err(AeroError::from)?))
}

/// Query for the change-replay endpoint: an RFC3339 `since` instant + optional
/// `limit`.
#[derive(Deserialize)]
struct ChangesQuery {
    since: String,
    limit: Option<i64>,
}

/// `GET /api/rooms/:id/changes?since=<rfc3339>` — messages edited or deleted
/// since `since`, so a client reconnecting after offline edits/deletes can
/// converge on mutations to messages it already holds (ROADMAP 方向一). The
/// message backfill (`/messages?since=<id>`) only covers NEW messages; this is
/// its companion. Tombstones (deleted messages) are included; the client removes
/// those and replaces the rest.
async fn room_changes(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Query(q): Query<ChangesQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room_id(&room_str)?;
    // Tenant guard FIRST (membership + deactivation), then charge the read.
    s.im.assert_room_access(auth.participant_id, room).await?;
    crate::ws_rate::check_ws_rate_room(&s, room).await?;
    // `/changes` keys on MUTATION TIME (edited_at/deleted_at), not message id, so
    // edits/deletes to already-held messages (whose id is <= the client's cursor)
    // are still returned. Parse the RFC3339 instant the contract documents. (The
    // split had rewritten this to an id cursor, which silently broke the feature.)
    let since = time::OffsetDateTime::parse(
        q.since.trim(),
        &time::format_description::well_known::Rfc3339,
    )
    .map_err(|e| AeroError::Invalid(format!("since must be an RFC3339 timestamp: {e}")))?;
    let limit = history_limit(q.limit);
    let msgs = s
        .messages
        .changes_since(room, since, limit)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(msgs).map_err(AeroError::from)?))
}
