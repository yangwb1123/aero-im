use super::*;
use aero_common::{Error as AeroError, Result as AeroResult, WorkspaceRole};

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
