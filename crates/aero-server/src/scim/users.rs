//! SCIM 2.0 User resource handlers (RFC 7643 §4.1 / RFC 7644).
//!
//! Split out of the `scim` module root; reaches the shared schema types and
//! helpers (`scim_workspace`, `scim_repo`, `scim_err`, `to_scim_user`,
//! `display_name_for`, `parse_participant_id`, `is_unique_violation`) via
//! `use super::*;`.

use super::{Deserialize, AeroError, ScimUser, display_name_for, State, Query, AppState, HeaderMap, Response, scim_workspace, scim_err, parse_filter, scim_repo, to_scim_user, IntoResponse, Json, ScimListResponse, ParticipantId, Path, parse_participant_id, StatusCode, is_unique_violation, validate_patch_operation_count};

// ============================================================ User handlers

const MAX_USER_NAME_BYTES: usize = 320;
const MAX_EXTERNAL_ID_BYTES: usize = 512;
const MAX_DISPLAY_NAME_BYTES: usize = 64;
const MAX_IDENTITY_ISSUER_BYTES: usize = 2 * 1024;

pub(super) fn map_user_write_error(error: aero_storage::ScimUserWriteError) -> AeroError {
    match error {
        aero_storage::ScimUserWriteError::OwnerDeprovision => AeroError::Conflict(
            "transfer workspace ownership before deprovisioning this user".into(),
        ),
        aero_storage::ScimUserWriteError::ChannelOwnerDeprovision => {
            AeroError::Conflict("transfer channel ownership before deprovisioning this user".into())
        }
        aero_storage::ScimUserWriteError::IdentityTombstoned => AeroError::Conflict(
            "external identity was erased and requires an administrative recovery".into(),
        ),
        aero_storage::ScimUserWriteError::IdentitySubjectRequired => AeroError::Invalid(
            "externalId is required when Snaplink identity binding is enabled".into(),
        ),
        aero_storage::ScimUserWriteError::InvalidIdentityIssuer => AeroError::Internal(
            anyhow::anyhow!("invalid SCIM identity issuer configuration"),
        ),
        aero_storage::ScimUserWriteError::IdentitySubjectImmutable => AeroError::Conflict(
            "externalId is immutable after it is bound to a login identity".into(),
        ),
        aero_storage::ScimUserWriteError::IdentityConflict => {
            AeroError::Conflict("external identity is already linked to another account".into())
        }
        aero_storage::ScimUserWriteError::Storage(error) => AeroError::from(error),
    }
}

/// When configured, SCIM `externalId` is the stable subject emitted by the
/// same `IdP` used for interactive OIDC login. Keeping this opt-in avoids
/// interpreting arbitrary legacy SCIM identifiers as login subjects.
fn configured_identity_issuer() -> Result<Option<String>, AeroError> {
    let Some(issuer) = std::env::var("AERO__SCIM__IDENTITY_ISSUER")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    validate_identity_issuer(&issuer)?;
    if let Some(oidc_issuer) = std::env::var("AERO__OIDC__ISSUER")
        .ok()
        .filter(|value| !value.is_empty())
    {
        if issuer != oidc_issuer {
            return Err(AeroError::Internal(anyhow::anyhow!(
                "SCIM identity issuer must exactly match AERO__OIDC__ISSUER"
            )));
        }
    }
    Ok(Some(issuer))
}

pub(super) fn validate_identity_issuer(issuer: &str) -> Result<(), AeroError> {
    if issuer.len() > MAX_IDENTITY_ISSUER_BYTES
        || issuer != issuer.trim()
        || issuer.chars().any(char::is_control)
    {
        return Err(AeroError::Internal(anyhow::anyhow!(
            "AERO__SCIM__IDENTITY_ISSUER must be an exact, bounded issuer"
        )));
    }
    Ok(())
}

fn validate_user_name(raw: &str) -> Result<&str, AeroError> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(AeroError::Invalid("userName is required".into()));
    }
    if value.len() > MAX_USER_NAME_BYTES {
        return Err(AeroError::Invalid("userName is too long".into()));
    }
    Ok(value)
}

fn validate_external_id(value: Option<&str>) -> Result<(), AeroError> {
    if value.is_some_and(|value| value.len() > MAX_EXTERNAL_ID_BYTES) {
        return Err(AeroError::Invalid("externalId is too long".into()));
    }
    Ok(())
}

fn normalized_display_name(payload: &ScimUser) -> Result<String, AeroError> {
    let display = display_name_for(payload);
    let display = display.trim();
    if display.is_empty() {
        return Err(AeroError::Invalid("display name is required".into()));
    }
    if display.len() > MAX_DISPLAY_NAME_BYTES {
        return Err(AeroError::Invalid("display name is too long".into()));
    }
    Ok(display.to_owned())
}

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
    s.participants
        .get(id)
        .await
        .ok()
        .flatten()
        .map(|p| p.display_name)
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
    let user_name = match validate_user_name(&payload.user_name) {
        Ok(value) => value,
        Err(error) => return scim_err(&error),
    };
    if let Err(error) = validate_external_id(payload.external_id.as_deref()) {
        return scim_err(&error);
    }
    let repo = scim_repo(&s);

    // Reject a duplicate userName up front with a clean 409 (the unique index is
    // the real guard, but this gives the precise SCIM error without a failed insert).
    match repo.find_by_user_name(ws, user_name).await {
        Ok(Some(_)) => {
            return scim_err(&AeroError::Conflict(format!(
                "userName '{user_name}' exists"
            )))
        }
        Ok(None) => {}
        Err(e) => return scim_err(&AeroError::from(e)),
    }

    let display = match normalized_display_name(&payload) {
        Ok(value) => value,
        Err(error) => return scim_err(&error),
    };
    // A SCIM-provisioned user has no local credentials. When the identity
    // issuer is configured, `externalId` is resolved through the same canonical
    // `(issuer, subject)` mapping used by interactive OIDC login.
    let identity_issuer = match configured_identity_issuer() {
        Ok(issuer) => issuer,
        Err(error) => return scim_err(&error),
    };
    match repo
        .provision_user_with_identity(
            ws,
            &display,
            user_name,
            payload.external_id.as_deref(),
            payload.active,
            identity_issuer.as_deref(),
        )
        .await
    {
        Ok(row) => {
            let resp = to_scim_user(&row, Some(&display));
            (StatusCode::CREATED, Json(resp)).into_response()
        }
        Err(aero_storage::ScimUserWriteError::Storage(e)) if is_unique_violation(&e) => scim_err(
            &AeroError::Conflict(format!("userName '{user_name}' exists")),
        ),
        Err(e) => scim_err(&map_user_write_error(e)),
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
    let user_name = match validate_user_name(&payload.user_name) {
        Ok(value) => value,
        Err(error) => return scim_err(&error),
    };
    if let Err(error) = validate_external_id(payload.external_id.as_deref()) {
        return scim_err(&error);
    }
    let repo = scim_repo(&s);

    let display = match normalized_display_name(&payload) {
        Ok(value) => value,
        Err(error) => return scim_err(&error),
    };
    let identity_issuer = match configured_identity_issuer() {
        Ok(issuer) => issuer,
        Err(error) => return scim_err(&error),
    };
    match repo
        .update_user_atomic_with_identity(
            ws,
            pid,
            aero_storage::scim::ScimUserUpdate {
                user_name: Some(user_name),
                external_id: Some(payload.external_id.as_deref()),
                active: Some(payload.active),
                display_name: Some(&display),
                identity_issuer: identity_issuer.as_deref(),
            },
        )
        .await
    {
        Ok(Some(row)) => {
            s.participant_cache.invalidate(&pid);
            Json(to_scim_user(&row, Some(&display))).into_response()
        }
        Ok(None) => scim_err(&AeroError::NotFound("user".into())),
        Err(aero_storage::ScimUserWriteError::Storage(e)) if is_unique_violation(&e) => scim_err(
            &AeroError::Conflict(format!("userName '{user_name}' exists")),
        ),
        Err(e) => scim_err(&map_user_write_error(e)),
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
    if let Err(error) = validate_patch_operation_count(patch.operations.len()) {
        return scim_err(&error);
    }
    let pid = match parse_participant_id(&id) {
        Ok(p) => p,
        Err(e) => return scim_err(&e),
    };
    let repo = scim_repo(&s);

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
                if let Some(e) = op
                    .value
                    .get("externalId")
                    .and_then(serde_json::Value::as_str)
                {
                    new_external_id = Some(Some(e.to_owned()));
                }
            }
            _ => {} // other paths (emails, etc.) are not mapped — documented seam.
        }
    }

    if let Some(value) = new_user_name.as_mut() {
        let normalized = match validate_user_name(value) {
            Ok(normalized) => normalized.to_owned(),
            Err(error) => return scim_err(&error),
        };
        *value = normalized;
    }
    if let Some(value) = new_external_id.as_ref().and_then(Option::as_deref) {
        if let Err(error) = validate_external_id(Some(value)) {
            return scim_err(&error);
        }
    }
    if let Some(value) = new_display.as_mut() {
        let normalized = value.trim().to_owned();
        if normalized.is_empty() {
            return scim_err(&AeroError::Invalid("display name cannot be empty".into()));
        }
        if normalized.len() > MAX_DISPLAY_NAME_BYTES {
            return scim_err(&AeroError::Invalid("display name is too long".into()));
        }
        *value = normalized;
    }
    let identity_issuer = match configured_identity_issuer() {
        Ok(issuer) => issuer,
        Err(error) => return scim_err(&error),
    };

    match repo
        .update_user_atomic_with_identity(
            ws,
            pid,
            aero_storage::scim::ScimUserUpdate {
                user_name: new_user_name.as_deref(),
                external_id: new_external_id.as_ref().map(Option::as_deref),
                active: new_active,
                display_name: new_display.as_deref(),
                identity_issuer: identity_issuer.as_deref(),
            },
        )
        .await
    {
        Ok(Some(row)) => {
            if new_display.is_some() {
                s.participant_cache.invalidate(&pid);
            }
            let display = new_display.or(participant_display_name(&s, pid).await);
            Json(to_scim_user(&row, display.as_deref())).into_response()
        }
        Ok(None) => scim_err(&AeroError::NotFound("user".into())),
        Err(aero_storage::ScimUserWriteError::Storage(e)) if is_unique_violation(&e) => {
            scim_err(&AeroError::Conflict("userName collision".to_owned()))
        }
        Err(e) => scim_err(&map_user_write_error(e)),
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
        Err(e) => scim_err(&map_user_write_error(e)),
    }
}
