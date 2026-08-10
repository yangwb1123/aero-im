//! Workspace / Org management HTTP API (ROADMAP 方向一 — multi-tenant foundation).
//!
//! Additive layer over the already-merged [`aero_storage::WorkspaceRepo`]:
//! create a workspace (creator becomes `Owner`), list the workspaces a caller
//! belongs to, list members, and invite / re-role / remove members with RBAC.
//!
//! Tenant-scoping of existing room/message queries is a *separate* later batch —
//! nothing here touches `RoomRepo` / `MessageRepo`.
//!
//! ## Authorization is pure and DB-free
//!
//! Every per-request privilege decision is a free function ([`authorize_invite`],
//! [`authorize_role_change`], [`authorize_remove`]) returning `Result<(),
//! AeroError>`. They compose the storage-side predicates (`role_can_invite`,
//! `role_can_assign`, `role_can_manage_member`, `role_can_remove`) and add the
//! HTTP failure mapping. Keeping them DB-free lets the test module exercise every
//! `(caller, target/subject)` role pair offline — mirroring how `aero-storage`
//! and the rest of `aero-server` keep testable logic separate from live SQL
//! (Postgres is absent in CI). The async handlers below are then a thin shell:
//! resolve ids + caller role from the repo, call the pure guard, run the repo
//! mutation.

use std::str::FromStr;

use aero_common::{
    AuditId, Error as AeroError, ParticipantId, Result as AeroResult, WorkspaceId, WorkspaceRole,
};
use aero_storage::{
    role_can_assign, role_can_invite, role_can_manage_member, role_can_remove,
    validate_retention_days, WorkspaceMemberWriteError, WorkspaceMuteRepo,
    WorkspaceNotifDefaultsRepo, WorkspaceRepo,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use aero_auth::AuthUser;

use crate::error::ApiResult;
use crate::state::AppState;

// ---------- Router ----------

/// Mount the workspace/org routes. Folded into the main router by
/// [`crate::routes::build`]; kept separate so the route table and its handlers
/// live next to the pure authorization logic they enforce.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/workspaces",
            post(create_workspace).get(list_workspaces),
        )
        // Owner-only erasure (ROADMAP 方向一 合规); GET of a single workspace is
        // intentionally not (yet) offered here — listing is via `/api/workspaces`.
        .route(
            "/api/workspaces/:id",
            axum::routing::patch(update_workspace).delete(delete_workspace),
        )
        // Owner-only full-tenant export (GDPR data portability).
        .route("/api/workspaces/:id/export", get(export_workspace))
        // Admin/owner: set or clear the per-workspace message-retention window.
        .route(
            "/api/workspaces/:id/retention",
            axum::routing::put(set_retention),
        )
        .route(
            "/api/workspaces/:id/members",
            get(list_members).post(add_member),
        )
        .route(
            "/api/workspaces/:id/members/:pid",
            axum::routing::patch(change_member_role).delete(remove_member),
        )
        .route("/api/workspaces/:id/audit", get(list_audit))
        // Filtered audit search (admin) + CSV export (operability).
        .route("/api/workspaces/:id/audit/export", get(export_audit_csv))
        // Workspace-wide mute (ROADMAP6 Lane A): suppress ALL notifications from a workspace.
        .route(
            "/api/workspaces/:id/mute",
            post(mute_workspace).delete(unmute_workspace),
        )
        .route("/api/workspaces/:id/muted", get(workspace_mute_status))
        // Workspace branding (ROADMAP7 Lane C): logo, color scheme, custom domain, description.
        .route(
            "/api/workspaces/:id/branding",
            axum::routing::patch(update_branding),
        )
        // ROADMAP12: workspace default notification level (admin only).
        .route(
            "/api/workspaces/:id/notification-defaults",
            get(get_notif_defaults).put(set_notif_defaults),
        )
}

/// Append an audit event without ever failing the caller's request: the trail is
/// observability, not a transactional invariant, so a logging hiccup must not
/// roll back a successful administrative action.
async fn audit(
    s: &AppState,
    workspace: WorkspaceId,
    actor: ParticipantId,
    action: &str,
    target: Option<&str>,
    detail: serde_json::Value,
) {
    if let Err(e) = s
        .audit
        .append(workspace, Some(actor), action, target, detail)
        .await
    {
        tracing::warn!(error = ?e, %workspace, action, "audit append failed");
    }
}

// ---------- Pure authorization decisions (DB-free, unit-tested) ----------

/// May `caller` invite a new member to be granted role `target`?
///
/// Two independent gates, both required: the caller must be allowed to invite at
/// all ([`role_can_invite`]), and must be allowed to *grant* the requested role
/// without escalating past their own ([`role_can_assign`]). Failure maps to
/// `403 Forbidden` so an unprivileged caller cannot distinguish "can't invite"
/// from "can't grant that role".
///
/// # Errors
/// [`AeroError::Forbidden`] when either gate denies the action.
pub fn authorize_invite(caller: WorkspaceRole, target: WorkspaceRole) -> AeroResult<()> {
    if role_can_invite(caller) && role_can_assign(caller, target) {
        Ok(())
    } else {
        Err(AeroError::Forbidden(
            "cannot invite member with that role".into(),
        ))
    }
}

/// May `caller` change an existing member (currently holding `subject`) to
/// `new_role`?
///
/// Both gates required: the caller may grant `new_role` without escalation
/// ([`role_can_assign`]) **and** the caller outranks-or-equals the member being
/// changed ([`role_can_manage_member`]) — so an admin can re-role members and
/// peer admins but never touch an owner, and nobody can be promoted above the
/// actor.
///
/// # Errors
/// [`AeroError::Forbidden`] when either gate denies the action.
pub fn authorize_role_change(
    caller: WorkspaceRole,
    new_role: WorkspaceRole,
    subject: WorkspaceRole,
) -> AeroResult<()> {
    if role_can_assign(caller, new_role) && role_can_manage_member(caller, subject) {
        Ok(())
    } else {
        Err(AeroError::Forbidden(
            "cannot change that member's role".into(),
        ))
    }
}

/// May `caller` remove a member currently holding `subject`?
///
/// Self-removal (`is_self`) is treated as "leaving": any non-owner member may
/// leave on their own, without admin rights — but an `Owner` may **not** leave,
/// since that would orphan the workspace (ownership transfer / deletion is a
/// distinct, later operation). Removing *someone else* requires the normal admin
/// gates: [`role_can_remove`] **and** [`role_can_manage_member`] (cannot remove a
/// strictly-more-privileged member).
///
/// # Errors
/// [`AeroError::Forbidden`] when the caller may not perform the removal.
pub fn authorize_remove(
    caller: WorkspaceRole,
    subject: WorkspaceRole,
    is_self: bool,
) -> AeroResult<()> {
    if is_self {
        // An owner leaving would orphan the workspace; everyone else may leave.
        return if caller == WorkspaceRole::Owner {
            Err(AeroError::Forbidden(
                "owner cannot leave; transfer ownership or delete the workspace".into(),
            ))
        } else {
            Ok(())
        };
    }
    if role_can_remove(caller) && role_can_manage_member(caller, subject) {
        Ok(())
    } else {
        Err(AeroError::Forbidden("cannot remove that member".into()))
    }
}

// ---------- Shared helpers ----------

fn parse_workspace_id(s: &str) -> AeroResult<WorkspaceId> {
    WorkspaceId::from_str(s).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

fn parse_participant_id(s: &str) -> AeroResult<ParticipantId> {
    ParticipantId::from_str(s).map_err(|e| AeroError::Invalid(format!("participant id: {e}")))
}

fn parse_audit_id(s: &str) -> AeroResult<AuditId> {
    AuditId::from_str(s).map_err(|e| AeroError::Invalid(format!("audit cursor: {e}")))
}

fn map_member_write_error(error: WorkspaceMemberWriteError) -> AeroError {
    match error {
        WorkspaceMemberWriteError::MemberNotFound => AeroError::NotFound("workspace member".into()),
        WorkspaceMemberWriteError::NotAuthorized => {
            AeroError::Forbidden("cannot manage that workspace member".into())
        }
        WorkspaceMemberWriteError::InvalidGuestRole => {
            AeroError::Invalid("single-channel guests must be managed through the guest API".into())
        }
        WorkspaceMemberWriteError::OwnerCannotLeave => AeroError::Forbidden(
            "owner cannot leave; transfer ownership or delete the workspace".into(),
        ),
        WorkspaceMemberWriteError::LastOwner => {
            AeroError::Conflict("transfer ownership before demoting the final owner".into())
        }
        WorkspaceMemberWriteError::ChannelOwnerProtected => AeroError::Conflict(
            "transfer channel ownership before removing this workspace member".into(),
        ),
        WorkspaceMemberWriteError::Storage(error) => AeroError::from(error),
    }
}

/// May `caller` read the workspace's audit trail? Restricted to admins/owners —
/// the trail exposes who-did-what across the tenant, so members/guests are denied.
///
/// # Errors
/// [`AeroError::Forbidden`] for non-administrators.
pub fn authorize_view_audit(caller: WorkspaceRole) -> AeroResult<()> {
    if caller.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden("audit trail requires admin".into()))
    }
}

/// May `caller` export the workspace's complete data snapshot? **Owner-only**
/// (ROADMAP 方向一 合规 — GDPR data portability). A full tenant export is the
/// most sensitive read in the system — every channel, message, member and the
/// audit trail — so it is restricted to the workspace owner; even admins are
/// denied. Delegates the owner check to [`WorkspaceRole::can_manage_workspace`].
///
/// # Errors
/// [`AeroError::Forbidden`] for any non-owner role.
pub fn authorize_export(caller: WorkspaceRole) -> AeroResult<()> {
    if caller.can_manage_workspace() {
        Ok(())
    } else {
        Err(AeroError::Forbidden(
            "workspace export requires owner".into(),
        ))
    }
}

/// May `caller` delete (erase) the entire workspace? **Owner-only** (ROADMAP
/// 方向一 合规 — right-to-be-forgotten). Hard-deleting a tenant and all its data
/// is irreversible, so it is restricted to the owner; admins cannot. Delegates
/// the owner check to [`WorkspaceRole::can_manage_workspace`].
///
/// # Errors
/// [`AeroError::Forbidden`] for any non-owner role.
pub fn authorize_delete(caller: WorkspaceRole) -> AeroResult<()> {
    if caller.can_manage_workspace() {
        Ok(())
    } else {
        Err(AeroError::Forbidden(
            "workspace deletion requires owner".into(),
        ))
    }
}

/// May `caller` set the workspace's message-retention policy? **Admin/owner**
/// (ROADMAP 方向一 合规 — 按组织的留存策略). Configuring retention is workspace
/// administration (like managing members), not the owner-only destructive
/// erasure of [`authorize_delete`] — so it gates on
/// [`WorkspaceRole::can_administer`], the same bar as the audit trail.
///
/// # Errors
/// [`AeroError::Forbidden`] for non-administrators (members / guests).
pub fn authorize_set_retention(caller: WorkspaceRole) -> AeroResult<()> {
    if caller.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden(
            "setting retention requires admin".into(),
        ))
    }
}

/// Only admin and owner may rename a workspace.
pub fn authorize_rename(caller: WorkspaceRole) -> AeroResult<()> {
    if caller.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden(
            "renaming a workspace requires admin".into(),
        ))
    }
}

/// Resolve the caller's role in a workspace, rejecting non-members.
///
/// Used by every member-scoped route so the "must be a member" check (and its
/// `403`) is written once. Returns the role on success.
async fn caller_role(
    repo: &WorkspaceRepo,
    workspace: WorkspaceId,
    caller: ParticipantId,
) -> AeroResult<WorkspaceRole> {
    repo.effective_member_role(workspace, caller)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::Forbidden("not a workspace member".into()))
}

// ---------- Handlers ----------

#[derive(Deserialize)]
struct CreateWorkspaceReq {
    name: String,
    slug: String,
}

/// `POST /api/workspaces` — create a workspace; the authed caller becomes its
/// `Owner` (the repo enrolls the creator atomically inside `create`).
async fn create_workspace(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateWorkspaceReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let name = req.name.trim();
    let slug = req.slug.trim();
    if name.is_empty() {
        return Err(AeroError::Invalid("workspace name is empty".into()).into());
    }
    if name.len() > 128 {
        return Err(AeroError::Invalid("workspace name too long".into()).into());
    }
    if !is_valid_slug(slug) {
        return Err(AeroError::Invalid("slug must be 1-64 chars of [a-z0-9-]".into()).into());
    }
    let ws = s
        .workspaces
        .create(name.to_owned(), slug.to_owned(), auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    audit(
        &s,
        ws.id,
        auth.participant_id,
        "workspace.create",
        None,
        serde_json::json!({ "name": name, "slug": slug }),
    )
    .await;
    Ok(Json(serde_json::to_value(ws).map_err(AeroError::from)?))
}

/// `GET /api/workspaces` — list workspaces the caller belongs to.
async fn list_workspaces(
    State(s): State<AppState>,
    auth: AuthUser,
) -> ApiResult<Json<serde_json::Value>> {
    let list = s
        .workspaces
        .list_for_participant(auth.participant_id)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(list).map_err(AeroError::from)?))
}

/// `GET /api/workspaces/:id/members` — list members; caller must be a member.
async fn list_members(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace_id(&id_str)?;
    // Membership gate: resolving the caller's role rejects non-members with 403.
    caller_role(&s.workspaces, ws, auth.participant_id).await?;
    let members = s
        .workspaces
        .list_members(ws)
        .await
        .map_err(AeroError::from)?;
    Ok(Json(
        serde_json::to_value(members).map_err(AeroError::from)?,
    ))
}

#[derive(Deserialize)]
struct AuditQuery {
    /// Keyset cursor: return events strictly older than this audit id (only used
    /// when no filter criteria are supplied).
    before: Option<String>,
    /// Page size (clamped server-side).
    limit: Option<i64>,
    /// Filter: exact action token (e.g. `member.add`).
    #[serde(default)]
    action: Option<String>,
    /// Filter: exact actor participant id.
    #[serde(default)]
    actor: Option<String>,
    /// Filter: exact target string.
    #[serde(default)]
    target: Option<String>,
    /// Filter: inclusive lower time bound (RFC 3339).
    #[serde(default)]
    after: Option<String>,
    /// Filter: inclusive upper time bound (RFC 3339).
    #[serde(default)]
    until: Option<String>,
}

impl AuditQuery {
    /// Whether any filter criterion was supplied (so the handler picks the
    /// AND-composed filtered query over the plain keyset walk).
    fn has_filters(&self) -> bool {
        self.action.is_some()
            || self.actor.is_some()
            || self.target.is_some()
            || self.after.is_some()
            || self.until.is_some()
    }
}

/// Parse an RFC 3339 timestamp from a query param, mapping a malformed value to a
/// `400`.
fn parse_rfc3339(s: &str) -> AeroResult<time::OffsetDateTime> {
    time::OffsetDateTime::parse(s.trim(), &time::format_description::well_known::Rfc3339)
        .map_err(|e| AeroError::Invalid(format!("timestamp: {e}")))
}

/// Resolve the filtered audit events for a query: the AND-composed filtered query
/// when any criterion is set, else the plain keyset walk. Shared by the JSON
/// listing and the CSV export so they apply identical filtering.
async fn resolve_audit_events(
    s: &AppState,
    ws: WorkspaceId,
    q: &AuditQuery,
) -> AeroResult<Vec<aero_storage::AuditEvent>> {
    if q.has_filters() {
        let actor = q.actor.as_deref().map(parse_participant_id).transpose()?;
        let since = q.after.as_deref().map(parse_rfc3339).transpose()?;
        let until = q.until.as_deref().map(parse_rfc3339).transpose()?;
        s.audit
            .list_for_workspace_filtered(
                ws,
                q.action.as_deref(),
                actor,
                q.target.as_deref(),
                since,
                until,
                q.limit,
            )
            .await
            .map_err(AeroError::from)
    } else {
        let before = q.before.as_deref().map(parse_audit_id).transpose()?;
        s.audit
            .list_for_workspace(ws, before, q.limit)
            .await
            .map_err(AeroError::from)
    }
}

/// `GET /api/workspaces/:id/audit` — admin/owner only: the workspace's audit
/// trail, newest first. Without filter params it is keyset-paginated via
/// `?before=<audit_id>&limit=<n>`. With any of `?action=&actor=&target=&after=&until=`
/// it switches to an AND-composed filtered search (operability — audit
/// search/filter).
async fn list_audit(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<AuditQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace_id(&id_str)?;
    let caller = caller_role(&s.workspaces, ws, auth.participant_id).await?;
    authorize_view_audit(caller)?;
    let events = resolve_audit_events(&s, ws, &q).await?;
    Ok(Json(serde_json::to_value(events).map_err(AeroError::from)?))
}

/// `GET /api/workspaces/:id/audit/export` — admin/owner only: the workspace's
/// audit trail as a CSV download (`timestamp,actor,action,target,details`),
/// honoring the same `?action=&actor=&target=&after=&until=&limit=` filters as the
/// JSON listing (operability — audit export). Emits `text/csv` with a
/// `Content-Disposition: attachment` so a browser downloads it.
async fn export_audit_csv(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Query(q): Query<AuditQuery>,
) -> ApiResult<axum::response::Response> {
    use axum::http::{header, HeaderMap, HeaderName, HeaderValue};
    use axum::response::IntoResponse;
    let ws = parse_workspace_id(&id_str)?;
    let caller = caller_role(&s.workspaces, ws, auth.participant_id).await?;
    authorize_view_audit(caller)?;
    let events = resolve_audit_events(&s, ws, &q).await?;
    let csv = aero_storage::events_to_csv(&events);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"audit.csv\""),
    );
    // Tamper-evident signature (ROADMAP 方向五, opt-in via AERO_AUDIT_SIGNING_KEY):
    // an offline verifier recomputes HMAC-SHA256 over the downloaded CSV with the
    // same key to prove the export was not altered. Unset key ⇒ no header (current
    // behaviour). The CSV body is left pure so spreadsheet tools parse it unchanged.
    if let Ok(key) = std::env::var("AERO_AUDIT_SIGNING_KEY") {
        if !key.is_empty() {
            let sig = aero_storage::audit::sign_csv(&csv, key.as_bytes());
            if let Ok(v) = HeaderValue::from_str(&format!("sha256={sig}")) {
                headers.insert(HeaderName::from_static("x-audit-signature"), v);
            }
        }
    }
    Ok((headers, csv).into_response())
}

/// `GET /api/workspaces/:id/export` — **owner-only**: a complete data snapshot
/// of the tenant (workspace row, members, channels with their messages, audit
/// trail), for GDPR-style data portability (ROADMAP 方向一 合规). Per-room
/// messages are bounded server-side (see
/// [`aero_storage::EXPORT_MESSAGES_PER_ROOM`]).
async fn export_workspace(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace_id(&id_str)?;
    let caller = caller_role(&s.workspaces, ws, auth.participant_id).await?;
    authorize_export(caller)?;
    let snapshot = s
        .workspaces
        .export(ws)
        .await
        .map_err(AeroError::from)?
        // A caller resolved a role above, so the workspace existed then; treat a
        // racing disappearance as 404.
        .ok_or_else(|| AeroError::NotFound("workspace".into()))?;
    // Record that an export was taken, into the workspace's own audit trail
    // (the export already happened, so this row is not part of the snapshot).
    audit(
        &s,
        ws,
        auth.participant_id,
        "workspace.export",
        None,
        serde_json::json!({}),
    )
    .await;
    Ok(Json(
        serde_json::to_value(snapshot).map_err(AeroError::from)?,
    ))
}

/// `DELETE /api/workspaces/:id` — **owner-only**: hard-delete the workspace and
/// everything scoped to it (ROADMAP 方向一 合规 — erasure). The repo runs the
/// cascade atomically in one transaction.
///
/// An `"workspace.delete"` audit event is emitted **before** the delete. Because
/// `audit_events` is itself `ON DELETE CASCADE` on `workspaces`, that row is then
/// removed along with the tenant — so the deletion is also recorded via
/// `tracing` (a durable, out-of-tenant log) to retain an erasure record.
/// Maximum length (in characters) of a workspace name.
const MAX_NAME_LEN: usize = 100;

#[derive(Deserialize)]
struct UpdateWorkspaceReq {
    /// New display name; required, non-empty, ≤ 100 chars.
    name: String,
}

/// `PATCH /api/workspaces/:id` — **admin/owner**: update the workspace's display
/// name. Returns the updated workspace row. Emits a `"workspace.renamed"` audit
/// event so the change is visible in the workspace's audit trail.
async fn update_workspace(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<UpdateWorkspaceReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace_id(&id_str)?;

    let name = req.name.trim().to_owned();
    if name.is_empty() {
        return Err(AeroError::Invalid("name must not be empty".into()).into());
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(
            AeroError::Invalid(format!("name must be at most {MAX_NAME_LEN} characters")).into(),
        );
    }

    let workspace = s
        .workspaces
        .update_name_authorized(ws, &name, auth.participant_id)
        .await?;

    audit(
        &s,
        ws,
        auth.participant_id,
        "workspace.renamed",
        None,
        serde_json::json!({ "new_name": name }),
    )
    .await;

    Ok(Json(
        serde_json::to_value(workspace).map_err(AeroError::from)?,
    ))
}

async fn delete_workspace(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<StatusCode> {
    let ws = parse_workspace_id(&id_str)?;
    let deleted = s
        .workspaces
        .delete_authorized(ws, auth.participant_id)
        .await?;
    if !deleted {
        // Raced with another deleter between the role check and the delete.
        return Err(AeroError::NotFound("workspace".into()).into());
    }
    // Durable erasure record outside the (now-deleted) tenant's audit trail.
    tracing::info!(%ws, actor = %auth.participant_id, "workspace.delete (erased)");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SetRetentionReq {
    /// Retention window in whole days, or `null` to clear the policy (keep
    /// forever). A missing key is treated the same as `null`.
    #[serde(default)]
    days: Option<i32>,
}

/// `PUT /api/workspaces/:id/retention` — **admin/owner**: set (or, with
/// `days: null`, clear) the workspace's message-retention window (ROADMAP 方向一
/// 合规 — 按组织的留存策略). A `Some(n)` opts the tenant into having messages
/// older than `n` days soft-deleted by the periodic sweep; `null` keeps messages
/// forever. `n < 1` is rejected as `400 Invalid` (a zero/negative window would
/// mark everything expired). Emits a `"workspace.retention_set"` audit event.
async fn set_retention(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<SetRetentionReq>,
) -> ApiResult<StatusCode> {
    let ws = parse_workspace_id(&id_str)?;
    // Reject a non-positive window before touching the DB (pure storage rule).
    validate_retention_days(req.days)
        .map_err(|n| AeroError::Invalid(format!("retention days must be >= 1, got {n}")))?;
    s.workspaces
        .set_retention_authorized(ws, req.days, auth.participant_id)
        .await?;
    audit(
        &s,
        ws,
        auth.participant_id,
        "workspace.retention_set",
        None,
        serde_json::json!({ "days": req.days }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct AddMemberReq {
    participant_id: String,
    role: WorkspaceRole,
}

/// `POST /api/workspaces/:id/members` — invite / add a member with a role.
async fn add_member(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<AddMemberReq>,
) -> ApiResult<StatusCode> {
    let ws = parse_workspace_id(&id_str)?;
    let target = parse_participant_id(&req.participant_id)?;
    let inserted = s
        .workspaces
        .add_member_authorized(ws, auth.participant_id, target, req.role)
        .await
        .map_err(map_member_write_error)?;
    audit(
        &s,
        ws,
        auth.participant_id,
        "member.add",
        Some(&target.to_string()),
        serde_json::json!({ "role": req.role }),
    )
    .await;
    // Only a newly-created ordinary membership receives onboarding defaults.
    // Replays and single-channel guests must never expand room access.
    if inserted && req.role != WorkspaceRole::Guest {
        crate::default_channels::auto_join_defaults(&s, ws, target).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ChangeRoleReq {
    role: WorkspaceRole,
}

/// `PATCH /api/workspaces/:id/members/:pid` — change a member's role.
async fn change_member_role(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((id_str, pid_str)): Path<(String, String)>,
    Json(req): Json<ChangeRoleReq>,
) -> ApiResult<StatusCode> {
    let ws = parse_workspace_id(&id_str)?;
    let subject_id = parse_participant_id(&pid_str)?;
    let subject_role = s
        .workspaces
        .change_member_role_authorized(ws, auth.participant_id, subject_id, req.role)
        .await
        .map_err(map_member_write_error)?;
    audit(
        &s,
        ws,
        auth.participant_id,
        "member.role_change",
        Some(&subject_id.to_string()),
        serde_json::json!({ "from": subject_role, "to": req.role }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/workspaces/:id/members/:pid` — remove (or self-leave) a member.
async fn remove_member(
    State(s): State<AppState>,
    auth: AuthUser,
    Path((id_str, pid_str)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let ws = parse_workspace_id(&id_str)?;
    let subject_id = parse_participant_id(&pid_str)?;
    let subject_role = s
        .workspaces
        .remove_member_authorized(ws, auth.participant_id, subject_id)
        .await
        .map_err(map_member_write_error)?;
    let is_self = subject_id == auth.participant_id;
    audit(
        &s,
        ws,
        auth.participant_id,
        "member.remove",
        Some(&subject_id.to_string()),
        serde_json::json!({ "was": subject_role, "self": is_self }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// A slug is 1-64 chars of lowercase ASCII letters, digits, or `-`.
fn is_valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

// ---------- Workspace mute (ROADMAP6 Lane A) ----------

fn workspace_mute_repo(s: &AppState) -> WorkspaceMuteRepo {
    WorkspaceMuteRepo::new(s.pg.clone())
}

fn parse_workspace_for_mute(s: &str) -> Result<WorkspaceId, AeroError> {
    WorkspaceId::from_str(s).map_err(|e| AeroError::Invalid(format!("workspace id: {e}")))
}

/// `POST /api/workspaces/:id/mute` — suppress ALL notifications from every channel
/// in the workspace. The caller must be a workspace member. Idempotent.
async fn mute_workspace(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let workspace = parse_workspace_for_mute(&id_str)?;
    // Verify caller is a workspace member.
    let ws_repo = WorkspaceRepo::new(s.pg.clone());
    if ws_repo
        .effective_member_role(workspace, auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .is_none()
    {
        return Err(AeroError::Forbidden(format!(
            "{} is not a member of workspace {}",
            auth.participant_id, workspace
        ))
        .into());
    }
    workspace_mute_repo(&s)
        .mute(auth.participant_id, workspace)
        .await?;
    Ok(Json(serde_json::json!({
        "workspace_id": workspace,
        "muted": true,
    })))
}

/// `DELETE /api/workspaces/:id/mute` — re-enable notifications from a previously
/// muted workspace. Idempotent: unmuting an un-muted workspace is a no-op.
async fn unmute_workspace(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let workspace = parse_workspace_for_mute(&id_str)?;
    workspace_mute_repo(&s)
        .unmute(auth.participant_id, workspace)
        .await?;
    Ok(Json(serde_json::json!({
        "workspace_id": workspace,
        "muted": false,
    })))
}

/// `GET /api/workspaces/:id/muted` — whether the caller has muted the workspace.
async fn workspace_mute_status(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let workspace = parse_workspace_for_mute(&id_str)?;
    let muted = workspace_mute_repo(&s)
        .is_muted(auth.participant_id, workspace)
        .await?;
    Ok(Json(serde_json::json!({
        "workspace_id": workspace,
        "muted": muted,
    })))
}

// ---------- Workspace branding (ROADMAP7 Lane C) ----------

#[derive(Deserialize)]
struct UpdateBrandingReq {
    /// URL of the workspace logo. `null` or missing = no change.
    #[serde(default)]
    logo_url: Option<String>,
    /// Color scheme token or hex color (e.g. `"#FF0000"`). `null` or missing = no change.
    #[serde(default)]
    color_scheme: Option<String>,
    /// Custom domain for the workspace. `null` or missing = no change.
    #[serde(default)]
    custom_domain: Option<String>,
    /// Human-readable description. `null` or missing = no change.
    #[serde(default)]
    description: Option<String>,
}

/// `PATCH /api/workspaces/:id/branding` — **admin/owner**: update the workspace's
/// branding fields (logo URL, color scheme, custom domain, description). Any field
/// that is absent or `null` in the request body is left unchanged (patch semantics).
/// Emits a `"workspace.branding_updated"` audit event on success.
async fn update_branding(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<UpdateBrandingReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace_id(&id_str)?;
    let workspace = s
        .workspaces
        .update_branding_authorized(
            ws,
            req.logo_url.as_deref(),
            req.color_scheme.as_deref(),
            req.custom_domain.as_deref(),
            req.description.as_deref(),
            auth.participant_id,
        )
        .await?;

    audit(
        &s,
        ws,
        auth.participant_id,
        "workspace.branding_updated",
        None,
        serde_json::json!({
            "logo_url": req.logo_url,
            "color_scheme": req.color_scheme,
            "custom_domain": req.custom_domain,
            "description": req.description,
        }),
    )
    .await;

    Ok(Json(
        serde_json::to_value(workspace).map_err(AeroError::from)?,
    ))
}

// ----------------------------------------- notification defaults (ROADMAP12, migration 0119)

#[derive(serde::Deserialize)]
struct NotifDefaultsReq {
    /// One of `"all"` / `"mentions"` / `"none"`.
    default_level: String,
}

/// `GET /api/workspaces/:id/notification-defaults` — fetch the workspace's
/// configured default channel notification level. Any workspace member may read
/// this; it returns `{"default_level": "all"}` when no default is set (system
/// default).
async fn get_notif_defaults(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace_id(&id_str)?;
    // Any workspace member may read the setting.
    if s.workspaces
        .effective_member_role(ws, auth.participant_id)
        .await
        .map_err(AeroError::from)?
        .is_none()
    {
        return Err(AeroError::Forbidden("not a workspace member".into()).into());
    }
    let repo = WorkspaceNotifDefaultsRepo::new(s.pg.clone());
    let level = repo.get(ws).await?.unwrap_or_else(|| "all".to_owned());
    Ok(Json(serde_json::json!({ "default_level": level })))
}

/// `PUT /api/workspaces/:id/notification-defaults` — set the workspace default
/// channel notification level. Admin/Owner only.
async fn set_notif_defaults(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
    Json(req): Json<NotifDefaultsReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let ws = parse_workspace_id(&id_str)?;
    if !matches!(req.default_level.as_str(), "all" | "mentions" | "none") {
        return Err(AeroError::Invalid(
            "default_level must be 'all', 'mentions', or 'none'".into(),
        )
        .into());
    }
    let repo = WorkspaceNotifDefaultsRepo::new(s.pg.clone());
    repo.set_authorized(ws, &req.default_level, auth.participant_id)
        .await?;
    Ok(Json(
        serde_json::json!({ "ok": true, "default_level": req.default_level }),
    ))
}

#[cfg(test)]
pub mod tests;
