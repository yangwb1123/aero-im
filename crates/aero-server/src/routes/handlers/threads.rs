// ----- Thread mute (migration 0088) -----
//
// The storage layer (`aero_storage::ThreadMuteRepo`) and the notification
// suppression in the dispatcher were already wired; what was missing was any way
// for a user to CREATE or REMOVE a mute. These three handlers are that control
// surface. A thread is identified by its root message id. `ThreadMuteRepo` is
// constructed inline from the shared pool (`s.pg`), mirroring how other handlers
// here build repos on demand — no `AppState` change. Resolving the root message's
// room (via `s.messages.get`, the same lookup `delete_message` uses) lets us
// `assert_room_access` first, so a caller can only mute a thread in a room they
// belong to. An unknown root message id is a 404.

/// Resolve the room owning `root` (a 404 when the message does not exist), then
/// assert the caller may access it. Shared by the mute/unmute/list handlers so the
/// access policy is identical across all three.
async fn assert_thread_room_access(
    s: &AppState,
    participant: ParticipantId,
    root: MessageId,
) -> AeroResult<()> {
    let msg = s
        .messages
        .get(root)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound(format!("message {root}")))?;
    s.im.assert_room_access(participant, msg.room_id).await
}

/// `POST /api/threads/:root_message_id/mute` — mute the thread rooted at this
/// message for the caller, so they stop receiving reply notifications for it. The
/// caller must be able to access the root message's room. Idempotent (re-muting is
/// a no-op). Always reports `muted: true`.
async fn thread_mute(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(root_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let root = MessageId::from_str(root_str.trim())
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    assert_thread_room_access(&s, auth.participant_id, root).await?;
    aero_storage::ThreadMuteRepo::new(s.pg.clone())
        .mute_authorized(auth.participant_id, root)
        .await?;
    Ok(Json(
        serde_json::json!({ "root_message_id": root, "muted": true }),
    ))
}

/// `DELETE /api/threads/:root_message_id/mute` — unmute the thread for the caller.
/// Owner-scoped at the SQL layer (only ever removes the caller's own mute), but we
/// still assert room access first for a consistent 404/403 with the mute path.
/// Unmuting a thread that was never muted is a no-op. Always reports `muted: false`.
async fn thread_unmute(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(root_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let root = MessageId::from_str(root_str.trim())
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    assert_thread_room_access(&s, auth.participant_id, root).await?;
    aero_storage::ThreadMuteRepo::new(s.pg.clone())
        .unmute_authorized(auth.participant_id, root)
        .await?;
    Ok(Json(
        serde_json::json!({ "root_message_id": root, "muted": false }),
    ))
}

/// `GET /api/threads/:root_message_id/mutes` — the participants who have MUTED this
/// thread (`ThreadMuteRepo::muted_by`). The caller must be able to access the root
/// message's room. NOTE: `ThreadMuteRepo` exposes no per-participant "threads I
/// muted" query, so there is no `/api/me/thread-mutes`; this lists the muters of a
/// single thread instead.
async fn thread_muters(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(root_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let root = MessageId::from_str(root_str.trim())
        .map_err(|e| AeroError::Invalid(format!("message id: {e}")))?;
    assert_thread_room_access(&s, auth.participant_id, root).await?;
    let muters = aero_storage::ThreadMuteRepo::new(s.pg.clone())
        .muted_by(root)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(
        serde_json::json!({ "root_message_id": root, "muters": muters }),
    ))
}
