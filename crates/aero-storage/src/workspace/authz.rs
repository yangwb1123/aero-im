//! Transaction-owned workspace governance authorization.

use aero_common::{Error, ParticipantId, WorkspaceId, WorkspaceRole};
use sqlx::{Postgres, Transaction};

/// Lock a workspace's governance boundary and prove that `participant` is still
/// an effective Owner/Admin in the same transaction that will perform a write.
///
/// Every caller must acquire this workspace row before locking its own resource
/// rows. Membership/deactivation governance follows the same order, so a writer
/// that waited for a demotion, account deletion, mandatory-2FA change, or
/// deactivation observes the committed state instead of reusing a stale
/// route-level decision.
pub(crate) async fn assert_effective_admin_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    participant: ParticipantId,
) -> Result<WorkspaceRole, Error> {
    let require_2fa = sqlx::query_scalar::<_, bool>(
        "SELECT require_2fa FROM workspaces WHERE id = $1 FOR UPDATE",
    )
    .bind(workspace.to_uuid())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| Error::NotFound(format!("workspace {workspace}")))?;

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

    let (is_human, account_active) = sqlx::query_as::<_, (bool, bool)>(
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
             SELECT 1
               FROM workspace_deactivations
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

    if !account_active || deactivated || !has_2fa {
        return Err(Error::Forbidden("workspace admin required".into()));
    }
    Ok(role)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use aero_common::{Error, ParticipantId, WorkspaceRole};
    use sqlx::PgPool;

    use crate::{BarrierRepo, IpAllowlistRepo, LegalHoldRepo, UserGroupRepo, WorkspaceRepo};

    async fn participant(pool: &PgPool, label: &str) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query(
            "INSERT INTO participants (id, kind, display_name)
             VALUES ($1, 'human', $2)",
        )
        .bind(id.to_uuid())
        .bind(format!("{label}-{id}"))
        .execute(pool)
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    #[ignore = "requires a migrated PostgreSQL database"]
    async fn governance_writes_share_one_effective_admin_revocation_boundary() {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect_lazy(&url)
            .unwrap();
        let owner = participant(&pool, "governance-owner").await;
        let admin = participant(&pool, "governance-admin").await;
        let workspaces = WorkspaceRepo::new(pool.clone());
        let workspace = workspaces
            .create(
                format!("Governance {owner}"),
                format!("governance-{owner}"),
                owner,
            )
            .await
            .unwrap()
            .id;
        workspaces
            .add_member(workspace, admin, WorkspaceRole::Admin)
            .await
            .unwrap();
        workspaces
            .set_region_code_authorized(workspace, Some("before"), admin)
            .await
            .unwrap();
        IpAllowlistRepo::new(pool.clone())
            .add_authorized(workspace, "10.0.0.0/8", Some("before"), admin)
            .await
            .unwrap();
        LegalHoldRepo::new(pool.clone())
            .create_authorized(workspace, None, "before", admin)
            .await
            .unwrap();
        let groups = UserGroupRepo::new(pool.clone());
        let group_a = groups
            .create(workspace, "authz-a", "Authz A", owner)
            .await
            .unwrap();
        let group_b = groups
            .create(workspace, "authz-b", "Authz B", owner)
            .await
            .unwrap();
        BarrierRepo::new(pool.clone())
            .create_authorized(workspace, group_a, group_b, admin)
            .await
            .unwrap();

        let mut demotion = pool.begin().await.unwrap();
        crate::ownership::lock_membership_governance(&mut demotion)
            .await
            .unwrap();
        sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .execute(&mut *demotion)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE workspace_members
                SET role = 'member'
              WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(admin.to_uuid())
        .execute(&mut *demotion)
        .await
        .unwrap();

        let raced_repo = workspaces.clone();
        let mut raced = tokio::spawn(async move {
            raced_repo
                .set_region_code_authorized(workspace, Some("raced"), admin)
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut raced)
                .await
                .is_err(),
            "governance write must wait behind the workspace revocation lock"
        );
        demotion.commit().await.unwrap();
        assert!(matches!(raced.await.unwrap(), Err(Error::Forbidden(_))));
        assert_eq!(
            workspaces.region_code(workspace).await.unwrap().as_deref(),
            Some("before")
        );
        assert!(matches!(
            IpAllowlistRepo::new(pool.clone())
                .add_authorized(workspace, "192.0.2.0/24", None, admin)
                .await,
            Err(Error::Forbidden(_))
        ));
        assert!(matches!(
            LegalHoldRepo::new(pool.clone())
                .create_authorized(workspace, None, "after demotion", admin)
                .await,
            Err(Error::Forbidden(_))
        ));
        assert!(matches!(
            BarrierRepo::new(pool)
                .create_authorized(workspace, group_b, group_a, admin)
                .await,
            Err(Error::Forbidden(_))
        ));
    }
}
