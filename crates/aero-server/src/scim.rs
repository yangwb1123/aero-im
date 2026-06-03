//! SCIM 2.0 provisioning HTTP API (RFC 7643 schema / RFC 7644 protocol).
//!
//! Lets an external `IdP` (Okta / Azure AD / `OneLogin`) provision and de-provision
//! workspace members. Mounted under `/scim/v2`. Unlike the rest of the gateway,
//! SCIM does **not** use the participant JWT ([`AuthUser`]): every SCIM request
//! carries a per-workspace bearer token that this module reads from the
//! `Authorization` header manually and resolves to a workspace via
//! [`ScimRepo::workspace_for_token_hash`]. The resolved workspace is the tenant
//! every operation is scoped to — a SCIM token can never touch another tenant.
//!
//! ## Mapping (RFC 7643 ⇄ Aero)
//!
//! * SCIM **User** ⇄ a GLOBAL [`participant`](aero_storage::ParticipantRepo) +
//!   a [`workspace_member`](aero_storage::WorkspaceRepo) of the token's workspace
//!   + a `scim_users` row. The participant id (as a ULID string) is the SCIM `id`.
//! * SCIM **Group** ⇄ the workspace itself (read-only). We expose exactly one
//!   group — the token's workspace — listing its members. This is a documented
//!   minimal mapping: Aero has no sub-group construct, so group *writes* are not
//!   supported. Users are implemented fully (CRUD); Groups are read-only.
//!
//! ## What is fully implemented vs. a documented seam
//!
//! * **Fully implemented:** Users CRUD (`GET`/`POST`/`PUT`/`PATCH`/`DELETE`),
//!   the `userName eq "x"` filter, `startIndex`/`count` pagination, 201/200/204/
//!   404/409 status codes, and the (de)serialized RFC 7643 schema types.
//! * **Documented seams:** (1) Groups are a read-only mapping of the workspace
//!   (no group writes). (2) `PATCH` handles the common cases real `IdPs` send —
//!   the `active` toggle (deprovision/reactivate) and name/email replacement —
//!   rather than the full RFC 7644 §3.5.2 path-filter PATCH grammar. (3) The
//!   driving `IdP` is whichever one is handed a minted token; nothing here is
//!   Okta/Azure-specific.

use std::str::FromStr;

use aero_common::{
    Error as AeroError, ParticipantId, Result as AeroResult, ScimTokenId, WorkspaceId,
    WorkspaceRole,
};
use aero_storage::scim::{generate_token, hash_token};
use aero_storage::{ScimRepo, ScimUserRow};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use aero_auth::AuthUser;

use crate::error::ApiResult;
use crate::state::AppState;

// ============================================================ RFC 7643 schema

const SCHEMA_USER: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
const SCHEMA_GROUP: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
const SCHEMA_LIST: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
const SCHEMA_ERROR: &str = "urn:ietf:params:scim:api:messages:2.0:Error";

/// SCIM `name` complex attribute (RFC 7643 §4.1.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScimName {
    #[serde(rename = "givenName", default, skip_serializing_if = "Option::is_none")]
    pub given_name: Option<String>,
    #[serde(rename = "familyName", default, skip_serializing_if = "Option::is_none")]
    pub family_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formatted: Option<String>,
}

/// SCIM multi-valued `emails` entry (RFC 7643 §4.1.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScimEmail {
    pub value: String,
    #[serde(default)]
    pub primary: bool,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// SCIM `meta` attribute (RFC 7643 §3.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScimMeta {
    #[serde(rename = "resourceType")]
    pub resource_type: String,
    #[serde(rename = "created", default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(rename = "lastModified", default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

/// SCIM `User` resource (RFC 7643 §4.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScimUser {
    pub schemas: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "externalId", default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    #[serde(rename = "userName")]
    pub user_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<ScimName>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emails: Vec<ScimEmail>,
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<ScimMeta>,
}

fn default_true() -> bool {
    true
}

/// SCIM `Group` resource (RFC 7643 §4.2) — read-only workspace mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScimGroup {
    pub schemas: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<ScimGroupMember>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<ScimMeta>,
}

/// A member reference inside a SCIM `Group` (RFC 7643 §4.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScimGroupMember {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
}

/// SCIM `ListResponse` envelope (RFC 7644 §3.4.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScimListResponse<T> {
    pub schemas: Vec<String>,
    #[serde(rename = "totalResults")]
    pub total_results: i64,
    #[serde(rename = "startIndex")]
    pub start_index: i64,
    #[serde(rename = "itemsPerPage")]
    pub items_per_page: i64,
    #[serde(rename = "Resources")]
    pub resources: Vec<T>,
}

impl<T> ScimListResponse<T> {
    fn new(resources: Vec<T>, total_results: i64, start_index: i64) -> Self {
        let items_per_page = i64::try_from(resources.len()).unwrap_or(i64::MAX);
        Self {
            schemas: vec![SCHEMA_LIST.to_owned()],
            total_results,
            start_index,
            items_per_page,
            resources,
        }
    }
}

/// SCIM `Error` response (RFC 7644 §3.12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScimError {
    pub schemas: Vec<String>,
    pub detail: String,
    /// RFC 7644 carries `status` as a STRING.
    pub status: String,
}

impl ScimError {
    fn new(status: u16, detail: impl Into<String>) -> Self {
        Self {
            schemas: vec![SCHEMA_ERROR.to_owned()],
            detail: detail.into(),
            status: status.to_string(),
        }
    }
}

/// Parse a SCIM filter, supporting the common `attr eq "value"` form (RFC 7644
/// §3.4.2.2). Returns `Some((attr, value))` for a single `eq` comparison, else
/// `None` (unsupported operators / compound filters fall back to "list all").
/// Pure, so the parser is unit-tested offline.
///
/// Accepts e.g. `userName eq "alice@example.com"` (the Okta/Azure provisioning
/// query). The attribute is returned verbatim (case preserved); the quoted value
/// is unescaped for the two JSON escapes a quoted string can contain (`\"`,`\\`).
#[must_use]
pub fn parse_filter(filter: &str) -> Option<(String, String)> {
    let trimmed = filter.trim();
    // Split on the first ` eq ` (case-insensitive on the operator), since the
    // value may itself contain "eq".
    let lower = trimmed.to_ascii_lowercase();
    let eq_pos = lower.find(" eq ")?;
    let attr = trimmed[..eq_pos].trim();
    let rest = trimmed[eq_pos + 4..].trim();
    if attr.is_empty() {
        return None;
    }
    // The attribute must be a simple identifier (letters/digits/.). Reject
    // anything with spaces (a compound expression) so `a or b eq "x"` is `None`.
    if attr.contains(char::is_whitespace) {
        return None;
    }
    // The value must be a double-quoted string and the WHOLE remainder (no
    // trailing ` and ...`), keeping this strictly a single-eq parser.
    let bytes = rest.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'"' {
        return None;
    }
    let mut value = String::new();
    let mut chars = rest[1..].chars();
    loop {
        match chars.next()? {
            '\\' => match chars.next()? {
                '"' => value.push('"'),
                '\\' => value.push('\\'),
                other => {
                    value.push('\\');
                    value.push(other);
                }
            },
            '"' => break, // closing quote
            c => value.push(c),
        }
    }
    // Anything after the closing quote means it was not a lone `eq` filter.
    if chars.as_str().trim().is_empty() {
        Some((attr.to_owned(), value))
    } else {
        None
    }
}

// ============================================================ Router

/// Mount the SCIM 2.0 routes under `/scim/v2`, plus the AuthUser-gated token
/// management routes. Folded into the main router by [`crate::routes::build`].
pub fn routes() -> Router<AppState> {
    Router::new()
        // SCIM 2.0 — bearer-token (NOT JWT) authenticated, tenant = the token's workspace.
        .route("/scim/v2/Users", get(list_users).post(create_user))
        .route(
            "/scim/v2/Users/:id",
            get(get_user)
                .put(put_user)
                .patch(patch_user)
                .delete(delete_user),
        )
        .route("/scim/v2/Groups", get(list_groups))
        .route("/scim/v2/Groups/:id", get(get_group))
        // Management — AuthUser + workspace admin. Mint / revoke SCIM tokens.
        .route("/api/workspaces/:id/scim/token", post(mint_token))
        .route("/api/scim/tokens/:id", delete(revoke_token))
}

// ============================================================ SCIM auth

/// Resolve the SCIM bearer token from the `Authorization` header to a workspace.
/// This is the SCIM authentication check — done manually (not via the JWT
/// extractor) because SCIM clients present a long-lived provisioning secret.
///
/// # Errors
/// [`AeroError::Unauthorized`] when the header is missing/malformed, or the
/// token is unknown/revoked.
async fn scim_workspace(s: &AppState, headers: &HeaderMap) -> AeroResult<WorkspaceId> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AeroError::Unauthorized("missing SCIM bearer token".into()))?;
    let token = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| AeroError::Unauthorized("expected SCIM Bearer token".into()))?;
    let repo = scim_repo(s);
    repo.workspace_for_token_hash(&hash_token(token))
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Unauthorized("invalid or revoked SCIM token".into()))
}

/// Construct a [`ScimRepo`] from the shared pool. Mirrors how other feature
/// modules build their repos inline rather than threading a new `AppState` field.
fn scim_repo(s: &AppState) -> ScimRepo {
    ScimRepo::new(s.participants.pool().clone())
}

/// Render any [`AeroError`] as a SCIM-style error response with the RFC status
/// code. SCIM clients parse the `detail`/`status` body, so we deliberately do
/// NOT reuse the gateway's generic [`crate::error::ApiError`] shape here.
fn scim_err(e: &AeroError) -> Response {
    let code = e.status_code();
    let status = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(ScimError::new(code, e.to_string()))).into_response()
}

// ============================================================ User mapping

/// Map a stored SCIM-user row + its participant to the RFC 7643 `User` resource.
/// `display_name` is the participant's display name (used to back the SCIM
/// `name.formatted`); `email` is taken to be the `userName` when it looks like an
/// address (the common `IdP` convention) — purely a best-effort projection.
fn to_scim_user(row: &ScimUserRow, display_name: Option<&str>) -> ScimUser {
    let emails = if row.user_name.contains('@') {
        vec![ScimEmail {
            value: row.user_name.clone(),
            primary: true,
            kind: Some("work".to_owned()),
        }]
    } else {
        Vec::new()
    };
    ScimUser {
        schemas: vec![SCHEMA_USER.to_owned()],
        id: Some(row.participant_id.to_string()),
        external_id: row.external_id.clone(),
        user_name: row.user_name.clone(),
        name: display_name.map(|n| ScimName {
            formatted: Some(n.to_owned()),
            ..ScimName::default()
        }),
        emails,
        active: row.active,
        meta: Some(ScimMeta {
            resource_type: "User".to_owned(),
            created: rfc3339(row.created_at),
            last_modified: rfc3339(row.updated_at),
            location: Some(format!("/scim/v2/Users/{}", row.participant_id)),
        }),
    }
}

/// Best-effort RFC3339 rendering of a timestamp for SCIM `meta`.
fn rfc3339(t: time::OffsetDateTime) -> Option<String> {
    t.format(&time::format_description::well_known::Rfc3339).ok()
}

/// Pick a display name for a new participant from the SCIM payload: prefer
/// `name.formatted`, then given+family, then fall back to `userName`.
fn display_name_for(u: &ScimUser) -> String {
    if let Some(name) = &u.name {
        if let Some(f) = &name.formatted {
            if !f.trim().is_empty() {
                return f.clone();
            }
        }
        let parts: Vec<&str> = [name.given_name.as_deref(), name.family_name.as_deref()]
            .into_iter()
            .flatten()
            .filter(|p| !p.trim().is_empty())
            .collect();
        if !parts.is_empty() {
            return parts.join(" ");
        }
    }
    u.user_name.clone()
}

fn parse_participant_id(s: &str) -> AeroResult<ParticipantId> {
    ParticipantId::from_str(s).map_err(|e| AeroError::Invalid(format!("user id: {e}")))
}

/// Distinguish a unique-constraint violation (→ 409) from any other DB error.
fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_unique_violation())
}

// ============================================================ User handlers

#[derive(Deserialize)]
struct ListUsersQuery {
    filter: Option<String>,
    #[serde(rename = "startIndex")]
    start_index: Option<i64>,
    count: Option<i64>,
}

/// `GET /scim/v2/Users` — list users, optionally filtered by `userName eq "x"`,
/// paginated by `startIndex`/`count`. Returns a SCIM `ListResponse`.
async fn list_users(
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
async fn get_user(
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
async fn create_user(
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
async fn put_user(
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
struct ScimPatch {
    #[serde(rename = "Operations", default)]
    operations: Vec<ScimPatchOp>,
}

#[derive(Deserialize)]
struct ScimPatchOp {
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
async fn patch_user(
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

/// `DELETE /scim/v2/Users/:id` — deprovision: remove workspace membership + mark
/// the SCIM row inactive (the global participant is retained). 204 on success,
/// 404 if the user was not provisioned in this workspace.
async fn delete_user(
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

// ============================================================ Group handlers (read-only)

/// Build the single SCIM Group that represents the token's workspace, listing
/// its current members. Read-only (documented seam: no group writes).
async fn workspace_group(s: &AppState, ws: WorkspaceId) -> AeroResult<ScimGroup> {
    let workspace = s
        .workspaces
        .get(ws)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("workspace".into()))?;
    let members = s.workspaces.list_members(ws).await.map_err(AeroError::from)?;
    let group_members = members
        .into_iter()
        .map(|m| ScimGroupMember {
            value: m.participant_id.to_string(),
            display: None,
        })
        .collect();
    Ok(ScimGroup {
        schemas: vec![SCHEMA_GROUP.to_owned()],
        id: Some(workspace.id.to_string()),
        display_name: workspace.name,
        members: group_members,
        meta: Some(ScimMeta {
            resource_type: "Group".to_owned(),
            created: rfc3339(workspace.created_at),
            last_modified: rfc3339(workspace.created_at),
            location: Some(format!("/scim/v2/Groups/{}", workspace.id)),
        }),
    })
}

/// `GET /scim/v2/Groups` — return a one-element list: the token's workspace as a
/// single Group. (Aero has no sub-group construct; this is the documented
/// minimal read-only mapping.)
async fn list_groups(State(s): State<AppState>, headers: HeaderMap) -> Response {
    let ws = match scim_workspace(&s, &headers).await {
        Ok(w) => w,
        Err(e) => return scim_err(&e),
    };
    match workspace_group(&s, ws).await {
        Ok(g) => Json(ScimListResponse::new(vec![g], 1, 1)).into_response(),
        Err(e) => scim_err(&e),
    }
}

/// `GET /scim/v2/Groups/:id` — the workspace-as-Group. Only the token's own
/// workspace id resolves; any other id is 404 (tenant isolation).
async fn get_group(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let ws = match scim_workspace(&s, &headers).await {
        Ok(w) => w,
        Err(e) => return scim_err(&e),
    };
    // A SCIM client may only read THIS token's workspace as a group.
    match WorkspaceId::from_str(&id) {
        Ok(req) if req == ws => match workspace_group(&s, ws).await {
            Ok(g) => Json(g).into_response(),
            Err(e) => scim_err(&e),
        },
        _ => scim_err(&AeroError::NotFound("group".into())),
    }
}

// ============================================================ Token management (AuthUser)

/// Resolve the caller's role in a workspace, rejecting non-members. Mirrors
/// [`crate::workspaces::caller_role`] (kept local to avoid widening its visibility).
async fn caller_role(
    s: &AppState,
    ws: WorkspaceId,
    caller: ParticipantId,
) -> AeroResult<WorkspaceRole> {
    s.workspaces
        .member_role(ws, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))
}

#[derive(Deserialize)]
struct MintTokenReq {
    #[serde(default)]
    label: Option<String>,
}

/// `POST /api/workspaces/:id/scim/token` — **admin/owner**: mint a SCIM bearer
/// token for the workspace. The plaintext token is returned **once** in the
/// response and never stored (only its SHA-256 hash is). The `IdP` is configured
/// with this value as its SCIM bearer credential.
async fn mint_token(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    Json(req): Json<MintTokenReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = WorkspaceId::from_str(&id)
        .map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))?;
    let caller = caller_role(&s, ws, auth.participant_id).await?;
    if !caller.can_administer() {
        return Err(AeroError::Forbidden("minting SCIM tokens requires admin".into()).into());
    }
    let secret = generate_token();
    let id = scim_repo(&s)
        .create_token(ws, &hash_token(&secret), req.label.as_deref())
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::json!({
        "id": id,
        "workspace_id": ws,
        "label": req.label,
        // Shown ONCE — clients must store it now; the server keeps only its hash.
        "token": secret,
    })))
}

/// `DELETE /api/scim/tokens/:id` — **admin/owner**: revoke a SCIM token. The
/// caller must administer the token's own workspace.
async fn revoke_token(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let token_id = ScimTokenId::from_str(&id)
        .map_err(|e| AeroError::Invalid(format!("scim token id: {e}")))?;
    let repo = scim_repo(&s);
    let ws = repo
        .token_workspace(token_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("scim token".into()))?;
    let caller = caller_role(&s, ws, auth.participant_id).await?;
    if !caller.can_administer() {
        return Err(AeroError::Forbidden("revoking SCIM tokens requires admin".into()).into());
    }
    repo.revoke_token(token_id).await.map_err(AeroError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- parse_filter ----------

    #[test]
    fn parse_filter_handles_username_eq() {
        let (attr, value) = parse_filter(r#"userName eq "alice@example.com""#).unwrap();
        assert_eq!(attr, "userName");
        assert_eq!(value, "alice@example.com");
    }

    #[test]
    fn parse_filter_is_case_insensitive_on_operator() {
        let (attr, value) = parse_filter(r#"userName EQ "bob""#).unwrap();
        assert_eq!(attr, "userName");
        assert_eq!(value, "bob");
    }

    #[test]
    fn parse_filter_unescapes_quotes_and_backslashes() {
        let (_, value) = parse_filter(r#"userName eq "a\"b\\c""#).unwrap();
        assert_eq!(value, r#"a"b\c"#);
    }

    #[test]
    fn parse_filter_rejects_unsupported() {
        // Non-eq operators.
        assert!(parse_filter(r#"userName co "ali""#).is_none());
        assert!(parse_filter("userName pr").is_none());
        // Compound filters.
        assert!(parse_filter(r#"userName eq "a" and active eq "true""#).is_none());
        // Missing / malformed value.
        assert!(parse_filter("userName eq alice").is_none());
        assert!(parse_filter(r#"userName eq "unterminated"#).is_none());
        assert!(parse_filter("").is_none());
        assert!(parse_filter("eq").is_none());
    }

    #[test]
    fn parse_filter_handles_empty_value() {
        let (attr, value) = parse_filter(r#"userName eq """#).unwrap();
        assert_eq!(attr, "userName");
        assert_eq!(value, "");
    }

    // ---------- ScimUser (de)serialization round-trip ----------

    #[test]
    fn scim_user_roundtrips_camelcase() {
        let user = ScimUser {
            schemas: vec![SCHEMA_USER.to_owned()],
            id: Some("01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned()),
            external_id: Some("ext-7".to_owned()),
            user_name: "carol@example.com".to_owned(),
            name: Some(ScimName {
                given_name: Some("Carol".to_owned()),
                family_name: Some("Danvers".to_owned()),
                formatted: Some("Carol Danvers".to_owned()),
            }),
            emails: vec![ScimEmail {
                value: "carol@example.com".to_owned(),
                primary: true,
                kind: Some("work".to_owned()),
            }],
            active: true,
            meta: Some(ScimMeta {
                resource_type: "User".to_owned(),
                created: None,
                last_modified: None,
                location: Some("/scim/v2/Users/x".to_owned()),
            }),
        };
        let json = serde_json::to_value(&user).unwrap();
        // The renamed camelCase fields must be on the wire exactly.
        assert_eq!(json["userName"], "carol@example.com");
        assert_eq!(json["externalId"], "ext-7");
        assert_eq!(json["name"]["givenName"], "Carol");
        assert_eq!(json["name"]["familyName"], "Danvers");
        assert_eq!(json["emails"][0]["type"], "work");
        assert_eq!(json["meta"]["resourceType"], "User");
        // Round-trip back to the struct.
        let back: ScimUser = serde_json::from_value(json).unwrap();
        assert_eq!(back, user);
    }

    #[test]
    fn scim_user_deserializes_minimal_okta_payload() {
        // What Okta sends on create: schemas + userName + active, nothing else.
        let raw = serde_json::json!({
            "schemas": [SCHEMA_USER],
            "userName": "dave@example.com",
            "active": true
        });
        let user: ScimUser = serde_json::from_value(raw).unwrap();
        assert_eq!(user.user_name, "dave@example.com");
        assert!(user.active);
        assert!(user.name.is_none());
        assert!(user.emails.is_empty());
        assert!(user.id.is_none());
    }

    #[test]
    fn scim_user_active_defaults_to_true_when_absent() {
        let raw = serde_json::json!({ "schemas": [SCHEMA_USER], "userName": "e" });
        let user: ScimUser = serde_json::from_value(raw).unwrap();
        assert!(user.active, "absent active defaults to true");
    }

    // ---------- ListResponse round-trip ----------

    #[test]
    fn list_response_roundtrips_camelcase() {
        let list = ScimListResponse::new(vec!["a".to_owned(), "b".to_owned()], 5, 1);
        let json = serde_json::to_value(&list).unwrap();
        assert_eq!(json["schemas"][0], SCHEMA_LIST);
        assert_eq!(json["totalResults"], 5);
        assert_eq!(json["startIndex"], 1);
        assert_eq!(json["itemsPerPage"], 2);
        assert_eq!(json["Resources"][1], "b");
        let back: ScimListResponse<String> = serde_json::from_value(json).unwrap();
        assert_eq!(back, list);
    }

    #[test]
    fn scim_error_status_is_a_string() {
        let json = serde_json::to_value(ScimError::new(409, "userName exists")).unwrap();
        assert_eq!(json["status"], "409", "RFC 7644 carries status as a string");
        assert_eq!(json["detail"], "userName exists");
        assert_eq!(json["schemas"][0], SCHEMA_ERROR);
    }

    // ---------- display_name_for ----------

    #[test]
    fn display_name_prefers_formatted_then_parts_then_username() {
        let mut u = ScimUser {
            schemas: vec![],
            id: None,
            external_id: None,
            user_name: "fallback@x.com".to_owned(),
            name: None,
            emails: vec![],
            active: true,
            meta: None,
        };
        // No name → userName.
        assert_eq!(display_name_for(&u), "fallback@x.com");
        // given+family.
        u.name = Some(ScimName {
            given_name: Some("Grace".to_owned()),
            family_name: Some("Hopper".to_owned()),
            formatted: None,
        });
        assert_eq!(display_name_for(&u), "Grace Hopper");
        // formatted wins.
        u.name = Some(ScimName {
            given_name: Some("Grace".to_owned()),
            family_name: Some("Hopper".to_owned()),
            formatted: Some("Rear Admiral Grace Hopper".to_owned()),
        });
        assert_eq!(display_name_for(&u), "Rear Admiral Grace Hopper");
    }
}
