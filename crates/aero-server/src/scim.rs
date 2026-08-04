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
//! * SCIM **Group** ⇄ a [`UserGroup`](aero_storage::UserGroupRepo) — a real,
//!   workspace-scoped named member group (the `user_groups` table). The
//!   [`UserGroupId`] (as a ULID string) is the SCIM Group `id`, `displayName` is
//!   the group's `name`, and `members[].value` are participant ids. Full CRUD +
//!   member `add`/`remove` PATCH is supported, scoped to the token's workspace.
//!
//! ## What is fully implemented vs. a documented seam
//!
//! * **Fully implemented:** Users CRUD (`GET`/`POST`/`PUT`/`PATCH`/`DELETE`),
//!   the `userName eq "x"` filter, `startIndex`/`count` pagination, 201/200/204/
//!   404/409 status codes, and the (de)serialized RFC 7643 schema types.
//!   Groups CRUD (`GET`/`POST`/`PUT`/`PATCH`/`DELETE`) mapped to real
//!   [`UserGroup`](aero_storage::UserGroupRepo)s, with `members` add/remove PATCH.
//! * **Documented seams:** (1) `PATCH` on a User handles the common cases real
//!   `IdPs` send — the `active` toggle (deprovision/reactivate) and name/email
//!   replacement — rather than the full RFC 7644 §3.5.2 path-filter PATCH grammar.
//!   (2) The driving `IdP` is whichever one is handed a minted token; nothing here
//!   is Okta/Azure-specific. (3) A SCIM Group has no `handle` (Aero's unique
//!   mention key); we derive one from `displayName` (slug + numeric suffix on
//!   collision), since SCIM only carries a `displayName`.

use std::str::FromStr;

use aero_common::{
    Error as AeroError, ParticipantId, Result as AeroResult, ScimTokenId, UserGroupId, WorkspaceId,
};
use aero_storage::scim::{generate_token, hash_token, MAX_SCIM_TOKENS_PER_WORKSPACE};
use aero_storage::{ScimRepo, ScimTokenWriteError, ScimUserRow, UserGroup, UserGroupRepo};
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

mod groups;
#[cfg(test)]
mod tests;
mod users;

// Re-export the public Group helpers so `crate::scim::slugify_handle` /
// `crate::scim::parse_group_patch_ops` / `crate::scim::GroupPatchAction` remain
// reachable at their original path after the split.
pub use groups::{parse_group_patch_ops, slugify_handle, GroupPatchAction};

// ============================================================ RFC 7643 schema

const SCHEMA_USER: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
const SCHEMA_GROUP: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
const SCHEMA_LIST: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
const SCHEMA_ERROR: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
const MAX_SCIM_PATCH_OPERATIONS: usize = 100;

fn validate_patch_operation_count(count: usize) -> AeroResult<()> {
    if count > MAX_SCIM_PATCH_OPERATIONS {
        return Err(AeroError::Invalid(format!(
            "Operations must contain at most {MAX_SCIM_PATCH_OPERATIONS} entries"
        )));
    }
    Ok(())
}

/// SCIM `name` complex attribute (RFC 7643 §4.1.1).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScimName {
    #[serde(rename = "givenName", default, skip_serializing_if = "Option::is_none")]
    pub given_name: Option<String>,
    #[serde(
        rename = "familyName",
        default,
        skip_serializing_if = "Option::is_none"
    )]
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
    #[serde(
        rename = "lastModified",
        default,
        skip_serializing_if = "Option::is_none"
    )]
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
    #[serde(
        rename = "externalId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
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
        .route(
            "/scim/v2/Users",
            get(users::list_users).post(users::create_user),
        )
        .route(
            "/scim/v2/Users/:id",
            get(users::get_user)
                .put(users::put_user)
                .patch(users::patch_user)
                .delete(users::delete_user),
        )
        .route(
            "/scim/v2/Groups",
            get(groups::list_groups).post(groups::create_group),
        )
        .route(
            "/scim/v2/Groups/:id",
            get(groups::get_group)
                .put(groups::put_group)
                .patch(groups::patch_group)
                .delete(groups::delete_group),
        )
        // Management — AuthUser + workspace admin. Mint / revoke SCIM tokens.
        .route("/api/workspaces/:id/scim/token", post(mint_token))
        .route("/api/workspaces/:id/scim/tokens", get(list_tokens))
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
    t.format(&time::format_description::well_known::Rfc3339)
        .ok()
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

// ============================================================ Token management (AuthUser)

#[derive(Deserialize)]
struct MintTokenReq {
    #[serde(default)]
    label: Option<String>,
}

fn map_token_write_error(error: ScimTokenWriteError) -> AeroError {
    match error {
        ScimTokenWriteError::TokenNotFound => AeroError::NotFound("scim token".into()),
        ScimTokenWriteError::QuotaExceeded => AeroError::Conflict(format!(
            "SCIM token quota exceeded (maximum {MAX_SCIM_TOKENS_PER_WORKSPACE} per workspace)"
        )),
        ScimTokenWriteError::Governance(error) => error,
        ScimTokenWriteError::Storage(error) => AeroError::from(error),
    }
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
    let ws =
        WorkspaceId::from_str(&id).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))?;
    let label = req
        .label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty());
    if label.is_some_and(|label| label.len() > 100) {
        return Err(AeroError::Invalid("SCIM token label is too long".into()).into());
    }
    let secret = generate_token();
    let id = scim_repo(&s)
        .create_token_authorized(ws, &hash_token(&secret), label, auth.participant_id)
        .await
        .map_err(map_token_write_error)?;
    Ok(Json(serde_json::json!({
        "id": id,
        "workspace_id": ws,
        "label": label,
        // Shown ONCE — clients must store it now; the server keeps only its hash.
        "token": secret,
    })))
}

/// `GET /api/workspaces/:id/scim/tokens` — list safe credential metadata.
///
/// Effective Owner/Admin access is required and rechecked under the same
/// workspace lock as the read. The response deliberately omits both bearer
/// plaintext and token hashes. It returns at most one sentinel row beyond the
/// normal quota so administrators can detect and revoke a legacy overage.
async fn list_tokens(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws =
        WorkspaceId::from_str(&id).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))?;
    let tokens = scim_repo(&s)
        .list_tokens_authorized(ws, auth.participant_id)
        .await
        .map_err(map_token_write_error)?;
    let over_quota = tokens.len() > MAX_SCIM_TOKENS_PER_WORKSPACE;
    Ok(Json(serde_json::json!({
        "tokens": tokens,
        "quota": MAX_SCIM_TOKENS_PER_WORKSPACE,
        "over_quota": over_quota,
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
    scim_repo(&s)
        .revoke_token_authorized(token_id, auth.participant_id)
        .await
        .map_err(map_token_write_error)?;
    Ok(StatusCode::NO_CONTENT)
}
