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
    Error as AeroError, ParticipantId, Result as AeroResult, WorkspaceId, WorkspaceRole,
};
use aero_storage::{
    role_can_assign, role_can_invite, role_can_manage_member, role_can_remove, WorkspaceRepo,
};
use axum::{
    extract::{Path, State},
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
        .route("/api/workspaces/:id/members", get(list_members).post(add_member))
        .route(
            "/api/workspaces/:id/members/:pid",
            axum::routing::patch(change_member_role).delete(remove_member),
        )
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
    // `add_member` is `ON CONFLICT DO NOTHING`, so it would NOT update an existing
    // row. Re-roling must therefore go through remove + add to actually persist
    // the new role for an already-present member.
    s.workspaces
        .remove_member(ws, subject_id)
        .await
        .map_err(AeroError::from)?;
    s.workspaces
        .add_member(ws, subject_id, req.role)
        .await
        .map_err(AeroError::from)?;
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
