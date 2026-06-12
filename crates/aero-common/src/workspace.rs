//! Workspace (tenant / org) domain types.
//!
//! Mirrors the `workspaces` / `workspace_members` schema in
//! `migrations/0006_workspaces.sql`. `participants` remain global identities; a
//! [`Workspace`] groups members (each with a [`WorkspaceRole`]) and channels.
//!
//! The role-privilege logic here is intentionally pure (no DB, no I/O) so it can
//! be unit-tested directly and reused by services/routes when tenant scoping is
//! threaded through in a later batch.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{ParticipantId, WorkspaceId};

// ---------- WorkspaceRole ----------

/// A member's privilege level within a workspace.
///
/// Ordering is by privilege, lowest → highest: `Guest < Member < Admin < Owner`.
/// `PartialOrd`/`Ord` follow declaration order, so comparisons read naturally
/// (e.g. `role >= WorkspaceRole::Admin`). Prefer the named helpers
/// ([`at_least`](WorkspaceRole::at_least), [`can_administer`](WorkspaceRole::can_administer))
/// over raw comparisons at call sites for clarity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceRole {
    /// External / limited participant. Lowest privilege.
    Guest,
    /// Regular member of the workspace.
    Member,
    /// Can manage members and most workspace settings.
    Admin,
    /// Full control, including deleting the workspace. Highest privilege.
    Owner,
}

impl WorkspaceRole {
    /// Privilege rank, higher = more privileged. Stable surface for callers that
    /// want a numeric comparison without relying on the derived `Ord`.
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Self::Guest => 0,
            Self::Member => 1,
            Self::Admin => 2,
            Self::Owner => 3,
        }
    }

    /// True if `self` is at least as privileged as `other`.
    ///
    /// ```
    /// use aero_common::WorkspaceRole;
    /// assert!(WorkspaceRole::Admin.at_least(WorkspaceRole::Member));
    /// assert!(WorkspaceRole::Owner.at_least(WorkspaceRole::Owner));
    /// assert!(!WorkspaceRole::Guest.at_least(WorkspaceRole::Member));
    /// ```
    #[must_use]
    pub fn at_least(self, other: WorkspaceRole) -> bool {
        self.rank() >= other.rank()
    }

    /// True if this role may administer the workspace (manage members, settings,
    /// roles). `Owner` and `Admin` qualify; `Member` and `Guest` do not.
    #[must_use]
    pub fn can_administer(self) -> bool {
        self.at_least(Self::Admin)
    }

    /// True if this role may delete / transfer the workspace itself. Owner-only.
    #[must_use]
    pub fn can_manage_workspace(self) -> bool {
        self == Self::Owner
    }

    /// Lowercase token used in the DB `role` column and on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Guest => "guest",
            Self::Member => "member",
            Self::Admin => "admin",
            Self::Owner => "owner",
        }
    }

    /// Parse from the DB / wire token. Unknown values yield `None` so callers can
    /// decide how to treat corrupt rows (mirrors the explicit-match style used by
    /// the other repos in `aero-storage`).
    #[must_use]
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "guest" => Some(Self::Guest),
            "member" => Some(Self::Member),
            "admin" => Some(Self::Admin),
            "owner" => Some(Self::Owner),
            _ => None,
        }
    }
}

// ---------- Workspace ----------

/// A tenant / org. Channels (`rooms`) and members belong to exactly one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    /// URL-safe unique handle (e.g. `acme-corp`).
    pub slug: String,
    /// The participant who created the workspace, if known.
    pub created_by: Option<ParticipantId>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Optional URL for the workspace's logo image (branding).
    pub logo_url: Option<String>,
    /// Optional color scheme token or hex color (branding).
    pub color_scheme: Option<String>,
    /// Optional custom domain for the workspace (branding).
    pub custom_domain: Option<String>,
    /// Optional human-readable description of the workspace.
    pub description: Option<String>,
}

/// One participant's membership in a workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceMember {
    pub workspace_id: WorkspaceId,
    pub participant_id: ParticipantId,
    pub role: WorkspaceRole,
    #[serde(with = "time::serde::rfc3339")]
    pub joined_at: OffsetDateTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rank_is_strictly_increasing() {
        assert!(WorkspaceRole::Guest.rank() < WorkspaceRole::Member.rank());
        assert!(WorkspaceRole::Member.rank() < WorkspaceRole::Admin.rank());
        assert!(WorkspaceRole::Admin.rank() < WorkspaceRole::Owner.rank());
    }

    #[test]
    fn derived_ord_matches_rank() {
        // Declaration order must equal privilege order so `>=` reads naturally.
        assert!(WorkspaceRole::Guest < WorkspaceRole::Member);
        assert!(WorkspaceRole::Member < WorkspaceRole::Admin);
        assert!(WorkspaceRole::Admin < WorkspaceRole::Owner);
        assert_eq!(WorkspaceRole::Owner, WorkspaceRole::Owner);
    }

    #[test]
    fn at_least_is_reflexive() {
        for r in [
            WorkspaceRole::Guest,
            WorkspaceRole::Member,
            WorkspaceRole::Admin,
            WorkspaceRole::Owner,
        ] {
            assert!(r.at_least(r), "{r:?} should be at_least itself");
        }
    }

    #[test]
    fn at_least_orders_correctly() {
        assert!(WorkspaceRole::Owner.at_least(WorkspaceRole::Admin));
        assert!(WorkspaceRole::Owner.at_least(WorkspaceRole::Guest));
        assert!(WorkspaceRole::Admin.at_least(WorkspaceRole::Member));
        assert!(!WorkspaceRole::Member.at_least(WorkspaceRole::Admin));
        assert!(!WorkspaceRole::Guest.at_least(WorkspaceRole::Member));
    }

    #[test]
    fn can_administer_is_admin_and_owner_only() {
        assert!(WorkspaceRole::Owner.can_administer());
        assert!(WorkspaceRole::Admin.can_administer());
        assert!(!WorkspaceRole::Member.can_administer());
        assert!(!WorkspaceRole::Guest.can_administer());
    }

    #[test]
    fn can_manage_workspace_is_owner_only() {
        assert!(WorkspaceRole::Owner.can_manage_workspace());
        assert!(!WorkspaceRole::Admin.can_manage_workspace());
        assert!(!WorkspaceRole::Member.can_manage_workspace());
        assert!(!WorkspaceRole::Guest.can_manage_workspace());
    }

    #[test]
    fn db_str_round_trips() {
        for r in [
            WorkspaceRole::Guest,
            WorkspaceRole::Member,
            WorkspaceRole::Admin,
            WorkspaceRole::Owner,
        ] {
            assert_eq!(WorkspaceRole::from_db_str(r.as_str()), Some(r));
        }
        assert_eq!(WorkspaceRole::from_db_str("nope"), None);
        assert_eq!(WorkspaceRole::from_db_str(""), None);
    }

    #[test]
    fn db_tokens_match_migration_check_constraint() {
        // These exact lowercase tokens are what 0006_workspaces.sql's CHECK allows.
        assert_eq!(WorkspaceRole::Owner.as_str(), "owner");
        assert_eq!(WorkspaceRole::Admin.as_str(), "admin");
        assert_eq!(WorkspaceRole::Member.as_str(), "member");
        assert_eq!(WorkspaceRole::Guest.as_str(), "guest");
    }

    #[test]
    fn serde_uses_lowercase_tokens() {
        let json = serde_json::to_string(&WorkspaceRole::Admin).unwrap();
        assert_eq!(json, "\"admin\"");
        let back: WorkspaceRole = serde_json::from_str("\"owner\"").unwrap();
        assert_eq!(back, WorkspaceRole::Owner);
    }
}
