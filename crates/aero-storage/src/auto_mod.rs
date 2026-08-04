//! Auto-moderation rules engine (migration 0111).
//!
//! Workspace admins define text-matching rules that the IM service evaluates
//! before message sends and edits. Three `match_type` values are supported:
//! `"contains"` (the default substring test), `"exact"` (full-string equality),
//! and `"prefix"`. All comparisons are case-insensitive. The only supported
//! action is `"block"`; both the HTTP and database boundaries reject any action
//! without runtime semantics.
//!
//! Purely additive: a NEW [`AutoModRuleRepo`]; no existing repo is touched.

use aero_common::{Error, ParticipantId, WorkspaceId, WorkspaceRole};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// Maximum active auto-moderation rules retained for one workspace.
///
/// Rule evaluation is synchronous on every message send/edit, so this is both
/// a product quota and a hard upper bound on request-time CPU/DB amplification.
pub const MAX_AUTO_MOD_RULES_PER_WORKSPACE: usize = 100;
const MAX_AUTO_MOD_RULES_PER_WORKSPACE_DB: i64 = 100;
const AUTO_MOD_RULE_RECOVERY_PAGE_SIZE: i64 = 101;

/// A bounded auto-moderation rule write failed.
#[derive(Debug, thiserror::Error)]
pub enum AutoModRuleWriteError {
    /// The workspace already owns the maximum number of rules.
    #[error("workspace auto-mod rule quota exceeded (maximum {MAX_AUTO_MOD_RULES_PER_WORKSPACE})")]
    QuotaExceeded,
    /// Authorization or database failure from the common domain boundary.
    #[error(transparent)]
    Domain(Error),
}

impl From<Error> for AutoModRuleWriteError {
    fn from(error: Error) -> Self {
        Self::Domain(error)
    }
}

impl From<sqlx::Error> for AutoModRuleWriteError {
    fn from(error: sqlx::Error) -> Self {
        Self::Domain(Error::Database(error))
    }
}

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
    /// What to do on a match. The schema currently permits only `"block"`.
    pub action: String,
}

impl AutoModRule {
    /// Whether this rule matches `text`. Case-insensitive.
    #[must_use]
    pub fn matches(&self, text: &str) -> bool {
        let lowercase_text = text.to_lowercase();
        let lowercase_pattern = self.pattern.to_lowercase();
        match self.match_type.as_str() {
            "exact" => lowercase_text == lowercase_pattern,
            "prefix" => lowercase_text.starts_with(&lowercase_pattern),
            _ => lowercase_text.contains(&lowercase_pattern), // "contains" default
        }
    }

    /// Match an already-lowercased canonical moderation projection.
    ///
    /// Repository reads and writes normalize `pattern` once, so callers that
    /// evaluate a rule set can lowercase the message once and reuse it across
    /// every rule rather than cloning the full message per comparison.
    #[must_use]
    pub fn matches_lowercase(&self, lowercase_text: &str) -> bool {
        match self.match_type.as_str() {
            "exact" => lowercase_text == self.pattern,
            "prefix" => lowercase_text.starts_with(&self.pattern),
            _ => lowercase_text.contains(&self.pattern), // "contains" default
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
    /// `"exact"`, or `"prefix"` and the only supported `action` is `"block"`.
    /// Database `CHECK` constraints enforce both sets. This is a low-level
    /// compatibility/setup helper; request paths must use
    /// [`Self::create_authorized`].
    ///
    /// # Errors
    /// Returns [`AutoModRuleWriteError::QuotaExceeded`] at the workspace cap and
    /// propagates database/constraint failures.
    pub async fn create(
        &self,
        workspace: WorkspaceId,
        pattern: &str,
        match_type: &str,
        action: &str,
        created_by: ParticipantId,
    ) -> Result<AutoModRule, AutoModRuleWriteError> {
        let mut tx = self.pg.begin().await?;
        lock_workspace(&mut tx, workspace).await?;
        reserve_rule_slot(&mut tx, workspace).await?;
        let row = insert_rule(&mut tx, workspace, pattern, match_type, action, created_by).await?;
        tx.commit().await?;
        Ok(row)
    }

    /// Create a rule only if `created_by` is still an effective workspace
    /// administrator under the same workspace lock and transaction as the
    /// insert. Request handlers must use this method instead of a pre-check plus
    /// [`Self::create`], which would leave a revocation race.
    ///
    /// # Errors
    /// Returns [`Error::Forbidden`] when the caller is no longer an effective
    /// admin and propagates storage failures.
    pub async fn create_authorized(
        &self,
        workspace: WorkspaceId,
        pattern: &str,
        match_type: &str,
        action: &str,
        created_by: ParticipantId,
    ) -> Result<AutoModRule, AutoModRuleWriteError> {
        let mut tx = self.pg.begin().await?;
        assert_effective_admin(&mut tx, workspace, created_by).await?;
        reserve_rule_slot(&mut tx, workspace).await?;
        let row = insert_rule(&mut tx, workspace, pattern, match_type, action, created_by).await?;
        tx.commit().await?;
        Ok(row)
    }

    /// A bounded recovery page of rules, ordered oldest first.
    ///
    /// Healthy workspaces return at most [`MAX_AUTO_MOD_RULES_PER_WORKSPACE`]
    /// rows. A legacy over-quota workspace returns one extra row so management
    /// clients can diagnose and delete rules until enforcement becomes healthy,
    /// without allowing an unbounded response/query.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query, wrapped in [`Error::from`].
    pub async fn list_for_workspace(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<AutoModRule>, Error> {
        let mut rows: Vec<AutoModRule> = sqlx::query_as(
            "SELECT id, workspace_id, pattern, match_type, action \
             FROM auto_mod_rules
             WHERE workspace_id = $1
             ORDER BY created_at, id
             LIMIT $2",
        )
        .bind(workspace.to_uuid())
        .bind(AUTO_MOD_RULE_RECOVERY_PAGE_SIZE)
        .fetch_all(&self.pg)
        .await
        .map_err(Error::from)?;
        normalize_rule_patterns(&mut rows);
        Ok(rows)
    }

    /// The bounded rule set used by send/edit enforcement.
    ///
    /// A legacy workspace with more than the configured cap fails closed rather
    /// than evaluating an attacker-amplified rule set. Management reads/deletes
    /// remain available through [`Self::list_for_workspace`] and
    /// [`Self::delete_authorized`] so an administrator can recover it.
    ///
    /// # Errors
    /// Returns [`Error::Conflict`] for an over-quota legacy configuration and
    /// propagates storage failures.
    pub async fn list_for_enforcement(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<AutoModRule>, Error> {
        let rules = self.list_for_workspace(workspace).await?;
        if rules.len() > MAX_AUTO_MOD_RULES_PER_WORKSPACE {
            return Err(Error::Conflict(format!(
                "workspace auto-mod configuration exceeds the maximum of \
                 {MAX_AUTO_MOD_RULES_PER_WORKSPACE} rules"
            )));
        }
        Ok(rules)
    }

    /// Delete one rule, workspace-scoped. Returns `true` iff a row was removed
    /// (idempotent: `false` on a second delete or a wrong workspace).
    /// This is a low-level compatibility/setup helper; request paths must use
    /// [`Self::delete_authorized`].
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete, wrapped in [`Error::from`].
    pub async fn delete(&self, rule_id: Uuid, workspace: WorkspaceId) -> Result<bool, Error> {
        let r = sqlx::query("DELETE FROM auto_mod_rules WHERE id = $1 AND workspace_id = $2")
            .bind(rule_id)
            .bind(workspace.to_uuid())
            .execute(&self.pg)
            .await
            .map_err(Error::from)?;
        Ok(r.rows_affected() > 0)
    }

    /// Delete a rule only if `deleted_by` remains an effective workspace
    /// administrator under the same workspace lock and transaction as the
    /// delete.
    ///
    /// # Errors
    /// Returns [`Error::Forbidden`] when the caller is no longer an effective
    /// admin and propagates storage failures.
    pub async fn delete_authorized(
        &self,
        rule_id: Uuid,
        workspace: WorkspaceId,
        deleted_by: ParticipantId,
    ) -> Result<bool, Error> {
        let mut tx = self.pg.begin().await?;
        assert_effective_admin(&mut tx, workspace, deleted_by).await?;
        let removed = sqlx::query("DELETE FROM auto_mod_rules WHERE id = $1 AND workspace_id = $2")
            .bind(rule_id)
            .bind(workspace.to_uuid())
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
        tx.commit().await?;
        Ok(removed)
    }
}

async fn lock_workspace(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
) -> Result<(), Error> {
    let exists = sqlx::query_scalar::<_, Uuid>(
        "SELECT id
           FROM workspaces
          WHERE id = $1
          FOR UPDATE",
    )
    .bind(workspace.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .is_some();
    if !exists {
        return Err(Error::NotFound(format!("workspace {workspace}")));
    }
    Ok(())
}

async fn reserve_rule_slot(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
) -> Result<(), AutoModRuleWriteError> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)
           FROM (
               SELECT 1
                 FROM auto_mod_rules
                WHERE workspace_id = $1
                LIMIT $2
           ) AS bounded_rules",
    )
    .bind(workspace.to_uuid())
    .bind(MAX_AUTO_MOD_RULES_PER_WORKSPACE_DB)
    .fetch_one(&mut **tx)
    .await?;
    if count >= MAX_AUTO_MOD_RULES_PER_WORKSPACE_DB {
        return Err(AutoModRuleWriteError::QuotaExceeded);
    }
    Ok(())
}

async fn insert_rule(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    pattern: &str,
    match_type: &str,
    action: &str,
    created_by: ParticipantId,
) -> Result<AutoModRule, AutoModRuleWriteError> {
    let normalized_pattern = pattern.to_lowercase();
    let row = sqlx::query_as(
        "INSERT INTO auto_mod_rules
             (workspace_id, pattern, match_type, action, created_by)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id, workspace_id, pattern, match_type, action",
    )
    .bind(workspace.to_uuid())
    .bind(normalized_pattern)
    .bind(match_type)
    .bind(action)
    .bind(created_by.to_uuid())
    .fetch_one(&mut **tx)
    .await?;
    Ok(row)
}

fn normalize_rule_patterns(rules: &mut [AutoModRule]) {
    for rule in rules {
        rule.pattern = rule.pattern.to_lowercase();
    }
}

async fn assert_effective_admin(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Result<(), Error> {
    // Governance mutations across this workspace take the same row lock. A
    // role/deactivation transaction that won the lock commits first; READ
    // COMMITTED then gives the checks below its fresh state after the wait.
    let require_2fa = sqlx::query_scalar::<_, bool>(
        "SELECT require_2fa FROM workspaces WHERE id = $1 FOR UPDATE",
    )
    .bind(workspace.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| Error::Forbidden("workspace admin required".into()))?;
    let role = sqlx::query_scalar::<_, String>(
        "SELECT role
           FROM workspace_members
          WHERE workspace_id = $1 AND participant_id = $2
          FOR UPDATE",
    )
    .bind(workspace.to_uuid())
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .and_then(|role| WorkspaceRole::from_db_str(&role))
    .filter(|role| role.can_administer())
    .ok_or_else(|| Error::Forbidden("workspace admin required".into()))?;
    debug_assert!(role.can_administer());

    let (is_human, active) = sqlx::query_as::<_, (bool, bool)>(
        "SELECT kind = 'human', deleted_at IS NULL
           FROM participants
          WHERE id = $1
          FOR SHARE",
    )
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or((true, false));
    let deactivated = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
             SELECT 1 FROM workspace_deactivations
              WHERE workspace_id = $1 AND participant_id = $2
         )",
    )
    .bind(workspace.to_uuid())
    .bind(participant.to_uuid())
    .fetch_one(&mut **tx)
    .await?;
    let has_2fa = if require_2fa && is_human {
        sqlx::query_scalar::<_, bool>(
            "SELECT activated
               FROM totp_secrets
              WHERE participant_id = $1
              FOR SHARE",
        )
        .bind(participant.to_uuid())
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or(false)
    } else {
        true
    };
    if !active || deactivated || !has_2fa {
        return Err(Error::Forbidden("workspace admin required".into()));
    }
    Ok(())
}

#[cfg(test)]
mod quota_tests;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use aero_common::{Error, ParticipantId, WorkspaceId, WorkspaceRole};

    use super::{AutoModRule, AutoModRuleRepo, AutoModRuleWriteError};
    use crate::WorkspaceRepo;

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
        let r = rule("prefix", "http://", "block");
        assert!(r.matches("http://example.com"));
        assert!(!r.matches("visit http://example.com"));
    }

    #[test]
    fn normalized_rules_reuse_one_lowercase_message_projection() {
        let contains = rule("contains", "blocked phrase", "block");
        let exact = rule("exact", "stop", "block");
        let prefix = rule("prefix", "https://blocked.example", "block");
        let lowercase_text = "this includes a blocked phrase".to_lowercase();

        assert!(contains.matches_lowercase(&lowercase_text));
        assert!(exact.matches_lowercase("stop"));
        assert!(prefix.matches_lowercase("https://blocked.example/path"));
        assert!(!contains.matches_lowercase("clean text"));
    }

    #[tokio::test]
    #[ignore = "requires a migrated PostgreSQL database"]
    async fn database_rejects_actions_without_runtime_semantics() {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy(&url)
            .expect("valid DATABASE_URL");
        let actor = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id,kind,display_name) VALUES ($1,'human',$2)")
            .bind(actor.to_uuid())
            .bind(format!("auto-mod-{actor}"))
            .execute(&pool)
            .await
            .expect("participant");
        let workspace = WorkspaceId::new();
        let mut tx = pool.begin().await.expect("begin workspace fixture");
        sqlx::query(
            "INSERT INTO workspaces (id,name,slug,created_by)
             VALUES ($1,$2,$3,$4)",
        )
        .bind(workspace.to_uuid())
        .bind("Auto-mod action constraint")
        .bind(format!("auto-mod-{workspace}"))
        .bind(actor.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("workspace");
        sqlx::query(
            "INSERT INTO workspace_members (workspace_id, participant_id, role)
             VALUES ($1, $2, 'owner')",
        )
        .bind(workspace.to_uuid())
        .bind(actor.to_uuid())
        .execute(&mut *tx)
        .await
        .expect("workspace owner");
        tx.commit().await.expect("commit workspace fixture");
        let repo = AutoModRuleRepo::new(pool);
        repo.create(workspace, "blocked", "contains", "block", actor)
            .await
            .expect("implemented action");
        for action in ["warn", "delete"] {
            assert!(
                repo.create(workspace, "ignored", "contains", action, actor)
                    .await
                    .is_err(),
                "{action} must be rejected by the database boundary"
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires a migrated PostgreSQL database"]
    async fn authorized_writes_recheck_effective_admin_after_revocation_wait() {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_lazy(&url)
            .expect("valid DATABASE_URL");
        let owner = ParticipantId::new();
        let admin = ParticipantId::new();
        for participant in [owner, admin] {
            sqlx::query("INSERT INTO participants (id,kind,display_name) VALUES ($1,'human',$2)")
                .bind(participant.to_uuid())
                .bind(format!("auto-mod-admin-{participant}"))
                .execute(&pool)
                .await
                .expect("participant");
        }
        let workspaces = WorkspaceRepo::new(pool.clone());
        let workspace = workspaces
            .create(
                format!("Auto-mod admin {owner}"),
                format!("auto-mod-admin-{owner}"),
                owner,
            )
            .await
            .expect("workspace")
            .id;
        workspaces
            .add_member(workspace, admin, WorkspaceRole::Admin)
            .await
            .expect("admin membership");
        let repo = AutoModRuleRepo::new(pool.clone());

        repo.create_authorized(workspace, "initial", "contains", "block", admin)
            .await
            .expect("effective admin creates");
        sqlx::query(
            "INSERT INTO totp_secrets (participant_id, secret, activated, activated_at)
             VALUES ($1, 'auto-mod-owner-test', true, now())",
        )
        .bind(owner.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE workspaces SET require_2fa = true WHERE id = $1")
            .bind(workspace.to_uuid())
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            repo.create_authorized(workspace, "no-2fa", "contains", "block", admin)
                .await,
            Err(AutoModRuleWriteError::Domain(Error::Forbidden(_)))
        ));
        sqlx::query(
            "INSERT INTO totp_secrets (participant_id, secret, activated, activated_at)
             VALUES ($1, 'auto-mod-test', true, now())",
        )
        .bind(admin.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
        repo.create_authorized(workspace, "with-2fa", "contains", "block", admin)
            .await
            .expect("mandatory 2FA gate accepts activated enrollment");
        sqlx::query("UPDATE workspaces SET require_2fa = false WHERE id = $1")
            .bind(workspace.to_uuid())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO workspace_deactivations
                 (workspace_id, participant_id, deactivated_by)
             VALUES ($1, $2, $3)",
        )
        .bind(workspace.to_uuid())
        .bind(admin.to_uuid())
        .bind(owner.to_uuid())
        .execute(&pool)
        .await
        .unwrap();
        assert!(matches!(
            repo.create_authorized(workspace, "deactivated", "contains", "block", admin)
                .await,
            Err(AutoModRuleWriteError::Domain(Error::Forbidden(_)))
        ));
        sqlx::query(
            "DELETE FROM workspace_deactivations
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(admin.to_uuid())
        .execute(&pool)
        .await
        .unwrap();

        let mut revoke_create = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .execute(&mut *revoke_create)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE workspace_members SET role = 'member'
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(admin.to_uuid())
        .execute(&mut *revoke_create)
        .await
        .unwrap();
        let create_repo = repo.clone();
        let mut create_task = tokio::spawn(async move {
            create_repo
                .create_authorized(workspace, "raced-create", "contains", "block", admin)
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut create_task)
                .await
                .is_err(),
            "create waits behind the revocation workspace lock"
        );
        revoke_create.commit().await.unwrap();
        assert!(matches!(
            create_task.await.unwrap(),
            Err(AutoModRuleWriteError::Domain(Error::Forbidden(_)))
        ));
        assert!(
            repo.list_for_workspace(workspace)
                .await
                .unwrap()
                .iter()
                .all(|rule| rule.pattern != "raced-create"),
            "the stale create never commits"
        );

        workspaces
            .change_member_role_authorized(workspace, owner, admin, WorkspaceRole::Admin)
            .await
            .expect("restore admin");
        let removable = repo
            .create_authorized(workspace, "raced-delete", "contains", "block", admin)
            .await
            .unwrap();
        let mut revoke_delete = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .execute(&mut *revoke_delete)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE workspace_members SET role = 'member'
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(admin.to_uuid())
        .execute(&mut *revoke_delete)
        .await
        .unwrap();
        let delete_repo = repo.clone();
        let mut delete_task = tokio::spawn(async move {
            delete_repo
                .delete_authorized(removable.id, workspace, admin)
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut delete_task)
                .await
                .is_err(),
            "delete waits behind the revocation workspace lock"
        );
        revoke_delete.commit().await.unwrap();
        assert!(matches!(
            delete_task.await.unwrap(),
            Err(Error::Forbidden(_))
        ));
        assert!(
            repo.list_for_workspace(workspace)
                .await
                .unwrap()
                .iter()
                .any(|rule| rule.id == removable.id),
            "the stale delete never commits"
        );

        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(workspace.to_uuid())
            .execute(&pool)
            .await
            .unwrap();
        for participant in [owner, admin] {
            sqlx::query("DELETE FROM participants WHERE id = $1")
                .bind(participant.to_uuid())
                .execute(&pool)
                .await
                .unwrap();
        }
    }
}
