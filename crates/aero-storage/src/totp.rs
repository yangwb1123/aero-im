//! TOTP (RFC 6238) secret repository — per-participant authenticator-app 2FA.
//!
//! Backs `migrations/0044_totp_secrets.sql`. One row per participant who has
//! enrolled an authenticator app: the shared base32 secret plus an `activated`
//! flag. Enrollment ([`TotpRepo::upsert_secret`]) stores the secret with
//! `activated = false`; the participant proves possession of the app by
//! submitting a current code, which flips activation ([`TotpRepo::activate`]).
//! Re-enrolling overwrites the secret and resets activation, so a half-finished
//! enrollment can always be restarted cleanly.
//!
//! The TOTP crypto (code derivation/verification) lives in
//! [`aero_auth::totp`](../../aero_auth/totp/index.html); this repo owns only the
//! per-participant secret + activation state. Keyed solely by `participant_id`
//! (the table's primary key), so it carries no surrogate id and no model struct —
//! it returns the bare secret / activation booleans. Purely additive: a NEW
//! [`TotpRepo`]; no existing repo is touched.

use aero_common::{ParticipantId, RoomId};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

/// Failures from a TOTP mutation that can change effective channel governance.
#[derive(Debug, thiserror::Error)]
pub enum TotpWriteError {
    #[error("channel {0} needs another effective owner before disabling 2FA")]
    ChannelOwnerProtected(RoomId),
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

/// Repository over the `totp_secrets` table (per-participant 2FA state).
///
/// Cheap to clone — it just wraps a [`PgPool`] (itself an `Arc` internally), so
/// feature modules build one inline via [`TotpRepo::new`].
#[derive(Clone)]
#[must_use]
pub struct TotpRepo {
    pool: PgPool,
}

impl TotpRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Enroll (or re-enroll) `participant` with a new shared `secret`. Idempotent
    /// by primary key: an existing row is overwritten and its activation **reset**
    /// (`activated = false`, fresh `created_at`, cleared `activated_at`).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the upsert.
    pub async fn upsert_secret(
        &self,
        participant: ParticipantId,
        secret: &str,
    ) -> Result<(), TotpWriteError> {
        let mut tx = self.pool.begin().await?;
        Self::upsert_secret_in_tx(&mut tx, participant, secret).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Transaction-scoped enroll body (B5-1 route path): the pool method's
    /// body minus begin/commit — the governance lock + re-enroll guard stay.
    /// An upsert ALWAYS writes, so the route appends the pair unconditionally.
    ///
    /// # Errors
    /// [`TotpWriteError::ChannelOwnerProtected`] on the same guard as the pool
    /// method; otherwise any [`sqlx::Error`].
    pub async fn upsert_secret_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        participant: ParticipantId,
        secret: &str,
    ) -> Result<(), TotpWriteError> {
        lock_totp_governance_scope(tx, participant).await?;
        let was_activated = sqlx::query_scalar::<_, bool>(
            "SELECT activated FROM totp_secrets WHERE participant_id = $1 FOR UPDATE",
        )
        .bind(participant.to_uuid())
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or(false);
        if was_activated {
            assert_totp_can_become_ineffective(tx, participant).await?;
        }
        sqlx::query(
            r"INSERT INTO totp_secrets (participant_id, secret)
               VALUES ($1, $2)
               ON CONFLICT (participant_id) DO UPDATE
                 SET secret = $2, activated = false, created_at = now(), activated_at = NULL",
        )
        .bind(participant.to_uuid())
        .bind(secret)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Fetch `participant`'s stored TOTP secret, or `None` if they have not
    /// enrolled.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn get_secret(
        &self,
        participant: ParticipantId,
    ) -> Result<Option<String>, sqlx::Error> {
        let row = sqlx::query_as::<_, (String,)>(
            "SELECT secret FROM totp_secrets WHERE participant_id = $1",
        )
        .bind(participant.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(secret,)| secret))
    }

    /// Whether `participant` has an **activated** 2FA enrollment (the login-time
    /// gate). `false` when there is no enrollment at all or it is still pending.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the query.
    pub async fn is_activated(&self, participant: ParticipantId) -> Result<bool, sqlx::Error> {
        let row = sqlx::query_as::<_, (bool,)>(
            "SELECT activated FROM totp_secrets WHERE participant_id = $1",
        )
        .bind(participant.to_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some_and(|(activated,)| activated))
    }

    /// Mark `participant`'s pending enrollment as activated. Returns `true` iff
    /// a not-yet-activated row flipped; a second call (or a call without an
    /// enrollment) is a no-op returning `false`.
    ///
    /// Security-event audit: a successful activation commits an
    /// `auth.totp.enroll` row in the SAME transaction (same-fate); a no-op
    /// audits nothing. Account-level event: nil default tenant. The B5-1 route
    /// orchestrates the governance PAIR on top of
    /// [`activate_in_tx`](Self::activate_in_tx); this pool method keeps the
    /// legacy audit-only append so existing `db_tests` pass verbatim.
    pub async fn activate(&self, participant: ParticipantId) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let activated = Self::activate_in_tx(&mut tx, participant).await?;
        if activated {
            crate::AuditRepo::append_in_tx(
                &mut tx,
                aero_common::WorkspaceId::nil(),
                Some(participant),
                crate::audit_governance::tokens::AUTH_TOTP_ENROLL,
                Some(&participant.to_string()),
                serde_json::json!({}),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(activated)
    }

    /// Transaction-scoped activation (B5-1 route path): the UPDATE only — no
    /// audit, no begin/commit; the route appends the pair only when `true`.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`].
    pub async fn activate_in_tx(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        participant: ParticipantId,
    ) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE totp_secrets SET activated = true, activated_at = now()
              WHERE participant_id = $1 AND activated = false",
        )
        .bind(participant.to_uuid())
        .execute(&mut **tx)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Remove `participant`'s 2FA enrollment entirely. Returns `true` iff a row
    /// was deleted; a second call (or a call without an enrollment) is a no-op.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn disable(&self, participant: ParticipantId) -> Result<bool, TotpWriteError> {
        let mut tx = self.pool.begin().await?;
        lock_totp_governance_scope(&mut tx, participant).await?;
        let activated = sqlx::query_scalar::<_, bool>(
            "SELECT activated FROM totp_secrets WHERE participant_id = $1 FOR UPDATE",
        )
        .bind(participant.to_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(false);
        if activated {
            assert_totp_can_become_ineffective(&mut tx, participant).await?;
        }
        let result = sqlx::query("DELETE FROM totp_secrets WHERE participant_id = $1")
            .bind(participant.to_uuid())
            .execute(&mut *tx)
            .await?;
        // Security-event audit: a successful removal commits an
        // `auth.totp.enroll` row in the SAME transaction (same-fate, after the
        // DELETE and before commit — lock ordering untouched); a no-op audits
        // nothing. Account-level event: nil default tenant. DP-1: the token
        // REPLACED `auth.totp.disabled` — the disable route stays outside the
        // §2.7 pair table (audit-only; no-miss-write monitors scope to pairs).
        if result.rows_affected() > 0 {
            crate::AuditRepo::append_in_tx(
                &mut tx,
                aero_common::WorkspaceId::nil(),
                Some(participant),
                crate::audit_governance::tokens::AUTH_TOTP_ENROLL,
                Some(&participant.to_string()),
                serde_json::json!({}),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }
}

/// Lock every current tenant and channel that can observe this participant's
/// 2FA state, before locking the TOTP row itself.
async fn lock_totp_governance_scope(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    participant: ParticipantId,
) -> Result<(), sqlx::Error> {
    // Migration 0198's statement trigger takes this fence before direct
    // UPDATE/DELETE statements lock a TOTP row.  Repository writes enter the
    // same fence explicitly so their subsequent participant-scoped locks never
    // widen a partially-held workspace set in reverse UUID order.
    sqlx::query("SELECT aero_lock_all_channel_governance()")
        .execute(&mut **tx)
        .await?;

    let workspaces = sqlx::query_scalar::<_, uuid::Uuid>(
        r"SELECT workspace.id
            FROM workspaces workspace
            JOIN workspace_members membership
              ON membership.workspace_id = workspace.id
             AND membership.participant_id = $1
           ORDER BY workspace.id
           FOR UPDATE OF workspace",
    )
    .bind(participant.to_uuid())
    .fetch_all(&mut **tx)
    .await?;
    if !workspaces.is_empty() {
        sqlx::query_scalar::<_, uuid::Uuid>(
            r"SELECT room.id
                FROM rooms room
               WHERE room.workspace_id = ANY($1)
                 AND room.kind = 'channel'
               ORDER BY room.workspace_id, room.id
               FOR UPDATE",
        )
        .bind(&workspaces)
        .fetch_all(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn assert_totp_can_become_ineffective(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    participant: ParticipantId,
) -> Result<(), TotpWriteError> {
    let room = sqlx::query_scalar::<_, uuid::Uuid>(
        r"SELECT room.id
            FROM rooms room
            JOIN workspaces workspace
              ON workspace.id = room.workspace_id
             AND workspace.require_2fa
            JOIN room_members owner_membership
              ON owner_membership.room_id = room.id
             AND owner_membership.participant_id = $1
             AND owner_membership.role = 'owner'
           WHERE room.kind = 'channel'
             AND aero_participant_has_effective_workspace_access(
                     room.workspace_id,
                     $1
                 )
             AND NOT aero_channel_has_other_effective_owner(room.id, $1)
           ORDER BY room.id
           LIMIT 1",
    )
    .bind(participant.to_uuid())
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(room) = room {
        Err(TotpWriteError::ChannelOwnerProtected(RoomId::from_uuid(
            room,
        )))
    } else {
        Ok(())
    }
}

// ───────────────────────────────────── Recovery codes (migrations 0122/0160) ──

fn hash_recovery_code(code: &str) -> String {
    let normalized = code.trim().to_ascii_uppercase();
    hex::encode(Sha256::digest(normalized.as_bytes()))
}

/// Repository over the `recovery_codes` table.
///
/// Backs `migrations/0122_recovery_codes.sql`, with plaintext-at-rest removed
/// by `migrations/0160_recovery_code_hashes.sql`. Each call to
/// [`generate`](RecoveryCodeRepo::generate) replaces any existing unused codes
/// with a fresh batch of 8 random single-use codes; a successful
/// [`verify`](RecoveryCodeRepo::verify) marks the code as used so it cannot be
/// replayed. Only normalized SHA-256 digests are stored.
#[derive(Clone)]
#[must_use]
pub struct RecoveryCodeRepo {
    pool: PgPool,
}

impl RecoveryCodeRepo {
    /// Build a repo over the given pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Generate a new batch of 8 recovery codes for `participant`, deleting any
    /// pre-existing unused ones first (at most one active batch at a time).
    /// Returns the 8 plaintext codes to show the user **once**.
    ///
    /// Security-event audit: every call replaces the batch, so an
    /// `auth.totp.recovery_codes_regenerated` row commits in the SAME
    /// transaction unconditionally. Account-level event: nil default tenant.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the database.
    pub async fn generate(&self, participant: ParticipantId) -> Result<Vec<String>, sqlx::Error> {
        // Replace the active batch atomically: a failed insert cannot strand the
        // participant with a partially-generated recovery set.
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM recovery_codes WHERE participant_id = $1 AND used_at IS NULL")
            .bind(participant.to_uuid())
            .execute(&mut *tx)
            .await?;

        let mut codes = Vec::with_capacity(8);
        for _ in 0..8u8 {
            // 16 uppercase hex characters provide 64 bits of entropy while
            // remaining practical to type from a printed recovery sheet.
            let raw = uuid::Uuid::new_v4().simple().to_string();
            let code = raw[..16].to_ascii_uppercase();
            let code_hash = hash_recovery_code(&code);
            sqlx::query("INSERT INTO recovery_codes (participant_id, code_hash) VALUES ($1, $2)")
                .bind(participant.to_uuid())
                .bind(code_hash)
                .execute(&mut *tx)
                .await?;
            codes.push(code);
        }
        crate::AuditRepo::append_in_tx(
            &mut tx,
            aero_common::WorkspaceId::nil(),
            Some(participant),
            "auth.totp.recovery_codes_regenerated",
            Some(&participant.to_string()),
            serde_json::json!({}),
        )
        .await?;
        tx.commit().await?;
        Ok(codes)
    }

    /// Verify `code` against `participant`'s unused recovery codes. If a
    /// matching unused code is found it is marked as used (consumed) and `true`
    /// is returned; otherwise returns `false` (wrong code or already used).
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the database.
    pub async fn verify(
        &self,
        participant: ParticipantId,
        code: &str,
    ) -> Result<bool, sqlx::Error> {
        let code_hash = hash_recovery_code(code);
        // Consume the one-time code ATOMICALLY: a single conditional UPDATE whose
        // `used_at IS NULL` guard IS the check. A prior SELECT-then-UPDATE was a
        // TOCTOU — two concurrent verifications of the same code both saw it unused
        // and both succeeded, honoring a one-time recovery code twice (MFA bypass).
        // Now exactly one writer's UPDATE matches the `used_at IS NULL` row; the
        // other matches no row → `false`. `RETURNING id` distinguishes consumed
        // (row) from already-used/wrong (no row).
        let consumed: Option<(uuid::Uuid,)> = sqlx::query_as(
            "UPDATE recovery_codes SET used_at = NOW() \
             WHERE participant_id = $1 AND code_hash = $2 AND used_at IS NULL \
             RETURNING id",
        )
        .bind(participant.to_uuid())
        .bind(code_hash)
        .fetch_optional(&self.pool)
        .await?;

        Ok(consumed.is_some())
    }

    /// Count how many unused recovery codes `participant` currently has.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the database.
    pub async fn count_unused(&self, participant: ParticipantId) -> Result<i64, sqlx::Error> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM recovery_codes \
             WHERE participant_id = $1 AND used_at IS NULL",
        )
        .bind(participant.to_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// Revoke every recovery code for `participant`.
    ///
    /// Called when TOTP is disabled so an old batch cannot silently become valid
    /// again after a later re-enrollment.
    ///
    /// # Errors
    /// Propagates any [`sqlx::Error`] from the delete.
    pub async fn delete_all(&self, participant: ParticipantId) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM recovery_codes WHERE participant_id = $1")
            .bind(participant.to_uuid())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::hash_recovery_code;

    #[test]
    fn recovery_code_hash_normalizes_case_and_surrounding_space() {
        let digest = hash_recovery_code("A1B2C3D4E5F60708");
        assert_eq!(digest, hash_recovery_code("  a1b2c3d4e5f60708  "));
        assert_ne!(digest, "A1B2C3D4E5F60708");
        assert_eq!(digest.len(), 64);
    }
}

/// PG-gated integration tests (run with a live Postgres + applied migrations):
///
/// ```text
/// DATABASE_URL=postgres://aero:aero_dev_pw@localhost:5432/aero \
///   cargo test -p aero-storage --lib -- --ignored totp
/// ```
#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::{RoomMemberRole, RoomRepo, WorkspaceRepo};
    use aero_common::{RoomKind, WorkspaceRole};

    fn pool() -> PgPool {
        let url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://aero:aero_dev_pw@localhost:5432/aero".into());
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(&url)
            .expect("connect_lazy never fails on a well-formed URL")
    }

    /// Create a throwaway participant so the test is self-contained.
    async fn mk_participant(p: &PgPool) -> ParticipantId {
        let id = ParticipantId::new();
        sqlx::query("INSERT INTO participants (id, kind, display_name) VALUES ($1, 'human', $2)")
            .bind(id.to_uuid())
            .bind(format!("totp-owner-{id}"))
            .execute(p)
            .await
            .expect("insert participant");
        id
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn totp_upsert_get_activate_disable() {
        let p = pool();
        let repo = TotpRepo::new(p.clone());
        let owner = mk_participant(&p).await;

        // No enrollment initially.
        assert!(
            repo.get_secret(owner).await.unwrap().is_none(),
            "no secret yet"
        );
        assert!(
            !repo.is_activated(owner).await.unwrap(),
            "not activated yet"
        );
        assert!(
            !repo.activate(owner).await.unwrap(),
            "activate without enrollment is a no-op"
        );

        // upsert → get reflects the secret; still pending (not activated).
        repo.upsert_secret(owner, "JBSWY3DPEHPK3PXP").await.unwrap();
        assert_eq!(
            repo.get_secret(owner).await.unwrap().as_deref(),
            Some("JBSWY3DPEHPK3PXP"),
            "stored secret round-trips"
        );
        assert!(
            !repo.is_activated(owner).await.unwrap(),
            "fresh enrollment is pending"
        );

        // activate flips is_activated; a second activate is a no-op.
        assert!(repo.activate(owner).await.unwrap(), "first activate flips");
        assert!(repo.is_activated(owner).await.unwrap(), "now activated");
        assert!(
            !repo.activate(owner).await.unwrap(),
            "second activate is a no-op"
        );

        // Re-enroll overwrites the secret and resets activation.
        repo.upsert_secret(owner, "MFRGGZDFMZTWQ2LK").await.unwrap();
        assert_eq!(
            repo.get_secret(owner).await.unwrap().as_deref(),
            Some("MFRGGZDFMZTWQ2LK"),
            "re-enroll overwrites the secret"
        );
        assert!(
            !repo.is_activated(owner).await.unwrap(),
            "re-enroll resets activation"
        );

        // disable removes the row; a second disable is a no-op.
        assert!(
            repo.disable(owner).await.unwrap(),
            "disable removes the enrollment"
        );
        assert!(
            repo.get_secret(owner).await.unwrap().is_none(),
            "no secret after disable"
        );
        assert!(
            !repo.is_activated(owner).await.unwrap(),
            "not activated after disable"
        );
        assert!(
            !repo.disable(owner).await.unwrap(),
            "second disable is a no-op"
        );

        // Security-event audit (AC1c): the first activate committed exactly one
        // `auth.totp.enroll` row and the disable exactly one (nil workspace,
        // actor = target = owner); activate-without-enrollment, the second
        // activate, the second disable, and the re-enroll (`upsert_secret` —
        // deliberately untouched) added ZERO rows. Counts scoped by actor_id
        // (sibling tests). DP-1: replaced `auth.totp.enabled`/`.disabled`.
        let enabled: (String, String, String) = sqlx::query_as(
            "SELECT workspace_id::text, actor_id::text, target
               FROM audit_events
              WHERE action = 'auth.totp.enroll' AND actor_id = $1",
        )
        .bind(owner.to_uuid())
        .fetch_one(&p)
        .await
        .expect("exactly one auth.totp.enroll row");
        assert_eq!(enabled.0, "00000000-0000-0000-0000-000000000000", "nil workspace");
        assert_eq!(enabled.1, owner.to_uuid().to_string(), "actor = owner");
        assert_eq!(enabled.2, owner.to_string(), "target = owner");
        let disabled: (String, String, String) = sqlx::query_as(
            "SELECT workspace_id::text, actor_id::text, target
               FROM audit_events
              WHERE action = 'auth.totp.enroll' AND actor_id = $1",
        )
        .bind(owner.to_uuid())
        .fetch_one(&p)
        .await
        .expect("exactly one auth.totp.enroll row (disable)");
        assert_eq!(disabled.0, "00000000-0000-0000-0000-000000000000", "nil workspace");
        assert_eq!(disabled.1, owner.to_uuid().to_string(), "actor = owner");
        assert_eq!(disabled.2, owner.to_string(), "target = owner");
        let enabled_total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_events WHERE action = 'auth.totp.enroll' AND actor_id = $1",
        )
        .bind(owner.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        let disabled_total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_events WHERE action = 'auth.totp.enroll' AND actor_id = $1",
        )
        .bind(owner.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(
            (enabled_total, disabled_total),
            (2, 2),
            "no-op activate/disable/re-enroll add no audit rows (2 = activate + disable)"
        );

        // Cleanup (FK NO ACTION, 0007): audit rows reference the participant.
        sqlx::query("DELETE FROM audit_events WHERE actor_id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .expect("delete audit rows before the participant");
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .expect("delete participant");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres with migrations"]
    async fn active_2fa_cannot_be_removed_from_the_sole_effective_channel_owner() {
        let p = pool();
        let repo = TotpRepo::new(p.clone());
        let workspaces = WorkspaceRepo::new(p.clone());
        let rooms = RoomRepo::new(p.clone());
        let workspace_owner = mk_participant(&p).await;
        let channel_owner = mk_participant(&p).await;
        let successor = mk_participant(&p).await;
        let workspace = workspaces
            .create(
                format!("totp-governance-{workspace_owner}"),
                format!("totp-governance-{workspace_owner}"),
                workspace_owner,
            )
            .await
            .unwrap()
            .id;
        for participant in [channel_owner, successor] {
            workspaces
                .add_member(workspace, participant, WorkspaceRole::Member)
                .await
                .unwrap();
        }
        let room = rooms
            .create_in_workspace(
                workspace,
                RoomKind::Channel,
                Some(format!("totp-governance-{channel_owner}")),
                channel_owner,
            )
            .await
            .unwrap()
            .id;
        rooms.add_member(room, successor).await.unwrap();
        for participant in [workspace_owner, channel_owner, successor] {
            repo.upsert_secret(participant, "JBSWY3DPEHPK3PXP")
                .await
                .unwrap();
            assert!(repo.activate(participant).await.unwrap());
        }
        workspaces
            .set_require_2fa_authorized(workspace, true, workspace_owner)
            .await
            .unwrap();

        assert!(matches!(
            repo.disable(channel_owner)
                .await
                .expect_err("disabling TOTP would orphan the channel"),
            TotpWriteError::ChannelOwnerProtected(protected) if protected == room
        ));
        assert!(matches!(
            repo.upsert_secret(channel_owner, "MFRGGZDFMZTWQ2LK")
                .await
                .expect_err("re-enrollment also resets activation"),
            TotpWriteError::ChannelOwnerProtected(protected) if protected == room
        ));
        assert!(repo.is_activated(channel_owner).await.unwrap());

        rooms
            .change_channel_member_role_authorized(
                room,
                channel_owner,
                successor,
                RoomMemberRole::Owner,
            )
            .await
            .unwrap();
        assert!(repo.disable(channel_owner).await.unwrap());
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn recovery_codes_are_hashed_and_consumed_once_under_race() {
        let p = pool();
        let repo = RecoveryCodeRepo::new(p.clone());
        let owner = mk_participant(&p).await;
        let stranger = mk_participant(&p).await;

        let codes = repo.generate(owner).await.unwrap();
        assert_eq!(codes.len(), 8);
        assert!(codes.iter().all(|code| code.len() == 16));
        assert_eq!(repo.count_unused(owner).await.unwrap(), 8);

        let stored: (String,) = sqlx::query_as(
            "SELECT code_hash FROM recovery_codes WHERE participant_id = $1 LIMIT 1",
        )
        .bind(owner.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(stored.0.len(), 64);
        assert!(!codes.contains(&stored.0), "database contains only digests");

        let code = codes[0].clone();
        let lower_code = code.to_ascii_lowercase();
        let (first, second) =
            tokio::join!(repo.verify(owner, &code), repo.verify(owner, &lower_code));
        let successes = usize::from(first.unwrap()) + usize::from(second.unwrap());
        assert_eq!(successes, 1, "a recovery code is honored exactly once");
        assert!(
            !repo.verify(owner, &code).await.unwrap(),
            "replay is rejected"
        );
        assert_eq!(repo.count_unused(owner).await.unwrap(), 7);

        assert!(
            !repo.verify(stranger, &codes[1]).await.unwrap(),
            "a code is scoped to its owner"
        );
        assert!(
            repo.verify(owner, &codes[1]).await.unwrap(),
            "wrong-owner attempt does not consume the code"
        );

        // Cleanup (FK NO ACTION, 0007): audit rows reference the participants.
        sqlx::query("DELETE FROM audit_events WHERE actor_id = ANY($1)")
            .bind([owner.to_uuid(), stranger.to_uuid()])
            .execute(&p)
            .await
            .expect("delete audit rows before the participants");
        sqlx::query("DELETE FROM participants WHERE id = ANY($1)")
            .bind([owner.to_uuid(), stranger.to_uuid()])
            .execute(&p)
            .await
            .expect("delete participants");
    }

    #[tokio::test]
    #[ignore = "requires live Postgres"]
    async fn recovery_code_generation_audits_regeneration_each_time() {
        let p = pool();
        let repo = RecoveryCodeRepo::new(p.clone());
        let owner = mk_participant(&p).await;

        // Every call replaces the batch — a backup-factor rotation — so EACH
        // call commits exactly one `auth.totp.recovery_codes_regenerated` row
        // (AC1e; unconditional, no no-op negative).
        repo.generate(owner).await.unwrap();
        repo.generate(owner).await.unwrap();
        let row: (String, String, String) = sqlx::query_as(
            "SELECT workspace_id::text, actor_id::text, target
               FROM audit_events
              WHERE action = 'auth.totp.recovery_codes_regenerated' AND actor_id = $1",
        )
        .bind(owner.to_uuid())
        .fetch_one(&p)
        .await
        .expect("exactly one recovery-codes row per generate");
        assert_eq!(row.0, "00000000-0000-0000-0000-000000000000", "nil workspace");
        assert_eq!(row.1, owner.to_uuid().to_string(), "actor = owner");
        assert_eq!(row.2, owner.to_string(), "target = owner");
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_events \
              WHERE action = 'auth.totp.recovery_codes_regenerated' AND actor_id = $1",
        )
        .bind(owner.to_uuid())
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(total, 2, "a second generate (batch rotation) emits a second row");

        // Cleanup (FK NO ACTION, 0007).
        sqlx::query("DELETE FROM audit_events WHERE actor_id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .expect("delete audit rows before the participant");
        sqlx::query("DELETE FROM participants WHERE id = $1")
            .bind(owner.to_uuid())
            .execute(&p)
            .await
            .expect("delete participant");
    }
}
