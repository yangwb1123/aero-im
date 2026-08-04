//! Transaction-owned authorization for participant-wide session revocation.

use aero_common::{ParticipantId, WorkspaceId, WorkspaceRole};

use super::SessionRepo;

/// Transaction-owned failures for a workspace owner revoking a member's
/// participant-wide login sessions.
#[derive(Debug, thiserror::Error)]
pub enum AdminSessionRevokeError {
    /// The workspace no longer exists.
    #[error("workspace not found")]
    WorkspaceNotFound,
    /// The target is not a current member of the workspace.
    #[error("workspace member not found")]
    MemberNotFound,
    /// The caller is not a currently-effective workspace owner, or the current
    /// role hierarchy does not permit managing the target.
    #[error("caller is not authorized to revoke this member's global sessions")]
    NotAuthorized,
    /// Database failure.
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

impl SessionRepo {
    /// Revoke a workspace member's participant-wide sessions under a current,
    /// transaction-owned authorization decision.
    ///
    /// `auth_sessions` has no workspace scope, so this operation has global
    /// impact and is deliberately owner-only. The transaction locks the
    /// workspace and both membership rows, revalidates the caller's active
    /// account, workspace-deactivation, and mandatory-2FA gates, then revokes
    /// and blacklists every active refresh session before releasing those
    /// governance locks. The target's raw membership is used intentionally so
    /// an owner can finish offboarding an already-deactivated member.
    ///
    /// Returns the number of active session rows transitioned. Repeating the
    /// operation is idempotent and returns zero.
    pub async fn revoke_workspace_member_sessions_authorized(
        &self,
        workspace: WorkspaceId,
        caller: ParticipantId,
        target: ParticipantId,
    ) -> Result<u64, AdminSessionRevokeError> {
        let mut tx = self.pool.begin().await?;
        let workspace_exists =
            sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
                .bind(workspace.to_uuid())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
        if !workspace_exists {
            return Err(AdminSessionRevokeError::WorkspaceNotFound);
        }

        let mut ids = vec![caller.to_uuid(), target.to_uuid()];
        ids.sort_unstable();
        ids.dedup();
        let roles = sqlx::query_as::<_, (uuid::Uuid, String)>(
            r"SELECT participant_id, role
                FROM workspace_members
               WHERE workspace_id = $1
                 AND participant_id = ANY($2)
               ORDER BY participant_id
               FOR UPDATE",
        )
        .bind(workspace.to_uuid())
        .bind(&ids)
        .fetch_all(&mut *tx)
        .await?;
        let role_for = |participant: ParticipantId| {
            roles
                .iter()
                .find(|(id, _)| *id == participant.to_uuid())
                .and_then(|(_, role)| WorkspaceRole::from_db_str(role))
        };
        let caller_role = role_for(caller).ok_or(AdminSessionRevokeError::NotAuthorized)?;
        let target_role = role_for(target).ok_or(AdminSessionRevokeError::MemberNotFound)?;

        let caller_effective =
            crate::workspace::members::effective_workspace_access_in_tx(&mut tx, workspace, caller)
                .await?;
        if !caller_effective
            || caller_role != WorkspaceRole::Owner
            || !crate::role_can_manage_member(caller_role, target_role)
        {
            return Err(AdminSessionRevokeError::NotAuthorized);
        }

        let row: (i64, i64) = sqlx::query_as(
            r"WITH revoked AS (
                   UPDATE auth_sessions
                      SET revoked_at = now()
                    WHERE participant_id = $1 AND revoked_at IS NULL
                RETURNING token_hash
               ),
               blacklisted AS (
                   INSERT INTO revoked_tokens (token_hash, participant_id)
                   SELECT token_hash, $1 FROM revoked
                   ON CONFLICT (token_hash) DO NOTHING
                   RETURNING token_hash
               )
               SELECT (SELECT COUNT(*) FROM revoked),
                      (SELECT COUNT(*) FROM blacklisted)",
        )
        .bind(target.to_uuid())
        .fetch_one(&mut *tx)
        .await?;
        let revoked = u64::try_from(row.0).unwrap_or(0);
        let target_text = target.to_string();
        crate::AuditRepo::append_in_tx(
            &mut tx,
            workspace,
            Some(caller),
            "session.revoked",
            Some(&target_text),
            serde_json::json!({
                "revoked": revoked,
                "scope": "participant_global",
                "target_role": target_role,
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(revoked)
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::revoked_token::hash_token;
    use sqlx::PgPool;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn owner(pool: &PgPool) -> ParticipantId {
        let participant = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(participant.to_uuid())
            .bind(format!("admin-session-owner-{participant}"))
            .execute(pool)
            .await
            .expect("insert participant");
        participant
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn workspace_global_revoke_requires_effective_owner_and_is_atomic() {
        let pool = pool();
        let sessions = SessionRepo::new(pool.clone());
        let workspaces = crate::WorkspaceRepo::new(pool.clone());
        let caller = owner(&pool).await;
        let admin = owner(&pool).await;
        let target = owner(&pool).await;
        let target_owner = owner(&pool).await;
        let workspace = workspaces
            .create(
                format!("session-governance-{caller}"),
                format!("session-governance-{caller}"),
                caller,
            )
            .await
            .expect("create workspace")
            .id;
        workspaces
            .add_member(workspace, admin, WorkspaceRole::Admin)
            .await
            .expect("enroll administrator");
        workspaces
            .add_member(workspace, target, WorkspaceRole::Member)
            .await
            .expect("enroll target");
        workspaces
            .add_member(workspace, target_owner, WorkspaceRole::Owner)
            .await
            .expect("enroll second owner");

        let target_hash = hash_token(&format!("admin-revoke-member-{target}"));
        let owner_hash = hash_token(&format!("admin-revoke-owner-{target_owner}"));
        sessions
            .record(target, &target_hash, Some("member-device"))
            .await
            .expect("record target session");
        sessions
            .record(target_owner, &owner_hash, Some("owner-device"))
            .await
            .expect("record owner session");

        assert!(matches!(
            sessions
                .revoke_workspace_member_sessions_authorized(workspace, admin, target_owner,)
                .await
                .expect_err("admin must never revoke an owner's global sessions"),
            AdminSessionRevokeError::NotAuthorized
        ));
        assert_eq!(
            sessions.list_active(target_owner).await.unwrap().len(),
            1,
            "denied authorization leaves the owner's session active"
        );

        sqlx::query(
            r"INSERT INTO totp_secrets
                  (participant_id, secret, activated, activated_at)
               VALUES ($1, 'admin-revoke-effective-owner', true, now())",
        )
        .bind(target_owner.to_uuid())
        .execute(&pool)
        .await
        .expect("enroll the second owner before requiring 2fa");
        sqlx::query("UPDATE workspaces SET require_2fa = true WHERE id = $1")
            .bind(workspace.to_uuid())
            .execute(&pool)
            .await
            .expect("require 2fa");
        assert!(matches!(
            sessions
                .revoke_workspace_member_sessions_authorized(workspace, caller, target)
                .await
                .expect_err("owner without mandatory 2fa is not effective"),
            AdminSessionRevokeError::NotAuthorized
        ));
        assert_eq!(
            sessions.list_active(target).await.unwrap().len(),
            1,
            "failed effective-access check must not partially revoke"
        );

        let totp = crate::TotpRepo::new(pool.clone());
        totp.upsert_secret(caller, "JBSWY3DPEHPK3PXP")
            .await
            .expect("enroll totp");
        assert!(totp.activate(caller).await.expect("activate totp"));
        sqlx::query(
            r"INSERT INTO workspace_deactivations
                  (workspace_id, participant_id, deactivated_by)
               VALUES ($1, $2, $3)",
        )
        .bind(workspace.to_uuid())
        .bind(target.to_uuid())
        .bind(caller.to_uuid())
        .execute(&pool)
        .await
        .expect("deactivate target before offboarding");

        assert_eq!(
            sessions
                .revoke_workspace_member_sessions_authorized(workspace, caller, target)
                .await
                .expect("effective owner can finish offboarding a deactivated member"),
            1
        );
        assert!(
            sessions.list_active(target).await.unwrap().is_empty(),
            "target has no active sessions"
        );
        assert!(
            crate::RevokedTokenRepo::new(pool.clone())
                .is_revoked(&target_hash)
                .await
                .unwrap(),
            "the refresh token was blacklisted in the same transaction"
        );

        sqlx::query("DELETE FROM auth_sessions WHERE participant_id = ANY($1)")
            .bind([target.to_uuid(), target_owner.to_uuid()])
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM revoked_tokens WHERE token_hash = ANY($1)")
            .bind(&[target_hash, owner_hash])
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(workspace.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
            .bind([
                caller.to_uuid(),
                admin.to_uuid(),
                target.to_uuid(),
                target_owner.to_uuid(),
            ])
            .execute(&pool)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn concurrent_owner_demotion_is_rechecked_before_global_revoke() {
        let pool = pool();
        let sessions = SessionRepo::new(pool.clone());
        let workspaces = crate::WorkspaceRepo::new(pool.clone());
        let caller = owner(&pool).await;
        let other_owner = owner(&pool).await;
        let target = owner(&pool).await;
        let workspace = workspaces
            .create(
                format!("session-race-{caller}"),
                format!("session-race-{caller}"),
                caller,
            )
            .await
            .expect("create workspace")
            .id;
        workspaces
            .add_member(workspace, other_owner, WorkspaceRole::Owner)
            .await
            .expect("enroll second owner");
        workspaces
            .add_member(workspace, target, WorkspaceRole::Member)
            .await
            .expect("enroll target");
        let token_hash = hash_token(&format!("session-race-target-{target}"));
        sessions
            .record(target, &token_hash, Some("race-device"))
            .await
            .expect("record target session");

        let mut demotion = pool.begin().await.expect("begin demotion");
        sqlx::query(
            r"UPDATE workspace_members
                  SET role = 'admin'
                WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(caller.to_uuid())
        .execute(&mut *demotion)
        .await
        .expect("stage owner demotion");

        let revoke_repo = sessions.clone();
        let mut revoke = tokio::spawn(async move {
            revoke_repo
                .revoke_workspace_member_sessions_authorized(workspace, caller, target)
                .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut revoke)
                .await
                .is_err(),
            "authorization waits for the in-flight governance change"
        );
        demotion.commit().await.expect("commit owner demotion");

        assert!(matches!(
            revoke.await.expect("revoke task joins"),
            Err(AdminSessionRevokeError::NotAuthorized)
        ));
        assert_eq!(
            sessions.list_active(target).await.unwrap().len(),
            1,
            "the now-admin caller cannot use a stale owner decision"
        );
        assert!(
            !crate::RevokedTokenRepo::new(pool.clone())
                .is_revoked(&token_hash)
                .await
                .unwrap(),
            "denied race does not blacklist the still-active token"
        );

        sqlx::query("DELETE FROM auth_sessions WHERE participant_id = $1")
            .bind(target.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM revoked_tokens WHERE token_hash = $1")
            .bind(&token_hash)
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(workspace.to_uuid())
            .execute(&pool)
            .await
            .ok();
        sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
            .bind([caller.to_uuid(), other_owner.to_uuid(), target.to_uuid()])
            .execute(&pool)
            .await
            .ok();
    }
}
