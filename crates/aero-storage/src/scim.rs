//! SCIM 2.0 provisioning repository (RFC 7643/7644 — Users + Groups).
//!
//! Backs `migrations/0015_scim.sql`. Two concerns, one repo:
//!
//! 1. **Tokens** (`scim_tokens`): per-workspace bearer tokens an external `IdP`
//!    (Okta / Azure AD) presents on every SCIM request. Only the SHA-256 *hash*
//!    of a token is stored — the plaintext is shown once at mint time and never
//!    persisted (mirrors password-hash handling). Resolving an incoming token to
//!    a workspace is the SCIM auth check.
//!
//! 2. **Users** ([`scim_users`]): a workspace-scoped SCIM identity layered over a
//!    GLOBAL [`participant`](crate::ParticipantRepo). A SCIM "User" is a
//!    `participant` + a [`workspace_member`](crate::WorkspaceRepo) of the token's
//!    workspace + a `scim_users` row carrying `userName` / `externalId` / `active`.
//!    De-provisioning marks the row inactive **and** removes the workspace
//!    membership, but never deletes the global participant (they may belong to
//!    other tenants).
//!
//! Purely additive: a NEW [`ScimRepo`]; no existing repo is touched. Token
//! hashing is a pure free function ([`hash_token`]) so it unit-tests offline,
//! mirroring how the rest of `aero-storage` keeps testable logic separate from
//! live SQL (Postgres is absent in CI).

use aero_common::{ParticipantId, ScimTokenId, WorkspaceId};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

/// One provisioned SCIM user row (the workspace-scoped identity over a global
/// participant). The handler maps this into the RFC 7643 `User` resource.
#[derive(Debug, Clone)]
pub struct ScimUserRow {
    pub workspace_id: WorkspaceId,
    pub participant_id: ParticipantId,
    pub user_name: String,
    pub external_id: Option<String>,
    pub active: bool,
    pub created_at: time::OffsetDateTime,
    pub updated_at: time::OffsetDateTime,
}

/// Generate a high-entropy SCIM bearer secret (256-bit, hex), prefixed `scim_`
/// so it is recognizable in logs/config. The plaintext is returned to the caller
/// exactly once at mint time; only its [`hash_token`] is persisted. Lives here
/// (next to the hash) so the server crate needs no `rand` dependency.
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

/// Hash a SCIM bearer token for storage / lookup.
///
/// SHA-256 hex. A SCIM token is a long random secret (not a low-entropy
/// password), so a fast cryptographic digest is the right primitive: it makes
/// the stored value useless if the table leaks, while keeping the per-request
/// resolve a single indexed lookup. Pure, so it is unit-tested without a DB.
#[must_use]
pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut s = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(s, "{byte:02x}");
    }
    s
}

/// Largest page a `list_users` call will return, regardless of requested count.
/// Mirrors the audit/history caps elsewhere in the crate.
const MAX_PAGE: i64 = 200;

/// Clamp a requested SCIM `count` into `1..=MAX_PAGE` (defaulting `None`/non-positive
/// to `MAX_PAGE`). Pure so it unit-tests without a DB.
#[must_use]
fn clamp_count(requested: Option<i64>) -> i64 {
    match requested {
        Some(n) if n >= 1 => n.min(MAX_PAGE),
        _ => MAX_PAGE,
    }
}

/// Normalize a SCIM `startIndex` (1-based, RFC 7644 §3.4.2) into a 0-based SQL
/// `OFFSET`. Absent or `< 1` is treated as the first page (offset 0). Pure.
#[must_use]
fn start_offset(start_index: Option<i64>) -> i64 {
    match start_index {
        Some(n) if n >= 1 => n - 1,
        _ => 0,
    }
}

#[derive(Clone)]
pub struct ScimRepo {
    pool: PgPool,
}

impl ScimRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    // ----------------------------------------------------------- tokens

    /// Mint a SCIM token for a workspace, storing only its hash. `token_hash`
    /// must already be [`hash_token`]ed by the caller (the route hashes the
    /// freshly-generated plaintext and returns the plaintext exactly once).
    /// Returns the new token's id.
    pub async fn create_token(
        &self,
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
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Resolve an incoming token hash to its workspace — the SCIM auth check.
    /// Returns `None` for an unknown OR revoked token (active-only).
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
        Ok(row.map(|(w,)| WorkspaceId::from_uuid(w)))
    }

    /// Revoke a token (idempotent — a no-op if already revoked / absent).
    /// Returns `true` if a still-active row was revoked by this call.
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

    /// The workspace a token id belongs to (regardless of revocation), so the
    /// management route can authorize a revoke against the caller's workspace.
    pub async fn token_workspace(
        &self,
        id: ScimTokenId,
    ) -> Result<Option<WorkspaceId>, sqlx::Error> {
        let row = sqlx::query_as::<_, (uuid::Uuid,)>(
            r"SELECT workspace_id FROM scim_tokens WHERE id = $1",
        )
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(w,)| WorkspaceId::from_uuid(w)))
    }

    // ------------------------------------------------------------ users

    /// Create the SCIM-user mapping row for an already-created participant. The
    /// caller is responsible for creating the participant + workspace membership;
    /// this records the workspace-scoped SCIM identity. A duplicate `userName`
    /// within the workspace violates the unique index and surfaces as a
    /// `sqlx::Error` the route maps to `409 Conflict`.
    pub async fn create_user(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
        user_name: &str,
        external_id: Option<&str>,
        active: bool,
    ) -> Result<ScimUserRow, sqlx::Error> {
        let now = time::OffsetDateTime::now_utc();
        sqlx::query(
            r"INSERT INTO scim_users
                 (workspace_id, participant_id, user_name, external_id, active, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, $6)",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(user_name)
        .bind(external_id)
        .bind(active)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(ScimUserRow {
            workspace_id: workspace,
            participant_id: participant,
            user_name: user_name.to_owned(),
            external_id: external_id.map(str::to_owned),
            active,
            created_at: now,
            updated_at: now,
        })
    }

    /// Fetch one SCIM user (by participant) within a workspace, or `None`.
    pub async fn get_user(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<Option<ScimUserRow>, sqlx::Error> {
        let row = sqlx::query_as::<_, ScimUserSqlRow>(
            r"SELECT workspace_id, participant_id, user_name, external_id, active, created_at, updated_at
               FROM scim_users
               WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(ScimUserRow::from))
    }

    /// Resolve a `userName` to its participant within a workspace (the common
    /// `userName eq "x"` provisioning lookup), or `None`.
    pub async fn find_by_user_name(
        &self,
        workspace: WorkspaceId,
        user_name: &str,
    ) -> Result<Option<ScimUserRow>, sqlx::Error> {
        let row = sqlx::query_as::<_, ScimUserSqlRow>(
            r"SELECT workspace_id, participant_id, user_name, external_id, active, created_at, updated_at
               FROM scim_users
               WHERE workspace_id = $1 AND user_name = $2",
        )
        .bind(workspace.to_uuid())
        .bind(user_name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(ScimUserRow::from))
    }

    /// List a workspace's SCIM users, optionally filtered by exact `userName`
    /// (the `userName eq "x"` filter), paginated by SCIM `startIndex`/`count`.
    /// Returns `(rows, total_results)` where `total` is the unpaginated count for
    /// the same filter (RFC 7644 `totalResults`).
    pub async fn list_users(
        &self,
        workspace: WorkspaceId,
        filter_user_name: Option<&str>,
        start_index: Option<i64>,
        count: Option<i64>,
    ) -> Result<(Vec<ScimUserRow>, i64), sqlx::Error> {
        let limit = clamp_count(count);
        let offset = start_offset(start_index);

        // Total for the same filter (NULL filter ⇒ count all).
        let (total,): (i64,) = sqlx::query_as(
            r"SELECT COUNT(*) FROM scim_users
               WHERE workspace_id = $1
                 AND ($2::text IS NULL OR user_name = $2)",
        )
        .bind(workspace.to_uuid())
        .bind(filter_user_name)
        .fetch_one(&self.pool)
        .await?;

        let rows = sqlx::query_as::<_, ScimUserSqlRow>(
            r"SELECT workspace_id, participant_id, user_name, external_id, active, created_at, updated_at
               FROM scim_users
               WHERE workspace_id = $1
                 AND ($2::text IS NULL OR user_name = $2)
               ORDER BY created_at ASC, participant_id ASC
               LIMIT $3 OFFSET $4",
        )
        .bind(workspace.to_uuid())
        .bind(filter_user_name)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        Ok((rows.into_iter().map(ScimUserRow::from).collect(), total))
    }

    /// Toggle a SCIM user's `active` flag (the deprovision/reactivate path).
    /// Returns the updated row, or `None` if no such SCIM user exists.
    pub async fn set_active(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
        active: bool,
    ) -> Result<Option<ScimUserRow>, sqlx::Error> {
        let row = sqlx::query_as::<_, ScimUserSqlRow>(
            r"UPDATE scim_users
                 SET active = $3, updated_at = now()
               WHERE workspace_id = $1 AND participant_id = $2
            RETURNING workspace_id, participant_id, user_name, external_id, active, created_at, updated_at",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(active)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(ScimUserRow::from))
    }

    /// Patch a SCIM user's `userName`, `external_id`, and/or `active`. A `None`
    /// argument leaves that field unchanged (the inner `Option` of `external_id`
    /// distinguishes "set to null" from "leave alone"). Returns the updated row,
    /// or `None` if no such SCIM user exists. A `userName` collision surfaces as
    /// a `sqlx::Error` (the unique index) the route maps to `409`.
    pub async fn update_user(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
        user_name: Option<&str>,
        external_id: Option<Option<&str>>,
        active: Option<bool>,
    ) -> Result<Option<ScimUserRow>, sqlx::Error> {
        let row = sqlx::query_as::<_, ScimUserSqlRow>(
            r"UPDATE scim_users SET
                 user_name   = COALESCE($3, user_name),
                 external_id = CASE WHEN $4::boolean THEN $5 ELSE external_id END,
                 active      = COALESCE($6, active),
                 updated_at  = now()
               WHERE workspace_id = $1 AND participant_id = $2
            RETURNING workspace_id, participant_id, user_name, external_id, active, created_at, updated_at",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .bind(user_name)
        .bind(external_id.is_some())
        .bind(external_id.flatten())
        .bind(active)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(ScimUserRow::from))
    }

    /// De-provision a SCIM user: mark the SCIM row inactive AND remove the
    /// workspace membership, atomically. The global `participant` is **never**
    /// deleted (they may belong to other tenants). Returns `true` if a SCIM row
    /// existed and was deactivated.
    ///
    /// The SCIM row is kept (not deleted) so the `IdP` can still GET the user by
    /// the same id and observe `active: false`, and later re-activate them.
    pub async fn delete_user(
        &self,
        workspace: WorkspaceId,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        let result = sqlx::query(
            r"UPDATE scim_users SET active = false, updated_at = now()
               WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await?;

        // Remove the workspace membership (the actual access revocation). No-op
        // if they were not a member; the participant identity is untouched.
        sqlx::query(
            r"DELETE FROM workspace_members
               WHERE workspace_id = $1 AND participant_id = $2",
        )
        .bind(workspace.to_uuid())
        .bind(participant.to_uuid())
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }
}

/// sqlx row shape for `scim_users` decoding.
#[derive(sqlx::FromRow)]
struct ScimUserSqlRow {
    workspace_id: uuid::Uuid,
    participant_id: uuid::Uuid,
    user_name: String,
    external_id: Option<String>,
    active: bool,
    created_at: time::OffsetDateTime,
    updated_at: time::OffsetDateTime,
}

impl From<ScimUserSqlRow> for ScimUserRow {
    fn from(r: ScimUserSqlRow) -> Self {
        Self {
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            participant_id: ParticipantId::from_uuid(r.participant_id),
            user_name: r.user_name,
            external_id: r.external_id,
            active: r.active,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_token_is_stable_hex_sha256() {
        // Known SHA-256 of the empty string and "abc" (FIPS 180-4 examples).
        assert_eq!(
            hash_token(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hash_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Hex, 64 chars, deterministic.
        let h = hash_token("a-scim-secret");
        assert_eq!(h.len(), 64);
        assert!(h.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(h, hash_token("a-scim-secret"));
        assert_ne!(h, hash_token("a-scim-secre")); // sensitive to input
    }

    #[test]
    fn generate_token_is_prefixed_high_entropy_and_hashable() {
        let a = generate_token();
        let b = generate_token();
        assert!(a.starts_with("scim_"));
        assert_eq!(a.len(), "scim_".len() + 64, "256-bit hex secret");
        assert_ne!(a, b, "two mints differ");
        // The generated secret hashes to a stable 64-char hex digest.
        assert_eq!(hash_token(&a).len(), 64);
    }

    #[test]
    fn clamp_count_defaults_and_bounds() {
        assert_eq!(clamp_count(None), MAX_PAGE);
        assert_eq!(clamp_count(Some(0)), MAX_PAGE);
        assert_eq!(clamp_count(Some(-5)), MAX_PAGE);
        assert_eq!(clamp_count(Some(1)), 1);
        assert_eq!(clamp_count(Some(50)), 50);
        assert_eq!(clamp_count(Some(10_000)), MAX_PAGE);
    }

    #[test]
    fn start_offset_is_one_based_to_zero_based() {
        assert_eq!(start_offset(None), 0);
        assert_eq!(start_offset(Some(0)), 0); // invalid, clamp to first page
        assert_eq!(start_offset(Some(-3)), 0);
        assert_eq!(start_offset(Some(1)), 0); // SCIM startIndex is 1-based
        assert_eq!(start_offset(Some(2)), 1);
        assert_eq!(start_offset(Some(51)), 50);
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored scim_
/// ```
///
/// They are `#[ignore]` so the default `cargo test` stays hermetic (no DB in CI).
#[cfg(test)]
mod db_tests {
    use super::*;
    use aero_common::WorkspaceRole;
    use crate::workspace::WorkspaceRepo;

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    async fn new_participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("scim-user-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    async fn new_workspace(repo: &WorkspaceRepo, owner: ParticipantId) -> WorkspaceId {
        repo.create("SCIM WS".into(), format!("scim-{}", WorkspaceId::new()), owner)
            .await
            .expect("create workspace")
            .id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn scim_token_create_resolve_and_revoke() {
        let p = pool();
        let scim = ScimRepo::new(p.clone());
        let ws_repo = WorkspaceRepo::new(p.clone());
        let owner = new_participant(&p).await;
        let ws = new_workspace(&ws_repo, owner).await;

        let secret = format!("tok-{}", uuid::Uuid::new_v4());
        let hash = hash_token(&secret);
        let id = scim.create_token(ws, &hash, Some("okta")).await.unwrap();

        // The plaintext's hash resolves to the workspace …
        assert_eq!(scim.workspace_for_token_hash(&hash).await.unwrap(), Some(ws));
        // … and an unknown hash does not.
        assert_eq!(scim.workspace_for_token_hash("deadbeef").await.unwrap(), None);
        assert_eq!(scim.token_workspace(id).await.unwrap(), Some(ws));

        // Revoking takes it out of active resolution.
        assert!(scim.revoke_token(id).await.unwrap(), "first revoke succeeds");
        assert_eq!(scim.workspace_for_token_hash(&hash).await.unwrap(), None);
        assert!(!scim.revoke_token(id).await.unwrap(), "second revoke is a no-op");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn scim_user_create_list_find_and_deactivate() {
        let p = pool();
        let scim = ScimRepo::new(p.clone());
        let ws_repo = WorkspaceRepo::new(p.clone());
        let owner = new_participant(&p).await;
        let ws = new_workspace(&ws_repo, owner).await;

        let participant = new_participant(&p).await;
        ws_repo.add_member(ws, participant, WorkspaceRole::Member).await.unwrap();
        let user_name = format!("alice-{participant}@example.com");
        let created = scim
            .create_user(ws, participant, &user_name, Some("ext-123"), true)
            .await
            .unwrap();
        assert!(created.active);

        // get + find_by_user_name round-trip.
        assert_eq!(
            scim.get_user(ws, participant).await.unwrap().map(|u| u.user_name.clone()),
            Some(user_name.clone())
        );
        let found = scim.find_by_user_name(ws, &user_name).await.unwrap().expect("found");
        assert_eq!(found.participant_id, participant);
        assert_eq!(found.external_id.as_deref(), Some("ext-123"));

        // list with the userName filter returns exactly this one, total reflects filter.
        let (rows, total) = scim
            .list_users(ws, Some(&user_name), Some(1), Some(50))
            .await
            .unwrap();
        assert_eq!(total, 1, "exactly one matches the filter");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].participant_id, participant);

        // Deactivate (deprovision): SCIM row inactive + membership removed,
        // participant retained.
        assert!(scim.delete_user(ws, participant).await.unwrap(), "row existed");
        let after = scim.get_user(ws, participant).await.unwrap().expect("row retained");
        assert!(!after.active, "marked inactive");
        assert!(!ws_repo.is_member(ws, participant).await.unwrap(), "membership removed");
        assert!(
            scim.get_user(ws, participant).await.unwrap().is_some(),
            "global participant + SCIM row retained for later GET/reactivation"
        );

        // set_active can reactivate.
        let reactivated = scim.set_active(ws, participant, true).await.unwrap().expect("exists");
        assert!(reactivated.active);
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn scim_duplicate_user_name_conflicts() {
        let p = pool();
        let scim = ScimRepo::new(p.clone());
        let ws_repo = WorkspaceRepo::new(p.clone());
        let owner = new_participant(&p).await;
        let ws = new_workspace(&ws_repo, owner).await;

        let a = new_participant(&p).await;
        let b = new_participant(&p).await;
        let name = format!("dup-{}@example.com", uuid::Uuid::new_v4());
        scim.create_user(ws, a, &name, None, true).await.unwrap();
        // A second user with the same userName in the same workspace must fail
        // (the unique index), which the route maps to 409 Conflict.
        assert!(
            scim.create_user(ws, b, &name, None, true).await.is_err(),
            "duplicate userName within a workspace is rejected"
        );
    }
}
