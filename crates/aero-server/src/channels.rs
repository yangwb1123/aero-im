//! Channel management HTTP surface: browse public channels, join/leave, archive,
//! and edit channel metadata (topic / description / visibility).
//!
//! Thin handlers — every business invariant (workspace membership, room
//! membership, the public+non-archived join rule, event broadcast) lives in
//! [`ImService`](aero_im_core::ImService). Mounted via [`routes`] and `.merge`d
//! into the gateway router, mirroring [`crate::workspaces`].

use std::str::FromStr;

use aero_auth::AuthUser;
use aero_common::{Error as AeroError, ParticipantId, RoomId, WorkspaceId};
use aero_storage::{MAX_CHANNEL_DESCRIPTION_CHARS, MAX_CHANNEL_TOPIC_CHARS};
use axum::{
    extract::{Path, Query, State},
    routing::{get, patch, post},
    Json, Router,
};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::state::AppState;

/// All channel routes, ready to `.merge` into the gateway router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/workspaces/:id/channels", get(list_channels))
        .route("/api/rooms/:id/join", post(join_channel))
        .route("/api/rooms/:id/leave", post(leave_channel))
        .route("/api/rooms/:id/archive", post(archive_channel))
        .route(
            "/api/rooms/:id/channel",
            axum::routing::patch(update_channel),
        )
        .route(
            "/api/rooms/:id/post-policy",
            get(get_post_policy).put(set_post_policy),
        )
        // ROADMAP7 Lane A: channel topic change history.
        .route("/api/rooms/:id/topic-history", get(list_topic_history))
        // ROADMAP9: server-side slowmode enforcement.
        .route("/api/rooms/:id/slowmode", patch(set_slowmode))
        // ROADMAP12: per-room reaction spam limit (admin/moderator only).
        .route(
            "/api/rooms/:id/reaction-limit",
            axum::routing::patch(set_reaction_limit),
        )
}

fn parse_room(s: &str) -> Result<RoomId, AeroError> {
    RoomId::from_str(s).map_err(|e| AeroError::Invalid(format!("room id: {e}")))
}

fn parse_workspace(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

/// Optional query-string parameters for `GET /api/workspaces/:id/channels`.
#[derive(serde::Deserialize, Default)]
struct ListChannelsQuery {
    /// Optional case-insensitive substring name filter (`ILIKE '%q%'`).
    #[serde(default)]
    q: Option<String>,
}

/// `GET /api/workspaces/:id/channels` — the workspace's public, joinable
/// channels. Caller must be a member of the workspace. An optional `?q=` query
/// parameter performs a case-insensitive substring match on the channel name.
async fn list_channels(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Query(params): Query<ListChannelsQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace(&id_str)?;
    let channels =
        s.im.list_workspace_channels(auth.participant_id, ws, params.q.as_deref())
            .await?;
    Ok(Json(
        serde_json::to_value(channels).map_err(AeroError::from)?,
    ))
}

/// `POST /api/rooms/:id/join` — join a public, non-archived channel.
async fn join_channel(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.join_channel(auth.participant_id, room).await?;
    s.room_member_cache.invalidate(&room);
    Ok(Json(serde_json::json!({ "joined": true })))
}

/// `POST /api/rooms/:id/leave` — leave a channel.
async fn leave_channel(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.leave_channel(auth.participant_id, room).await?;
    s.room_member_cache.invalidate(&room);
    Ok(Json(serde_json::json!({ "left": true })))
}

#[derive(Deserialize)]
struct ArchiveReq {
    archived: bool,
}

/// `POST /api/rooms/:id/archive` — archive or un-archive a channel. Caller must
/// be a member of the room.
async fn archive_channel(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<ArchiveReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.archive_channel(auth.participant_id, room, req.archived)
        .await?;
    Ok(Json(serde_json::json!({ "archived": req.archived })))
}

#[derive(Deserialize)]
struct UpdateChannelReq {
    // `Option<Option<_>>` is deliberate (matches `update_me` in `crate::routes`):
    // outer = field present; inner = nullable on the wire, so an explicit `null`
    // clears the value while an omitted field is left untouched.
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    #[allow(clippy::option_option)]
    topic: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    #[allow(clippy::option_option)]
    description: Option<Option<String>>,
    #[serde(default)]
    is_private: Option<bool>,
}

/// Distinguish "absent" from "present and null" for a nullable JSON field, so a
/// PATCH can clear a value (`null`) vs. leave it untouched (omitted). Mirrors the
/// `update_me` avatar handling in [`crate::routes`].
#[allow(clippy::option_option)]
fn deserialize_optional_field<'de, D, T>(d: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// `PATCH /api/rooms/:id/channel` — update channel metadata (topic, description,
/// visibility). Only provided fields change. Caller must be a member of the room.
async fn update_channel(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<UpdateChannelReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    // Trim provided text fields; an empty/whitespace string clears the value.
    let topic = req
        .topic
        .map(|inner| inner.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()));
    let description = req
        .description
        .map(|inner| inner.map(|d| d.trim().to_owned()).filter(|d| !d.is_empty()));
    if topic
        .as_ref()
        .and_then(Option::as_ref)
        .is_some_and(|value| value.chars().count() > MAX_CHANNEL_TOPIC_CHARS)
    {
        return Err(AeroError::Invalid(format!(
            "topic is too long (max {MAX_CHANNEL_TOPIC_CHARS} chars)"
        ))
        .into());
    }
    if description
        .as_ref()
        .and_then(Option::as_ref)
        .is_some_and(|value| value.chars().count() > MAX_CHANNEL_DESCRIPTION_CHARS)
    {
        return Err(AeroError::Invalid(format!(
            "description is too long (max {MAX_CHANNEL_DESCRIPTION_CHARS} chars)"
        ))
        .into());
    }

    // Snapshot the metadata the request touches BEFORE the setter mutates it, so
    // the post-write diff records true old→new values (compliance audit trail).
    // Best-effort: a read failure simply yields no "before" and is logged later.
    s.im.assert_channel_access(auth.participant_id, room)
        .await?;
    let before = read_channel_meta(&s, room).await;

    s.im.set_channel_meta(
        auth.participant_id,
        room,
        topic.clone(),
        description.clone(),
        req.is_private,
    )
    .await?;

    // Diff old-vs-new and emit ONE audit row per PATCH listing only the fields
    // that actually changed. The setter already committed; auditing is
    // best-effort and never fails the request.
    if let Some(before) = before {
        let after = read_channel_meta(&s, room).await;
        if let Some(after) = after {
            let changed = diff_channel_meta(&before, &after);
            if !changed.is_empty() {
                audit_channel_meta_changed(&s, room, auth.participant_id, changed).await;
            }
        }
    }
    Ok(Json(serde_json::json!({ "updated": true })))
}

#[derive(Deserialize)]
struct PostPolicyReq {
    /// `"everyone"` (default — any member may post) or `"admins"` (announcements
    /// only: room creator or workspace Admin/Owner). Validated in the service.
    policy: String,
}

/// `PUT /api/rooms/:id/post-policy` — set a room's post policy (announcement
/// channels). Body `{ "policy": "everyone" | "admins" }`. The caller must be the
/// room creator OR a workspace Admin/Owner; an invalid policy string is a 400.
async fn set_post_policy(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<PostPolicyReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;

    // Snapshot the policy BEFORE mutating, so the diff records the true old→new
    // (compliance audit trail). Best-effort: a read failure yields no "before".
    s.im.assert_channel_access(auth.participant_id, room)
        .await?;
    let before = read_channel_meta(&s, room).await;

    s.im.set_room_post_policy(auth.participant_id, room, &req.policy)
        .await?;

    // Audit only when the policy actually changed; one row per PUT. The setter
    // already committed, so auditing is best-effort and never fails the request.
    if let Some(before) = before {
        let after = read_channel_meta(&s, room).await;
        if let Some(after) = after {
            let changed = diff_channel_meta(&before, &after);
            if !changed.is_empty() {
                audit_channel_meta_changed(&s, room, auth.participant_id, changed).await;
            }
        }
    }
    Ok(Json(serde_json::json!({ "policy": req.policy })))
}

/// `GET /api/rooms/:id/post-policy` — read a room's post policy. Caller must be
/// able to access the room.
async fn get_post_policy(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    let policy = s.im.room_post_policy(auth.participant_id, room).await?;
    Ok(Json(serde_json::json!({ "policy": policy })))
}

// --------------------------------------------------- topic history (ROADMAP7 Lane A)

#[derive(Deserialize)]
struct TopicHistoryQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

fn default_limit() -> i64 {
    20
}

/// `GET /api/rooms/:id/topic-history?limit=20&offset=0` — channel topic change
/// history, newest first. Caller must be able to access the room.
async fn list_topic_history(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Query(q): Query<TopicHistoryQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_channel_access(auth.participant_id, room)
        .await?;
    let entries = s
        .topic_history
        .list(room, q.limit.max(1).min(100), q.offset.max(0))
        .await?;
    Ok(Json(serde_json::json!({ "history": entries })))
}

// --------------------------------------------------- metadata change audit trail
//
// Channel topic/description/visibility/post-policy edits are privileged room
// administration — the same class as `channel_role.changed`, which IS audited.
// These helpers close that asymmetry: snapshot the room's metadata before the
// service setter, diff it after, and (when anything changed) append ONE
// `channel.metadata_changed` audit row per request. Modeled on
// [`crate::channel_roles::audit_role_changed`]: resolve the room's workspace,
// best-effort `s.audit.append`, warn-on-fail, never fail the request.

/// The subset of channel metadata covered by the audit trail, snapshotted before
/// and after a mutation so the change can be diffed. `topic`/`description` are
/// `None` when unset (the column is NULL); `post_policy` always has a value
/// (NOT NULL DEFAULT `everyone`, migration 0030).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChannelMeta {
    topic: Option<String>,
    description: Option<String>,
    is_private: bool,
    post_policy: String,
}

/// Snapshot a room's audited metadata directly from `rooms`, or `None` if the
/// room is absent or the read fails. Read-only single-row lookup over the shared
/// pool; deliberately swallows errors (logged) so an audit read can never break
/// the mutation it is observing.
async fn read_channel_meta(s: &AppState, room: RoomId) -> Option<ChannelMeta> {
    let row = sqlx::query_as::<_, (Option<String>, Option<String>, bool, Option<String>)>(
        r"SELECT topic, description, is_private, post_policy FROM rooms WHERE id = $1",
    )
    .bind(room.to_uuid())
    .fetch_optional(&s.pg)
    .await;
    match row {
        Ok(Some((topic, description, is_private, post_policy))) => Some(ChannelMeta {
            topic,
            description,
            is_private,
            // Mirror `RoomRepo::post_policy`: a (defensively) NULL policy reads as
            // the open default rather than an empty string.
            post_policy: post_policy.unwrap_or_else(|| "everyone".to_owned()),
        }),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error = ?e, %room, "channel metadata audit: read failed");
            None
        }
    }
}

/// Render an optional text field for the audit detail JSON: a present value is its
/// string, an unset (`None`) value is JSON `null`.
fn meta_field_json(value: Option<&str>) -> serde_json::Value {
    match value {
        Some(v) => serde_json::Value::String(v.to_owned()),
        None => serde_json::Value::Null,
    }
}

/// Diff two metadata snapshots into one `{field, old, new}` object per CHANGED
/// field (unchanged fields are skipped). Returns an empty `Vec` for a no-op edit,
/// so the caller writes NO audit row. Pure, so the diff is unit-tested without a
/// DB.
fn diff_channel_meta(before: &ChannelMeta, after: &ChannelMeta) -> Vec<serde_json::Value> {
    let mut changed = Vec::new();
    if before.topic != after.topic {
        changed.push(serde_json::json!({
            "field": "topic",
            "old": meta_field_json(before.topic.as_deref()),
            "new": meta_field_json(after.topic.as_deref()),
        }));
    }
    if before.description != after.description {
        changed.push(serde_json::json!({
            "field": "description",
            "old": meta_field_json(before.description.as_deref()),
            "new": meta_field_json(after.description.as_deref()),
        }));
    }
    if before.is_private != after.is_private {
        changed.push(serde_json::json!({
            "field": "visibility",
            "old": before.is_private,
            "new": after.is_private,
        }));
    }
    if before.post_policy != after.post_policy {
        changed.push(serde_json::json!({
            "field": "post_policy",
            "old": before.post_policy,
            "new": after.post_policy,
        }));
    }
    changed
}

/// Record a `channel.metadata_changed` audit event attributing it to `actor`
/// against the room's workspace, carrying the list of changed `{field, old, new}`
/// entries. Best-effort throughout (modeled on
/// [`crate::channel_roles::audit_role_changed`]): failing to resolve the
/// workspace or to append is warn-logged and swallowed (the metadata change
/// already committed); a room with no resolvable workspace is silently skipped.
/// Caller guarantees `changed` is non-empty.
async fn audit_channel_meta_changed(
    s: &AppState,
    room: RoomId,
    actor: ParticipantId,
    changed: Vec<serde_json::Value>,
) {
    let workspace = match s.rooms.room_workspace(room).await {
        Ok(Some(ws)) => ws,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(error = ?e, %room, "channel_metadata audit: resolve workspace failed");
            return;
        }
    };
    if let Err(e) = s
        .audit
        .append(
            workspace,
            Some(actor),
            "channel.metadata_changed",
            Some(&room.to_string()),
            serde_json::json!({
                "room_id": room.to_string(),
                "changed": changed,
            }),
        )
        .await
    {
        tracing::warn!(error = ?e, %workspace, "channel.metadata_changed audit append failed");
    }
}

// --------------------------------------------------- slowmode (ROADMAP9, migration 0107)

#[derive(Deserialize)]
struct SlowmodeReq {
    /// Slowmode interval in seconds. `0` disables slowmode. Clamped to `[0, 21600]`
    /// (6 hours max, matching Discord's ceiling).
    seconds: i32,
}

/// `PATCH /api/rooms/:id/slowmode` — set (or disable with `seconds=0`) the room's
/// slowmode interval. Caller must be the room creator or a workspace Admin/Owner.
async fn set_slowmode(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<SlowmodeReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    s.im.assert_channel_access(auth.participant_id, room)
        .await?;
    let seconds = req.seconds.clamp(0, 21_600);
    s.im.set_channel_slowmode(auth.participant_id, room, seconds)
        .await?;
    Ok(Json(
        serde_json::json!({ "ok": true, "slowmode_seconds": seconds }),
    ))
}

// ---------------------------------------- reaction limit (ROADMAP12, migration 0118)

#[derive(Deserialize)]
struct ReactionLimitReq {
    /// `null` clears the limit; a positive integer sets the per-user per-message cap.
    max_reactions_per_user: Option<i32>,
}

/// `PATCH /api/rooms/:id/reaction-limit` — set (or clear with `null`) the
/// per-user reaction cap for a room. Caller must be the room creator or a
/// workspace Admin/Owner.
async fn set_reaction_limit(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(room_str): Path<String>,
    Json(req): Json<ReactionLimitReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let room = parse_room(&room_str)?;
    // Validate: if set, must be positive.
    if let Some(cap) = req.max_reactions_per_user {
        if cap < 1 {
            return Err(AeroError::Invalid(
                "max_reactions_per_user must be a positive integer or null".into(),
            )
            .into());
        }
    }
    s.im.assert_channel_access(auth.participant_id, room)
        .await?;
    s.im.set_channel_reaction_limit(auth.participant_id, room, req.max_reactions_per_user)
        .await?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "max_reactions_per_user": req.max_reactions_per_user
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(
        topic: Option<&str>,
        description: Option<&str>,
        is_private: bool,
        post_policy: &str,
    ) -> ChannelMeta {
        ChannelMeta {
            topic: topic.map(str::to_owned),
            description: description.map(str::to_owned),
            is_private,
            post_policy: post_policy.to_owned(),
        }
    }

    #[test]
    fn diff_identical_meta_is_empty() {
        // A no-op edit (nothing actually changed) yields no entries, so the caller
        // writes NO audit row.
        let m = meta(Some("standup"), Some("the team"), false, "everyone");
        assert!(diff_channel_meta(&m, &m).is_empty());
    }

    #[test]
    fn diff_reports_only_changed_fields_with_old_and_new() {
        let before = meta(Some("old topic"), Some("old desc"), false, "everyone");
        // topic + visibility change; description + post_policy unchanged.
        let after = meta(Some("new topic"), Some("old desc"), true, "everyone");
        let changed = diff_channel_meta(&before, &after);
        assert_eq!(changed.len(), 2, "only the two changed fields are reported");

        let topic = &changed[0];
        assert_eq!(topic["field"], "topic");
        assert_eq!(topic["old"], "old topic");
        assert_eq!(topic["new"], "new topic");

        let visibility = &changed[1];
        assert_eq!(visibility["field"], "visibility");
        assert_eq!(visibility["old"], false);
        assert_eq!(visibility["new"], true);
    }

    #[test]
    fn diff_models_cleared_text_field_as_json_null() {
        // Clearing the topic (Some -> None) records old=value, new=null; the
        // unchanged description is omitted entirely.
        let before = meta(Some("had a topic"), Some("keep me"), false, "everyone");
        let after = meta(None, Some("keep me"), false, "everyone");
        let changed = diff_channel_meta(&before, &after);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0]["field"], "topic");
        assert_eq!(changed[0]["old"], "had a topic");
        assert_eq!(changed[0]["new"], serde_json::Value::Null);
    }

    #[test]
    fn diff_setting_text_field_from_unset_records_null_old() {
        // Setting a previously-unset description records old=null, new=value.
        let before = meta(None, None, true, "everyone");
        let after = meta(None, Some("now has a description"), true, "everyone");
        let changed = diff_channel_meta(&before, &after);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0]["field"], "description");
        assert_eq!(changed[0]["old"], serde_json::Value::Null);
        assert_eq!(changed[0]["new"], "now has a description");
    }

    #[test]
    fn diff_reports_post_policy_change() {
        let before = meta(None, None, false, "everyone");
        let after = meta(None, None, false, "admins");
        let changed = diff_channel_meta(&before, &after);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0]["field"], "post_policy");
        assert_eq!(changed[0]["old"], "everyone");
        assert_eq!(changed[0]["new"], "admins");
    }

    #[test]
    fn meta_field_json_maps_some_to_string_and_none_to_null() {
        assert_eq!(meta_field_json(Some("x")), serde_json::json!("x"));
        assert_eq!(meta_field_json(None), serde_json::Value::Null);
    }
}

/// PG-gated integration test (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-server --lib -- --ignored channel_metadata_
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_storage::{AuditRepo, RoomRepo, WorkspaceRepo};
    use sqlx::PgPool;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway participant + workspace + channel room (with the actor
    /// enrolled as a member) so the metadata-audit test is self-contained. The
    /// participant is inserted first because `audit_events.actor_id` and
    /// `rooms.created_by` are FK-constrained.
    async fn fixture(p: &PgPool) -> (WorkspaceId, RoomId, ParticipantId) {
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(actor.to_uuid())
            .bind(format!("chan-meta-actor-{actor}"))
            .execute(p)
            .await
            .expect("insert participant");

        let ws = WorkspaceRepo::new(p.clone())
            .create(
                "Chan Meta WS".into(),
                format!("chan-meta-{}", WorkspaceId::new()),
                actor,
            )
            .await
            .expect("insert workspace")
            .id;

        let room = RoomRepo::new(p.clone())
            .create_in_workspace(
                ws,
                aero_common::RoomKind::Channel,
                Some(format!("chan-meta-room-{}", RoomId::new())),
                actor,
            )
            .await
            .expect("insert room")
            .id;
        RoomRepo::new(p.clone())
            .set_visibility(room, false)
            .await
            .expect("make fixture channel public");
        (ws, room, actor)
    }

    /// Read the audited metadata snapshot directly from `rooms`, mirroring the
    /// production [`read_channel_meta`] SELECT (which takes `&AppState`, so it is
    /// reproduced here against a bare pool).
    async fn read_meta(p: &PgPool, room: RoomId) -> ChannelMeta {
        let (topic, description, is_private, post_policy) =
            sqlx::query_as::<_, (Option<String>, Option<String>, bool, Option<String>)>(
                "SELECT topic, description, is_private, post_policy FROM rooms WHERE id = $1",
            )
            .bind(room.to_uuid())
            .fetch_one(p)
            .await
            .expect("read room meta");
        ChannelMeta {
            topic,
            description,
            is_private,
            post_policy: post_policy.unwrap_or_else(|| "everyone".to_owned()),
        }
    }

    /// End-to-end of the audit trail against live PG: a real metadata change
    /// (topic + description + visibility) appends exactly one
    /// `channel.metadata_changed` row attributed to the actor and targeting the
    /// room, whose `changed` JSON carries each field's old+new; a subsequent
    /// no-op change appends nothing.
    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn channel_metadata_change_is_audited_and_noop_is_not() {
        let p = pool();
        let rooms = RoomRepo::new(p.clone());
        let audit = AuditRepo::new(p.clone());
        let (ws, room, actor) = fixture(&p).await;

        // --- A real edit: topic (unset -> set), description (unset -> set),
        //     visibility (false -> true). Snapshot before, mutate, diff, audit ---
        let before = read_meta(&p, room).await;
        assert_eq!(before.topic, None);
        assert_eq!(before.description, None);
        assert!(!before.is_private);

        rooms.set_topic(room, Some("daily standup")).await.unwrap();
        rooms
            .set_description(room, Some("the team channel"))
            .await
            .unwrap();
        rooms.set_visibility(room, true).await.unwrap();

        let after = read_meta(&p, room).await;
        let changed = diff_channel_meta(&before, &after);
        assert_eq!(changed.len(), 3, "topic + description + visibility changed");
        assert!(!changed.is_empty());

        // Append the audit row exactly as `audit_channel_meta_changed` does.
        let workspace = rooms
            .room_workspace(room)
            .await
            .unwrap()
            .expect("room has workspace");
        assert_eq!(workspace, ws);
        audit
            .append(
                workspace,
                Some(actor),
                "channel.metadata_changed",
                Some(&room.to_string()),
                serde_json::json!({ "room_id": room.to_string(), "changed": changed }),
            )
            .await
            .unwrap();

        // --- Assert the trail: exactly one metadata_changed row for this room ---
        let trail = audit.list_for_workspace(ws, None, None).await.unwrap();
        let rows: Vec<_> = trail
            .iter()
            .filter(|e| {
                e.action == "channel.metadata_changed"
                    && e.target.as_deref() == Some(&room.to_string())
            })
            .collect();
        assert_eq!(rows.len(), 1, "one metadata audit row for the room");
        assert_eq!(rows[0].actor_id, Some(actor), "attributed to the editor");

        let logged = rows[0].detail["changed"]
            .as_array()
            .expect("changed is an array");
        assert_eq!(logged.len(), 3);
        let by_field = |f: &str| -> &serde_json::Value {
            logged
                .iter()
                .find(|c| c["field"] == f)
                .unwrap_or_else(|| panic!("missing changed field {f}"))
        };
        // topic: null -> "daily standup"
        assert_eq!(by_field("topic")["old"], serde_json::Value::Null);
        assert_eq!(by_field("topic")["new"], "daily standup");
        // description: null -> "the team channel"
        assert_eq!(by_field("description")["old"], serde_json::Value::Null);
        assert_eq!(by_field("description")["new"], "the team channel");
        // visibility: false -> true
        assert_eq!(by_field("visibility")["old"], false);
        assert_eq!(by_field("visibility")["new"], true);

        // --- A no-op change writes nothing: re-applying identical values yields an
        //     empty diff, so no second audit row is appended ---
        let before2 = read_meta(&p, room).await;
        rooms.set_topic(room, Some("daily standup")).await.unwrap();
        rooms
            .set_description(room, Some("the team channel"))
            .await
            .unwrap();
        rooms.set_visibility(room, true).await.unwrap();
        let after2 = read_meta(&p, room).await;
        let changed2 = diff_channel_meta(&before2, &after2);
        assert!(changed2.is_empty(), "identical re-apply is a no-op diff");

        let trail2 = audit.list_for_workspace(ws, None, None).await.unwrap();
        let repeats = trail2
            .iter()
            .filter(|e| {
                e.action == "channel.metadata_changed"
                    && e.target.as_deref() == Some(&room.to_string())
            })
            .count();
        assert_eq!(repeats, 1, "the no-op change appended no second audit row");
    }
}
