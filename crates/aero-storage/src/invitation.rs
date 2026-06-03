//! Workspace invitation / invite-link repository.
//!
//! Backs `migrations/0017_invitations.sql`. A workspace admin/owner mints an
//! [`Invitation`] — either an email invite or an open shareable link — and a
//! logged-in invitee redeems it (by its token) to become a `workspace_member`
//! with the invite's [`WorkspaceRole`]. An invite may be one-shot or multi-use,
//! bounded by an optional `max_uses` and/or `expires_at`.
//!
//! Only the SHA-256 *hash* of the token is stored ([`hash_token`]); the plaintext
//! is generated once at creation ([`generate_token`]) and returned to the caller,
//! never persisted — mirroring password / provisioning-token handling.
//!
//! Purely additive: a NEW [`InvitationRepo`]; no existing repo is touched. The
//! redeemability predicate ([`invitation_is_redeemable`]) is a pure free function
//! (no DB, no clock) so it unit-tests directly, mirroring how the rest of
//! `aero-storage` keeps testable logic separate from live SQL (Postgres is absent
//! in CI).

use aero_common::{InvitationId, ParticipantId, WorkspaceId, WorkspaceRole};
use rand::RngCore;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;

// ---------------------------------------------------------------- Pure crypto

/// Number of random bytes behind a generated invite token (256 bits).
const TOKEN_BYTES: usize = 32;

/// Generate a fresh, high-entropy invite token (256-bit, lowercase hex).
///
/// Returned to the creator exactly once (embedded in the shareable
/// `invite_url`); only its [`hash_token`] is stored. Pure aside from the RNG.
#[must_use]
pub fn generate_token() -> String {
    let mut buf = [0u8; TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}

/// SHA-256 hex of an invite token. The active-invite lookup keys on this so the
/// plaintext token never has to be stored. Deterministic + pure, so it is
/// unit-tested without a DB.
#[must_use]
pub fn hash_token(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    hex::encode(h.finalize())
}

// ----------------------------------------------------------------- Data model

/// One workspace invitation. Listing intentionally omits the token (only its
/// hash is stored anyway); the plaintext is shown once at creation time.
#[derive(Debug, Clone, Serialize)]
pub struct Invitation {
    pub id: InvitationId,
    pub workspace_id: WorkspaceId,
    /// `None` = open shareable link; otherwise the address the invite targets
    /// (informational — accept does not hard-block on a mismatch this iteration).
    pub email: Option<String>,
    /// The role granted to the invitee on accept.
    pub role: WorkspaceRole,
    /// The admin/owner who created the invite, if still known.
    pub created_by: Option<ParticipantId>,
    /// `None` = unlimited uses; otherwise the invite is exhausted at this count.
    pub max_uses: Option<i32>,
    pub use_count: i32,
    /// `None` = never expires.
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// `None` = active; set on revoke.
    #[serde(with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<OffsetDateTime>,
}

/// Raw `invitations` row shape (column order matches every SELECT below).
type InvitationRow = (
    uuid::Uuid,           // id
    uuid::Uuid,           // workspace_id
    Option<String>,       // email
    String,               // role
    Option<uuid::Uuid>,   // created_by
    Option<i32>,          // max_uses
    i32,                  // use_count
    Option<OffsetDateTime>, // expires_at
    OffsetDateTime,       // created_at
    Option<OffsetDateTime>, // revoked_at
);

fn row_into_invitation(r: InvitationRow) -> Invitation {
    let (id, ws, email, role, created_by, max_uses, use_count, expires_at, created_at, revoked_at) =
        r;
    Invitation {
        id: InvitationId::from_uuid(id),
        workspace_id: WorkspaceId::from_uuid(ws),
        email,
        // Default an unrecognized token to the least-privileged role rather than
        // dropping the row, so listing never silently shrinks.
        role: WorkspaceRole::from_db_str(&role).unwrap_or(WorkspaceRole::Guest),
        created_by: created_by.map(ParticipantId::from_uuid),
        max_uses,
        use_count,
        expires_at,
        created_at,
        revoked_at,
    }
}

/// The columns selected by every invitation read, in a stable order.
const SELECT_COLS: &str = "id, workspace_id, email, role, created_by, max_uses, \
                           use_count, expires_at, created_at, revoked_at";

// -------------------------------------------------------------------- The repo

#[derive(Clone)]
pub struct InvitationRepo {
    pool: PgPool,
}

impl InvitationRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create an invitation, returning its generated id. The caller supplies the
    /// already-hashed token ([`hash_token`]) so the plaintext never reaches this
    /// crate's SQL.
    // Each parameter maps 1:1 to a distinct, non-defaultable invitation column;
    // bundling them into a struct would only add an indirection with no clarity
    // win at the single call site.
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        &self,
        workspace: WorkspaceId,
        token_hash: &str,
        email: Option<&str>,
        role: WorkspaceRole,
        created_by: Option<ParticipantId>,
        max_uses: Option<i32>,
        expires_at: Option<OffsetDateTime>,
    ) -> Result<InvitationId, sqlx::Error> {
        let id = InvitationId::new();
        sqlx::query(
            r"INSERT INTO invitations
                (id, workspace_id, token_hash, email, role, created_by,
                 max_uses, use_count, expires_at, created_at)
              VALUES ($1, $2, $3, $4, $5, $6, $7, 0, $8, now())",
        )
        .bind(id.to_uuid())
        .bind(workspace.to_uuid())
        .bind(token_hash)
        .bind(email)
        .bind(role.as_str())
        .bind(created_by.map(|p| p.to_uuid()))
        .bind(max_uses)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Resolve an *active* invitation by its token hash, as of `now`: not revoked,
    /// not expired, and not exhausted (`use_count < max_uses` when a cap is set).
    ///
    /// The redeemability filter is applied in Rust (via [`invitation_is_redeemable`])
    /// rather than in SQL so the rule has a single, unit-tested home; the query
    /// only resolves the hash → row.
    pub async fn find_active_by_token_hash(
        &self,
        token_hash: &str,
        now: OffsetDateTime,
    ) -> Result<Option<Invitation>, sqlx::Error> {
        let row = sqlx::query_as::<_, InvitationRow>(&format!(
            "SELECT {SELECT_COLS} FROM invitations WHERE token_hash = $1",
        ))
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_into_invitation).filter(|inv| {
            invitation_is_redeemable(
                inv.revoked_at.is_some(),
                inv.expires_at,
                inv.use_count,
                inv.max_uses,
                now,
            )
        }))
    }

    /// All invitations for a workspace, newest first. Tokens are never selected
    /// (only their hash is stored), so this is safe for an admin listing.
    pub async fn list_for_workspace(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Vec<Invitation>, sqlx::Error> {
        let rows = sqlx::query_as::<_, InvitationRow>(&format!(
            "SELECT {SELECT_COLS} FROM invitations \
             WHERE workspace_id = $1 ORDER BY created_at DESC, id DESC",
        ))
        .bind(workspace.to_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_into_invitation).collect())
    }

    /// Atomically record one redemption of an active invite, returning `true` if a
    /// row was updated. The `WHERE` re-checks redeemability so two concurrent
    /// accepts cannot push `use_count` past `max_uses` (the loser updates nothing
    /// and gets `false`): expired/revoked/exhausted invites increment nothing.
    pub async fn increment_use(&self, id: InvitationId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE invitations
                 SET use_count = use_count + 1
               WHERE id = $1
                 AND revoked_at IS NULL
                 AND (expires_at IS NULL OR expires_at > now())
                 AND (max_uses IS NULL OR use_count < max_uses)",
        )
        .bind(id.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Revoke an invitation (idempotent: only the first call flips `revoked_at`).
    /// Returns `true` if this call performed the revoke, `false` if it was already
    /// revoked or does not exist.
    pub async fn revoke(&self, id: InvitationId) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            r"UPDATE invitations SET revoked_at = now()
               WHERE id = $1 AND revoked_at IS NULL",
        )
        .bind(id.to_uuid())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Fetch a single invitation (active or not), or `None`. Used by the revoke
    /// route to resolve the invite's workspace for the admin authorization check.
    pub async fn get(&self, id: InvitationId) -> Result<Option<Invitation>, sqlx::Error> {
        let row = sqlx::query_as::<_, InvitationRow>(&format!(
            "SELECT {SELECT_COLS} FROM invitations WHERE id = $1",
        ))
        .bind(id.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(row_into_invitation))
    }
}

// ---------- Pure redeemability predicate (DB-free, unit-tested) ----------

/// Is an invitation redeemable, given its current state as of `now`?
///
/// An invite is redeemable when it is **not** revoked, **not** past its
/// `expires_at` (when set), and **not** exhausted (`use_count < max_uses` when a
/// cap is set; an unset `max_uses` means unlimited). Pure — no DB, no wall clock
/// — so each clause is exercised offline.
#[must_use]
pub fn invitation_is_redeemable(
    revoked: bool,
    expires_at: Option<OffsetDateTime>,
    use_count: i32,
    max_uses: Option<i32>,
    now: OffsetDateTime,
) -> bool {
    if revoked {
        return false;
    }
    if let Some(exp) = expires_at {
        if now >= exp {
            return false;
        }
    }
    if let Some(cap) = max_uses {
        if use_count >= cap {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(secs).expect("valid timestamp")
    }

    #[test]
    fn hash_is_deterministic_and_differs_per_token() {
        assert_eq!(hash_token("abc"), hash_token("abc"));
        assert_ne!(hash_token("abc"), hash_token("abd"));
        // SHA-256 hex is 64 chars.
        assert_eq!(hash_token("anything").len(), 64);
    }

    #[test]
    fn generated_tokens_are_unique_hex() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b, "two tokens must not collide");
        // 32 random bytes => 64 hex chars, all hex digits.
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn redeemable_when_fresh_unlimited_and_active() {
        // Not revoked, no expiry, no cap → always redeemable.
        assert!(invitation_is_redeemable(false, None, 0, None, t(1_000)));
        assert!(invitation_is_redeemable(false, None, 9_999, None, t(1_000)));
    }

    #[test]
    fn not_redeemable_when_revoked() {
        // Revoked dominates even an otherwise-perfectly-valid invite.
        assert!(!invitation_is_redeemable(true, None, 0, None, t(1_000)));
        assert!(!invitation_is_redeemable(true, Some(t(2_000)), 0, Some(10), t(1_000)));
    }

    #[test]
    fn not_redeemable_when_expired_boundary_inclusive() {
        let exp = t(2_000);
        // Strictly before expiry → ok.
        assert!(invitation_is_redeemable(false, Some(exp), 0, None, t(1_999)));
        // Exactly at expiry → NOT redeemable (`now >= exp`).
        assert!(!invitation_is_redeemable(false, Some(exp), 0, None, exp));
        // After expiry → not redeemable.
        assert!(!invitation_is_redeemable(false, Some(exp), 0, None, t(2_001)));
    }

    #[test]
    fn not_redeemable_when_exhausted() {
        // max_uses = 3: counts 0..=2 are fine, 3 (and beyond) are exhausted.
        assert!(invitation_is_redeemable(false, None, 2, Some(3), t(1_000)));
        assert!(!invitation_is_redeemable(false, None, 3, Some(3), t(1_000)));
        assert!(!invitation_is_redeemable(false, None, 4, Some(3), t(1_000)));
        // A single-use invite: usable at 0, exhausted at 1.
        assert!(invitation_is_redeemable(false, None, 0, Some(1), t(1_000)));
        assert!(!invitation_is_redeemable(false, None, 1, Some(1), t(1_000)));
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored invitation_
/// ```
///
/// They are `#[ignore]` so the default `cargo test` stays hermetic (no DB in CI);
/// the orchestrator runs them against a live database.
#[cfg(test)]
mod db_tests {
    use super::*;

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
            .bind(format!("invite-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    async fn new_workspace(p: &PgPool, owner: ParticipantId) -> WorkspaceId {
        let ws = WorkspaceId::new();
        sqlx::query(
            "INSERT INTO workspaces (id, name, slug, created_by, created_at) \
             VALUES ($1, $2, $3, $4, now())",
        )
        .bind(ws.to_uuid())
        .bind("Invite Test WS")
        .bind(format!("inv-{ws}"))
        .bind(owner.to_uuid())
        .execute(p)
        .await
        .expect("insert workspace");
        ws
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn invitation_create_find_increment_then_exhaust() {
        let p = pool();
        let repo = InvitationRepo::new(p.clone());
        let owner = new_participant(&p).await;
        let ws = new_workspace(&p, owner).await;

        let token = generate_token();
        let hash = hash_token(&token);
        let id = repo
            .create(ws, &hash, Some("a@example.com"), WorkspaceRole::Member, Some(owner), Some(1), None)
            .await
            .expect("create invite");

        let now = OffsetDateTime::now_utc();
        // Found while active, with its fields intact.
        let found = repo
            .find_active_by_token_hash(&hash, now)
            .await
            .unwrap()
            .expect("active invite resolves");
        assert_eq!(found.id, id);
        assert_eq!(found.workspace_id, ws);
        assert_eq!(found.role, WorkspaceRole::Member);
        assert_eq!(found.email.as_deref(), Some("a@example.com"));
        assert_eq!(found.max_uses, Some(1));
        assert_eq!(found.use_count, 0);

        // First redemption succeeds; the single-use invite is now exhausted.
        assert!(repo.increment_use(id).await.unwrap(), "first use recorded");
        // A second increment finds nothing (cap reached) → false.
        assert!(!repo.increment_use(id).await.unwrap(), "exhausted invite increments nothing");
        // And it no longer resolves as active.
        assert!(
            repo.find_active_by_token_hash(&hash, now).await.unwrap().is_none(),
            "exhausted invite is not active"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn invitation_revoke_makes_it_inactive() {
        let p = pool();
        let repo = InvitationRepo::new(p.clone());
        let owner = new_participant(&p).await;
        let ws = new_workspace(&p, owner).await;

        let token = generate_token();
        let hash = hash_token(&token);
        let id = repo
            .create(ws, &hash, None, WorkspaceRole::Guest, Some(owner), None, None)
            .await
            .expect("create open link");

        let now = OffsetDateTime::now_utc();
        assert!(repo.find_active_by_token_hash(&hash, now).await.unwrap().is_some());

        // Revoke is idempotent: first call flips it, second is a no-op.
        assert!(repo.revoke(id).await.unwrap(), "first revoke applies");
        assert!(!repo.revoke(id).await.unwrap(), "second revoke is a no-op");

        // A revoked invite no longer resolves, and cannot be incremented.
        assert!(repo.find_active_by_token_hash(&hash, now).await.unwrap().is_none());
        assert!(!repo.increment_use(id).await.unwrap(), "revoked invite increments nothing");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn invitation_expired_is_not_active() {
        let p = pool();
        let repo = InvitationRepo::new(p.clone());
        let owner = new_participant(&p).await;
        let ws = new_workspace(&p, owner).await;

        let token = generate_token();
        let hash = hash_token(&token);
        // Already expired one hour ago.
        let past = OffsetDateTime::now_utc() - time::Duration::hours(1);
        repo.create(ws, &hash, None, WorkspaceRole::Member, Some(owner), None, Some(past))
            .await
            .expect("create expired invite");

        let now = OffsetDateTime::now_utc();
        assert!(
            repo.find_active_by_token_hash(&hash, now).await.unwrap().is_none(),
            "expired invite is not active"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn invitation_list_is_workspace_scoped_and_newest_first() {
        let p = pool();
        let repo = InvitationRepo::new(p.clone());
        let owner = new_participant(&p).await;
        let ws_a = new_workspace(&p, owner).await;
        let ws_b = new_workspace(&p, owner).await;

        let id1 = repo
            .create(ws_a, &hash_token(&generate_token()), None, WorkspaceRole::Member, Some(owner), None, None)
            .await
            .unwrap();
        let id2 = repo
            .create(ws_a, &hash_token(&generate_token()), None, WorkspaceRole::Guest, Some(owner), None, None)
            .await
            .unwrap();
        // A B-scoped invite must never appear in A's listing.
        repo.create(ws_b, &hash_token(&generate_token()), None, WorkspaceRole::Member, Some(owner), None, None)
            .await
            .unwrap();

        let a_list = repo.list_for_workspace(ws_a).await.unwrap();
        assert_eq!(a_list.len(), 2, "only A's two invites");
        assert!(a_list.iter().all(|i| i.workspace_id == ws_a), "strictly A-scoped");
        // Newest first: id2 (created later) precedes id1.
        assert_eq!(a_list[0].id, id2);
        assert_eq!(a_list[1].id, id1);
    }
}
