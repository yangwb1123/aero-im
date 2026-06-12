//! Auto-moderation rules engine (migration 0111).
//!
//! Workspace admins define text-matching rules that the IM service evaluates at
//! message-send time. Three `match_type` values are supported: `"contains"` (the
//! default substring test), `"exact"` (full-string equality), and `"prefix"`.
//! All comparisons are case-insensitive. The only action active at send time is
//! `"block"` — `"delete"` and `"warn"` are reserved for future use.
//!
//! Purely additive: a NEW [`AutoModRuleRepo`]; no existing repo is touched.

use aero_common::{Error, ParticipantId, WorkspaceId};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// One auto-moderation rule row — a pattern + match strategy + action.
///
/// A storage-layer projection of an `auto_mod_rules` row. `Serialize` so a
/// handler can return it directly as JSON. `FromRow` for the sqlx query decoder.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AutoModRule {
    /// The rule's unique id.
    pub id: Uuid,
    /// The workspace this rule applies to.
    pub workspace_id: Uuid,
    /// The pattern to match against the message text.
    pub pattern: String,
    /// How to match: `"contains"` | `"exact"` | `"prefix"`.
    pub match_type: String,
    /// What to do on a match: `"block"` | `"delete"` | `"warn"`.
    pub action: String,
}

impl AutoModRule {
    /// Whether this rule matches `text`. Case-insensitive.
    #[must_use]
    pub fn matches(&self, text: &str) -> bool {
        let text_lower = text.to_lowercase();
        let pattern_lower = self.pattern.to_lowercase();
        match self.match_type.as_str() {
            "exact" => text_lower == pattern_lower,
            "prefix" => text_lower.starts_with(&pattern_lower),
            _ => text_lower.contains(&pattern_lower), // "contains" default
        }
    }
}

/// Repository over the `auto_mod_rules` table.
///
/// Cheap to clone — wraps a [`PgPool`]; feature modules build one inline via
/// [`AutoModRuleRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct AutoModRuleRepo {
    pub pg: PgPool,
}

impl AutoModRuleRepo {
    /// Build a repo over the given pool.
    pub fn new(pg: PgPool) -> Self {
        Self { pg }
    }

    /// Create a new rule for `workspace`. The `match_type` must be `"contains"`,
    /// `"exact"`, or `"prefix"`; the `action` must be `"block"`, `"delete"`, or
    /// `"warn"`. The DB `CHECK` enforces this — an invalid value produces a
    /// constraint violation that propagates as [`Error::Db`].
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] (including constraint violations) from the
    /// insert, wrapped in [`Error::from`].
    pub async fn create(
        &self,
        workspace: WorkspaceId,
        pattern: &str,
        match_type: &str,
        action: &str,
        created_by: ParticipantId,
    ) -> Result<AutoModRule, Error> {
        let row: AutoModRule = sqlx::query_as(
            "INSERT INTO auto_mod_rules \
             (workspace_id, pattern, match_type, action, created_by) \
             VALUES ($1, $2, $3, $4, $5) \
             RETURNING id, workspace_id, pattern, match_type, action",
        )
        .bind(workspace.to_uuid())
        .bind(pattern)
        .bind(match_type)
        .bind(action)
        .bind(created_by.to_uuid())
        .fetch_one(&self.pg)
        .await
        .map_err(Error::from)?;
        Ok(row)
    }

    /// All rules for a workspace, ordered by creation time (oldest first).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query, wrapped in [`Error::from`].
    pub async fn list_for_workspace(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<AutoModRule>, Error> {
        let rows: Vec<AutoModRule> = sqlx::query_as(
            "SELECT id, workspace_id, pattern, match_type, action \
             FROM auto_mod_rules WHERE workspace_id = $1 ORDER BY created_at",
        )
        .bind(workspace.to_uuid())
        .fetch_all(&self.pg)
        .await
        .map_err(Error::from)?;
        Ok(rows)
    }

    /// Delete one rule, workspace-scoped. Returns `true` iff a row was removed
    /// (idempotent: `false` on a second delete or a wrong workspace).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete, wrapped in [`Error::from`].
    pub async fn delete(
        &self,
        rule_id: Uuid,
        workspace: WorkspaceId,
    ) -> Result<bool, Error> {
        let r = sqlx::query(
            "DELETE FROM auto_mod_rules WHERE id = $1 AND workspace_id = $2",
        )
        .bind(rule_id)
        .bind(workspace.to_uuid())
        .execute(&self.pg)
        .await
        .map_err(Error::from)?;
        Ok(r.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::AutoModRule;

    fn rule(match_type: &str, pattern: &str, action: &str) -> AutoModRule {
        AutoModRule {
            id: uuid::Uuid::nil(),
            workspace_id: uuid::Uuid::nil(),
            pattern: pattern.to_owned(),
            match_type: match_type.to_owned(),
            action: action.to_owned(),
        }
    }

    #[test]
    fn contains_match_is_case_insensitive() {
        let r = rule("contains", "badword", "block");
        assert!(r.matches("This contains BADWORD in it"));
        assert!(!r.matches("clean text"));
    }

    #[test]
    fn exact_match_requires_full_equality() {
        let r = rule("exact", "stop", "block");
        assert!(r.matches("STOP"));
        assert!(!r.matches("don't stop"));
    }

    #[test]
    fn prefix_match_checks_start_of_string() {
        let r = rule("prefix", "http://", "warn");
        assert!(r.matches("http://example.com"));
        assert!(!r.matches("visit http://example.com"));
    }
}
