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
    validate_retention_days, WorkspaceMuteRepo, WorkspaceRepo,
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
        .route("/api/workspaces", post(create_workspace).get(list_workspaces))
        // Owner-only erasure (ROADMAP 方向一 合规); GET of a single workspace is
        // intentionally not (yet) offered here — listing is via `/api/workspaces`.
        .route(
            "/api/workspaces/:id",
            axum::routing::patch(update_workspace).delete(delete_workspace),
        )
        // Owner-only full-tenant export (GDPR data portability).
        .route("/api/workspaces/:id/export", get(export_workspace))
        // Admin/owner: set or clear the per-workspace message-retention window.
        .route("/api/workspaces/:id/retention", axum::routing::put(set_retention))
        .route("/api/workspaces/:id/members", get(list_members).post(add_member))
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
    if let Err(e) = s.audit.append(workspace, Some(actor), action, target, detail).await {
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
        Err(AeroError::Forbidden("cannot invite member with that role".into()))
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
        Err(AeroError::Forbidden("cannot change that member's role".into()))
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
        Err(AeroError::Forbidden("workspace export requires owner".into()))
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
        Err(AeroError::Forbidden("workspace deletion requires owner".into()))
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
        Err(AeroError::Forbidden("setting retention requires admin".into()))
    }
}

/// Only admin and owner may rename a workspace.
pub fn authorize_rename(caller: WorkspaceRole) -> AeroResult<()> {
    if caller.can_administer() {
        Ok(())
    } else {
        Err(AeroError::Forbidden("renaming a workspace requires admin".into()))
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
    repo.member_role(workspace, caller)
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
        return Err(AeroError::Invalid(
            "slug must be 1-64 chars of [a-z0-9-]".into(),
        )
        .into());
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
    let members = s.workspaces.list_members(ws).await.map_err(AeroError::from)?;
    Ok(Json(serde_json::to_value(members).map_err(AeroError::from)?))
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
    use axum::response::IntoResponse;
    let ws = parse_workspace_id(&id_str)?;
    let caller = caller_role(&s.workspaces, ws, auth.participant_id).await?;
    authorize_view_audit(caller)?;
    let events = resolve_audit_events(&s, ws, &q).await?;
    let csv = aero_storage::events_to_csv(&events);
    let headers = [
        (axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8"),
        (
            axum::http::header::CONTENT_DISPOSITION,
            "attachment; filename=\"audit.csv\"",
        ),
    ];
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
    audit(&s, ws, auth.participant_id, "workspace.export", None, serde_json::json!({})).await;
    Ok(Json(serde_json::to_value(snapshot).map_err(AeroError::from)?))
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
    let caller = caller_role(&s.workspaces, ws, auth.participant_id).await?;
    authorize_rename(caller)?;

    let name = req.name.trim().to_owned();
    if name.is_empty() {
        return Err(AeroError::Invalid("name must not be empty".into()).into());
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(AeroError::Invalid(
            format!("name must be at most {MAX_NAME_LEN} characters"),
        ).into());
    }

    let updated = s.workspaces.update_name(ws, &name).await.map_err(AeroError::from)?;
    if !updated {
        return Err(AeroError::NotFound("workspace".into()).into());
    }

    audit(
        &s,
        ws,
        auth.participant_id,
        "workspace.renamed",
        None,
        serde_json::json!({ "new_name": name }),
    )
    .await;

    let workspace = s
        .workspaces
        .get(ws)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("workspace".into()))?;
    Ok(Json(serde_json::to_value(workspace).map_err(AeroError::from)?))
}

async fn delete_workspace(
    State(s): State<AppState>,
    auth: AuthUser,
    Path(id_str): Path<String>,
) -> ApiResult<StatusCode> {
    let ws = parse_workspace_id(&id_str)?;
    let caller = caller_role(&s.workspaces, ws, auth.participant_id).await?;
    authorize_delete(caller)?;
    // Best-effort audit BEFORE deletion (the row cascade-deletes with the tenant).
    audit(&s, ws, auth.participant_id, "workspace.delete", None, serde_json::json!({})).await;
    let deleted = s.workspaces.delete(ws).await.map_err(AeroError::from)?;
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
    let caller = caller_role(&s.workspaces, ws, auth.participant_id).await?;
    authorize_set_retention(caller)?;
    // Reject a non-positive window before touching the DB (pure storage rule).
    validate_retention_days(req.days)
        .map_err(|n| AeroError::Invalid(format!("retention days must be >= 1, got {n}")))?;
    s.workspaces
        .set_retention(ws, req.days)
        .await
        .map_err(AeroError::from)?;
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
    let caller = caller_role(&s.workspaces, ws, auth.participant_id).await?;
    // Pure RBAC decision: caller may invite *and* may grant the requested role.
    authorize_invite(caller, req.role)?;
    s.workspaces
        .add_member(ws, target, req.role)
        .await
        .map_err(AeroError::from)?;
    audit(
        &s,
        ws,
        auth.participant_id,
        "member.add",
        Some(&target.to_string()),
        serde_json::json!({ "role": req.role }),
    )
    .await;
    // Onboarding: auto-join the new member into this workspace's default channels
    // (Wave 12). Best-effort — never fails the member-add.
    crate::default_channels::auto_join_defaults(&s, ws, target).await;
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
    let caller = caller_role(&s.workspaces, ws, auth.participant_id).await?;
    // The subject must already be a member (else there's no role to change).
    let subject_role = s
        .workspaces
        .member_role(ws, subject_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("workspace member".into()))?;
    // Pure RBAC decision over (caller, new_role, current subject role).
    authorize_role_change(caller, req.role, subject_role)?;
    // Single idempotent upsert: `update_member_role` overwrites an existing
    // member's role atomically, replacing the prior remove-then-add dance (which
    // briefly dropped the member and risked leaving them removed if the re-add
    // failed).
    s.workspaces
        .update_member_role(ws, subject_id, req.role)
        .await
        .map_err(AeroError::from)?;
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
    let caller = caller_role(&s.workspaces, ws, auth.participant_id).await?;
    let subject_role = s
        .workspaces
        .member_role(ws, subject_id)
        .await
        .map_err(AeroError::from)?
        .ok_or_else(|| AeroError::NotFound("workspace member".into()))?;
    let is_self = subject_id == auth.participant_id;
    // Pure RBAC decision; self-removal ("leave") has its own owner-orphan rule.
    authorize_remove(caller, subject_role, is_self)?;
    s.workspaces
        .remove_member(ws, subject_id)
        .await
        .map_err(AeroError::from)?;
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
    if ws_repo.member_role(workspace, auth.participant_id).await.map_err(AeroError::from)?.is_none() {
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

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [WorkspaceRole; 4] = [
        WorkspaceRole::Guest,
        WorkspaceRole::Member,
        WorkspaceRole::Admin,
        WorkspaceRole::Owner,
    ];

    /// HTTP status a guard's error maps to (or 200 on `Ok`). Lets the role-matrix
    /// tests assert the *status* a denial would surface, not just allow/deny.
    fn status_of(r: &AeroResult<()>) -> u16 {
        r.as_ref().err().map_or(200, AeroError::status_code)
    }

    fn allowed(r: &AeroResult<()>) -> bool {
        r.is_ok()
    }

    #[test]
    fn audit_view_is_admin_and_owner_only() {
        assert!(allowed(&authorize_view_audit(WorkspaceRole::Owner)));
        assert!(allowed(&authorize_view_audit(WorkspaceRole::Admin)));
        assert!(!allowed(&authorize_view_audit(WorkspaceRole::Member)));
        assert!(!allowed(&authorize_view_audit(WorkspaceRole::Guest)));
        // Denials surface as 403, not 404/500.
        assert_eq!(status_of(&authorize_view_audit(WorkspaceRole::Member)), 403);
    }

    // ----- authorize_export / authorize_delete (compliance, owner-only) -----

    #[test]
    fn export_is_owner_only_over_all_roles() {
        // Exhaustive over the 4 roles: only Owner may export; everyone else is
        // denied — including Admin (export is stricter than admin/audit).
        for r in ALL {
            assert_eq!(
                allowed(&authorize_export(r)),
                r == WorkspaceRole::Owner,
                "export allowed only for owner, role {r:?}"
            );
        }
    }

    #[test]
    fn export_non_owner_denials_are_403() {
        // Every non-owner denial is an authorization failure (403), not 400/404.
        for r in [WorkspaceRole::Guest, WorkspaceRole::Member, WorkspaceRole::Admin] {
            assert_eq!(status_of(&authorize_export(r)), 403, "role {r:?}");
        }
    }

    #[test]
    fn delete_is_owner_only_over_all_roles() {
        for r in ALL {
            assert_eq!(
                allowed(&authorize_delete(r)),
                r == WorkspaceRole::Owner,
                "delete allowed only for owner, role {r:?}"
            );
        }
    }

    #[test]
    fn delete_non_owner_denials_are_403() {
        for r in [WorkspaceRole::Guest, WorkspaceRole::Member, WorkspaceRole::Admin] {
            assert_eq!(status_of(&authorize_delete(r)), 403, "role {r:?}");
        }
    }

    #[test]
    fn export_and_delete_agree_with_can_manage_workspace() {
        // The guards delegate to the same owner predicate, so they must track it
        // exactly for every role (no drift between storage and HTTP layers).
        for r in ALL {
            assert_eq!(allowed(&authorize_export(r)), r.can_manage_workspace());
            assert_eq!(allowed(&authorize_delete(r)), r.can_manage_workspace());
        }
    }

    // ----- authorize_set_retention (compliance, admin/owner-only) -----

    #[test]
    fn set_retention_is_admin_and_owner_only_over_all_roles() {
        // Exhaustive over the 4 roles: admin + owner may set retention; member
        // and guest may not. Unlike export/delete (owner-only), retention is a
        // workspace-administration action, so admin qualifies too.
        for r in ALL {
            assert_eq!(
                allowed(&authorize_set_retention(r)),
                r.can_administer(),
                "retention allowed only for admin/owner, role {r:?}"
            );
        }
        assert!(allowed(&authorize_set_retention(WorkspaceRole::Owner)));
        assert!(allowed(&authorize_set_retention(WorkspaceRole::Admin)));
        assert!(!allowed(&authorize_set_retention(WorkspaceRole::Member)));
        assert!(!allowed(&authorize_set_retention(WorkspaceRole::Guest)));
    }

    #[test]
    fn set_retention_non_admin_denials_are_403() {
        // Member and guest denials surface as 403 (authorization), not 400/404.
        for r in [WorkspaceRole::Guest, WorkspaceRole::Member] {
            assert_eq!(status_of(&authorize_set_retention(r)), 403, "role {r:?}");
        }
    }

    #[test]
    fn set_retention_matches_audit_view_gate() {
        // Retention-set and audit-view share the same admin bar, so they must
        // agree for every role (both are `can_administer`-gated).
        for r in ALL {
            assert_eq!(
                allowed(&authorize_set_retention(r)),
                allowed(&authorize_view_audit(r)),
                "role {r:?}"
            );
        }
    }

    // ----- authorize_rename -----

    #[test]
    fn rename_is_admin_and_owner_only_over_all_roles() {
        for r in ALL {
            assert_eq!(
                allowed(&authorize_rename(r)),
                r.can_administer(),
                "rename allowed only for admin/owner, role {r:?}"
            );
        }
    }

    #[test]
    fn rename_non_admin_denials_are_403() {
        for r in [WorkspaceRole::Guest, WorkspaceRole::Member] {
            assert_eq!(status_of(&authorize_rename(r)), 403, "role {r:?}");
        }
    }

    // ----- authorize_invite -----

    #[test]
    fn invite_only_admins_and_owners_can_invite_at_all() {
        // Guests/members can never invite, regardless of the target role.
        for target in ALL {
            assert!(!allowed(&authorize_invite(WorkspaceRole::Guest, target)));
            assert!(!allowed(&authorize_invite(WorkspaceRole::Member, target)));
        }
    }

    #[test]
    fn invite_admin_can_grant_up_to_admin_but_not_owner() {
        assert!(allowed(&authorize_invite(WorkspaceRole::Admin, WorkspaceRole::Guest)));
        assert!(allowed(&authorize_invite(WorkspaceRole::Admin, WorkspaceRole::Member)));
        assert!(allowed(&authorize_invite(WorkspaceRole::Admin, WorkspaceRole::Admin)));
        // No privilege escalation: an admin cannot mint an owner.
        assert!(!allowed(&authorize_invite(WorkspaceRole::Admin, WorkspaceRole::Owner)));
    }

    #[test]
    fn invite_owner_can_grant_any_role() {
        for target in ALL {
            assert!(allowed(&authorize_invite(WorkspaceRole::Owner, target)), "target {target:?}");
        }
    }

    #[test]
    fn invite_never_escalates_for_any_actor() {
        // Exhaustive: for every (caller, target), granting strictly above the
        // caller must be denied.
        for caller in ALL {
            for target in ALL {
                if target.rank() > caller.rank() {
                    assert!(
                        !allowed(&authorize_invite(caller, target)),
                        "caller {caller:?} must not invite higher {target:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn invite_denials_are_403() {
        // A denial is an authorization failure, not a 400/404.
        assert_eq!(status_of(&authorize_invite(WorkspaceRole::Member, WorkspaceRole::Member)), 403);
        assert_eq!(status_of(&authorize_invite(WorkspaceRole::Admin, WorkspaceRole::Owner)), 403);
    }

    // ----- authorize_role_change -----

    #[test]
    fn role_change_members_and_guests_can_never_change_anyone() {
        for new_role in ALL {
            for subject in ALL {
                assert!(!allowed(&authorize_role_change(WorkspaceRole::Member, new_role, subject)));
                assert!(!allowed(&authorize_role_change(WorkspaceRole::Guest, new_role, subject)));
            }
        }
    }

    #[test]
    fn role_change_admin_cannot_touch_owner_subject() {
        // Even setting a low new_role, an admin may not re-role an owner.
        for new_role in ALL {
            assert!(!allowed(&authorize_role_change(
                WorkspaceRole::Admin,
                new_role,
                WorkspaceRole::Owner
            )));
        }
    }

    #[test]
    fn role_change_admin_can_rerole_non_owner_within_limits() {
        // Admin re-roling a member: may set guest/member/admin, not owner.
        assert!(allowed(&authorize_role_change(
            WorkspaceRole::Admin,
            WorkspaceRole::Guest,
            WorkspaceRole::Member
        )));
        assert!(allowed(&authorize_role_change(
            WorkspaceRole::Admin,
            WorkspaceRole::Admin,
            WorkspaceRole::Member
        )));
        assert!(!allowed(&authorize_role_change(
            WorkspaceRole::Admin,
            WorkspaceRole::Owner,
            WorkspaceRole::Member
        )));
    }

    #[test]
    fn role_change_owner_can_set_any_role_on_any_subject() {
        for new_role in ALL {
            for subject in ALL {
                assert!(
                    allowed(&authorize_role_change(WorkspaceRole::Owner, new_role, subject)),
                    "new_role {new_role:?} subject {subject:?}"
                );
            }
        }
    }

    #[test]
    fn role_change_never_escalates_target_above_actor() {
        for caller in ALL {
            for new_role in ALL {
                for subject in ALL {
                    if new_role.rank() > caller.rank() {
                        assert!(
                            !allowed(&authorize_role_change(caller, new_role, subject)),
                            "caller {caller:?} new_role {new_role:?} subject {subject:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn role_change_never_acts_on_more_privileged_subject() {
        for caller in ALL {
            for new_role in ALL {
                for subject in ALL {
                    if subject.rank() > caller.rank() {
                        assert!(
                            !allowed(&authorize_role_change(caller, new_role, subject)),
                            "caller {caller:?} acting on higher subject {subject:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn role_change_denials_are_403() {
        assert_eq!(
            status_of(&authorize_role_change(
                WorkspaceRole::Member,
                WorkspaceRole::Member,
                WorkspaceRole::Member
            )),
            403
        );
    }

    // ----- authorize_remove (other people) -----

    #[test]
    fn remove_members_and_guests_can_remove_nobody_else() {
        for subject in ALL {
            assert!(!allowed(&authorize_remove(WorkspaceRole::Member, subject, false)));
            assert!(!allowed(&authorize_remove(WorkspaceRole::Guest, subject, false)));
        }
    }

    #[test]
    fn remove_admin_can_remove_at_or_below_but_not_owner() {
        assert!(allowed(&authorize_remove(WorkspaceRole::Admin, WorkspaceRole::Guest, false)));
        assert!(allowed(&authorize_remove(WorkspaceRole::Admin, WorkspaceRole::Member, false)));
        assert!(allowed(&authorize_remove(WorkspaceRole::Admin, WorkspaceRole::Admin, false)));
        assert!(!allowed(&authorize_remove(WorkspaceRole::Admin, WorkspaceRole::Owner, false)));
    }

    #[test]
    fn remove_owner_can_remove_anyone_else() {
        for subject in ALL {
            assert!(
                allowed(&authorize_remove(WorkspaceRole::Owner, subject, false)),
                "subject {subject:?}"
            );
        }
    }

    #[test]
    fn remove_never_acts_on_more_privileged_subject() {
        for caller in ALL {
            for subject in ALL {
                if subject.rank() > caller.rank() {
                    assert!(
                        !allowed(&authorize_remove(caller, subject, false)),
                        "caller {caller:?} removing higher {subject:?}"
                    );
                }
            }
        }
    }

    // ----- authorize_remove (self / leave) -----

    #[test]
    fn self_leave_allowed_for_non_owners() {
        // A guest/member/admin may leave on their own, even without admin rights.
        for r in [WorkspaceRole::Guest, WorkspaceRole::Member, WorkspaceRole::Admin] {
            // subject == caller's own role for a self-removal.
            assert!(allowed(&authorize_remove(r, r, true)), "self-leave for {r:?}");
        }
    }

    #[test]
    fn self_leave_forbidden_for_owner() {
        // Owner leaving would orphan the workspace.
        assert!(!allowed(&authorize_remove(WorkspaceRole::Owner, WorkspaceRole::Owner, true)));
        assert_eq!(
            status_of(&authorize_remove(WorkspaceRole::Owner, WorkspaceRole::Owner, true)),
            403
        );
    }

    #[test]
    fn member_can_leave_but_not_remove_others() {
        // The asymmetry that makes self-removal a distinct rule: a member may
        // leave (self) yet cannot remove any other member.
        assert!(allowed(&authorize_remove(WorkspaceRole::Member, WorkspaceRole::Member, true)));
        assert!(!allowed(&authorize_remove(WorkspaceRole::Member, WorkspaceRole::Member, false)));
    }

    #[test]
    fn remove_denials_are_403() {
        assert_eq!(status_of(&authorize_remove(WorkspaceRole::Member, WorkspaceRole::Member, false)), 403);
    }

    // ----- slug validation -----

    #[test]
    fn slug_accepts_lowercase_alnum_and_hyphen() {
        assert!(is_valid_slug("acme"));
        assert!(is_valid_slug("acme-corp"));
        assert!(is_valid_slug("a1-b2-c3"));
        assert!(is_valid_slug("x"));
    }

    #[test]
    fn slug_rejects_empty_uppercase_spaces_and_overlong() {
        assert!(!is_valid_slug(""));
        assert!(!is_valid_slug("Acme")); // uppercase
        assert!(!is_valid_slug("acme corp")); // space
        assert!(!is_valid_slug("acme_corp")); // underscore not allowed
        assert!(!is_valid_slug(&"a".repeat(65))); // too long
    }
}
