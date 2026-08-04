//! SCIM bearer credential storage and transaction-owned administration.

use aero_common::{ParticipantId, ScimTokenId, WorkspaceId};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};

use super::ScimRepo;

/// Maximum retained SCIM bearer credential records for one workspace.
///
/// The quota counts both active and revoked rows. Counting only active tokens
/// would let a caller grow the credential table without bound by repeatedly
/// minting and revoking. When a workspace at the cap has a revoked credential,
/// the next mint evicts the oldest revoked record transactionally; active
/// credentials are never evicted.
pub const MAX_SCIM_TOKENS_PER_WORKSPACE: usize = 100;
const MAX_SCIM_TOKENS_PER_WORKSPACE_DB: i64 = 100;
const SCIM_TOKEN_RECOVERY_PAGE_SIZE: i64 = 101;
const LIST_TOKENS_SQL: &str = r"SELECT id, workspace_id, label, created_at, revoked_at
                                 FROM scim_tokens
                                WHERE workspace_id = $1
                                ORDER BY (revoked_at IS NOT NULL),
                                         created_at DESC,
                                         id
                                LIMIT $2";

type ScimTokenSqlRow = (
    uuid::Uuid,
    uuid::Uuid,
    Option<String>,
    time::OffsetDateTime,
    Option<time::OffsetDateTime>,
);

/// Safe management projection for one SCIM bearer credential.
///
/// The bearer secret and its hash are deliberately absent. Administrators can
/// discover and revoke old credentials without any API ever recovering or
/// exposing credential material.
#[derive(Debug, Clone, Serialize)]
pub struct ScimTokenRecord {
    pub id: ScimTokenId,
    pub workspace_id: WorkspaceId,
    pub label: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: time::OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<time::OffsetDateTime>,
}

/// Failures from request-facing SCIM credential mutations.
#[derive(Debug, thiserror::Error)]
pub enum ScimTokenWriteError {
    #[error("SCIM token not found")]
    TokenNotFound,
    #[error("workspace SCIM token quota exceeded (maximum {MAX_SCIM_TOKENS_PER_WORKSPACE})")]
    QuotaExceeded,
    #[error(transparent)]
    Governance(#[from] aero_common::Error),
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

/// Generate a high-entropy SCIM bearer secret (256-bit, hex), prefixed `scim_`
/// so it is recognizable in logs/config. The plaintext is returned to the caller
/// exactly once at mint time; only its [`hash_token`] is persisted.
#[must_use]
pub fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut hex = String::with_capacity(bytes.len() * 2 + 5);
    hex.push_str("scim_");
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Hash a high-entropy SCIM bearer token for indexed storage and lookup.
#[must_use]
pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

impl ScimRepo {
    /// Mint a SCIM token for setup/tests, storing only its hash.
    ///
    /// Request paths must use [`Self::create_token_authorized`].
    pub async fn create_token(
        &self,
        workspace: WorkspaceId,
        token_hash: &str,
        label: Option<&str>,
    ) -> Result<ScimTokenId, ScimTokenWriteError> {
        let mut tx = self.pool.begin().await?;
        lock_token_workspace(&mut tx, workspace).await?;
        reserve_token_slot(&mut tx, workspace).await?;
        let id = insert_token(&mut tx, workspace, token_hash, label).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Mint a credential under the same workspace lock and transaction as the
    /// effective Owner/Admin recheck and bounded slot reservation.
    pub async fn create_token_authorized(
        &self,
        workspace: WorkspaceId,
        token_hash: &str,
        label: Option<&str>,
        created_by: ParticipantId,
    ) -> Result<ScimTokenId, ScimTokenWriteError> {
        let mut tx = self.pool.begin().await?;
        crate::workspace::authz::assert_effective_admin_in_tx(&mut tx, workspace, created_by)
            .await?;
        reserve_token_slot(&mut tx, workspace).await?;
        let id = insert_token(&mut tx, workspace, token_hash, label).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// List a bounded recovery page of safe credential metadata.
    ///
    /// Active credentials sort first so a legacy over-quota workspace can
    /// always revoke visible live credentials. Healthy workspaces return at
    /// most [`MAX_SCIM_TOKENS_PER_WORKSPACE`] rows. A legacy over-quota
    /// workspace returns one sentinel row beyond the cap, without allowing an
    /// unbounded query or response.
    pub async fn list_tokens(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<ScimTokenRecord>, sqlx::Error> {
        let rows = sqlx::query_as::<_, ScimTokenSqlRow>(LIST_TOKENS_SQL)
            .bind(workspace.to_uuid())
            .bind(SCIM_TOKEN_RECOVERY_PAGE_SIZE)
            .fetch_all(&self.pool)
            .await?;
        Ok(token_records(rows))
    }

    /// List the bounded credential inventory under a transactionally current
    /// effective Owner/Admin decision.
    pub async fn list_tokens_authorized(
        &self,
        workspace: WorkspaceId,
        listed_by: ParticipantId,
    ) -> Result<Vec<ScimTokenRecord>, ScimTokenWriteError> {
        let mut tx = self.pool.begin().await?;
        crate::workspace::authz::assert_effective_admin_in_tx(&mut tx, workspace, listed_by)
            .await?;
        let rows = sqlx::query_as::<_, ScimTokenSqlRow>(LIST_TOKENS_SQL)
            .bind(workspace.to_uuid())
            .bind(SCIM_TOKEN_RECOVERY_PAGE_SIZE)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(token_records(rows))
    }

    /// Resolve an active token hash to its workspace.
    pub async fn workspace_for_token_hash(
        &self,
        token_hash: &str,
    ) -> Result<Option<WorkspaceId>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT workspace_id FROM scim_tokens
               WHERE token_hash = $1 AND revoked_at IS NULL",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(workspace,)| WorkspaceId::from_uuid(workspace)))
    }

    /// Low-level idempotent revoke for setup/tests.
    ///
    /// Request paths must use [`Self::revoke_token_authorized`].
    pub async fn revoke_token(&self, id: ScimTokenId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE scim_tokens SET revoked_at = now()
               WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(id.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Revoke a credential under the token workspace's governance lock with a
    /// transactionally current effective Owner/Admin decision.
    pub async fn revoke_token_authorized(
        &self,
        id: ScimTokenId,
        revoked_by: ParticipantId,
    ) -> Result<bool, ScimTokenWriteError> {
        let mut tx = self.pool.begin().await?;
        let workspace = sqlx::query_scalar::<_, uuid::Uuid>(
            "SELECT workspace_id FROM scim_tokens WHERE id = $1",
        )
        .bind(id.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .map(WorkspaceId::from_uuid)
        .ok_or(ScimTokenWriteError::TokenNotFound)?;
        crate::workspace::authz::assert_effective_admin_in_tx(&mut tx, workspace, revoked_by)
            .await?;
        let exists = sqlx::query_scalar::<_, bool>(
            r"SELECT true
                FROM scim_tokens
               WHERE id = $1 AND workspace_id = $2
               FOR UPDATE",
        )
        .bind(id.to_uuid())
        .bind(workspace.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !exists {
            return Err(ScimTokenWriteError::TokenNotFound);
        }
        let removed = sqlx::query(
            r"UPDATE scim_tokens SET revoked_at = now()
               WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(id.to_uuid())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        trim_legacy_revoked_excess(&mut tx, workspace).await?;
        tx.commit().await?;
        Ok(removed)
    }

    /// The workspace a token id belongs to, regardless of revocation.
    pub async fn token_workspace(
        &self,
        id: ScimTokenId,
    ) -> Result<Option<WorkspaceId>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid,)>(
            "SELECT workspace_id FROM scim_tokens WHERE id = $1",
        )
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(workspace,)| WorkspaceId::from_uuid(workspace)))
    }
}

async fn lock_token_workspace(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
) -> Result<(), ScimTokenWriteError> {
    let exists =
        sqlx::query_scalar::<_, bool>("SELECT true FROM workspaces WHERE id = $1 FOR UPDATE")
            .bind(workspace.to_uuid())
            .fetch_optional(&mut **tx)
            .await?
            .is_some();
    if !exists {
        return Err(aero_common::Error::NotFound(format!("workspace {workspace}")).into());
    }
    Ok(())
}

async fn reserve_token_slot(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
) -> Result<(), ScimTokenWriteError> {
    let bounded_count = sqlx::query_scalar::<_, i64>(
        r"SELECT COUNT(*)
             FROM (
                 SELECT 1
                   FROM scim_tokens
                  WHERE workspace_id = $1
                  LIMIT $2
             ) AS bounded_tokens",
    )
    .bind(workspace.to_uuid())
    .bind(SCIM_TOKEN_RECOVERY_PAGE_SIZE)
    .fetch_one(&mut **tx)
    .await?;

    if bounded_count > MAX_SCIM_TOKENS_PER_WORKSPACE_DB {
        return Err(ScimTokenWriteError::QuotaExceeded);
    }
    if bounded_count == MAX_SCIM_TOKENS_PER_WORKSPACE_DB {
        let pruned = sqlx::query(
            r"DELETE FROM scim_tokens
                WHERE id = (
                    SELECT id
                      FROM scim_tokens
                     WHERE workspace_id = $1
                       AND revoked_at IS NOT NULL
                     ORDER BY revoked_at, created_at, id
                     LIMIT 1
                     FOR UPDATE
                )",
        )
        .bind(workspace.to_uuid())
        .execute(&mut **tx)
        .await?
        .rows_affected();
        if pruned == 0 {
            return Err(ScimTokenWriteError::QuotaExceeded);
        }
    }
    Ok(())
}

async fn insert_token(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    token_hash: &str,
    label: Option<&str>,
) -> Result<ScimTokenId, sqlx::Error> {
    let id = ScimTokenId::new();
    sqlx::query(
        r"INSERT INTO scim_tokens (id, workspace_id, token_hash, label, created_at)
           VALUES ($1, $2, $3, $4, now())",
    )
    .bind(id.to_uuid())
    .bind(workspace.to_uuid())
    .bind(token_hash)
    .bind(label)
    .execute(&mut **tx)
    .await?;
    Ok(id)
}

/// Bring a legacy inventory toward the hard cap without ever deleting a live
/// credential. The request-facing revoke path calls this while holding the
/// workspace lock, so revoking one of 101 active legacy tokens immediately
/// restores a healthy inventory of 100 records. If the excess already consists
/// of revoked rows, one idempotent revoke can purge all removable excess.
async fn trim_legacy_revoked_excess(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r"WITH excess AS (
               SELECT GREATEST(COUNT(*) - $2, 0) AS count
                 FROM scim_tokens
                WHERE workspace_id = $1
           ),
           doomed AS (
               SELECT id
                 FROM scim_tokens
                WHERE workspace_id = $1
                  AND revoked_at IS NOT NULL
                ORDER BY revoked_at, created_at, id
                LIMIT (SELECT count FROM excess)
           )
           DELETE FROM scim_tokens
            WHERE id IN (SELECT id FROM doomed)",
    )
    .bind(workspace.to_uuid())
    .bind(MAX_SCIM_TOKENS_PER_WORKSPACE_DB)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn token_records(rows: Vec<ScimTokenSqlRow>) -> Vec<ScimTokenRecord> {
    rows.into_iter()
        .map(
            |(id, workspace_id, label, created_at, revoked_at)| ScimTokenRecord {
                id: ScimTokenId::from_uuid(id),
                workspace_id: WorkspaceId::from_uuid(workspace_id),
                label,
                created_at,
                revoked_at,
            },
        )
        .collect()
}

#[cfg(test)]
mod quota_db_tests {
    use super::*;
    use crate::WorkspaceRepo;

    fn pool(max_connections: u32) -> sqlx::PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(max_connections)
            .connect_lazy(&url)
            .expect("valid DATABASE_URL")
    }

    async fn fixture(pool: &sqlx::PgPool, label: &str) -> (ParticipantId, WorkspaceId, ScimRepo) {
        let owner = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(owner.to_uuid())
            .bind(format!("scim-token-{label}-{owner}"))
            .execute(pool)
            .await
            .expect("insert owner");
        let workspace = WorkspaceRepo::new(pool.clone())
            .create(
                format!("SCIM token {label} {owner}"),
                format!("scim-token-{label}-{owner}"),
                owner,
            )
            .await
            .expect("create workspace")
            .id;
        (owner, workspace, ScimRepo::new(pool.clone()))
    }

    async fn seed_active_tokens(pool: &sqlx::PgPool, workspace: WorkspaceId, count: i64) {
        let prefix = format!("seed-{workspace}-");
        sqlx::query(
            r"INSERT INTO scim_tokens
                 (id, workspace_id, token_hash, label, created_at)
               SELECT gen_random_uuid(), $1, $2 || n::text, 'seed', now()
                 FROM generate_series(1::bigint, $3) AS n",
        )
        .bind(workspace.to_uuid())
        .bind(prefix)
        .bind(count)
        .execute(pool)
        .await
        .expect("seed SCIM tokens");
    }

    async fn token_count(pool: &sqlx::PgPool, workspace: WorkspaceId) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM scim_tokens WHERE workspace_id = $1")
            .bind(workspace.to_uuid())
            .fetch_one(pool)
            .await
            .expect("count SCIM tokens")
    }

    async fn cleanup(pool: &sqlx::PgPool, owner: ParticipantId, workspace: WorkspaceId) {
        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(workspace.to_uuid())
            .execute(pool)
            .await
            .expect("delete workspace");
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(owner.to_uuid())
            .execute(pool)
            .await
            .expect("delete owner");
    }

    #[tokio::test]
    #[ignore = "requires a migrated PostgreSQL database"]
    async fn scim_token_quota_is_atomic_under_concurrent_mints() {
        let pool = pool(4);
        let (owner, workspace, repo) = fixture(&pool, "quota-race").await;
        seed_active_tokens(&pool, workspace, MAX_SCIM_TOKENS_PER_WORKSPACE_DB - 1).await;

        let first_hash = hash_token(&format!("contender-a-{}", uuid::Uuid::new_v4()));
        let second_hash = hash_token(&format!("contender-b-{}", uuid::Uuid::new_v4()));
        let first =
            repo.create_token_authorized(workspace, &first_hash, Some("contender-a"), owner);
        let second =
            repo.create_token_authorized(workspace, &second_hash, Some("contender-b"), owner);
        let (first, second) = tokio::join!(first, second);
        let mut succeeded = 0;
        let mut quota_rejected = 0;
        for result in [first, second] {
            match result {
                Ok(_) => succeeded += 1,
                Err(ScimTokenWriteError::QuotaExceeded) => quota_rejected += 1,
                Err(error) => panic!("unexpected mint result: {error}"),
            }
        }

        assert_eq!(succeeded, 1);
        assert_eq!(quota_rejected, 1);
        assert_eq!(
            token_count(&pool, workspace).await,
            MAX_SCIM_TOKENS_PER_WORKSPACE_DB
        );
        assert_eq!(
            repo.list_tokens_authorized(workspace, owner)
                .await
                .unwrap()
                .len(),
            MAX_SCIM_TOKENS_PER_WORKSPACE
        );

        cleanup(&pool, owner, workspace).await;
    }

    #[tokio::test]
    #[ignore = "requires a migrated PostgreSQL database"]
    async fn scim_token_legacy_overage_is_bounded_and_recoverable_by_revoke() {
        let pool = pool(3);
        let (owner, workspace, repo) = fixture(&pool, "legacy-recovery").await;
        seed_active_tokens(&pool, workspace, MAX_SCIM_TOKENS_PER_WORKSPACE_DB + 1).await;

        let recovery_page = repo.list_tokens_authorized(workspace, owner).await.unwrap();
        assert_eq!(
            recovery_page.len(),
            MAX_SCIM_TOKENS_PER_WORKSPACE + 1,
            "inventory exposes only one bounded over-quota sentinel"
        );
        assert!(recovery_page.iter().all(|token| token.revoked_at.is_none()));
        assert!(matches!(
            repo.create_token_authorized(
                workspace,
                &hash_token("legacy-overage-mint"),
                None,
                owner
            )
            .await,
            Err(ScimTokenWriteError::QuotaExceeded)
        ));

        let legacy_victim = recovery_page[0].id;
        assert!(repo
            .revoke_token_authorized(legacy_victim, owner)
            .await
            .unwrap());
        assert_eq!(
            token_count(&pool, workspace).await,
            MAX_SCIM_TOKENS_PER_WORKSPACE_DB,
            "legacy revoke purges removable excess under the workspace lock"
        );
        assert_eq!(repo.token_workspace(legacy_victim).await.unwrap(), None);

        let retained_victim = repo.list_tokens(workspace).await.unwrap()[0].id;
        assert!(repo
            .revoke_token_authorized(retained_victim, owner)
            .await
            .unwrap());
        assert_eq!(
            token_count(&pool, workspace).await,
            MAX_SCIM_TOKENS_PER_WORKSPACE_DB,
            "normal revocation remains auditable until its slot is reused"
        );
        assert!(repo
            .token_workspace(retained_victim)
            .await
            .unwrap()
            .is_some());

        let replacement_hash = hash_token(&format!("replacement-{}", uuid::Uuid::new_v4()));
        repo.create_token_authorized(workspace, &replacement_hash, Some("replacement"), owner)
            .await
            .expect("a revoked record is evicted to free one bounded slot");
        assert_eq!(
            token_count(&pool, workspace).await,
            MAX_SCIM_TOKENS_PER_WORKSPACE_DB
        );
        assert_eq!(repo.token_workspace(retained_victim).await.unwrap(), None);
        assert_eq!(
            repo.workspace_for_token_hash(&replacement_hash)
                .await
                .unwrap(),
            Some(workspace)
        );

        cleanup(&pool, owner, workspace).await;
    }
}
