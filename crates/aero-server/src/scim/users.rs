//! SCIM 2.0 User resource handlers (RFC 7643 §4.1 / RFC 7644).
//!
//! Split out of the `scim` module root; reaches the shared schema types and
//! helpers (`scim_workspace`, `scim_repo`, `scim_err`, `to_scim_user`,
//! `display_name_for`, `parse_participant_id`, `is_unique_violation`) via
//! `use super::*;`.

use super::*;

// ============================================================ User handlers

#[derive(Deserialize)]
pub(super) struct ListUsersQuery {
    filter: Option<String>,
    #[serde(rename = "startIndex")]
    start_index: Option<i64>,
    count: Option<i64>,
}

/// `GET /scim/v2/Users` — list users, optionally filtered by `userName eq "x"`,
/// paginated by `startIndex`/`count`. Returns a SCIM `ListResponse`.
pub(super) async fn list_users(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListUsersQuery>,
) -> Response {
    let ws = match scim_workspace(&s, &headers).await {
        Ok(w) => w,
        Err(e) => return scim_err(&e),
    };
    // Only `userName eq "x"` is honored; any other filter lists all (RFC permits
    // returning the full set when a filter is unsupported).
    let filter_user_name = q
        .filter
        .as_deref()
        .and_then(parse_filter)
        .filter(|(attr, _)| attr.eq_ignore_ascii_case("userName"))
        .map(|(_, v)| v);

    let repo = scim_repo(&s);
    let (rows, total) = match repo
        .list_users(ws, filter_user_name.as_deref(), q.start_index, q.count)
        .await
    {
        Ok(r) => r,
        Err(e) => return scim_err(&AeroError::from(e)),
    };
    let mut resources = Vec::with_capacity(rows.len());
    for row in &rows {
        let display = participant_display_name(&s, row.participant_id).await;
        resources.push(to_scim_user(row, display.as_deref()));
    }
    let start_index = q.start_index.filter(|n| *n >= 1).unwrap_or(1);
    Json(ScimListResponse::new(resources, total, start_index)).into_response()
}

/// Fetch a participant's display name, swallowing errors to `None` (SCIM `name`
/// is best-effort and must never fail a list/get).
async fn participant_display_name(s: &AppState, id: ParticipantId) -> Option<String> {
    s.participants.get(id).await.ok().flatten().map(|p| p.display_name)
}

/// `GET /scim/v2/Users/:id` — fetch one user (404 if not provisioned here).
pub(super) async fn get_user(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let ws = match scim_workspace(&s, &headers).await {
        Ok(w) => w,
        Err(e) => return scim_err(&e),
    };
    let pid = match parse_participant_id(&id) {
        Ok(p) => p,
        Err(e) => return scim_err(&e),
    };
    let repo = scim_repo(&s);
    match repo.get_user(ws, pid).await {
        Ok(Some(row)) => {
            let display = participant_display_name(&s, pid).await;
            Json(to_scim_user(&row, display.as_deref())).into_response()
        }
        Ok(None) => scim_err(&AeroError::NotFound("user".into())),
        Err(e) => scim_err(&AeroError::from(e)),
    }
}

/// `POST /scim/v2/Users` — provision a user: create a global participant, enroll
/// them in the token's workspace as a `Member`, and record the SCIM mapping.
/// 201 on success; 409 if the `userName` already exists in this workspace.
pub(super) async fn create_user(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<ScimUser>,
) -> Response {
    let ws = match scim_workspace(&s, &headers).await {
        Ok(w) => w,
        Err(e) => return scim_err(&e),
    };
    let user_name = payload.user_name.trim();
    if user_name.is_empty() {
        return scim_err(&AeroError::Invalid("userName is required".into()));
    }
    let repo = scim_repo(&s);

    // Reject a duplicate userName up front with a clean 409 (the unique index is
    // the real guard, but this gives the precise SCIM error without a failed insert).
    match repo.find_by_user_name(ws, user_name).await {
        Ok(Some(_)) => {
            return scim_err(&AeroError::Conflict(format!("userName '{user_name}' exists")))
        }
        Ok(None) => {}
        Err(e) => return scim_err(&AeroError::from(e)),
    }

    // Create the global participant. A SCIM-provisioned user has NO local
    // credentials (login is delegated to the IdP via SSO), so we create a bare
    // `Human` participant through the credential-less `create_bot` path rather
    // than `create_human` (which would require an email + password hash).
    let display = display_name_for(&payload);
    let participant = match s
        .participants
        .create_bot(
            &display,
            aero_common::ParticipantKind::Human,
            None,
            None,
        )
        .await
    {
        Ok(p) => p,
        Err(e) => return scim_err(&AeroError::from(e)),
    };

    // Enroll in the workspace as a regular Member.
    if let Err(e) = s
        .workspaces
        .add_member(ws, participant.id, WorkspaceRole::Member)
        .await
    {
        return scim_err(&AeroError::from(e));
    }

    // Record the SCIM mapping.
    match repo
        .create_user(
            ws,
            participant.id,
            user_name,
            payload.external_id.as_deref(),
            payload.active,
        )
        .await
    {
        Ok(row) => {
            let resp = to_scim_user(&row, Some(&display));
            (StatusCode::CREATED, Json(resp)).into_response()
        }
        Err(e) if is_unique_violation(&e) => {
            scim_err(&AeroError::Conflict(format!("userName '{user_name}' exists")))
        }
        Err(e) => scim_err(&AeroError::from(e)),
    }
}

/// `PUT /scim/v2/Users/:id` — replace a user (RFC 7644 §3.5.1). Updates
/// `userName`/`externalId`/`active` and the participant's display name from the
/// body. 200 on success, 404 if absent, 409 on a `userName` collision.
pub(super) async fn put_user(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(payload): Json<ScimUser>,
) -> Response {
    let ws = match scim_workspace(&s, &headers).await {
        Ok(w) => w,
        Err(e) => return scim_err(&e),
    };
    let pid = match parse_participant_id(&id) {
        Ok(p) => p,
        Err(e) => return scim_err(&e),
    };
    let user_name = payload.user_name.trim();
    if user_name.is_empty() {
        return scim_err(&AeroError::Invalid("userName is required".into()));
    }
    let repo = scim_repo(&s);

    // Reactivation (active true) restores workspace membership; deactivation
    // removes it — keep membership consistent with the SCIM `active` flag.
    apply_active_membership(&s, ws, pid, payload.active).await;

    let display = display_name_for(&payload);
    // Best-effort profile update (display name).
    let _ = s
        .participants
        .update_profile(pid, Some(&display), None)
        .await;

    match repo
        .update_user(
            ws,
            pid,
            Some(user_name),
            Some(payload.external_id.as_deref()),
            Some(payload.active),
        )
        .await
    {
        Ok(Some(row)) => Json(to_scim_user(&row, Some(&display))).into_response(),
        Ok(None) => scim_err(&AeroError::NotFound("user".into())),
        Err(e) if is_unique_violation(&e) => {
            scim_err(&AeroError::Conflict(format!("userName '{user_name}' exists")))
        }
        Err(e) => scim_err(&AeroError::from(e)),
    }
}

/// A minimal SCIM PATCH body (RFC 7644 §3.5.2). Real `IdPs` (`Okta`/`Azure`) send an
/// `Operations` array; we handle the common `replace` operations that set
/// `active`, `userName`, and name/email — the deprovision toggle and profile
/// edits — rather than the full path-filter grammar (a documented seam).
#[derive(Deserialize)]
pub(super) struct ScimPatch {
    #[serde(rename = "Operations", default)]
    operations: Vec<ScimPatchOp>,
}

#[derive(Deserialize)]
pub(super) struct ScimPatchOp {
    #[serde(default)]
    op: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    value: serde_json::Value,
}

/// `PATCH /scim/v2/Users/:id` — apply the supported subset of patch operations.
/// Primarily the `active` toggle used by `IdPs` for deprovisioning, plus
/// `userName`/name replacement. 200 with the updated user, 404 if absent.
pub(super) async fn patch_user(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(patch): Json<ScimPatch>,
) -> Response {
    let ws = match scim_workspace(&s, &headers).await {
        Ok(w) => w,
        Err(e) => return scim_err(&e),
    };
    let pid = match parse_participant_id(&id) {
        Ok(p) => p,
        Err(e) => return scim_err(&e),
    };
    let repo = scim_repo(&s);

    // Resolve the existing user first so an unknown id is a clean 404.
    let existing = match repo.get_user(ws, pid).await {
        Ok(Some(row)) => row,
        Ok(None) => return scim_err(&AeroError::NotFound("user".into())),
        Err(e) => return scim_err(&AeroError::from(e)),
    };

    // Fold the operations into a desired (user_name, external_id, active, name) delta.
    let mut new_user_name: Option<String> = None;
    let mut new_external_id: Option<Option<String>> = None;
    let mut new_active: Option<bool> = None;
    let mut new_display: Option<String> = None;
    for op in &patch.operations {
        if !matches!(op.op.to_ascii_lowercase().as_str(), "replace" | "add") {
            continue; // `remove` and unknown ops are ignored (documented seam).
        }
        match op.path.as_deref() {
            Some("active") => new_active = op.value.as_bool().or(new_active),
            Some("userName") => {
                new_user_name = op.value.as_str().map(str::to_owned).or(new_user_name);
            }
            Some("externalId") => {
                new_external_id = Some(op.value.as_str().map(str::to_owned));
            }
            Some(p) if p.eq_ignore_ascii_case("name.formatted") || p == "displayName" => {
                new_display = op.value.as_str().map(str::to_owned).or(new_display);
            }
            // No `path`: a whole-resource replace — pull known fields out of the value object.
            None => {
                if let Some(v) = op.value.as_bool() {
                    let _ = v; // a bare bool with no path is meaningless; skip.
                }
                if let Some(a) = op.value.get("active").and_then(serde_json::Value::as_bool) {
                    new_active = Some(a);
                }
                if let Some(u) = op.value.get("userName").and_then(serde_json::Value::as_str) {
                    new_user_name = Some(u.to_owned());
                }
                if let Some(e) = op.value.get("externalId").and_then(serde_json::Value::as_str) {
                    new_external_id = Some(Some(e.to_owned()));
                }
            }
            _ => {} // other paths (emails, etc.) are not mapped — documented seam.
        }
    }

    // Keep membership consistent with a changed `active`.
    if let Some(active) = new_active {
        apply_active_membership(&s, ws, pid, active).await;
    }
    if let Some(name) = &new_display {
        let _ = s.participants.update_profile(pid, Some(name), None).await;
    }

    match repo
        .update_user(
            ws,
            pid,
            new_user_name.as_deref(),
            new_external_id.as_ref().map(Option::as_deref),
            new_active,
        )
        .await
    {
        Ok(Some(row)) => {
            let display = new_display
                .or(participant_display_name(&s, pid).await);
            Json(to_scim_user(&row, display.as_deref())).into_response()
        }
        Ok(None) => scim_err(&AeroError::NotFound("user".into())),
        Err(e) if is_unique_violation(&e) => scim_err(&AeroError::Conflict(
            "userName collision".to_owned(),
        )),
        Err(e) => {
            // The existing row was found; surface other DB errors as 500.
            let _ = existing;
            scim_err(&AeroError::from(e))
        }
    }
}

/// Reconcile workspace membership with the SCIM `active` flag: active ⇒ ensure
/// member, inactive ⇒ remove membership. Best-effort (errors are logged, not
/// fatal to the SCIM op, which still updates the row's `active`).
async fn apply_active_membership(
    s: &AppState,
    ws: WorkspaceId,
    pid: ParticipantId,
    active: bool,
) {
    let result = if active {
        s.workspaces.add_member(ws, pid, WorkspaceRole::Member).await
    } else {
        s.workspaces.remove_member(ws, pid).await
    };
    if let Err(e) = result {
        tracing::warn!(error = ?e, %ws, %pid, active, "scim membership reconcile failed");
    }
}

/// `DELETE /scim/v2/Users/:id` — deprovision: remove the SCIM mapping row (RFC
/// 7644 §3.6 — a subsequent GET 404s) + revoke workspace membership; the global
/// participant identity is retained. 204 on success, 404 if the user was not
/// provisioned in this workspace. (Deactivation without deletion is `PATCH
/// active=false`, which keeps the row.)
pub(super) async fn delete_user(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let ws = match scim_workspace(&s, &headers).await {
        Ok(w) => w,
        Err(e) => return scim_err(&e),
    };
    let pid = match parse_participant_id(&id) {
        Ok(p) => p,
        Err(e) => return scim_err(&e),
    };
    let repo = scim_repo(&s);
    match repo.delete_user(ws, pid).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => scim_err(&AeroError::NotFound("user".into())),
        Err(e) => scim_err(&AeroError::from(e)),
    }
}
